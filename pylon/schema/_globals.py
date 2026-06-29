from __future__ import annotations

import dataclasses
import types
import typing
from typing import Any

MISSING = dataclasses.MISSING


def _unwrap_optional(annotation: Any) -> tuple[bool, Any]:
    if isinstance(annotation, types.UnionType):
        args = typing.get_args(annotation)
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1 and len(non_none) < len(args):
            return True, non_none[0]
        return False, annotation
    if typing.get_origin(annotation) is typing.Union:
        args = typing.get_args(annotation)
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1 and len(non_none) < len(args):
            return True, non_none[0]
    return False, annotation


class GlobalAnnotation:
    __slots__ = ("scalar_type", "required", "computed_expr")

    def __init__(self, scalar_type: Any, required: bool, computed_expr: str | None = None) -> None:
        self.scalar_type = scalar_type
        self.required = required
        self.computed_expr = computed_expr

    def __repr__(self) -> str:
        return f"GlobalAnnotation({self.scalar_type!r}, required={self.required})"


class Global:
    """Global variable annotation for schema modules.

    Session globals are injected per-request via ``client.with_globals({})``.
    Computed globals are defined with a PyQL expression and evaluated at query time.

    Usage::

        current_user_id: Global[pylon.UUID]               # required session global
        current_user_id: Global[pylon.UUID | None]         # optional session global
        current_user: Global[pylon.UUID | None, 'select User.id filter User.email = global current_user_email']
    """

    @classmethod
    def __class_getitem__(cls, params: Any) -> GlobalAnnotation:
        if isinstance(params, tuple) and len(params) == 2:
            type_param, expr = params
            if not isinstance(expr, str):
                raise TypeError(
                    f"Global computed expression must be a string literal, "
                    f"got {type(expr).__name__!r}"
                )
            nullable, scalar_type = _unwrap_optional(type_param)
            return GlobalAnnotation(scalar_type=scalar_type, required=not nullable, computed_expr=expr)
        nullable, scalar_type = _unwrap_optional(params)
        return GlobalAnnotation(scalar_type=scalar_type, required=not nullable)


@dataclasses.dataclass
class GlobalDescriptor:
    """Collected metadata for a single module-level global variable."""

    name: str
    module: str
    scalar_type: Any  # pylon scalar class, e.g. pylon.UUID, or raw Python type
    required: bool
    default: Any = dataclasses.field(default_factory=lambda: dataclasses.MISSING)
    computed_expr: str | None = None

    def __repr__(self) -> str:
        default_part = "" if self.default is MISSING else f", default={self.default!r}"
        return (
            f"GlobalDescriptor({self.name!r}, module={self.module!r}, "
            f"scalar_type={self.scalar_type!r}, required={self.required}{default_part})"
        )


def _infer_module_name(module: Any) -> str:
    override = getattr(module, "__pylon_module__", None)
    if isinstance(override, str):
        return override
    module_path = getattr(module, "__name__", "default")
    return module_path.rpartition(".")[-1] or module_path


def collect_module_globals(module: Any) -> list[GlobalDescriptor]:
    """Scan a Python module for Global[T] annotations and return descriptors.

    Handles both eager annotations and ``from __future__ import annotations``
    (string-deferred) by calling ``typing.get_type_hints()``, with a fallback
    to the raw ``__annotations__`` dict on evaluation errors.
    """
    try:
        hints = typing.get_type_hints(module)
    except Exception:
        hints = dict(getattr(module, "__annotations__", {}))

    module_name = _infer_module_name(module)
    result: list[GlobalDescriptor] = []
    for name, annotation in hints.items():
        if name.startswith("_"):
            continue
        if not isinstance(annotation, GlobalAnnotation):
            continue
        default = module.__dict__.get(name, MISSING)
        result.append(
            GlobalDescriptor(
                name=name,
                module=module_name,
                scalar_type=annotation.scalar_type,
                required=annotation.required,
                default=default,
                computed_expr=annotation.computed_expr,
            )
        )
    return result
