from __future__ import annotations

from typing import TYPE_CHECKING, Any

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
    registry: dict[str, type],
) -> list:
    """Decode asyncpg Records into Python objects using the shape embedded in query.

    Each record must have a ``result`` column containing the anonymous PostgreSQL
    record tuple produced by the compiled SQL.  The shape descriptor in ``query``
    drives the decoding; ``registry`` maps short type names to dataclass types.
    """
    shape = query.shape
    return [_decode(record["result"], shape, registry) for record in records]


def _decode(value: Any, node: dict, registry: dict[str, type]) -> Any:
    kind = node["kind"]

    if kind == "scalar":
        return value[node["position"]]

    if kind in ("raw_scalar", "json_scalar"):
        return value

    if kind == "object":
        pos = node["position"]
        # Root object sits at the top level; nested objects are at a tuple position.
        obj_tuple = value if pos == 0 else value[pos]
        if obj_tuple is None:
            return None
        fields = node["fields"]
        # fields[0] is always the auto-injected __type__ discriminator (position 0); skip it.
        # Explicit __type__ requested by the user appears at position > 0 and is included.
        kwargs = {
            f["name"]: _decode(obj_tuple, f, registry)
            for f in fields
            if not (f["name"] == "__type__" and f["position"] == 0)
        }
        # Use the actual per-row __type__ value (obj_tuple[0]) for class lookup.
        # For concrete types it equals the static type_name; for polymorphic (interface)
        # queries it gives the real concrete type.
        actual_type = obj_tuple[0] if obj_tuple else None
        type_name = actual_type or node.get("type_name")
        if type_name:
            short = type_name.split("::")[-1]
            cls = registry.get(short)
            if cls is not None:
                obj = object.__new__(cls)
                obj.__dict__.update(kwargs)
                obj.__dict__["__pylon_type__"] = type_name
                return obj
        return kwargs

    if kind == "named_tuple":
        pos = node.get("position", 0)
        # Root-level named tuples arrive as the raw decoded jsonb dict; nested ones
        # sit at a positional index inside the parent composite row.
        raw = value if (value is None or isinstance(value, dict)) else value[pos]
        if raw is None:
            return None
        type_name = node.get("type_name")
        if type_name and isinstance(raw, dict):
            cls = registry.get(type_name)
            if cls is not None:
                return cls(**raw)
        return raw

    if kind == "enum":
        raw = value[node["position"]]
        if raw is None:
            return None
        enum_type = node["enum_type"]
        short = enum_type.split("::")[-1]
        cls = registry.get(enum_type) or registry.get(short)
        if cls is not None:
            return cls(raw)
        return raw

    if kind == "array":
        arr = value[node["position"]] or []
        element = node["element"]
        # Array elements are anonymous records; decode each one as a root object.
        return [_decode(item, {**element, "position": 0}, registry) for item in arr]

    if kind == "tuple":
        return tuple(_decode(value, e, registry) for e in node["elements"])

    if kind == "group":
        key_nodes = node["key_nodes"]
        key_obj = {kn["name"]: _decode(value, kn, registry) for kn in key_nodes}
        grouping = list(value[node["grouping_position"]] or [])
        elements = [
            _decode(item, {**node["element"], "position": 0}, registry)
            for item in (value[node["elements_position"]] or [])
        ]
        return {"key": key_obj, "grouping": grouping, "elements": elements}

    raise ValueError(f"unknown shape node kind: {kind!r}")


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
