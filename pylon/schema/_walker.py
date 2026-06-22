"""Schema walker: resolves, validates, and converts the collected schema to a
SchemaDescriptor (PyO3 object) ready to be handed to pylon-core.

Entry point: walk(types, enums, custom_scalars, globals_) → SchemaDescriptor
"""

from __future__ import annotations

import dataclasses
import importlib
import typing
from typing import Any

MISSING = dataclasses.MISSING

# ── Error type ─────────────────────────────────────────────────────────────────


class SchemaError(Exception):
    """Raised for schema validation failures during pylon.finalize()."""


# ── Helpers ────────────────────────────────────────────────────────────────────


def _qualified(module: str, name: str) -> str:
    return f"{module}::{name}"


def _pylon_module_of(cls: type) -> str:
    return cls.__pylon_config__.module  # type: ignore[attr-defined]


def _pylon_name_of(cls: type) -> str:
    return cls.__pylon_config__.name  # type: ignore[attr-defined]


def _is_pylon_type(cls: type) -> bool:
    return hasattr(cls, "__pylon_config__")


# ── Type index ─────────────────────────────────────────────────────────────────


def _build_type_index(
    types: list[type],
) -> tuple[dict[str, type], dict[int, str]]:
    """Return (qualified_name→class, id(class)→qualified_name)."""
    type_map: dict[str, type] = {}
    class_to_qname: dict[int, str] = {}

    for cls in types:
        cfg = cls.__pylon_config__
        qname = _qualified(cfg.module, cfg.name)
        if qname in type_map:
            raise SchemaError(
                f"Duplicate type name {qname!r}: "
                f"both {type_map[qname]!r} and {cls!r}"
            )
        type_map[qname] = cls
        class_to_qname[id(cls)] = qname

    return type_map, class_to_qname


# ── Lazy ref resolution ────────────────────────────────────────────────────────


def _import_from_lazy(module_path: str, class_name: str, anchor_cls: type) -> type | None:
    """Import a class by resolving a relative module path from anchor_cls.__module__."""
    anchor = anchor_cls.__module__ or ""
    try:
        if module_path.startswith("."):
            level = len(module_path) - len(module_path.lstrip("."))
            relative_name = module_path.lstrip(".")
            parts = anchor.split(".")
            if level > len(parts):
                raise SchemaError(
                    f"Relative import {module_path!r} ascends above the package root "
                    f"(anchor: {anchor!r})"
                )
            base_parts = parts[:-level] if level else parts
            full_module = ".".join(base_parts)
            if relative_name:
                full_module = f"{full_module}.{relative_name}" if full_module else relative_name
        else:
            full_module = module_path

        mod = importlib.import_module(full_module)
        return getattr(mod, class_name, None)
    except ImportError as exc:
        raise SchemaError(
            f"Cannot resolve lazy ref: failed to import {module_path!r} "
            f"relative to {anchor!r}: {exc}"
        ) from exc


def _resolve_target(
    target: Any,
    source_cls: type,
    class_to_qname: dict[int, str],
    type_map: dict[str, type],
    label: str,
) -> str:
    """Resolve a link_target or through value to its qualified name string."""
    from ._lazy import _Lazy

    # Unwrap Annotated[T, _Lazy(path)]
    if typing.get_origin(target) is typing.Annotated:
        args = typing.get_args(target)
        inner = args[0]
        lazy_markers = [a for a in args[1:] if isinstance(a, _Lazy)]
        if lazy_markers and isinstance(inner, str):
            resolved = _import_from_lazy(lazy_markers[0].module_path, inner, source_cls)
            if resolved is None or not _is_pylon_type(resolved):
                raise SchemaError(
                    f"{label}: lazy ref {inner!r} from {lazy_markers[0].module_path!r} "
                    f"did not resolve to a Pylon type"
                )
            target = resolved
        elif isinstance(inner, type):
            target = inner
        else:
            raise SchemaError(
                f"{label}: unresolvable Annotated type {target!r}"
            )

    if isinstance(target, type):
        cls_id = id(target)
        if cls_id in class_to_qname:
            return class_to_qname[cls_id]
        if _is_pylon_type(target):
            cfg = target.__pylon_config__
            qname = _qualified(cfg.module, cfg.name)
            if qname in type_map:
                return qname
            raise SchemaError(
                f"{label}: target type {target.__name__!r} has a "
                f"__pylon_config__ but was not collected by the registry — "
                f"did you import its module before calling pylon.finalize()?"
            )
        raise SchemaError(
            f"{label}: {target!r} is not a Pylon type (no __pylon_config__)"
        )

    if isinstance(target, str):
        # Unqualified name lookup: accept the unique match or error on ambiguity.
        matches = [qn for qn in type_map if qn.endswith(f"::{target}") or qn == target]
        if len(matches) == 1:
            return matches[0]
        if len(matches) == 0:
            raise SchemaError(f"{label}: type {target!r} not found in schema")
        raise SchemaError(
            f"{label}: ambiguous type name {target!r} — "
            f"matches {matches!r}; use the qualified form 'module::Name'"
        )

    raise SchemaError(f"{label}: cannot resolve link target {target!r}")


def _resolve_links(
    types: list[type],
    type_map: dict[str, type],
    class_to_qname: dict[int, str],
) -> None:
    """Mutate FieldMeta.link_target / .through to qualified name strings."""
    for cls in types:
        cfg = cls.__pylon_config__
        for field_name, meta in cfg.fields.items():
            if meta.kind in ("link", "multilink") and meta.link_target is not None:
                label = f"{cfg.module}::{cfg.name}.{field_name} link_target"
                meta.link_target = _resolve_target(
                    meta.link_target, cls, class_to_qname, type_map, label
                )
            if meta.kind == "multilink" and meta.through is not None:
                label = f"{cfg.module}::{cfg.name}.{field_name} through"
                meta.through = _resolve_target(
                    meta.through, cls, class_to_qname, type_map, label
                )


# ── Cycle detection ────────────────────────────────────────────────────────────


def _detect_required_link_cycles(
    types: list[type],
    class_to_qname: dict[int, str],
) -> None:
    """Raise SchemaError if there is a cycle of required (non-nullable) links.

    A required-link cycle (A→B→A) makes INSERT impossible without deferred FK
    constraints. The walker rejects these eagerly.
    """
    # Build adjacency: qname → set of required link targets (qnames)
    adj: dict[str, set[str]] = {}
    for cls in types:
        cfg = cls.__pylon_config__
        src_qname = class_to_qname[id(cls)]
        targets: set[str] = set()
        for meta in cfg.fields.values():
            if meta.kind == "link" and not meta.nullable and isinstance(meta.link_target, str):
                targets.add(meta.link_target)
        adj[src_qname] = targets

    # DFS cycle detection
    WHITE, GRAY, BLACK = 0, 1, 2
    color: dict[str, int] = {q: WHITE for q in adj}
    path: list[str] = []

    def dfs(node: str) -> None:
        color[node] = GRAY
        path.append(node)
        for nb in adj.get(node, set()):
            if nb not in color:
                continue  # not in schema (external ref?)
            if color[nb] == GRAY:
                cycle_start = path.index(nb)
                cycle = " → ".join(path[cycle_start:] + [nb])
                raise SchemaError(
                    f"Required-link cycle detected: {cycle}. "
                    "Make at least one link in the cycle nullable to break it."
                )
            if color[nb] == WHITE:
                dfs(nb)
        path.pop()
        color[node] = BLACK

    for node in list(adj):
        if color[node] == WHITE:
            dfs(node)


# ── Interface conformance ──────────────────────────────────────────────────────


def _validate_interfaces(
    types: list[type],
    class_to_qname: dict[int, str],
) -> None:
    """Verify that each concrete type satisfies all interfaces in its MRO."""
    for cls in types:
        cfg = cls.__pylon_config__
        if cfg.abstract:
            continue  # abstract types themselves are not checked

        effective = _effective_fields(cls)

        for base in cls.__mro__[1:]:
            if not _is_pylon_type(base):
                continue
            bcfg = base.__pylon_config__
            if not (bcfg.abstract and bcfg.materialized):
                continue  # not an interface

            for field_name, imeta in bcfg.fields.items():
                if field_name not in effective:
                    raise SchemaError(
                        f"{cfg.module}::{cfg.name} does not satisfy interface "
                        f"{bcfg.module}::{bcfg.name}: missing field {field_name!r}"
                    )
                # Kind must match
                cmeta = effective[field_name]
                if cmeta.kind != imeta.kind:
                    raise SchemaError(
                        f"{cfg.module}::{cfg.name}.{field_name}: interface expects "
                        f"kind={imeta.kind!r}, got kind={cmeta.kind!r}"
                    )


# ── Inheritance flattening ─────────────────────────────────────────────────────


def _effective_fields(cls: type) -> dict[str, Any]:
    """Collect all Pylon fields visible on cls, merging inherited fields.

    Walks the MRO from most-distant ancestor to cls itself. Own fields shadow
    inherited fields with the same name.
    """
    result: dict[str, Any] = {}
    for base in reversed(cls.__mro__):
        if _is_pylon_type(base):
            result.update(base.__pylon_config__.fields)
    return result


def _find_pylon_parents(
    cls: type,
    class_to_qname: dict[int, str],
) -> tuple[list[str], list[str]]:
    """Return (abstract_parents, interfaces) from the MRO of cls."""
    parents: list[str] = []
    interfaces: list[str] = []
    for base in cls.__mro__[1:]:
        if not _is_pylon_type(base):
            continue
        cls_id = id(base)
        if cls_id not in class_to_qname:
            continue
        bcfg = base.__pylon_config__
        if bcfg.abstract and bcfg.materialized:
            interfaces.append(class_to_qname[cls_id])
        elif bcfg.abstract and not bcfg.materialized:
            parents.append(class_to_qname[cls_id])
    return parents, interfaces


def _collect_inherited_cit(cls: type) -> tuple[list[Any], list[Any], list[Any]]:
    """Collect constraints, indexes, triggers from abstract non-materialized parents.

    These have no table of their own, so their DDL-level metadata must propagate
    to the concrete subtype.
    """
    constraints: list[Any] = []
    indexes: list[Any] = []
    triggers: list[Any] = []
    for base in reversed(cls.__mro__[1:]):
        if not _is_pylon_type(base):
            continue
        bcfg = base.__pylon_config__
        if bcfg.abstract and not bcfg.materialized:
            constraints.extend(bcfg.constraints)
            indexes.extend(bcfg.indexes)
            triggers.extend(bcfg.triggers)
    return constraints, indexes, triggers


# ── PG type resolution ─────────────────────────────────────────────────────────


def _to_pg_type(scalar_type: Any) -> str:
    from ._scalars import PG_TYPE_MAP, Scalar, _PylonScalar, SHORTHAND_MAP
    from ._enums import Enum as PylonEnum

    # Built-in Pylon scalar
    if isinstance(scalar_type, type) and issubclass(scalar_type, _PylonScalar):
        return PG_TYPE_MAP.get(scalar_type, "text")

    # Python shorthand (should already be resolved by _annotation_to_meta, but
    # handle defensively)
    if scalar_type in SHORTHAND_MAP:
        return PG_TYPE_MAP.get(SHORTHAND_MAP[scalar_type], "text")

    # Custom named scalar (decorator form): use its base PG type for the domain
    if isinstance(scalar_type, type) and issubclass(scalar_type, Scalar):
        base = getattr(scalar_type, "__pylon_base__", None)
        if base and issubclass(base, _PylonScalar):
            return PG_TYPE_MAP.get(base, "text")
        return "text"

    # Enum type → schema-qualified PostgreSQL ENUM type reference
    if isinstance(scalar_type, type) and issubclass(scalar_type, PylonEnum):
        mod = getattr(scalar_type, "__pylon_module__", None) or \
            (scalar_type.__module__ or "default").rpartition(".")[-1] or "default"
        return f'"{mod.replace(chr(34), chr(34)*2)}"."{scalar_type.__name__.replace(chr(34), chr(34)*2)}"'

    # Generic Python types (list[str], dict, etc.) → jsonb
    origin = typing.get_origin(scalar_type)
    if origin is list:
        args = typing.get_args(scalar_type)
        if args:
            elem_pg = _to_pg_type(SHORTHAND_MAP.get(args[0], args[0]))
            if not elem_pg.startswith("jsonb"):
                return f"{elem_pg}[]"
        return "jsonb"
    if origin in (dict, set):
        return "jsonb"

    return "text"


def _scalar_type_name(scalar_type: Any) -> str:
    """Return a short human-readable name for the scalar type (for Rust ScalarDescriptor.base)."""
    if hasattr(scalar_type, "__name__"):
        return scalar_type.__name__
    return repr(scalar_type)


# ── Default SQL generation ─────────────────────────────────────────────────────


def _python_value_to_sql(value: Any) -> str | None:
    import decimal as _decimal
    import uuid as _uuid

    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return str(value)
    if isinstance(value, _decimal.Decimal):
        return str(value)
    if isinstance(value, str):
        return "'" + value.replace("'", "''") + "'"
    if isinstance(value, _uuid.UUID):
        return f"'{value}'"
    return None


def _make_default_sql(meta: Any) -> str | None:
    from ._constraints import Default
    from ._constraints import _NowType

    for c in meta.constraints:
        if isinstance(c, Default):
            if isinstance(c.sentinel, _NowType):
                return "now()"
            # Other Default sentinels: no server-side expression yet.
            return None

    if meta.default is not MISSING and meta.default is not None:
        return _python_value_to_sql(meta.default)

    return None


# ── Constraint compilation ─────────────────────────────────────────────────────


def _field_checks_and_exclusive(
    meta: Any,
    col: str,
) -> tuple[list[str], bool]:
    """Return (check_sql_list, is_exclusive) for the field's constraints."""
    from ._constraints import (
        Exclusive,
        MaxExValue,
        MaxLen,
        MaxValue,
        MinExValue,
        MinLen,
        MinValue,
        OneOf,
        Regexp,
    )

    checks: list[str] = []
    is_exclusive = False

    for c in meta.constraints:
        if c is Exclusive:
            is_exclusive = True
        elif isinstance(c, MinValue):
            checks.append(f"{col} >= {c.value!r}")
        elif isinstance(c, MaxValue):
            checks.append(f"{col} <= {c.value!r}")
        elif isinstance(c, MinExValue):
            checks.append(f"{col} > {c.value!r}")
        elif isinstance(c, MaxExValue):
            checks.append(f"{col} < {c.value!r}")
        elif isinstance(c, MaxLen):
            checks.append(f"char_length({col}) <= {c.length!r}")
        elif isinstance(c, MinLen):
            checks.append(f"char_length({col}) >= {c.length!r}")
        elif isinstance(c, Regexp):
            escaped = c.pattern.replace("'", "''")
            checks.append(f"{col} ~ '{escaped}'")
        elif isinstance(c, OneOf):
            literals = ", ".join(
                "'" + str(v).replace("'", "''") + "'" for v in c.values
            )
            checks.append(f"{col} IN ({literals})")

    return checks, is_exclusive


# ── Field descriptor builders ──────────────────────────────────────────────────


def _make_property_desc(name: str, meta: Any, _core: Any) -> Any:
    from ._scalars import UUID

    is_pk = name == "id" and meta.scalar_type is UUID
    if is_pk:
        return _core.PropertyDescriptor(
            name="id",
            pg_type="uuid",
            nullable=False,
            default_sql="uuidv7()",
            description=meta.description,
            check_constraints=[],
            is_exclusive=True,
            is_pk=True,
        )

    pg_type = _to_pg_type(meta.scalar_type)
    checks, is_exclusive = _field_checks_and_exclusive(meta, name)
    default_sql = _make_default_sql(meta)

    rewrites = [
        _core.RewriteEntry(on=int(r.on), handler=r.handler)
        for r in meta.rewrites
    ]

    return _core.PropertyDescriptor(
        name=name,
        pg_type=pg_type,
        nullable=meta.nullable,
        default_sql=default_sql,
        description=meta.description,
        check_constraints=checks,
        is_exclusive=is_exclusive,
        is_pk=False,
        is_readonly=meta.is_readonly,
        rewrites=rewrites,
    )


def _make_on_delete_policies(on_delete: list[Any], _core: Any) -> list[Any]:
    return [_core.OnDeletePolicy(side=od.side.name, action=od.action.name) for od in on_delete]


def _make_link_desc(name: str, meta: Any, _core: Any) -> Any:
    from ._constraints import Exclusive

    is_exclusive = any(c is Exclusive for c in meta.constraints)
    rewrites = [
        _core.RewriteEntry(on=int(r.on), handler=r.handler)
        for r in meta.rewrites
    ]
    return _core.LinkDescriptor(
        name=name,
        target=meta.link_target,  # already a qualified string
        nullable=meta.nullable,
        description=meta.description,
        is_exclusive=is_exclusive,
        is_readonly=meta.is_readonly,
        rewrites=rewrites,
        on_delete=_make_on_delete_policies(meta.on_delete, _core),
    )


def _make_multilink_desc(name: str, meta: Any, _core: Any) -> Any:
    return _core.MultiLinkDescriptor(
        name=name,
        target=meta.link_target,  # already a qualified string
        through=meta.through,      # already a qualified string or None
        nullable=meta.nullable,
        description=meta.description,
        on_delete=_make_on_delete_policies(meta.on_delete, _core),
    )


def _make_computed_desc(name: str, meta: Any, _core: Any) -> Any:
    return _core.ComputedDescriptor(
        name=name,
        expression=meta.expression,
        return_type=None,  # resolved by compiler, not walker
    )


# ── Index / trigger / constraint builders ─────────────────────────────────────


def _make_index_desc(idx: Any, _core: Any) -> Any:
    if idx.is_expression:
        return _core.IndexDescriptor(
            fields=[],
            expression=idx.field if isinstance(idx.field, str) else None,
            unique=False,
            unless=idx.unless,
        )
    fields = list(idx.field) if isinstance(idx.field, (tuple, list)) else [idx.field]
    return _core.IndexDescriptor(
        fields=fields,
        expression=None,
        unique=False,
        unless=idx.unless,
    )


def _make_trigger_desc(trig: Any, _core: Any) -> Any:
    return _core.TriggerDescriptor(
        on=int(trig.on),
        timing=trig.timing.value,
        handler=trig.handler,
    )


def _make_exclusive_constraint(c: Any, _core: Any) -> Any:
    # c is an Exclusive instance with .fields and .unless
    return _core.ExclusiveConstraint(fields=list(c.fields), unless=c.unless)


def _make_expression_constraint(c: Any, _core: Any) -> Any:
    return _core.ExpressionConstraint(expr=c.expr)


# ── Type descriptor builder ────────────────────────────────────────────────────


def _build_type_descriptor(
    cls: type,
    class_to_qname: dict[int, str],
    _core: Any,
) -> Any:
    from ._constraints import Exclusive, Expression

    cfg = cls.__pylon_config__
    effective = _effective_fields(cls)
    parents, interfaces = _find_pylon_parents(cls, class_to_qname)

    properties: list[Any] = []
    links: list[Any] = []
    multilinks: list[Any] = []
    computed: list[Any] = []

    for field_name, meta in effective.items():
        if meta.kind == "property":
            properties.append(_make_property_desc(field_name, meta, _core))
        elif meta.kind == "link":
            links.append(_make_link_desc(field_name, meta, _core))
        elif meta.kind == "multilink":
            multilinks.append(_make_multilink_desc(field_name, meta, _core))
        elif meta.kind == "computed":
            computed.append(_make_computed_desc(field_name, meta, _core))

    # Merge in class-level C/I/T from abstract non-materialized parents.
    inherited_constraints, inherited_indexes, inherited_triggers = (
        _collect_inherited_cit(cls)
    )
    all_constraints = inherited_constraints + list(cfg.constraints)
    all_indexes = inherited_indexes + list(cfg.indexes)
    all_triggers = inherited_triggers + list(cfg.triggers)

    exclusive_constraints = [
        _make_exclusive_constraint(c, _core)
        for c in all_constraints
        if isinstance(c, Exclusive)
    ]
    expression_constraints = [
        _make_expression_constraint(c, _core)
        for c in all_constraints
        if isinstance(c, Expression)
    ]
    index_descs = [_make_index_desc(idx, _core) for idx in all_indexes]
    trigger_descs = [_make_trigger_desc(t, _core) for t in all_triggers]

    return _core.TypeDescriptor(
        name=cfg.name,
        module=cfg.module,
        table=cfg.table,
        properties=properties,
        links=links,
        multilinks=multilinks,
        computed=computed,
        abstract_=cfg.abstract,
        materialized=cfg.materialized,
        description=cfg.description,
        parents=parents,
        interfaces=interfaces,
        exclusive_constraints=exclusive_constraints,
        expression_constraints=expression_constraints,
        indexes=index_descs,
        triggers=trigger_descs,
    )


# ── Scalar / enum / global builders ───────────────────────────────────────────


def _build_scalar_descriptor(cls: type, _core: Any) -> Any:
    from ._scalars import PG_TYPE_MAP, _PylonScalar

    base_cls = getattr(cls, "__pylon_base__", None)
    base_name = base_cls.__name__ if base_cls else "Str"
    pg_type = PG_TYPE_MAP.get(base_cls, "text") if base_cls else "text"

    # Inline constraints on the scalar itself (from @pylon.scalar(Str, MinValue(0)))
    scalar_constraints = getattr(cls, "__pylon_constraints__", ())
    checks, _ = _field_checks_and_exclusive(
        type("_m", (), {"constraints": list(scalar_constraints), "rewrites": []})(),
        "value",  # conventional name inside DOMAIN CHECK
    )

    module = getattr(cls, "__pylon_module__", None) or (
        (cls.__module__ or "default").rpartition(".")[-1] or "default"
    )

    return _core.ScalarDescriptor(
        name=cls.__name__,
        module=module,
        base=base_name,
        pg_type=pg_type,
        check_constraints=checks,
    )


def _build_enum_descriptor(cls: type, _core: Any) -> Any:
    module = getattr(cls, "__pylon_module__", None) or (
        (cls.__module__ or "default").rpartition(".")[-1] or "default"
    )
    members = [m.name for m in cls]
    return _core.EnumDescriptor(name=cls.__name__, module=module, members=members)


def _build_global_descriptor(g: Any, _core: Any) -> Any:
    from ._scalars import _PylonScalar

    scalar_cls = g.scalar_type
    if isinstance(scalar_cls, type) and issubclass(scalar_cls, _PylonScalar):
        scalar_type_name = scalar_cls.__name__
    else:
        scalar_type_name = getattr(scalar_cls, "__name__", repr(scalar_cls))

    default_expr: str | None = None
    if g.default is not MISSING:
        default_expr = _python_value_to_sql(g.default)

    return _core.GlobalDescriptor(
        name=g.name,
        module=g.module,
        scalar_type=scalar_type_name,
        required=g.required,
        default_expr=default_expr,
    )


# ── Main entry point ───────────────────────────────────────────────────────────


def walk(
    types: list[type],
    enums: list[type],
    custom_scalars: list[type],
    globals_: list[Any],
) -> Any:
    """Walk the collected schema and return a pylon._core.SchemaDescriptor.

    Performs, in order:
    1.  Duplicate name detection
    2.  Lazy forward-reference resolution
    3.  Required-link cycle detection
    4.  Interface conformance validation
    5.  Inheritance flattening (fields + constraints/indexes/triggers from abstract parents)
    6.  PyO3 descriptor construction
    """
    from pylon import _core  # local import to allow testing without Rust binary

    # Phase 1 — build indexes
    type_map, class_to_qname = _build_type_index(types)

    # Phase 2 — resolve lazy refs (mutates FieldMeta in-place)
    _resolve_links(types, type_map, class_to_qname)

    # Phase 3 — cycle detection
    _detect_required_link_cycles(types, class_to_qname)

    # Phase 4 — interface conformance
    _validate_interfaces(types, class_to_qname)

    # Phase 5+6 — build PyO3 descriptors
    type_descs = [
        _build_type_descriptor(cls, class_to_qname, _core) for cls in types
    ]
    scalar_descs = [_build_scalar_descriptor(cls, _core) for cls in custom_scalars]
    enum_descs = [_build_enum_descriptor(cls, _core) for cls in enums]
    global_descs = [_build_global_descriptor(g, _core) for g in globals_]

    return _core.SchemaDescriptor(
        types=type_descs,
        scalars=scalar_descs,
        enums=enum_descs,
        globals=global_descs,
    )
