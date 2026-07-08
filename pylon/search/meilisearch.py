from __future__ import annotations

import logging
from collections import defaultdict
from typing import Any

import httpx

from pylon._core import compile_search_index_fetch
from pylon.worker import IndexKind, IndexWorker

log = logging.getLogger(__name__)


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
        self._base_url = base_url.rstrip("/")
        self._api_key = api_key
        self._timeout = timeout
        self._client: httpx.AsyncClient | None = None

    async def __aenter__(self) -> "MeilisearchClient":
        headers: dict[str, str] = {}
        if self._api_key:
            headers["Authorization"] = f"Bearer {self._api_key}"
        self._client = httpx.AsyncClient(headers=headers, timeout=self._timeout)
        return self

    async def __aexit__(self, *_: object) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None

    def _http(self) -> httpx.AsyncClient:
        if self._client is None:
            raise RuntimeError("MeilisearchClient must be used as an async context manager")
        return self._client

    async def search(
        self,
        index: str,
        query_text: str,
        *,
        size: int = 10,
    ) -> list[tuple[str, float]]:
        """Full-text search; returns ``[(id, score)]`` ordered by relevance."""
        url = f"{self._base_url}/indexes/{index}/search"
        resp = await self._http().post(
            url,
            json={"q": query_text, "limit": size, "showRankingScore": True},
        )
        resp.raise_for_status()
        hits = resp.json()["hits"]
        return [(str(h["id"]), h.get("_rankingScore", 1.0)) for h in hits]

    async def index_document(
        self,
        index: str,
        doc_id: str,
        fields: dict[str, Any],
    ) -> None:
        """Upsert a document into the Meilisearch index."""
        url = f"{self._base_url}/indexes/{index}/documents"
        resp = await self._http().put(url, json=[{"id": doc_id, **fields}])
        resp.raise_for_status()

    async def delete_document(self, index: str, doc_id: str) -> None:
        """Remove a document from the Meilisearch index (no-op if not found)."""
        url = f"{self._base_url}/indexes/{index}/documents/{doc_id}"
        resp = await self._http().delete(url)
        if resp.status_code != 404:
            resp.raise_for_status()

    async def create_index(self, index: str) -> None:
        """Create the Meilisearch index if it does not already exist."""
        url = f"{self._base_url}/indexes"
        resp = await self._http().post(url, json={"uid": index, "primaryKey": "id"})
        if resp.status_code not in (200, 201, 202):
            data = resp.json()
            if data.get("code") != "index_already_exists":
                resp.raise_for_status()


class MeilisearchWorker(IndexWorker):
    """Processes ``IndexOutbox`` rows with ``index_kind = 'Meilisearch'``.

    Usage::

        meili = MeilisearchClient("http://localhost:7700", api_key="masterKey")
        async with meili:
            worker = MeilisearchWorker(conn, schema=schema, client=meili)
            await worker.run()
    """

    index_kind = IndexKind.MEILISEARCH

    def __init__(
        self,
        conn: Any,
        *,
        schema: Any,
        client: MeilisearchClient,
    ) -> None:
        super().__init__(conn)
        self._schema = schema
        self._client = client
        self._fetch_sql: dict[tuple[str, str | None], str] = {}

    def _ensure_fetch_sql(self, type_name: str, index_name: str | None) -> str | None:
        key = (type_name, index_name)
        if key not in self._fetch_sql:
            try:
                self._fetch_sql[key] = compile_search_index_fetch(
                    type_name, self._schema, index_name=index_name
                )
            except Exception:
                log.warning(
                    "MeilisearchWorker: cannot compile fetch SQL for (%s, %s)", type_name, index_name
                )
                return None
        return self._fetch_sql[key]

    def _deferred_index_name(self, type_name: str, index_name: str | None) -> str:
        module, _, tname = type_name.rpartition("::")
        base = f"{module}__{tname}".lower()
        return f"{base}__{index_name.lower()}" if index_name else base

    def _search_index_fields(self, type_name: str, index_name: str | None) -> list[str]:
        td = next(
            (t for t in self._schema.types if f"{t.module}::{t.name}" == type_name),
            None,
        )
        if td is None:
            return []
        si = next((s for s in td.search_indexes if s.index_name == index_name), None)
        return [f.name for f in si.fields] if si else []

    async def process_batch(self, rows: list[Any]) -> None:
        groups: dict[tuple[str, str | None, str], list[Any]] = defaultdict(list)
        for row in rows:
            op = row.get("operation", "index")
            key = (row["type_name"], row["index_name"], op)
            groups[key].append(row)

        for (type_name, index_name, operation), group_rows in groups.items():
            meili_index = self._deferred_index_name(type_name, index_name)

            if operation == "delete":
                for row in group_rows:
                    try:
                        await self._client.delete_document(meili_index, str(row["object_id"]))
                    except Exception:
                        log.exception(
                            "MeilisearchWorker: delete_document failed for %s/%s",
                            meili_index, row["object_id"],
                        )
                        raise
                continue

            fetch_sql = self._ensure_fetch_sql(type_name, index_name)
            if fetch_sql is None:
                continue

            ids = [r["object_id"] for r in group_rows]
            records = await self._conn.fetch(fetch_sql, ids)
            if not records:
                continue

            field_names = self._search_index_fields(type_name, index_name)
            for record in records:
                doc_id = str(record["id"])
                source_text = record["source_text"] or ""
                if field_names and "\n" in source_text:
                    parts = source_text.split("\n", maxsplit=len(field_names) - 1)
                    doc_body = {name: part for name, part in zip(field_names, parts)}
                else:
                    doc_body = {"text": source_text}
                try:
                    await self._client.index_document(meili_index, doc_id, doc_body)
                except Exception:
                    log.exception(
                        "MeilisearchWorker: index_document failed for %s/%s",
                        meili_index, doc_id,
                    )
                    raise
