from __future__ import annotations

import copy
import dataclasses
import re
import sys
import types
import typing
import uuid
from typing import Any

from . import _collector
from ._constraints import Default, Description, Exclusive, Expression, Readonly
from ._pointers import (
    ComputedAnnotation,
    LinkAnnotation,
    MultiLinkAnnotation,
    PropertyAnnotation,
)
from ._indexes import Index, SearchIndex, VectorIndex
from ._triggers import Rewrite, Trigger
from ._meta import MISSING, PointerMeta, PylonConfig
from ._scalars import SHORTHAND_MAP
from ._scalars import UUID as PylonUUID

_PASCAL_RE = re.compile(r"(?<=[a-z0-9])(?=[A-Z])")


# ── PEP 649 / Python 3.14 compatible annotation access ───────────────────────
#
# In Python 3.14, class annotations are stored via PEP 649 (lazy __annotate__
# function).  cls.__dict__["__annotations__"] is None until the annotations are
# forced-evaluated, so we must use annotationlib (3.14+) or fall back to
# cls.__dict__ on older releases.


def _get_own_annotations(cls: type) -> dict[str, Any]:
    """Return cls's own annotations dict (not inherited), forcing evaluation."""
    try:
        import annotationlib  # Python 3.14+
        return annotationlib.get_annotations(cls, format=annotationlib.Format.VALUE)
    except ImportError:
        return dict(cls.__dict__.get("__annotations__") or {})


# ── Optional unwrapping ────────────────────────────────────────────────────────


def _unwrap_optional(annotation: Any) -> tuple[bool, Any]:
    """Return (is_nullable, inner_annotation) for any T | None form."""
    # Python 3.10+ union syntax: X | None → types.UnionType
    if isinstance(annotation, types.UnionType):
        args = typing.get_args(annotation)
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1 and len(non_none) < len(args):
            return True, non_none[0]
        return False, annotation

    # typing.Optional[X] / typing.Union[X, None]
    if typing.get_origin(annotation) is typing.Union:
        args = typing.get_args(annotation)
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1 and len(non_none) < len(args):
            return True, non_none[0]

    return False, annotation


# ── Default resolution ─────────────────────────────────────────────────────────


def _resolve_default(
    cls_default: Any,
    constraints: list[Any],
) -> tuple[Any, Any]:
    """Return (default_value, default_factory).

    Rules:
    - Default(Now) in constraints → Python side uses None (server sets the value).
    - Mutable class-level default → factory that deepcopies the captured value.
    - Plain scalar default → returned as-is.
    - No default → (MISSING, MISSING).
    """
    for c in constraints:
        if isinstance(c, Default):
            return None, MISSING

    if cls_default is MISSING:
        return MISSING, MISSING

    if isinstance(cls_default, list | dict | set):
        captured = copy.deepcopy(cls_default)
        return MISSING, lambda v=captured: copy.deepcopy(v)

    return cls_default, MISSING


# ── Annotation → PointerMeta ─────────────────────────────────────────────────────


def _annotation_to_meta(
    name: str,
    annotation: Any,
    nullable: bool,
    cls_default: Any,
) -> PointerMeta:
    if isinstance(annotation, PropertyAnnotation):
        description = next(
            (c.text for c in annotation.constraints if isinstance(c, Description)), None
        )
        rewrites = [c for c in annotation.constraints if isinstance(c, Rewrite)]
        is_readonly = any(c is Readonly for c in annotation.constraints)
        constraints = [
            c for c in annotation.constraints
            if not isinstance(c, (Description, Rewrite)) and c is not Readonly
        ]
        default, factory = _resolve_default(cls_default, annotation.constraints)
        return PointerMeta(
            name=name,
            kind="property",
            scalar_type=annotation.scalar_type,
            nullable=nullable,
            constraints=constraints,
            default=default,
            default_factory=factory,
            description=description,
            rewrites=rewrites,
            is_readonly=is_readonly,
        )

    if isinstance(annotation, LinkAnnotation):
        description = next(
            (c.text for c in annotation.constraints if isinstance(c, Description)), None
        )
        rewrites = [c for c in annotation.constraints if isinstance(c, Rewrite)]
        is_readonly = any(c is Readonly for c in annotation.constraints)
        constraints = [
            c for c in annotation.constraints
            if not isinstance(c, (Description, Rewrite)) and c is not Readonly
        ]
        return PointerMeta(
            name=name,
            kind="link",
            scalar_type=None,
            nullable=nullable,
            constraints=constraints,
            default=None if nullable else MISSING,
            default_factory=MISSING,
            description=description,
            link_target=annotation.target_type,
            rewrites=rewrites,
            on_delete=list(annotation.on_delete),
            is_readonly=is_readonly,
        )

    if isinstance(annotation, MultiLinkAnnotation):
        return PointerMeta(
            name=name,
            kind="multilink",
            scalar_type=None,
            nullable=nullable,
            constraints=[],
            default=MISSING,
            default_factory=list,
            link_target=annotation.target_type,
            through=annotation.through_type,
            on_delete=list(annotation.on_delete),
        )

    if isinstance(annotation, ComputedAnnotation):
        return PointerMeta(
            name=name,
            kind="computed",
            scalar_type=annotation.return_type,
            nullable=True,
            constraints=[],
            default=None,
            default_factory=MISSING,
            expression=annotation.expression,
        )

    # Shorthand: raw Python type (str, int, bool, uuid.UUID, …).
    scalar_type = SHORTHAND_MAP.get(annotation, annotation)
    default, factory = _resolve_default(cls_default, [])
    return PointerMeta(
        name=name,
        kind="property",
        scalar_type=scalar_type,
        nullable=nullable,
        constraints=[],
        default=default,
        default_factory=factory,
    )


# ── Dataclass preparation ──────────────────────────────────────────────────────


def _inject_repr(cls: type) -> None:
    """Replace the dataclass-generated __repr__ with a conventional one.

    Null values render as {} and the class name includes the module prefix
    (e.g. ``default::Person {id: UUID('...'), name: 'Alice', age: {}}``).
    Only attributes actually present in __dict__ are shown (partial shapes).
    """
    def __repr__(self) -> str:
        cfg = getattr(type(self), "__pylon_config__", None)
        qname = (
            f"{cfg.module}::{cfg.name}" if cfg else type(self).__name__
        )
        pylon_type = vars(self).get("__pylon_type__")
        if pylon_type:
            qname = pylon_type
        pairs = ", ".join(
            f"{k}={{}}" if v is None else f"{k}={v!r}"
            for k, v in vars(self).items()
            if k != "__pylon_type__"
        )
        return f"{qname} {{{pairs}}}"

    cls.__repr__ = __repr__  # type: ignore[method-assign]


def _prepare_dataclass(cls: type, pointer_metas: dict[str, PointerMeta]) -> None:
    """Inject dataclasses.field() specs into the class dict before @dataclass runs.

    @dataclass only understands mutable defaults when expressed as
    field(default_factory=...). This function converts them, and also injects
    field(default=None) for nullable fields that have no explicit class-level
    attribute.
    """
    for name, meta in pointer_metas.items():
        if meta.kind == "computed":
            setattr(cls, name, dataclasses.field(init=False, default=None))
            continue

        if meta.kind == "multilink":
            # Always an empty list at construction time; the query populates it.
            setattr(cls, name, dataclasses.field(default_factory=list))
            continue

        if meta.default_factory is not MISSING:
            setattr(cls, name, dataclasses.field(default_factory=meta.default_factory))
            continue

        current = cls.__dict__.get(name, MISSING)

        if meta.default is not MISSING and current is MISSING:
            # The meta carries a resolved default (None for nullable fields or
            # Default(Now) constraints) but no class attribute exists yet.
            setattr(cls, name, dataclasses.field(default=meta.default))
            continue

        if meta.nullable and current is MISSING:
            # Optional annotation (e.g. name: str | None) with no explicit
            # default: inject implicit None.
            setattr(cls, name, dataclasses.field(default=None))


# ── Module / table inference ───────────────────────────────────────────────────


def _infer_module(cls: type) -> str:
    """Infer the Pylon module name from the class's defining Python module.

    Checks for a __pylon_module__ variable in the defining file first, then
    falls back to the last component of the dotted module path.
    """
    defining = sys.modules.get(cls.__module__)
    if defining is not None:
        override = getattr(defining, "__pylon_module__", None)
        if isinstance(override, str):
            return override
    module_path = cls.__module__ or "default"
    return module_path.rpartition(".")[-1] or module_path


def _to_table_name(type_name: str) -> str:
    """Return the PostgreSQL table name for a type.

    The table name is the type's own name, preserving PascalCase.  The module
    maps to the PostgreSQL schema, so account::Account → account."Account".
    The caller is responsible for schema-qualifying the name in DDL.
    """
    return type_name


# ── Pylon base detection and id injection ─────────────────────────────────────


def _has_pylon_base(cls: type) -> bool:
    for base in cls.__mro__[1:]:
        if hasattr(base, "__pylon_config__"):
            return True
    return False


def _inject_id(cls: type) -> None:
    """Prepend id: uuid.UUID | None to the class annotations.

    The id is generated by PostgreSQL (uuidv7) on INSERT; at construction time
    it is None. Query results always carry a populated id.
    """
    existing = _get_own_annotations(cls)
    cls.__annotations__ = {"id": uuid.UUID | None} | existing


# ── Annotation collection ──────────────────────────────────────────────────────


def _collect_annotations(cls: type) -> dict[str, Any]:
    """Return the class's own annotations with string annotations resolved.

    Uses typing.get_type_hints() with include_extras=True so that
    Annotated[T, ...] metadata (e.g. pylon.lazy) is preserved. Falls back to
    the own-annotations dict if resolution fails (e.g. unresolvable forward
    references at this point in the import cycle).

    Uses _get_own_annotations() instead of cls.__dict__["__annotations__"] for
    Python 3.14 PEP 649 compatibility.
    """
    own = _get_own_annotations(cls)
    own_names = set(own.keys())
    if not own_names:
        return {}
    try:
        all_hints = typing.get_type_hints(cls, include_extras=True)
        return {k: v for k, v in all_hints.items() if k in own_names}
    except Exception:
        return own


# ── Core builder ───────────────────────────────────────────────────────────────


def _build_type(
    cls: type,
    *,
    abstract: bool = False,
    materialized: bool = True,
    module: str | None = None,
    name: str | None = None,
    table: str | None = None,
    junction: bool = False,
) -> type:
    # Drain class-body expressions that were registered during the class body.
    exprs = _collector.drain()

    class_indexes = [e for e in exprs if isinstance(e, Index)]
    class_vector_indexes = [e for e in exprs if isinstance(e, VectorIndex)]
    class_search_indexes = [e for e in exprs if isinstance(e, SearchIndex)]
    class_constraints = [e for e in exprs if isinstance(e, (Exclusive, Expression))]
    class_triggers = [e for e in exprs if isinstance(e, Trigger)]

    default_vi = [vi for vi in class_vector_indexes if vi.index_name is None]
    if len(default_vi) > 1:
        raise ValueError(
            f"Type {cls.__name__!r}: at most one bare (default) VectorIndex is allowed; "
            f"assign additional indexes to named attributes."
        )
    default_si = [si for si in class_search_indexes if si.index_name is None]
    if len(default_si) > 1:
        raise ValueError(
            f"Type {cls.__name__!r}: at most one bare (default) SearchIndex is allowed; "
            f"assign additional indexes to named attributes."
        )
    class_desc_exprs = [e for e in exprs if isinstance(e, Description)]

    description = (
        class_desc_exprs[0].text
        if class_desc_exprs
        else ((cls.__doc__ or "").strip() or None)
    )

    # Inject the id property when no parent Pylon type already provides one.
    if not _has_pylon_base(cls):
        _inject_id(cls)

    annotations = _collect_annotations(cls)

    pointer_metas: dict[str, PointerMeta] = {}
    for pointer_name, annotation in annotations.items():
        if pointer_name.startswith("_"):
            continue
        if junction and pointer_name in ("source", "target"):
            raise ValueError(
                f"Junction type {cls.__name__!r}: 'source' and 'target' are reserved "
                f"names — they are injected automatically by Pylon."
            )
        nullable, inner = _unwrap_optional(annotation)
        cls_default = cls.__dict__.get(pointer_name, MISSING)
        meta = _annotation_to_meta(pointer_name, inner, nullable, cls_default)
        if junction and meta.kind != "property":
            raise ValueError(
                f"Junction type {cls.__name__!r}: pointer {pointer_name!r} is a "
                f"{meta.kind!r}; junction types only support scalar properties."
            )
        pointer_metas[pointer_name] = meta

    _prepare_dataclass(cls, pointer_metas)
    dataclasses.dataclass(cls, kw_only=True)
    _inject_repr(cls)

    resolved_module = module or _infer_module(cls)
    resolved_name = name or cls.__name__
    resolved_table = table or _to_table_name(resolved_name)

    cls.__pylon_config__ = PylonConfig(
        module=resolved_module,
        name=resolved_name,
        table=resolved_table,
        abstract=abstract,
        materialized=materialized,
        pointers=pointer_metas,
        constraints=class_constraints,
        indexes=class_indexes,
        vector_indexes=class_vector_indexes,
        search_indexes=class_search_indexes,
        triggers=class_triggers,
        description=description,
        junction=junction,
    )

    from . import _registry
    _registry.register_type(cls)

    return cls


# ── Public decorators ──────────────────────────────────────────────────────────


def type_decorator(
    cls: type | None = None,
    *,
    abstract: bool = False,
    materialized: bool = True,
    module: str | None = None,
    name: str | None = None,
    table: str | None = None,
) -> Any:
    """@pylon.type — concrete, table-backed schema type.

    Can be used with or without arguments::

        @pylon.type
        class Product: ...

        @pylon.type(module='catalog', table='catalog_items')
        class Product: ...
    """

    def _wrap(c: type) -> type:
        return _build_type(
            c,
            abstract=abstract,
            materialized=materialized,
            module=module,
            name=name,
            table=table,
        )

    return _wrap(cls) if cls is not None else _wrap


def abstract_decorator(
    cls: type | None = None,
    *,
    module: str | None = None,
    name: str | None = None,
) -> Any:
    """@pylon.abstract — abstract base type; no DB object is created.

    Pointers and constraints defined here are inherited by concrete subtypes.

    Usage::

        @pylon.abstract
        class Auditable:
            created_at: Property[pylon.DateTime, Default(Now)]
            updated_at: Property[pylon.DateTime, Default(Now)]
    """

    def _wrap(c: type) -> type:
        return _build_type(
            c, abstract=True, materialized=False, module=module, name=name
        )

    return _wrap(cls) if cls is not None else _wrap


def interface_decorator(
    cls: type | None = None,
    *,
    module: str | None = None,
    name: str | None = None,
) -> Any:
    """@pylon.interface — abstract type materialised as a PostgreSQL view.

    Combine abstract=True with materialized=True. Useful for shared query
    surfaces across unrelated concrete types.

    Usage::

        @pylon.interface
        class Publishable:
            published_at: Property[pylon.DateTime] | None
    """

    def _wrap(c: type) -> type:
        return _build_type(
            c, abstract=True, materialized=True, module=module, name=name
        )

    return _wrap(cls) if cls is not None else _wrap


def junction_decorator(
    cls: type | None = None,
    *,
    module: str | None = None,
    name: str | None = None,
) -> Any:
    """@pylon.junction — mark a type as a junction table for MultiLink.

    Junction types hold extra link properties for many-to-many relationships.
    They may only declare scalar properties; 'source' and 'target' are reserved.
    The actual junction table name is derived from the MultiLink that references
    this junction type — the class name is not used as the table name.

    Usage::

        @pylon.junction
        class ProductTag:
            weight: Property[pylon.Float64, MinValue(0)]
            created_at: Property[pylon.DateTime, Default(Now)]

        @pylon.type
        class Product:
            tags: MultiLink[Tag, through(ProductTag)]
    """

    def _wrap(c: type) -> type:
        return _build_type(
            c, abstract=False, materialized=True, junction=True, module=module, name=name
        )

    return _wrap(cls) if cls is not None else _wrap
