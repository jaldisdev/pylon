"""Global thread-safe registry of all schema-decorated types, enums, and scalars.

Populated automatically at import time as each @pylon.type / @pylon.abstract /
@pylon.interface / @pylon.enum / @pylon.scalar decorator runs. Consumed once by
pylon.finalize() to build the SchemaDescriptor.
"""

from __future__ import annotations

import threading

_lock = threading.Lock()
_types: list[type] = []
_enums: list[type] = []
_custom_scalars: list[type] = []
_named_tuples: list[type] = []


def register_type(cls: type) -> None:
    with _lock:
        _types.append(cls)


def register_enum(cls: type) -> None:
    with _lock:
        _enums.append(cls)


def register_scalar(cls: type) -> None:
    with _lock:
        _custom_scalars.append(cls)


def register_named_tuple(cls: type) -> None:
    with _lock:
        _named_tuples.append(cls)


def snapshot() -> tuple[list[type], list[type], list[type]]:
    """Return (types, enums, custom_scalars) without clearing the registry."""
    with _lock:
        return list(_types), list(_enums), list(_custom_scalars)


def named_tuples_snapshot() -> list[type]:
    """Return registered named tuple classes."""
    with _lock:
        return list(_named_tuples)


def clear() -> None:
    """Remove all entries. Mainly useful in tests."""
    with _lock:
        _types.clear()
        _enums.clear()
        _custom_scalars.clear()
        _named_tuples.clear()
