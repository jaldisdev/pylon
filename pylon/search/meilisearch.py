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


class MeilisearchClient:
    """Thin async wrapper around the Meilisearch REST API.

    Covers the three operations needed by Pylon:
    - ``search``         → full-text query, returns (id, score) pairs
    - ``index_document`` → upsert a document
    - ``delete_document``→ remove a document

    ``base_url`` should be e.g. ``"http://localhost:7700"``.
    ``api_key`` is optional (required when Meilisearch is started with a master key).
    """

    def __init__(
        self,
        base_url: str,
        *,
        api_key: str | None = None,
        timeout: float = 10.0,
    ) -> None:
        self._base_url = base_url.rstrip('/')
        self._api_key = api_key
        self._timeout = timeout
        self._client: httpx.AsyncClient | None = None

    async def __aenter__(self) -> MeilisearchClient:
        headers: dict[str, str] = {}
        if self._api_key:
            headers['Authorization'] = f'Bearer {self._api_key}'
        self._client = httpx.AsyncClient(headers=headers, timeout=self._timeout)
        return self

    async def __aexit__(self, *_: object) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None

    def _http(self) -> httpx.AsyncClient:
        if self._client is None:
            raise RuntimeError('MeilisearchClient must be used as an async context manager')
        return self._client

    async def search(
        self,
        index: str,
        query_text: str,
        *,
        size: int = 10,
    ) -> list[tuple[str, float]]:
        """Full-text search; returns ``[(id, score)]`` ordered by relevance."""
        url = f'{self._base_url}/indexes/{index}/search'
        resp = await self._http().post(
            url,
            json={'q': query_text, 'limit': size, 'showRankingScore': True},
        )
        resp.raise_for_status()
        hits = resp.json()['hits']
        return [(str(h['id']), h.get('_rankingScore', 1.0)) for h in hits]

    async def index_document(
        self,
        index: str,
        doc_id: str,
        fields: dict[str, Any],
    ) -> None:
        """Upsert a document into the Meilisearch index."""
        url = f'{self._base_url}/indexes/{index}/documents'
        resp = await self._http().put(url, json=[{'id': doc_id, **fields}])
        resp.raise_for_status()

    async def delete_document(self, index: str, doc_id: str) -> None:
        """Remove a document from the Meilisearch index (no-op if not found)."""
        url = f'{self._base_url}/indexes/{index}/documents/{doc_id}'
        resp = await self._http().delete(url)
        if resp.status_code != 404:
            resp.raise_for_status()

    async def create_index(self, index: str) -> None:
        """Create the Meilisearch index if it does not already exist."""
        url = f'{self._base_url}/indexes'
        resp = await self._http().post(url, json={'uid': index, 'primaryKey': 'id'})
        if resp.status_code not in (200, 201, 202):
            data = resp.json()
            if data.get('code') != 'index_already_exists':
                resp.raise_for_status()
