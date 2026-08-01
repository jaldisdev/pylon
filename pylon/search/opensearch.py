#
# This source file is part of the Pylon open source project.
#
# Copyright (c) 2026 Jaldis B.V.
#
# Licensed under the MIT OR Apache-2.0 license (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://opensource.org/licenses/MIT
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

from __future__ import annotations

from typing import Any

import httpx


class OpenSearchClient:
    """Thin async wrapper around the OpenSearch REST API.

    Covers the three operations needed by Pylon:
    - ``search``   → full-text query, returns (id, score) pairs
    - ``index``    → upsert a document
    - ``delete``   → remove a document

    ``base_url`` should be e.g. ``"http://localhost:9200"``.
    ``auth`` is an optional ``(user, password)`` tuple.
    """

    def __init__(
        self,
        base_url: str,
        *,
        auth: tuple[str, str] | None = None,
        timeout: float = 10.0,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._auth = auth
        self._timeout = timeout
        self._client: httpx.AsyncClient | None = None

    async def __aenter__(self) -> "OpenSearchClient":
        self._client = httpx.AsyncClient(
            auth=self._auth,
            timeout=self._timeout,
        )
        return self

    async def __aexit__(self, *_: object) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None

    def _http(self) -> httpx.AsyncClient:
        if self._client is None:
            raise RuntimeError("OpenSearchClient must be used as an async context manager")
        return self._client

    async def search(
        self,
        index: str,
        query_text: str,
        *,
        fields: list[str] | None = None,
        size: int = 10,
    ) -> list[tuple[str, float]]:
        """Full-text search; returns ``[(id, score)]`` ordered by relevance."""
        body: dict[str, Any] = {
            "query": {
                "multi_match": {
                    "query": query_text,
                    "fields": fields or ["*"],
                    "type": "best_fields",
                }
            },
            "_source": False,
            "size": size,
        }
        url = f"{self._base_url}/{index}/_search"
        resp = await self._http().post(url, json=body)
        resp.raise_for_status()
        hits = resp.json()["hits"]["hits"]
        return [(h["_id"], h["_score"]) for h in hits]

    async def index_document(
        self,
        index: str,
        doc_id: str,
        fields: dict[str, Any],
    ) -> None:
        """Upsert a document into the OpenSearch index."""
        url = f"{self._base_url}/{index}/_doc/{doc_id}"
        resp = await self._http().put(url, json=fields)
        resp.raise_for_status()

    async def delete_document(self, index: str, doc_id: str) -> None:
        """Remove a document from the OpenSearch index (no-op if not found)."""
        url = f"{self._base_url}/{index}/_doc/{doc_id}"
        resp = await self._http().delete(url)
        if resp.status_code != 404:
            resp.raise_for_status()

    async def create_index(
        self,
        index: str,
        mappings: dict[str, Any] | None = None,
    ) -> None:
        """Create the OpenSearch index if it does not already exist."""
        url = f"{self._base_url}/{index}"
        body: dict[str, Any] = {}
        if mappings:
            body["mappings"] = mappings
        resp = await self._http().put(url, json=body)
        if resp.status_code == 400:
            data = resp.json()
            if "resource_already_exists_exception" in data.get("error", {}).get("type", ""):
                return
        resp.raise_for_status()
