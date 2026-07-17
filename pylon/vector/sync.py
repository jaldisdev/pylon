from __future__ import annotations

import itertools
import logging
from collections import defaultdict
from typing import Any

from pylon._core import compile_index_fetch
from pylon.worker import IndexKind, IndexWorker

log = logging.getLogger(__name__)

WRITE_VECTOR_SQL_TPL = """
UPDATE {table}
SET {col} = $2::vector
WHERE "id" = $1
"""


class VectorIndexWorker(IndexWorker):
    """Processes ``IndexOutbox`` rows with ``index_kind = 'Vector'``.

    Requires one embedding provider per ``(type_name, index_name)`` pair —
    pass them as a mapping in ``providers``::

        worker = VectorIndexWorker(
            conn,
            schema=schema,
            providers={("default::Product", None): MistralProvider()},
        )

    ``index_name=None`` targets the default (bare) index.
    """

    index_kind = IndexKind.VECTOR

    def __init__(
        self,
        conn: Any,
        *,
        schema: Any,
        providers: dict[tuple[str, str | None], Any],
    ) -> None:
        super().__init__(conn)
        self._schema = schema
        self._providers = providers
        # Pre-compile fetch SQL keyed by (type_name, index_name).
        self._fetch_sql: dict[tuple[str, str | None], str] = {}
        for type_name, index_name in providers:
            self._fetch_sql[(type_name, index_name)] = compile_index_fetch(
                type_name, schema, index_name=index_name
            )

    def _table_and_col(self, type_name: str, index_name: str | None) -> tuple[str, str]:
        td = next(
            (t for t in self._schema.types
             if f"{t.module}::{t.name}" == type_name),
            None,
        )
        if td is None:
            raise ValueError(f"VectorIndexWorker: unknown type '{type_name}'")
        vi = next(
            (v for v in td.vector_indexes if v.index_name == index_name),
            None,
        )
        if vi is None:
            key = index_name or "<default>"
            raise ValueError(
                f"VectorIndexWorker: no VectorIndex '{key}' on type '{type_name}'"
            )
        pg_schema = "public" if td.module == "default" else td.module
        table = f'"{pg_schema}"."{td.table}"'
        col = f'"{vi.column_name}"'
        return table, col

    async def process_batch(self, rows: list[Any]) -> None:
        groups: dict[tuple[str, str | None], list[Any]] = defaultdict(list)
        for row in rows:
            key = (row["type_name"], row["index_name"])
            groups[key].append(row)

        for (type_name, index_name), group_rows in groups.items():
            provider = self._providers.get((type_name, index_name))
            if provider is None:
                log.warning(
                    "VectorIndexWorker: no provider for (%s, %s); skipping",
                    type_name,
                    index_name,
                )
                continue

            fetch_sql = self._fetch_sql.get((type_name, index_name))
            if fetch_sql is None:
                log.warning(
                    "VectorIndexWorker: no fetch SQL for (%s, %s); skipping",
                    type_name,
                    index_name,
                )
                continue

            ids = [r["object_id"] for r in group_rows]
            records = await self._conn.query_named(fetch_sql, [ids])
            if not records:
                continue

            texts = [r["source_text"] for r in records]
            vectors = await provider.embed_batch(texts)

            table, col = self._table_and_col(type_name, index_name)
            write_sql = f'UPDATE {table} SET {col} = $2::vector WHERE "id" = $1'
            for r, vec in zip(records, vectors):
                vec_str = f"[{','.join(str(x) for x in vec)}]"
                await self._conn.execute(write_sql, [r["id"], vec_str])
