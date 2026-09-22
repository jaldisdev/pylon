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

"""Schema walker: resolves, validates, and converts the collected schema to a
SchemaDescriptor (PyO3 object) ready to be handed to pylon-core.

Entry point: walk(types, enums, custom_scalars, globals_) → SchemaDescriptor
"""

from __future__ import annotations

import dataclasses
import importlib
import typing
from typing import Any

# Safe at module scope despite the walker/functions cycle noted in
# `_functions.py`: that module only reaches back into `_walker` from inside a
# function body, so nothing here runs during its import.
from ._functions import Volatility

MISSING = dataclasses.MISSING

# ── Error type ─────────────────────────────────────────────────────────────────


class SchemaError(Exception):
    """Raised for schema validation failures during pylon.finalize()."""


# ── Helpers ────────────────────────────────────────────────────────────────────


def _qualified(module: str, name: str) -> str:
    return f'{module}::{name}'


def _pylon_module_of(cls: type) -> str:
    return cls.__pylon_config__.module  # type: ignore[attr-defined]


def _pylon_name_of(cls: type) -> str:
    return cls.__pylon_config__.name  # type: ignore[attr-defined]


def _is_pylon_type(cls: type) -> bool:
    return hasattr(cls, '__pylon_config__')


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
            raise SchemaError(f'Duplicate type name {qname!r}: both {type_map[qname]!r} and {cls!r}')
        type_map[qname] = cls
        class_to_qname[id(cls)] = qname

    return type_map, class_to_qname


# ── Lazy ref resolution ────────────────────────────────────────────────────────


def _import_from_lazy(module_path: str, class_name: str, anchor: Any) -> type | None:
    """Import a class by resolving a relative module path from the anchor's module.

    `anchor` is whatever declared the reference — a class for a link target, a
    function for a return annotation. Only its `__module__` is read.
    """
    anchor = getattr(anchor, '__module__', '') or ''
    try:
        if module_path.startswith('.'):
            level = len(module_path) - len(module_path.lstrip('.'))
            relative_name = module_path.lstrip('.')
            parts = anchor.split('.')
            if level > len(parts):
                raise SchemaError(
                    f'Relative import {module_path!r} ascends above the package root (anchor: {anchor!r})'
                )
            base_parts = parts[:-level] if level else parts
            full_module = '.'.join(base_parts)
            if relative_name:
                full_module = f'{full_module}.{relative_name}' if full_module else relative_name
        else:
            full_module = module_path

        mod = importlib.import_module(full_module)
        return getattr(mod, class_name, None)
    except ImportError as exc:
        raise SchemaError(
            f'Cannot resolve lazy ref: failed to import {module_path!r} relative to {anchor!r}: {exc}'
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
        # `Annotated['Author', lazy(...)]` does not keep `'Author'` as a plain
        # string: both `Annotated` itself and `typing.get_type_hints` normalize
        # a string parameter into a `ForwardRef`. Matching only `str` here
        # meant the lazy path was never taken and every lazy ref fell through
        # to the "unresolvable Annotated type" error below — with or without
        # `from __future__ import annotations`.
        inner_name = inner if isinstance(inner, str) else getattr(inner, '__forward_arg__', None)
        if lazy_markers and inner_name is not None:
            resolved = _import_from_lazy(lazy_markers[0].module_path, inner_name, source_cls)
            if resolved is None or not _is_pylon_type(resolved):
                raise SchemaError(
                    f'{label}: lazy ref {inner_name!r} from {lazy_markers[0].module_path!r} '
                    f'did not resolve to a Pylon type'
                )
            target = resolved
        elif isinstance(inner, type):
            target = inner
        else:
            raise SchemaError(f'{label}: unresolvable Annotated type {target!r}')

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
                f'{label}: target type {target.__name__!r} has a '
                f'__pylon_config__ but was not collected by the registry — '
                f'did you import its module before calling pylon.finalize()?'
            )
        raise SchemaError(f'{label}: {target!r} is not a Pylon type (no __pylon_config__)')

    if isinstance(target, str):
        # Unqualified name lookup: accept the unique match or error on ambiguity.
        matches = [qn for qn in type_map if qn.endswith(f'::{target}') or qn == target]
        if len(matches) == 1:
            return matches[0]
        if len(matches) == 0:
            raise SchemaError(f'{label}: type {target!r} not found in schema')
        raise SchemaError(
            f"{label}: ambiguous type name {target!r} — matches {matches!r}; use the qualified form 'module::Name'"
        )

    raise SchemaError(f'{label}: cannot resolve link target {target!r}')


def _resolve_links(
    types: list[type],
    type_map: dict[str, type],
    class_to_qname: dict[int, str],
) -> None:
    """Mutate PointerMeta.link_target / .through to qualified name strings."""
    for cls in types:
        cfg = cls.__pylon_config__
        for pointer_name, meta in cfg.pointers.items():
            if meta.kind in ('link', 'multilink') and meta.link_target is not None:
                label = f'{cfg.module}::{cfg.name}.{pointer_name} link_target'
                meta.link_target = _resolve_target(meta.link_target, cls, class_to_qname, type_map, label)
            if meta.kind in ('link', 'multilink') and meta.through is not None:
                label = f'{cfg.module}::{cfg.name}.{pointer_name} through'
                meta.through = _resolve_target(meta.through, cls, class_to_qname, type_map, label)


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
            if meta.kind == 'link' and not meta.nullable and isinstance(meta.link_target, str):
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
                cycle = ' → '.join([*path[cycle_start:], nb])
                raise SchemaError(
                    f'Required-link cycle detected: {cycle}. Make at least one link in the cycle nullable to break it.'
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
    """Validate junction type usage and return junction_qname → (source_table, pointer_name) map.

    Enforces:
    - Each junction type is referenced by exactly one link or multi-link.
    - Junction types have no link or multilink pointers (already enforced at decoration time,
      but re-checked here for types that arrive from non-decorator paths).
    """
    # Build reverse map: junction_qname → (source_table, pointer_name)
    junction_to_pointer: dict[str, tuple[str, str]] = {}

    for cls in types:
        cfg = cls.__pylon_config__
        for fn, meta in cfg.pointers.items():
            if meta.kind not in ('link', 'multilink') or meta.through is None:
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
            if through_qname in junction_to_pointer:
                other_src_table, other_pointer = junction_to_pointer[through_qname]
                raise SchemaError(
                    f'Junction type {through_qname!r} is referenced by more than one '
                    f'link: {other_src_table!r}.{other_pointer!r} and '
                    f'{src_qname!r}.{fn!r}. Each junction type may only be used by '
                    f'a single link or multi-link.'
                )
            junction_to_pointer[through_qname] = (cfg.table, fn)

    # Every junction type must be referenced by exactly one link or multi-link.
    for cls in types:
        cfg = cls.__pylon_config__
        if not cfg.junction:
            continue
        qname = class_to_qname[id(cls)]
        if qname not in junction_to_pointer:
            raise SchemaError(
                f'Junction type {qname!r} is not referenced by any link or multi-link. '
                f"Junction types must be used as the 'through' parameter of exactly "
                f'one link or multi-link pointer.'
            )

    return junction_to_pointer


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
                        f'{cfg.module}::{cfg.name} does not satisfy interface '
                        f'{bcfg.module}::{bcfg.name}: missing pointer {pointer_name!r}'
                    )
                # Kind must match
                cmeta = effective[pointer_name]
                if cmeta.kind != imeta.kind:
                    raise SchemaError(
                        f'{cfg.module}::{cfg.name}.{pointer_name}: interface expects '
                        f'kind={imeta.kind!r}, got kind={cmeta.kind!r}'
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
) -> tuple[list[str], list[str], list[str]]:
    """Return (abstract_parents, interfaces, concrete bases) from the MRO of
    cls — the bases nearest first, each a type with a table of its own that
    this one extends."""
    parents: list[str] = []
    interfaces: list[str] = []
    bases: list[str] = []
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
        elif not bcfg.junction:
            bases.append(class_to_qname[cls_id])
    return parents, interfaces, bases


def _collect_inherited_cit(cls: type) -> tuple[list[Any], list[Any], list[Any]]:
    """Collect constraints and indexes from abstract non-materialized parents,
    and triggers from every parent.

    The former have no table of their own, so their DDL-level metadata must
    propagate to the concrete subtype.
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
        # A trigger fires for the rows of every type below the one declaring
        # it, and each of those has a table of its own to fire it on.
        triggers.extend(bcfg.triggers)
    return constraints, indexes, triggers


# ── PG type resolution ─────────────────────────────────────────────────────────


def _pg_schema(module: str) -> str:
    return 'public' if module == 'default' else module


def _to_pg_type(scalar_type: Any) -> str:
    from ._enums import Enum as PylonEnum
    from ._scalars import PG_TYPE_MAP, SHORTHAND_MAP, Scalar, _PylonScalar

    # Every call site (plain property types, computed-pointer types, tuple
    # members, function params/return types) expects a scalar — an object
    # type (a @pylon.type/@pylon.interface class) has no business here;
    # object *references* are Link/MultiLink, an entirely separate code
    # path. Without this guard, an object type falls through every branch
    # below to the final `return "text"` fallback, silently — e.g.
    # `Array[Product]`/`Tuple[Product, str]` would resolve to `text[]`/
    # `jsonb` with no error at all (confirmed live before this check).
    if _is_pylon_type(scalar_type):
        raise SchemaError(
            f'invalid type {_qualified(_pylon_module_of(scalar_type), _pylon_name_of(scalar_type))!r}: '
            f'expected a scalar type, got an object type'
        )

    # Built-in Pylon scalar
    if isinstance(scalar_type, type) and issubclass(scalar_type, _PylonScalar):
        return PG_TYPE_MAP.get(scalar_type, 'text')

    # Python shorthand (should already be resolved by _annotation_to_meta, but
    # handle defensively)
    if scalar_type in SHORTHAND_MAP:
        return PG_TYPE_MAP.get(SHORTHAND_MAP[scalar_type], 'text')

    # Custom named scalar (decorator or functional form): every read/write/
    # cast/comparison site relies on `pg_type` being a plain base type (see
    # `PropertyDescriptor.pg_type`'s own doc comment on the Rust side), so
    # this always resolves to the scalar's base type — a *registered*
    # scalar's own DOMAIN name is exposed separately, via
    # `_domain_type_ref`/`PropertyDescriptor.column_type`, consulted only
    # for the column's DDL type.
    if isinstance(scalar_type, type) and issubclass(scalar_type, Scalar):
        base = getattr(scalar_type, '__pylon_base__', None)
        if base and issubclass(base, _PylonScalar):
            return PG_TYPE_MAP.get(base, 'text')
        return 'text'

    # Named tuple type → jsonb with type marker
    from ._named_tuples import NamedTuple as PylonNamedTuple

    if isinstance(scalar_type, type) and issubclass(scalar_type, PylonNamedTuple):
        mod = (
            getattr(scalar_type, '__pylon_module__', None)
            or (scalar_type.__module__ or 'default').rpartition('.')[-1]
            or 'default'
        )
        return f'__nt__:{mod}::{scalar_type.__name__}'

    # Structural tuple type (pylon.Tuple[...]) → plain jsonb, no registered type
    # to decode into. Still resolve each element's own pg_type (discarding the
    # result) purely so the object-type guard above fires for any element —
    # this branch would otherwise never look past `TupleAnnotation` itself, so
    # a `Tuple[Product, str]` used directly as a scalar_type (e.g. a function
    # param/return annotation) would resolve to "jsonb" with no error, same
    # bug class as the direct-element case. Recurses naturally for nested
    # tuples via this same branch.
    from ._pointers import ArrayAnnotation, TupleAnnotation

    if isinstance(scalar_type, TupleAnnotation):
        for element in scalar_type.elements:
            _to_pg_type(element.type_)
        return 'jsonb'

    # Structural array type (pylon.Array[T] or a bare list[T]) → a real
    # Postgres array of the element's own pg_type, never jsonb — arrays
    # decode natively via pylon-pgcon's wire decoder, unlike tuples (see
    # resolve_cast_pg_type on the Rust side, which follows the same rule
    # for `<array<T>>` casts).
    if isinstance(scalar_type, ArrayAnnotation):
        return f'{_to_pg_type(scalar_type.element)}[]'

    # Enum type → schema-qualified PostgreSQL ENUM type reference
    if isinstance(scalar_type, type) and issubclass(scalar_type, PylonEnum):
        mod = (
            getattr(scalar_type, '__pylon_module__', None)
            or (scalar_type.__module__ or 'default').rpartition('.')[-1]
            or 'default'
        )
        return (
            f'"{_pg_schema(mod).replace(chr(34), chr(34) * 2)}"."{scalar_type.__name__.replace(chr(34), chr(34) * 2)}"'
        )

    # Generic Python types (list[str], dict, etc.) → jsonb
    origin = typing.get_origin(scalar_type)
    if origin is list:
        args = typing.get_args(scalar_type)
        if args:
            elem_pg = _to_pg_type(SHORTHAND_MAP.get(args[0], args[0]))
            if not elem_pg.startswith('jsonb'):
                return f'{elem_pg}[]'
        return 'jsonb'
    if origin in (dict, set):
        return 'jsonb'

    return 'text'


def _domain_type_ref(scalar_type: Any) -> str | None:
    """Schema-qualified PostgreSQL DOMAIN name for *scalar_type*, or None.

    Only a *registered* custom scalar (decorator form, or functional form
    with `name=`) has a nominal PostgreSQL identity to hang a domain off of
    — see `pylon.scalar`'s docstring. Consumed solely by
    `PropertyDescriptor.column_type` for the column's own DDL type; every
    other use of the property keeps resolving through `_to_pg_type`'s plain
    base type, so a domain-typed column still casts/compares/decodes
    exactly like its base type everywhere except its own `CREATE TABLE` /
    `ADD COLUMN` definition.
    """
    from ._scalars import Scalar

    if not (isinstance(scalar_type, type) and issubclass(scalar_type, Scalar)):
        return None
    from . import _registry

    if scalar_type not in _registry.snapshot()[2]:
        return None
    mod = getattr(scalar_type, '__pylon_module__', None) or (
        (scalar_type.__module__ or 'default').rpartition('.')[-1] or 'default'
    )
    return f'"{_pg_schema(mod).replace(chr(34), chr(34) * 2)}"."{scalar_type.__name__.replace(chr(34), chr(34) * 2)}"'


# Canonical PyQL-style type names for every built-in Pylon scalar marker
# class (pylon.Str, pylon.UUID, ...) — mirrors `pylon-server`'s own
# type-name table (duplicated here rather than imported: schema-descriptor
# construction needs to work with zero server-layer involvement).
_PYQL_TYPE_NAME_BY_CLASS = {
    'Str': 'std::str',
    'Int16': 'std::int16',
    'Int32': 'std::int32',
    'Int64': 'std::int64',
    'Float32': 'std::float32',
    'Float64': 'std::float64',
    'Decimal': 'std::decimal',
    'Bool': 'std::bool',
    'DateTime': 'std::datetime',
    'LocalDateTime': 'cal::local_datetime',
    'LocalDate': 'cal::local_date',
    'LocalTime': 'cal::local_time',
    'Duration': 'std::duration',
    'UUID': 'std::uuid',
    'JSON': 'std::json',
    'Bytes': 'std::bytes',
    'Sequence': 'std::int64',  # sequences are backed by int64
}


def _pyql_type_name(scalar_type: Any) -> str | None:
    """Renders *scalar_type* (a Pylon scalar class, an Array/TupleAnnotation
    instance, or an enum/named-tuple class) as a PyQL-style type-name
    string — e.g. ``"std::str"``, ``"default::Gender"``,
    ``"tuple<x: std::float64, y: std::float64>"``, ``"array<std::str>"``.

    Used for `GlobalDescriptor.scalar_type` so a Rust-side consumer (the
    schema/globals introspection endpoints) can render a global's type
    without needing live Python annotations — `_build_global_descriptor`
    previously stored a bare `__name__`/`repr()` here, which was never a
    real PyQL type string and was outright broken (a non-deterministic
    object repr) for Array/Tuple-typed globals.
    """
    from ._enums import Enum as PylonEnum
    from ._named_tuples import NamedTuple as PylonNamedTuple
    from ._pointers import ArrayAnnotation, TupleAnnotation
    from ._scalars import SHORTHAND_MAP

    if isinstance(scalar_type, TupleAnnotation):
        positional = all(e.name is None for e in scalar_type.elements)
        parts: list[str] = []
        for e in scalar_type.elements:
            elem_text = _pyql_type_name(e.type_)
            if elem_text is None:
                return None
            parts.append(elem_text if positional else f'{e.name}: {elem_text}')
        return f'tuple<{", ".join(parts)}>'
    if isinstance(scalar_type, ArrayAnnotation):
        elem_text = _pyql_type_name(scalar_type.element)
        return f'array<{elem_text}>' if elem_text is not None else None
    if typing.get_origin(scalar_type) is list and typing.get_args(scalar_type):
        element = SHORTHAND_MAP.get(typing.get_args(scalar_type)[0], typing.get_args(scalar_type)[0])
        elem_text = _pyql_type_name(element)
        return f'array<{elem_text}>' if elem_text is not None else None
    if isinstance(scalar_type, type) and issubclass(scalar_type, (PylonEnum, PylonNamedTuple)):
        mod = getattr(scalar_type, '__pylon_module__', None) or (
            (scalar_type.__module__ or 'default').rpartition('.')[-1] or 'default'
        )
        return f'{mod}::{scalar_type.__name__}'
    if isinstance(scalar_type, type):
        builtin_name = _PYQL_TYPE_NAME_BY_CLASS.get(scalar_type.__name__)
        if builtin_name:
            return builtin_name
        if hasattr(scalar_type, '__pylon_base__'):
            mod = getattr(scalar_type, '__pylon_module__', None) or (
                (scalar_type.__module__ or 'default').rpartition('.')[-1] or 'default'
            )
            return f'{mod}::{scalar_type.__name__}'
        # A real `@pylon.type`-decorated schema type (e.g. a computed
        # global like `current_user: Global[Person | None, "select ..."]`)
        # — reuses this file's own `_is_pylon_type`/`_pylon_module_of`/
        # `_pylon_name_of` rather than re-deriving the module inline, since
        # `__pylon_config__.module` (not `__pylon_module__`) is the
        # authoritative source for these.
        if _is_pylon_type(scalar_type):
            return _qualified(_pylon_module_of(scalar_type), _pylon_name_of(scalar_type))
    return None


def _scalar_type_name(scalar_type: Any) -> str:
    """Return a short human-readable name for the scalar type (for Rust ScalarDescriptor.base)."""
    if hasattr(scalar_type, '__name__'):
        return scalar_type.__name__
    return repr(scalar_type)


# ── Default SQL generation ─────────────────────────────────────────────────────


def _python_value_to_sql(value: Any) -> str | None:
    import decimal as _decimal
    import uuid as _uuid

    if isinstance(value, bool):
        return 'true' if value else 'false'
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

    from pylon.modelquery import _Node

    from ._constraints import Default, _NowType, _SequenceNextType
    from ._enums import Enum

    for c in meta.constraints:
        if isinstance(c, Default):
            s = c.sentinel
            if isinstance(s, _NowType):
                return 'now()'
            if isinstance(s, _SequenceNextType):
                return None  # handled separately in _make_property_desc
            # Before the `str` check: a Pylon enum member *is* a `str`, but its
            # value is a label, not a PyQL expression -- read as one it fails to
            # parse and the column silently ends up with no default at all.
            if isinstance(s, Enum):
                return "'" + s.value.replace("'", "''") + "'"
            if isinstance(s, (str, _Node)):
                return None  # PyQL expression — handled by _make_default_pyql
            if s is None:
                return 'NULL'
            # bool must be checked before int (bool is a subclass of int)
            if isinstance(s, bool):
                return 'true' if s else 'false'
            if isinstance(s, (int, float, _decimal.Decimal)):
                return str(s)
            # Anything else is unrepresentable as SQL. Reported by
            # `_make_default_pyql`, which sees the same sentinel and is the
            # one place that can tell "no default here" from "a default that
            # silently produced nothing".
            return None

    if meta.default is not MISSING and meta.default is not None:
        return _python_value_to_sql(meta.default)

    return None


def _make_default_pyql(meta: Any) -> str | None:
    """Return a PyQL expression string for `Default(...)`, or None.

    Accepts both the string form (`Default('std::uuid_generate_v7()')`) and
    an expression built from the `std` namespace
    (`Default(std.uuid_generate_v7())`), which renders to the same text.

    A sentinel this function can't turn into PyQL *and* that
    `_make_default_sql` couldn't turn into SQL used to fall through both and
    emit no default at all — silently, so the column just had no default and
    nothing said why. Such a sentinel now raises instead.
    """
    import decimal as _decimal

    from pylon.modelquery import _Node, render_default_expr

    from ._constraints import Default, _NowType, _SequenceNextType
    from ._enums import Enum

    for c in meta.constraints:
        if not isinstance(c, Default):
            continue
        s = c.sentinel
        if isinstance(s, Enum):
            return None  # a SQL literal — see `_make_default_sql`
        if isinstance(s, str):
            return s
        if isinstance(s, _Node):
            return render_default_expr(s)
        # Everything `_make_default_sql` handles on the SQL side.
        if s is None or isinstance(s, (_NowType, _SequenceNextType, bool, int, float, _decimal.Decimal)):
            return None
        raise TypeError(
            f'Default({s!r}) is not a supported default for {meta.name!r} — pass a literal value, '
            'a PyQL expression string, or a `std`/`math`/`cal` expression such as '
            'Default(std.uuid_generate_v7())'
        )

    return None


# ── Constraint compilation ─────────────────────────────────────────────────────


def _field_checks_and_exclusive(
    meta: Any,
    col: str,
) -> tuple[list[str], bool]:
    """Return (check_sql_list, is_exclusive) for the pointer's constraints."""
    import enum as _enum

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

    # Quoted: a pointer may legitimately be named after a SQL keyword, and an
    # unquoted one silently changes what the CHECK means — a property called
    # `default` produced `CHECK (char_length(default) <= 1024)`, which
    # PostgreSQL rejects with "DEFAULT is not allowed in this context".
    col = f'"{col.replace(chr(34), chr(34) * 2)}"'

    checks: list[str] = []
    is_exclusive = False

    for c in meta.constraints:
        if c is Exclusive:
            is_exclusive = True
        elif isinstance(c, MinValue):
            checks.append(f'{col} >= {c.value!r}')
        elif isinstance(c, MaxValue):
            checks.append(f'{col} <= {c.value!r}')
        elif isinstance(c, MinExValue):
            checks.append(f'{col} > {c.value!r}')
        elif isinstance(c, MaxExValue):
            checks.append(f'{col} < {c.value!r}')
        elif isinstance(c, MaxLen):
            checks.append(f'char_length({col}) <= {c.length!r}')
        elif isinstance(c, MinLen):
            checks.append(f'char_length({col}) >= {c.length!r}')
        elif isinstance(c, Regexp):
            escaped = c.pattern.replace("'", "''")
            checks.append(f"{col} ~ '{escaped}'")
        elif isinstance(c, OneOf):
            # `str()` on an enum member gives "Type.Member", not the value the
            # column actually holds, so the comparison never matched (and the
            # literal was rejected outright for an enum-typed column).
            values = [v.value if isinstance(v, _enum.Enum) else v for v in c.values]
            literals = ', '.join("'" + str(v).replace("'", "''") + "'" for v in values)
            checks.append(f'{col} IN ({literals})')

    return checks, is_exclusive


# ── Pointer descriptor builders ────────────────────────────────────────────────


def _make_property_desc(name: str, meta: Any, _core: Any) -> Any:
    from ._constraints import Default, _SequenceNextType
    from ._scalars import UUID
    from ._scalars import Scalar as PylonScalar

    is_pk = name == 'id' and meta.scalar_type is UUID
    if is_pk:
        id_default_pyql = _make_default_pyql(meta)
        return _core.PropertyDescriptor(
            name='id',
            pg_type='uuid',
            nullable=False,
            default_sql=None if id_default_pyql else 'uuidv7()',
            default_pyql=id_default_pyql,
            description=meta.description,
            check_constraints=[],
            is_exclusive=True,
            is_pk=True,
        )

    pg_type = _to_pg_type(meta.scalar_type)
    domain_type = _domain_type_ref(meta.scalar_type)

    # An anonymous (functional-form `pylon.scalar(Base, ...)`, no `name=`)
    # scalar has no nominal PostgreSQL domain of its own (see
    # _domain_type_ref), so its inline constraints only take effect by
    # folding them into whichever property actually uses it. A *registered*
    # scalar already enforces its own constraints via its DOMAIN's CHECK, so
    # its constraints are left out here to avoid enforcing the same rule
    # twice.
    combined_constraints = list(meta.constraints)
    scalar_type = meta.scalar_type
    if domain_type is None and isinstance(scalar_type, type) and issubclass(scalar_type, PylonScalar):
        combined_constraints.extend(getattr(scalar_type, '__pylon_constraints__', ()))
    checks, is_exclusive = _field_checks_and_exclusive(type('_m', (), {'constraints': combined_constraints})(), name)

    # SequenceNext default: generate nextval('"module"."Name_seq"')
    default_sql = None
    for c in meta.constraints:
        if isinstance(c, Default) and isinstance(c.sentinel, _SequenceNextType):
            scalar_type = meta.scalar_type
            if isinstance(scalar_type, type) and issubclass(scalar_type, PylonScalar):
                mod = getattr(scalar_type, '__pylon_module__', None) or (
                    (scalar_type.__module__ or 'default').rpartition('.')[-1] or 'default'
                )
                seq_name = f'{scalar_type.__name__}_seq'
                default_sql = f"""nextval('"{_pg_schema(mod)}"."{seq_name}"')"""
            break
    default_pyql = None
    if default_sql is None:
        default_sql = _make_default_sql(meta)
    if default_sql is None:
        default_pyql = _make_default_pyql(meta)

    rewrites = [_core.RewriteEntry(on=int(r.on), handler=r.handler) for r in meta.rewrites]

    from ._pointers import TupleAnnotation

    tuple_members = (
        [_build_tuple_member(e.name, e.type_, _core) for e in meta.scalar_type.elements]
        if isinstance(meta.scalar_type, TupleAnnotation)
        else None
    )

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
        tuple_members=tuple_members,
        column_type=domain_type,
    )


def _make_on_delete_policies(on_delete: list[Any], _core: Any) -> list[Any]:
    return [_core.OnDeletePolicy(side=od.side.name, action=od.action.name) for od in on_delete]


def _make_link_desc(name: str, meta: Any, _core: Any) -> Any:
    from ._constraints import Exclusive

    is_exclusive = any(c is Exclusive for c in meta.constraints)
    rewrites = [_core.RewriteEntry(on=int(r.on), handler=r.handler) for r in meta.rewrites]
    return _core.LinkDescriptor(
        name=name,
        target=meta.link_target,  # already a qualified string
        nullable=meta.nullable,
        through=meta.through,  # already a qualified string, or None
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
        through=meta.through,  # already a qualified string or None
        nullable=meta.nullable,
        default_pyql=_make_default_pyql(meta),
        description=meta.description,
        on_delete=_make_on_delete_policies(meta.on_delete, _core),
    )


def _make_computed_desc(name: str, meta: Any, _core: Any) -> Any:
    from ._pointers import LinkAnnotation, MultiLinkAnnotation

    # An object-valued computed — `Computed[MultiLink[Order], "(select
    # .orders limit 5)"]` — has no scalar type to declare. Passing it through
    # `_to_pg_type` would fall off the end of every branch and land on the
    # `"text"` fallback, which then gets compared against what the expression
    # actually produces and rejected as a mismatch. `None` means "nothing to
    # check", which is exactly the truth here; the pointer's real shape comes
    # from the link it selects.
    declared = meta.scalar_type
    if isinstance(declared, (LinkAnnotation, MultiLinkAnnotation)) or _is_pylon_type(declared):
        return _core.ComputedDescriptor(name=name, expression=meta.expression, return_type=None)
    return _core.ComputedDescriptor(
        name=name,
        expression=meta.expression,
        # The user's own declared `Computed[ReturnType, "expr"]` type — same
        # helper _make_property_desc uses for a plain property's pg_type.
        return_type=_to_pg_type(declared),
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
    if '.' in ref:
        prefix, pointer = ref.rsplit('.', 1)
        if prefix != type_name:
            raise SchemaError(
                f'VectorPointer {ref!r}: type prefix {prefix!r} does not match enclosing type {type_name!r}'
            )
    else:
        pointer = ref
    if pointer not in valid_pointers:
        raise SchemaError(f'VectorPointer {ref!r}: pointer {pointer!r} not found on type {type_name!r}')
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


def _make_partition_desc(part: Any, _core: Any, type_name: str, effective: dict) -> Any:
    """Build the Rust `PartitionDescriptor`, checking what only the walker can
    see — that the named pointer exists on this type and is a required
    property. (The Rust `validate_partitions` pass re-checks these against the
    finished schema; catching them here reports them against the class the
    author actually wrote.)
    """
    meta = effective.get(part.pointer)
    if meta is None:
        raise SchemaError(f'Partition on {type_name!r}: pointer {part.pointer!r} is not a property of this type')
    if getattr(meta, 'kind', None) != 'property':
        raise SchemaError(
            f'Partition on {type_name!r}: {part.pointer!r} is a {getattr(meta, "kind", "pointer")}, '
            f'not a property — a partition key must be a stored column'
        )
    if getattr(meta, 'nullable', False):
        raise SchemaError(
            f'Partition on {type_name!r}: {part.pointer!r} is optional — a partition key can never be empty'
        )
    return _core.PartitionDescriptor(
        pointer=part.pointer,
        interval=part.interval,
        premake=part.premake,
        retention=part.retention,
    )


def _resolve_search_pointer(ref: str, type_name: str, valid_pointers: set[str]) -> str:
    if '.' in ref:
        prefix, pointer = ref.rsplit('.', 1)
        if prefix != type_name:
            raise SchemaError(
                f'SearchPointer {ref!r}: type prefix {prefix!r} does not match enclosing type {type_name!r}'
            )
    else:
        pointer = ref
    if pointer not in valid_pointers:
        raise SchemaError(f'SearchPointer {ref!r}: pointer {pointer!r} not found on type {type_name!r}')
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
    signal_ops_by_class: dict[type, int] | None = None,
) -> Any:
    from ._constraints import Exclusive, Expression

    cfg = cls.__pylon_config__
    effective = _effective_pointers(cls)
    parents, interfaces, bases = _find_pylon_parents(cls, class_to_qname)

    properties: list[Any] = []
    links: list[Any] = []
    multilinks: list[Any] = []
    computed: list[Any] = []

    for pointer_name, meta in effective.items():
        if meta.kind == 'property':
            properties.append(_make_property_desc(pointer_name, meta, _core))
        elif meta.kind == 'link':
            links.append(_make_link_desc(pointer_name, meta, _core))
        elif meta.kind == 'multilink':
            multilinks.append(_make_multilink_desc(pointer_name, meta, _core))
        elif meta.kind == 'computed':
            computed.append(_make_computed_desc(pointer_name, meta, _core))

    # Merge in class-level C/I/T from abstract non-materialized parents.
    inherited_constraints, inherited_indexes, inherited_triggers = _collect_inherited_cit(cls)
    all_constraints = inherited_constraints + list(cfg.constraints)
    all_indexes = inherited_indexes + list(cfg.indexes)
    all_triggers = inherited_triggers + list(cfg.triggers)

    # An `Expression` written inside `Property[T, ...]`/`Link[T, ...]` is a
    # CHECK on this type's table just like one written in the class body —
    # what it being on the pointer settles is only *which* type owns it.
    # On a pointer, `__subject__` is that pointer's own value rather than the
    # row — the one thing promoting it to the type loses, so it is resolved
    # here while the pointer it came from is still known.
    pointer_expressions = [
        _core.ExpressionConstraint(expr=c.expr.replace('__subject__', f'.{pointer_name}'))
        for pointer_name, meta in effective.items()
        for c in getattr(meta, 'constraints', ())
        if isinstance(c, Expression)
    ]
    exclusive_constraints = [_make_exclusive_constraint(c, _core) for c in all_constraints if isinstance(c, Exclusive)]
    expression_constraints = [
        _make_expression_constraint(c, _core) for c in all_constraints if isinstance(c, Expression)
    ] + pointer_expressions
    index_descs = [_make_index_desc(idx, _core) for idx in all_indexes]
    vector_index_descs = [
        _make_vector_index_desc(vi, _core, cfg.name, set(effective.keys())) for vi in cfg.vector_indexes
    ]
    search_index_descs = [
        _make_search_index_desc(si, _core, cfg.name, set(effective.keys())) for si in cfg.search_indexes
    ]
    trigger_descs = [_make_trigger_desc(t, _core) for t in all_triggers]
    partition_desc = _make_partition_desc(cfg.partition, _core, cfg.name, effective) if cfg.partition else None

    # Combined on= bitmask across every @pylon.signal handler registered
    # for this type — the live handler callables themselves never cross
    # into the schema, only this bitmask does (see `_registry.SignalRegistration`).
    combined_signal_ops = (signal_ops_by_class or {}).get(cls, 0)
    signal_descs = [_core.SignalEntry(on=combined_signal_ops)] if combined_signal_ops else []

    # Junction types: derive the actual table name from the link/multi-link that references them.
    if cfg.junction and junction_to_ml is not None:
        qname = _qualified(cfg.module, cfg.name)
        source_table, ml_name = junction_to_ml.get(qname, (cfg.table, cfg.name))
        actual_table = f'{source_table}.{ml_name}'
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
        bases=bases,
        exclusive_constraints=exclusive_constraints,
        expression_constraints=expression_constraints,
        indexes=index_descs,
        vector_indexes=vector_index_descs,
        partition=partition_desc,
        search_indexes=search_index_descs,
        triggers=trigger_descs,
        junction=cfg.junction,
        signals=signal_descs,
    )


# ── Scalar / enum / global builders ───────────────────────────────────────────


def _build_scalar_descriptor(cls: type, _core: Any) -> Any:
    from ._scalars import PG_TYPE_MAP
    from ._scalars import Sequence as SequenceScalar

    base_cls = getattr(cls, '__pylon_base__', None)
    base_name = base_cls.__name__ if base_cls else 'Str'
    pg_type = PG_TYPE_MAP.get(base_cls, 'text') if base_cls else 'text'
    is_sequence = isinstance(base_cls, type) and issubclass(base_cls, SequenceScalar)

    # Inline constraints on the scalar itself (from @pylon.scalar(Str, MinValue(0)))
    scalar_constraints = getattr(cls, '__pylon_constraints__', ())
    checks, _ = _field_checks_and_exclusive(
        type('_m', (), {'constraints': list(scalar_constraints), 'rewrites': []})(),
        'value',  # conventional name inside DOMAIN CHECK
    )

    module = getattr(cls, '__pylon_module__', None) or ((cls.__module__ or 'default').rpartition('.')[-1] or 'default')

    return _core.ScalarDescriptor(
        name=cls.__name__,
        module=module,
        base=base_name,
        pg_type=pg_type,
        check_constraints=checks,
        is_sequence=is_sequence,
    )


def _build_enum_descriptor(cls: type, _core: Any) -> Any:
    module = getattr(cls, '__pylon_module__', None) or ((cls.__module__ or 'default').rpartition('.')[-1] or 'default')
    members = [m.name for m in cls]
    return _core.EnumDescriptor(name=cls.__name__, module=module, members=members)


def _build_tuple_member(name: str | None, annotation: Any, _core: Any) -> Any:
    """Build one _core.TupleMember from a raw member type annotation — a
    plain scalar/enum type, a registered NamedTuple class, or a nested
    TupleAnnotation (pylon.Tuple[...]). Shared by nominal named-tuple
    dataclass fields and structural pylon.Tuple[...] elements; recurses for a
    nested tuple member."""
    from ._enums import Enum as PylonEnum
    from ._named_tuples import NamedTuple as PylonNamedTuple
    from ._pointers import TupleAnnotation

    if isinstance(annotation, TupleAnnotation):
        members = [_build_tuple_member(e.name, e.type_, _core) for e in annotation.elements]
        return _core.TupleMember(name, 'tuple', members=members)

    if isinstance(annotation, type) and issubclass(annotation, PylonNamedTuple):
        mod = getattr(annotation, '__pylon_module__', None) or (
            (annotation.__module__ or 'default').rpartition('.')[-1] or 'default'
        )
        return _core.TupleMember(name, 'namedTuple', module=mod, type_name=annotation.__name__)

    if isinstance(annotation, type) and issubclass(annotation, PylonEnum):
        mod = getattr(annotation, '__pylon_module__', None) or (
            (annotation.__module__ or 'default').rpartition('.')[-1] or 'default'
        )
        return _core.TupleMember(name, 'enum', module=mod, type_name=annotation.__name__)

    return _core.TupleMember(name, 'scalar', pg_type=_to_pg_type(annotation))


def _build_named_tuple_descriptor(cls: type, _core: Any) -> Any:
    module = getattr(cls, '__pylon_module__', None) or ((cls.__module__ or 'default').rpartition('.')[-1] or 'default')
    members = [_build_tuple_member(name, annotation, _core) for name, annotation in cls.__annotations__.items()]
    return _core.NamedTupleDescriptor(name=cls.__name__, module=module, members=members)


def _build_global_descriptor(g: Any, _core: Any) -> Any:
    # A real PyQL-style type name (e.g. "std::str", "array<std::str>"),
    # not a bare `__name__`/`repr()` — see `_pyql_type_name`'s own
    # docstring for why the previous version of this was broken for
    # Array/Tuple-typed globals. Falls back to `__name__`/`repr()` only if
    # `_pyql_type_name` genuinely can't classify the value at all (should
    # not happen in practice — every real global scalar_type is one of the
    # cases it covers).
    scalar_type_name = _pyql_type_name(g.scalar_type) or getattr(g.scalar_type, '__name__', repr(g.scalar_type))

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


# ── Channel descriptor builder ────────────────────────────────────────────────


def _build_channel_descriptor(c: Any, _core: Any) -> Any:
    from pylon.datatypes import Object as _PylonObject

    from ._channels import wire_name_for_channel

    wire_name = wire_name_for_channel(c)
    payload_type = c.payload_type

    # `pylon.Object(doc_id=uuid.UUID, score=float)` — an ad hoc named-field
    # payload with no backing table. Reuses the exact same `Object` class
    # query results decode into: the constructor doesn't care whether its
    # kwarg values are data or types, so this call already produced a real
    # `Object` instance whose attributes happen to hold type objects —
    # introspect those back out via `dataclasses.fields`.
    if isinstance(payload_type, _PylonObject):
        fields = dataclasses.fields(payload_type)
        object_fields = [(f.name, _to_pg_type(getattr(payload_type, f.name))) for f in fields]
        return _core.ChannelDescriptor(
            name=c.name,
            module=c.module,
            wire_name=wire_name,
            payload_kind='object',
            payload_object_fields=object_fields,
            description=c.description,
        )

    # A registered @pylon.type/@pylon.interface — the whole object is the payload.
    if isinstance(payload_type, type) and _is_pylon_type(payload_type):
        type_ref = _qualified(_pylon_module_of(payload_type), _pylon_name_of(payload_type))
        return _core.ChannelDescriptor(
            name=c.name,
            module=c.module,
            wire_name=wire_name,
            payload_kind='type',
            payload_type_ref=type_ref,
            description=c.description,
        )

    # Otherwise a plain scalar (str, uuid.UUID, a registered custom scalar, ...).
    pg_type = _to_pg_type(payload_type)
    return _core.ChannelDescriptor(
        name=c.name,
        module=c.module,
        wire_name=wire_name,
        payload_kind='scalar',
        payload_scalar_pg_type=pg_type,
        description=c.description,
    )


def _validate_channels(channels: list) -> None:
    """Cross-schema wire-name uniqueness + reserved-prefix rejection.

    Postgres NOTIFY/LISTEN channels have no schema namespacing at all (a
    flat, database-wide identifier space) — unlike every other named
    construct here, a name collision between two Channels in *different*
    Pylon modules is just as real a conflict as one in the same module, so
    this checks across the whole schema rather than per-module.
    """
    from ._channels import RESERVED_WIRE_NAME_PREFIX, wire_name_for_channel

    seen: dict[str, Any] = {}
    for c in channels:
        wire_name = wire_name_for_channel(c)
        if wire_name.startswith(RESERVED_WIRE_NAME_PREFIX):
            raise SchemaError(
                f'Channel {_qualified(c.module, c.name)!r} has wire name {wire_name!r}, '
                f'which starts with the reserved {RESERVED_WIRE_NAME_PREFIX!r} prefix '
                f"(used internally by Pylon's own cache/signal/index channels) — "
                f'pick a different name= or variable name.'
            )
        if wire_name in seen:
            other = seen[wire_name]
            raise SchemaError(
                f'Duplicate channel wire name {wire_name!r}: '
                f'both {_qualified(other.module, other.name)!r} and {_qualified(c.module, c.name)!r} '
                f'resolve to the same PostgreSQL NOTIFY/LISTEN channel.'
            )
        seen[wire_name] = c


# ── Function descriptor builder ───────────────────────────────────────────────


def _parse_return_annotation(
    annotation: Any,
    type_map: dict[str, Any],
    class_to_qname: dict[int, str],
    anchor: Any = None,
) -> tuple[str, bool, bool, bool]:
    """Parse a return type annotation into (return_pg_type, is_object, is_set, is_polymorphic).

    return_pg_type is either a PostgreSQL type string (for scalars) or a
    qualified type name like 'default::Account' (for object returns).
    """
    import typing as _typing

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

    # Unwrap Annotated[T, lazy(...)].
    #
    # A lazy reference has to be resolved here, not just unwrapped: its inner
    # value is a ForwardRef (both `Annotated` and `get_type_hints` normalize a
    # string parameter into one), so leaving it alone meant the annotation was
    # never recognised as an object type and the function silently came out as
    # `RETURNS text` instead of `RETURNS TABLE(...)`. Only a *directly
    # referenced* class hit the object path, which is why this survived — a
    # schema whose modules import each other has to use `lazy`.
    if _typing.get_origin(annotation) is _typing.Annotated:
        from ._lazy import _Lazy

        a_args = _typing.get_args(annotation)
        inner = a_args[0]
        lazy_markers = [a for a in a_args[1:] if isinstance(a, _Lazy)]
        inner_name = inner if isinstance(inner, str) else getattr(inner, '__forward_arg__', None)
        if lazy_markers and inner_name is not None and anchor is not None:
            resolved = _import_from_lazy(lazy_markers[0].module_path, inner_name, anchor)
            if resolved is None or not _is_pylon_type(resolved):
                raise SchemaError(
                    f'lazy ref {inner_name!r} from {lazy_markers[0].module_path!r} did not resolve to a Pylon type'
                )
            annotation = resolved
        else:
            annotation = inner

    # Check if this is a Pylon object type
    if isinstance(annotation, type) and hasattr(annotation, '__pylon_config__'):
        cfg = annotation.__pylon_config__
        qname = f'{cfg.module}::{cfg.name}'
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
    if isinstance(annotation, type) and hasattr(annotation, '__pylon_config__'):
        return 'uuid'
    return _to_pg_type(annotation)


def _build_function_descriptor(
    func: Any,
    type_map: dict[str, Any],
    class_to_qname: dict[int, str],
    _core: Any,
    seen_signatures: dict[tuple[str, str, tuple[str, ...]], Any] | None = None,
) -> Any:
    import typing as _typing

    config = func.__pylon_function__
    # `get_type_hints` evaluates annotations in the function's own module
    # globals, which will not contain a type the module deliberately does not
    # import — the whole point of `pylon.lazy` is to avoid that import. Offer
    # every unambiguously-named Pylon type as a local so those resolve; an
    # ambiguous short name is left out on purpose rather than guessed at, and
    # its `lazy` marker carries the module anyway.
    localns: dict[str, Any] = {}
    ambiguous: set[str] = set()
    for qname, cls in type_map.items():
        short = qname.split('::')[-1]
        if short in localns and localns[short] is not cls:
            ambiguous.add(short)
        localns[short] = cls
    for short in ambiguous:
        localns.pop(short, None)

    try:
        hints = _typing.get_type_hints(func, include_extras=True, localns=localns)
    except Exception as exc:
        # Falling back to the raw `__annotations__` strings used to be silent,
        # and a string annotation matches none of the object-type checks below
        # — so an unresolvable name did not fail, it quietly produced a scalar
        # `RETURNS text` function whose body still selected objects.
        raise SchemaError(
            f"function '{config.module}::{config.name}': cannot resolve its type "
            f'annotations ({exc}). A type referenced only through `pylon.lazy` '
            f'must still be importable by name, or referenced unambiguously.'
        ) from exc

    # Build parameter descriptors
    sig = __import__('inspect').signature(func)
    params = []
    param_pg_types = []
    for param_name, param in sig.parameters.items():
        annotation = hints.get(param_name, param.annotation)
        if annotation is __import__('inspect').Parameter.empty:
            raise SchemaError(
                f"function '{config.module}::{config.name}' parameter '{param_name}' has no type annotation"
            )
        pg_type = _param_pg_type(annotation)
        params.append(_core.FunctionParamDescriptor(name=param_name, pg_type=pg_type))
        param_pg_types.append(pg_type)

    if seen_signatures is not None:
        sig_key = (config.module, config.name, tuple(param_pg_types))
        if sig_key in seen_signatures:
            raise SchemaError(
                f"Duplicate function signature '{config.module}::{config.name}"
                f"({', '.join(param_pg_types)})': both "
                f'{seen_signatures[sig_key]!r} and {func!r}'
            )
        seen_signatures[sig_key] = func

    # Parse return type
    return_annotation = hints.get('return', __import__('inspect').Parameter.empty)
    if return_annotation is __import__('inspect').Parameter.empty:
        raise SchemaError(f"function '{config.module}::{config.name}' has no return type annotation")
    return_pg_type, return_is_object, return_is_set, return_is_polymorphic = _parse_return_annotation(
        return_annotation, type_map, class_to_qname, anchor=func
    )

    if return_is_object and not return_is_set:
        raise SchemaError(
            f"function '{config.module}::{config.name}': object-returning functions must "
            f'annotate the return type as set[T], not a bare T — single-object returns '
            f'are not supported'
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


# ── Main entry point ───────────────────────────────────────────────────────────


def walk(
    types: list[type],
    enums: list[type],
    custom_scalars: list[type],
    globals_: list[Any],
    functions: list[Any] | None = None,
    aliases: list[Any] | None = None,
    named_tuples: list[type] | None = None,
    signals: list[Any] | None = None,
    channels: list[Any] | None = None,
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

    # Phase 4.6 — combine every @pylon.signal registration's on= bitmask
    # per target class (the handler callables themselves stay out of the
    # schema entirely — see `_registry.SignalRegistration`).
    signal_ops_by_class: dict[type, int] = {}
    for reg in signals or ():
        signal_ops_by_class[reg.target] = signal_ops_by_class.get(reg.target, 0) | reg.on

    # Phase 4.7 — channel wire-name validation (cross-schema: Postgres
    # NOTIFY/LISTEN channels have no schema namespacing at all).
    _validate_channels(channels or [])

    # Phase 5+6 — build PyO3 descriptors
    type_descs = [
        _build_type_descriptor(cls, class_to_qname, _core, junction_to_ml, signal_ops_by_class) for cls in types
    ]
    scalar_descs = [_build_scalar_descriptor(cls, _core) for cls in custom_scalars]
    enum_descs = [_build_enum_descriptor(cls, _core) for cls in enums]
    named_tuple_descs = [_build_named_tuple_descriptor(cls, _core) for cls in (named_tuples or [])]
    global_descs = [_build_global_descriptor(g, _core) for g in globals_]
    seen_fn_signatures: dict[tuple[str, str, tuple[str, ...]], Any] = {}
    fn_descs = [
        _build_function_descriptor(f, type_map, class_to_qname, _core, seen_fn_signatures) for f in (functions or [])
    ]
    alias_descs = [_core.AliasDescriptor(name=a.name, module=a.module, expr=a.expr) for a in (aliases or [])]
    channel_descs = [_build_channel_descriptor(c, _core) for c in (channels or [])]

    schema = _core.SchemaDescriptor(
        types=type_descs,
        scalars=scalar_descs,
        enums=enum_descs,
        named_tuples=named_tuple_descs,
        globals=global_descs,
        functions=fn_descs,
        aliases=alias_descs,
        channels=channel_descs,
    )
    # Every function body, computed-pointer expression, and property/link
    # default gets compiled and its actual produced type checked against its
    # own declared type — raises pylon.exceptions.SchemaError (wrapping every
    # mismatch found, not just the first) if anything's off. See
    # crates/pylon-core/src/validate.rs for the (best-effort, not exhaustive)
    # scope of what this can detect.
    _core.validate_schema_types(schema)
    return schema
