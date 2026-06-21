from __future__ import annotations

import enum as _stdlib_enum
import sys
from typing import Any


class Enum(str, _stdlib_enum.Enum):
    """Base class for all Pylon enum types.

    Member names are PascalCase; each member's value equals its name string.
    PostgreSQL stores the names verbatim in the enum column.

    Usage::

        @pylon.enum('Active', 'Inactive', 'Pending')
        class Status(pylon.Enum):
            pass

        Status.Active          # <Status.Active: 'Active'>
        Status.Active.value    # 'Active'
    """


def enum_decorator(*members: str) -> Any:
    """Class decorator that builds a Pylon enum from positional member name strings.

    Returns a new Enum subclass; the decorated class body is discarded since
    the members are provided entirely via the decorator arguments.
    """

    def _decorator(cls: type) -> type[Enum]:
        new_enum: type[Enum] = Enum(cls.__name__, {m: m for m in members})  # type: ignore[call-overload]
        # Preserve the defining module so the walker can infer the pylon module.
        new_enum.__module__ = cls.__module__

        # Infer and attach the pylon module name (same logic as _decorators._infer_module).
        defining = sys.modules.get(cls.__module__)
        if defining is not None:
            override = getattr(defining, "__pylon_module__", None)
            pylon_module: str = (
                override
                if isinstance(override, str)
                else (cls.__module__ or "default").rpartition(".")[-1] or "default"
            )
        else:
            pylon_module = (cls.__module__ or "default").rpartition(".")[-1] or "default"
        new_enum.__pylon_module__ = pylon_module  # type: ignore[attr-defined]

        from . import _registry
        _registry.register_enum(new_enum)
        return new_enum

    return _decorator
