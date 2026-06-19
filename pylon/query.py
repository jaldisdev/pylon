from __future__ import annotations

from pylon._core import (
    CompiledQuery,
    SchemaDescriptor,
    compile as _core_compile,
    deserialize as _core_deserialize,
)

_singleton: SchemaDescriptor | None = None


def compile(query: str, *, schema: SchemaDescriptor | None = None) -> CompiledQuery:
    """Compile a PyQL string to SQL. Raises PyQLError (or a subclass) on failure.

    If schema is omitted, falls back to the process-level singleton SchemaDescriptor.
    Synchronous — compilation is CPU-bound; async lives at the DB execution layer.
    """
    return _core_compile(query, schema if schema is not None else _get_singleton())


def deserialize(
    records: list,
    query: CompiledQuery,
    registry: object,
) -> list:
    """Decode asyncpg Records into Python objects using the shape embedded in query.

    The top-level result is always a list since PyQL select is always set-valued.
    """
    return _core_deserialize(records, query, registry)


def _get_singleton() -> SchemaDescriptor:
    if _singleton is None:
        raise RuntimeError(
            "No SchemaDescriptor singleton has been installed. "
            "Either pass schema= explicitly or call pylon.query._set_singleton() "
            "from your framework startup hook (AppConfig.ready() / ASGI lifespan)."
        )
    return _singleton


def _set_singleton(schema: SchemaDescriptor) -> None:
    """Install the process-level SchemaDescriptor singleton.

    Called by framework startup hooks to eagerly set the singleton before
    any queries are compiled.
    """
    global _singleton
    _singleton = schema
