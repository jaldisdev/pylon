from __future__ import annotations

import logging
from collections import defaultdict
from typing import Any

from pylon._core import compile_search_index_fetch
from pylon.worker import IndexKind, IndexWorker

from .opensearch import OpenSearchClient

log = logging.getLogger(__name__)


class OpenSearchWorker(IndexWorker):
    """Processes ``IndexOutbox`` rows with ``index_kind = 'OpenSearch'``.

    Requires one ``OpenSearchClient`` (shared) and the Pylon schema descriptor.
    Each batch is split by (type_name, index_name) and dispatched to
    ``OpenSearchClient.index_document`` / ``delete_document``.

    Usage::

        os_client = OpenSearchClient("http://localhost:9200")
        async with os_client:
            worker = OpenSearchWorker(conn, schema=schema, client=os_client)
            await worker.run()
    """

    index_kind = IndexKind.OPEN_SEARCH

    def __init__(
        self,
        conn: Any,
        *,
        schema: Any,
        client: OpenSearchClient,
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
                log.warning("OpenSearchWorker: cannot compile fetch SQL for (%s, %s)", type_name, index_name)
                return None
        return self._fetch_sql[key]

    def _deferred_index_name(self, type_name: str, index_name: str | None) -> str:
        """Derive index name from type + (optional) index name."""
        module, _, tname = type_name.rpartition("::")
        base = f"{module}__{tname}".lower()
        return f"{base}__{index_name.lower()}" if index_name else base

    def _search_index_fields(self, type_name: str, index_name: str | None) -> list[str]:
        td = next(
            (t for t in self._schema.types
             if f"{t.module}::{t.name}" == type_name),
            None,
        )
        if td is None:
            return []
        si = next(
            (s for s in td.search_indexes if s.index_name == index_name),
            None,
        )
        return [f.name for f in si.fields] if si else []

    async def process_batch(self, rows: list[Any]) -> None:
        groups: dict[tuple[str, str | None, str], list[Any]] = defaultdict(list)
        for row in rows:
            op = row.get("operation", "index")
            key = (row["type_name"], row["index_name"], op)
            groups[key].append(row)

        for (type_name, index_name, operation), group_rows in groups.items():
            os_index = self._deferred_index_name(type_name, index_name)

            if operation == "delete":
                for row in group_rows:
                    try:
                        await self._client.delete_document(os_index, str(row["object_id"]))
                    except Exception:
                        log.exception(
                            "OpenSearchWorker: delete_document failed for %s/%s",
                            os_index, row["object_id"],
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
                # source_text is a concatenation; split into named fields if possible
                source_text = record["source_text"] or ""
                if field_names and "\n" in source_text:
                    parts = source_text.split("\n", maxsplit=len(field_names) - 1)
                    doc_body = {name: part for name, part in zip(field_names, parts)}
                else:
                    doc_body = {"text": source_text}
                try:
                    await self._client.index_document(os_index, doc_id, doc_body)
                except Exception:
                    log.exception(
                        "OpenSearchWorker: index_document failed for %s/%s",
                        os_index, doc_id,
                    )
                    raise
