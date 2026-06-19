from __future__ import annotations

from pylon._core import SchemaDescriptor
from pylon._core import export_schema as _core_export_schema


def export(*, schema: SchemaDescriptor | None = None) -> str:
    """Export the full schema as a PostgreSQL DDL string.

    If schema is omitted, falls back to the process-level singleton SchemaDescriptor.
    PyQL fragments (computed columns, constraints, mutation rewrite trigger bodies)
    are compiled to SQL inline during export. Failures surface as PyQLFragmentError.
    The returned string is valid PostgreSQL DDL ready for Atlas or direct inspection.
    """
    if schema is None:
        from pylon.query import _get_singleton
        schema = _get_singleton()
    return _core_export_schema(schema)
