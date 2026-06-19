from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pylon._core import CompiledQuery, SchemaDescriptor

_singleton: SchemaDescriptor | None = None


def compile(query: str, *, schema: SchemaDescriptor | None = None) -> CompiledQuery:
    """Compile a PyQL string to SQL. Raises PyQLError (or a subclass) on failure.

    If schema is omitted, falls back to the process-level singleton SchemaDescriptor.
    Synchronous — compilation is CPU-bound; async lives at the DB execution layer.
    """
    from pylon._core import compile as _core_compile

    return _core_compile(query, schema if schema is not None else _get_schema())


def deserialize(
    records: list,
    query: CompiledQuery,
    registry: object,
) -> list:
    """Decode asyncpg Records into Python objects using the shape embedded in query.

    The top-level result is always a list since PyQL select is always set-valued.
    """
    from pylon._core import deserialize as _core_deserialize

    return _core_deserialize(records, query, registry)


def _get_schema() -> SchemaDescriptor:
    if _singleton is None:
        raise RuntimeError(
            "No SchemaDescriptor singleton has been installed. "
            "Either pass schema= explicitly or call pylon.query._set_schema() "
            "from your framework startup hook (AppConfig.ready() / ASGI lifespan)."
        )
    return _singleton


def _set_schema(schema: SchemaDescriptor) -> None:
    """Install the process-level SchemaDescriptor singleton.

    Called by framework startup hooks to eagerly set the singleton before
    any queries are compiled.
    """
    global _singleton
    _singleton = schema
