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
    """Mutate PointerMeta.link_target / .through to qualified name strings."""
    for cls in types:
        cfg = cls.__pylon_config__
        for pointer_name, meta in cfg.pointers.items():
            if meta.kind in ("link", "multilink") and meta.link_target is not None:
                label = f"{cfg.module}::{cfg.name}.{pointer_name} link_target"
                meta.link_target = _resolve_target(
                    meta.link_target, cls, class_to_qname, type_map, label
                )
            if meta.kind == "multilink" and meta.through is not None:
                label = f"{cfg.module}::{cfg.name}.{pointer_name} through"
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
        for meta in cfg.pointers.values():
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


# ── Junction validation ────────────────────────────────────────────────────────


def _validate_junctions(
    types: list[type],
    class_to_qname: dict[int, str],
) -> dict[str, tuple[str, str]]:
    """Validate junction type usage and return junction_qname → (source_table, ml_name) map.

    Enforces:
    - Each junction type is referenced by exactly one MultiLink.
    - Junction types have no link or multilink pointers (already enforced at decoration time,
      but re-checked here for types that arrive from non-decorator paths).
    """
    # Build reverse map: junction_qname → (source_type_cfg, ml_name)
    junction_to_ml: dict[str, tuple[str, str]] = {}  # qname → (source_table, ml_name)

    for cls in types:
        cfg = cls.__pylon_config__
        for fn, meta in cfg.pointers.items():
            if meta.kind != "multilink" or meta.through is None:
                continue
            through_qname = meta.through  # already resolved to a qname string
            through_cls = next(
                (t for t in types if class_to_qname.get(id(t)) == through_qname),
                None,
            )
            if through_cls is None:
                continue  # unresolved — caught elsewhere
            through_cfg = through_cls.__pylon_config__
            if not through_cfg.junction:
                continue  # old-style through type — not a junction, no restriction

            src_qname = class_to_qname[id(cls)]
            if through_qname in junction_to_ml:
                other_src_table, other_ml = junction_to_ml[through_qname]
                raise SchemaError(
                    f"Junction type {through_qname!r} is referenced by more than one "
                    f"MultiLink: {other_src_table!r}.{other_ml!r} and "
                    f"{src_qname!r}.{fn!r}. Each junction type may only be used by "
                    f"a single MultiLink."
                )
            junction_to_ml[through_qname] = (cfg.table, fn)

    # Every junction type must be referenced by exactly one MultiLink.
    for cls in types:
        cfg = cls.__pylon_config__
        if not cfg.junction:
            continue
        qname = class_to_qname[id(cls)]
        if qname not in junction_to_ml:
            raise SchemaError(
                f"Junction type {qname!r} is not referenced by any MultiLink. "
                f"Junction types must be used as the 'through' parameter of exactly "
                f"one MultiLink pointer."
            )

    return junction_to_ml


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

        effective = _effective_pointers(cls)

        for base in cls.__mro__[1:]:
            if not _is_pylon_type(base):
                continue
            bcfg = base.__pylon_config__
            if not (bcfg.abstract and bcfg.materialized):
                continue  # not an interface

            for pointer_name, imeta in bcfg.pointers.items():
                if pointer_name not in effective:
                    raise SchemaError(
                        f"{cfg.module}::{cfg.name} does not satisfy interface "
                        f"{bcfg.module}::{bcfg.name}: missing pointer {pointer_name!r}"
                    )
                # Kind must match
                cmeta = effective[pointer_name]
                if cmeta.kind != imeta.kind:
                    raise SchemaError(
                        f"{cfg.module}::{cfg.name}.{pointer_name}: interface expects "
                        f"kind={imeta.kind!r}, got kind={cmeta.kind!r}"
                    )


# ── Inheritance flattening ─────────────────────────────────────────────────────


def _effective_pointers(cls: type) -> dict[str, Any]:
    """Collect all Pylon pointers visible on cls, merging inherited pointers.

    Walks the MRO from most-distant ancestor to cls itself. Own pointers shadow
    inherited pointers with the same name.
    """
    result: dict[str, Any] = {}
    for base in reversed(cls.__mro__):
        if _is_pylon_type(base):
            result.update(base.__pylon_config__.pointers)
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


def _pg_schema(module: str) -> str:
    return "public" if module == "default" else module


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

    # Named tuple type → jsonb with type marker
    from ._named_tuples import NamedTuple as PylonNamedTuple
    if isinstance(scalar_type, type) and issubclass(scalar_type, PylonNamedTuple):
        mod = getattr(scalar_type, "__pylon_module__", None) or \
            (scalar_type.__module__ or "default").rpartition(".")[-1] or "default"
        return f"__nt__:{mod}::{scalar_type.__name__}"

    # Enum type → schema-qualified PostgreSQL ENUM type reference
    if isinstance(scalar_type, type) and issubclass(scalar_type, PylonEnum):
        mod = getattr(scalar_type, "__pylon_module__", None) or \
            (scalar_type.__module__ or "default").rpartition(".")[-1] or "default"
        return f'"{_pg_schema(mod).replace(chr(34), chr(34)*2)}"."{scalar_type.__name__.replace(chr(34), chr(34)*2)}"'

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
    """Return a SQL literal/expression for the property's default, or None."""
    import decimal as _decimal
    from ._constraints import Default, _NowType, _SequenceNextType

    for c in meta.constraints:
        if isinstance(c, Default):
            s = c.sentinel
            if isinstance(s, _NowType):
                return "now()"
            if isinstance(s, _SequenceNextType):
                return None  # handled separately in _make_property_desc
            if isinstance(s, str):
                return None  # PyQL expression — handled by _make_default_pyql
            if s is None:
                return "NULL"
            # bool must be checked before int (bool is a subclass of int)
            if isinstance(s, bool):
                return "true" if s else "false"
            if isinstance(s, (int, float, _decimal.Decimal)):
                return str(s)
            return None  # unknown sentinel

    if meta.default is not MISSING and meta.default is not None:
        return _python_value_to_sql(meta.default)

    return None


def _make_default_pyql(meta: Any) -> str | None:
    """Return a PyQL expression string from Default(str), or None."""
    from ._constraints import Default, _SequenceNextType, _NowType

    for c in meta.constraints:
        if isinstance(c, Default) and isinstance(c.sentinel, str):
            return c.sentinel

    return None


# ── Constraint compilation ─────────────────────────────────────────────────────


def _field_checks_and_exclusive(
    meta: Any,
    col: str,
) -> tuple[list[str], bool]:
    """Return (check_sql_list, is_exclusive) for the pointer's constraints."""
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


# ── Pointer descriptor builders ────────────────────────────────────────────────


def _make_property_desc(name: str, meta: Any, _core: Any) -> Any:
    from ._constraints import Default, _SequenceNextType
    from ._scalars import UUID, Scalar as PylonScalar

    is_pk = name == "id" and meta.scalar_type is UUID
    if is_pk:
        id_default_pyql = _make_default_pyql(meta)
        return _core.PropertyDescriptor(
            name="id",
            pg_type="uuid",
            nullable=False,
            default_sql=None if id_default_pyql else "uuidv7()",
            default_pyql=id_default_pyql,
            description=meta.description,
            check_constraints=[],
            is_exclusive=True,
            is_pk=True,
        )

    pg_type = _to_pg_type(meta.scalar_type)
    checks, is_exclusive = _field_checks_and_exclusive(meta, name)

    # SequenceNext default: generate nextval('"module"."Name_seq"')
    default_sql = None
    for c in meta.constraints:
        if isinstance(c, Default) and isinstance(c.sentinel, _SequenceNextType):
            scalar_type = meta.scalar_type
            if isinstance(scalar_type, type) and issubclass(scalar_type, PylonScalar):
                mod = getattr(scalar_type, "__pylon_module__", None) or (
                    (scalar_type.__module__ or "default").rpartition(".")[-1] or "default"
                )
                seq_name = f"{scalar_type.__name__}_seq"
                default_sql = f"""nextval('"{_pg_schema(mod)}"."{seq_name}"')"""
            break
    default_pyql = None
    if default_sql is None:
        default_sql = _make_default_sql(meta)
    if default_sql is None:
        default_pyql = _make_default_pyql(meta)

    rewrites = [
        _core.RewriteEntry(on=int(r.on), handler=r.handler)
        for r in meta.rewrites
    ]

    return _core.PropertyDescriptor(
        name=name,
        pg_type=pg_type,
        nullable=meta.nullable,
        default_sql=default_sql,
        default_pyql=default_pyql,
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
        default_pyql=_make_default_pyql(meta),
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
        default_pyql=_make_default_pyql(meta),
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
            pointers=[],
            expression=idx.pointer if isinstance(idx.pointer, str) else None,
            unique=False,
            unless=idx.unless,
        )
    pointers = list(idx.pointer) if isinstance(idx.pointer, (tuple, list)) else [idx.pointer]
    return _core.IndexDescriptor(
        pointers=pointers,
        expression=None,
        unique=False,
        unless=idx.unless,
    )


def _resolve_vector_pointer(ref: str, type_name: str, valid_pointers: set[str]) -> str:
    if "." in ref:
        prefix, pointer = ref.rsplit(".", 1)
        if prefix != type_name:
            raise SchemaError(
                f"VectorPointer {ref!r}: type prefix {prefix!r} does not match enclosing type {type_name!r}"
            )
    else:
        pointer = ref
    if pointer not in valid_pointers:
        raise SchemaError(
            f"VectorPointer {ref!r}: pointer {pointer!r} not found on type {type_name!r}"
        )
    return pointer


def _make_vector_index_desc(vi: Any, _core: Any, type_name: str, valid_pointers: set[str]) -> Any:
    pointers = [_resolve_vector_pointer(vp.ref, type_name, valid_pointers) for vp in vi._vector_pointers]
    return _core.VectorIndexDescriptor(
        pointers=pointers,
        model=vi.model,
        metric=vi.metric,
        dimensions=vi.dimensions,
        index_name=vi.index_name,
    )


def _resolve_search_pointer(ref: str, type_name: str, valid_pointers: set[str]) -> str:
    if "." in ref:
        prefix, pointer = ref.rsplit(".", 1)
        if prefix != type_name:
            raise SchemaError(
                f"SearchPointer {ref!r}: type prefix {prefix!r} does not match enclosing type {type_name!r}"
            )
    else:
        pointer = ref
    if pointer not in valid_pointers:
        raise SchemaError(
            f"SearchPointer {ref!r}: pointer {pointer!r} not found on type {type_name!r}"
        )
    return pointer


def _make_search_index_desc(si: Any, _core: Any, type_name: str, valid_pointers: set[str]) -> Any:
    pointers = []
    for sp in si._search_pointers:
        pointer_name = _resolve_search_pointer(sp.ref, type_name, valid_pointers)
        pointers.append(_core.SearchPointerDescriptor(name=pointer_name, weight=sp.weight_category.value))
    return _core.SearchIndexDescriptor(
        backend=si.backend.value,
        pointers=pointers,
        index_name=si.index_name,
    )


def _make_trigger_desc(trig: Any, _core: Any) -> Any:
    return _core.TriggerDescriptor(
        on=int(trig.on),
        timing=trig.timing.value,
        handler=trig.handler,
    )


def _make_exclusive_constraint(c: Any, _core: Any) -> Any:
    # c is an Exclusive instance with .pointers and .unless
    return _core.ExclusiveConstraint(pointers=list(c.pointers), unless=c.unless)


def _make_expression_constraint(c: Any, _core: Any) -> Any:
    return _core.ExpressionConstraint(expr=c.expr)


# ── Type descriptor builder ────────────────────────────────────────────────────


def _build_type_descriptor(
    cls: type,
    class_to_qname: dict[int, str],
    _core: Any,
    junction_to_ml: dict[str, tuple[str, str]] | None = None,
) -> Any:
    from ._constraints import Exclusive, Expression

    cfg = cls.__pylon_config__
    effective = _effective_pointers(cls)
    parents, interfaces = _find_pylon_parents(cls, class_to_qname)

    properties: list[Any] = []
    links: list[Any] = []
    multilinks: list[Any] = []
    computed: list[Any] = []

    for pointer_name, meta in effective.items():
        if meta.kind == "property":
            properties.append(_make_property_desc(pointer_name, meta, _core))
        elif meta.kind == "link":
            links.append(_make_link_desc(pointer_name, meta, _core))
        elif meta.kind == "multilink":
            multilinks.append(_make_multilink_desc(pointer_name, meta, _core))
        elif meta.kind == "computed":
            computed.append(_make_computed_desc(pointer_name, meta, _core))

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
    vector_index_descs = [
        _make_vector_index_desc(vi, _core, cfg.name, set(effective.keys()))
        for vi in cfg.vector_indexes
    ]
    search_index_descs = [
        _make_search_index_desc(si, _core, cfg.name, set(effective.keys()))
        for si in cfg.search_indexes
    ]
    trigger_descs = [_make_trigger_desc(t, _core) for t in all_triggers]

    # Junction types: derive the actual table name from the MultiLink that references them.
    if cfg.junction and junction_to_ml is not None:
        qname = _qualified(cfg.module, cfg.name)
        source_table, ml_name = junction_to_ml.get(qname, (cfg.table, cfg.name))
        actual_table = f"{source_table}.{ml_name}"
    else:
        actual_table = cfg.table

    return _core.TypeDescriptor(
        name=cfg.name,
        module=cfg.module,
        table=actual_table,
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
        vector_indexes=vector_index_descs,
        search_indexes=search_index_descs,
        triggers=trigger_descs,
        junction=cfg.junction,
    )


# ── Scalar / enum / global builders ───────────────────────────────────────────


def _build_scalar_descriptor(cls: type, _core: Any) -> Any:
    from ._scalars import PG_TYPE_MAP, Sequence as SequenceScalar, _PylonScalar

    base_cls = getattr(cls, "__pylon_base__", None)
    base_name = base_cls.__name__ if base_cls else "Str"
    pg_type = PG_TYPE_MAP.get(base_cls, "text") if base_cls else "text"
    is_sequence = isinstance(base_cls, type) and issubclass(base_cls, SequenceScalar)

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
        is_sequence=is_sequence,
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
        computed_expr=g.computed_expr,
    )


# ── Function descriptor builder ───────────────────────────────────────────────


def _parse_return_annotation(
    annotation: Any,
    type_map: dict[str, Any],
    class_to_qname: dict[int, str],
) -> tuple[str, bool, bool, bool]:
    """Parse a return type annotation into (return_pg_type, is_object, is_set, is_polymorphic).

    return_pg_type is either a PostgreSQL type string (for scalars) or a
    qualified type name like 'default::Account' (for object returns).
    """
    import typing as _typing
    from ._lazy import _Lazy

    is_set = False
    is_object = False
    is_polymorphic = False

    # Unwrap Optional (T | None): strip None from union
    origin = _typing.get_origin(annotation)
    if origin is _typing.Union:
        args = [a for a in _typing.get_args(annotation) if a is not type(None)]
        if len(args) == 1:
            annotation = args[0]
            origin = _typing.get_origin(annotation)

    # Unwrap set[T]
    if origin is set:
        is_set = True
        args = _typing.get_args(annotation)
        annotation = args[0] if args else annotation
        # Re-check for Optional inside set[T | None]
        inner_origin = _typing.get_origin(annotation)
        if inner_origin is _typing.Union:
            inner_args = [a for a in _typing.get_args(annotation) if a is not type(None)]
            if len(inner_args) == 1:
                annotation = inner_args[0]

    # Unwrap Annotated[T, lazy(...)]
    if _typing.get_origin(annotation) is _typing.Annotated:
        a_args = _typing.get_args(annotation)
        annotation = a_args[0]

    # Check if this is a Pylon object type
    if isinstance(annotation, type) and hasattr(annotation, "__pylon_config__"):
        cfg = annotation.__pylon_config__
        qname = f"{cfg.module}::{cfg.name}"
        is_object = True
        is_polymorphic = cfg.abstract and cfg.materialized
        return qname, is_object, is_set, is_polymorphic

    # Check by id in class_to_qname
    if isinstance(annotation, type) and id(annotation) in class_to_qname:
        qname = class_to_qname[id(annotation)]
        td_cls = type_map.get(qname)
        if td_cls is not None:
            cfg = td_cls.__pylon_config__
            is_object = True
            is_polymorphic = cfg.abstract and cfg.materialized
            return qname, is_object, is_set, is_polymorphic

    # Must be a scalar type
    pg_type = _to_pg_type(annotation)
    return pg_type, False, is_set, False


def _param_pg_type(annotation: Any) -> str:
    """Resolve a parameter type annotation to a PostgreSQL type string."""
    import typing as _typing
    # Unwrap Optional
    origin = _typing.get_origin(annotation)
    if origin is _typing.Union:
        args = [a for a in _typing.get_args(annotation) if a is not type(None)]
        if args:
            annotation = args[0]
    # Unwrap Annotated
    if _typing.get_origin(annotation) is _typing.Annotated:
        annotation = _typing.get_args(annotation)[0]
    # Object types: use uuid (FK-like param)
    if isinstance(annotation, type) and hasattr(annotation, "__pylon_config__"):
        return "uuid"
    return _to_pg_type(annotation)


def _build_function_descriptor(
    func: Any,
    type_map: dict[str, Any],
    class_to_qname: dict[int, str],
    _core: Any,
) -> Any:
    import typing as _typing

    config = func.__pylon_function__
    hints = {}
    try:
        hints = _typing.get_type_hints(func, include_extras=True)
    except Exception:
        hints = getattr(func, "__annotations__", {})

    # Build parameter descriptors
    sig = __import__("inspect").signature(func)
    params = []
    for param_name, param in sig.parameters.items():
        annotation = hints.get(param_name, param.annotation)
        if annotation is __import__("inspect").Parameter.empty:
            raise SchemaError(
                f"function '{config.module}::{config.name}' parameter '{param_name}' "
                f"has no type annotation"
            )
        pg_type = _param_pg_type(annotation)
        params.append(_core.FunctionParamDescriptor(name=param_name, pg_type=pg_type))

    # Parse return type
    return_annotation = hints.get("return", __import__("inspect").Parameter.empty)
    if return_annotation is __import__("inspect").Parameter.empty:
        raise SchemaError(
            f"function '{config.module}::{config.name}' has no return type annotation"
        )
    return_pg_type, return_is_object, return_is_set, return_is_polymorphic = (
        _parse_return_annotation(return_annotation, type_map, class_to_qname)
    )

    if return_is_object and not return_is_set:
        raise SchemaError(
            f"function '{config.module}::{config.name}': object-returning functions must "
            f"annotate the return type as set[T], not a bare T — single-object returns "
            f"are not supported"
        )

    volatility = config.volatility or Volatility.Volatile

    return _core.FunctionDescriptor(
        name=config.name,
        module=config.module,
        params=params,
        return_pg_type=return_pg_type,
        body=config.body,
        return_is_object=return_is_object,
        return_is_set=return_is_set,
        return_is_polymorphic=return_is_polymorphic,
        volatility=volatility,
    )


# Import Volatility for use in walker
from ._functions import Volatility


# ── Main entry point ───────────────────────────────────────────────────────────


def walk(
    types: list[type],
    enums: list[type],
    custom_scalars: list[type],
    globals_: list[Any],
    functions: list[Any] | None = None,
    aliases: list[Any] | None = None,
) -> Any:
    """Walk the collected schema and return a pylon._core.SchemaDescriptor.

    Performs, in order:
    1.  Duplicate name detection
    2.  Lazy forward-reference resolution
    3.  Required-link cycle detection
    4.  Interface conformance validation
    5.  Inheritance flattening (pointers + constraints/indexes/triggers from abstract parents)
    6.  PyO3 descriptor construction
    """
    from pylon import _core  # local import to allow testing without Rust binary

    # Phase 1 — build indexes
    type_map, class_to_qname = _build_type_index(types)

    # Phase 2 — resolve lazy refs (mutates PointerMeta in-place)
    _resolve_links(types, type_map, class_to_qname)

    # Phase 3 — cycle detection
    _detect_required_link_cycles(types, class_to_qname)

    # Phase 4 — interface conformance
    _validate_interfaces(types, class_to_qname)

    # Phase 4.5 — junction type validation
    junction_to_ml = _validate_junctions(types, class_to_qname)

    # Phase 5+6 — build PyO3 descriptors
    type_descs = [
        _build_type_descriptor(cls, class_to_qname, _core, junction_to_ml) for cls in types
    ]
    scalar_descs = [_build_scalar_descriptor(cls, _core) for cls in custom_scalars]
    enum_descs = [_build_enum_descriptor(cls, _core) for cls in enums]
    global_descs = [_build_global_descriptor(g, _core) for g in globals_]
    fn_descs = [
        _build_function_descriptor(f, type_map, class_to_qname, _core)
        for f in (functions or [])
    ]
    alias_descs = [
        _core.AliasDescriptor(name=a.name, module=a.module, expr=a.expr)
        for a in (aliases or [])
    ]

    return _core.SchemaDescriptor(
        types=type_descs,
        scalars=scalar_descs,
        enums=enum_descs,
        globals=global_descs,
        functions=fn_descs,
        aliases=alias_descs,
    )
