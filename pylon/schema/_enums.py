from __future__ import annotations

import enum as _stdlib_enum
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
        return Enum(cls.__name__, {m: m for m in members})  # type: ignore[call-overload]

    return _decorator
