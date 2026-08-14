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

"""Global thread-safe registry of all schema-decorated types, enums, scalars,
and signal handlers.

Populated automatically at import time as each @pylon.type / @pylon.abstract /
@pylon.interface / @pylon.enum / @pylon.scalar / @pylon.signal decorator runs.
Consumed once by pylon.finalize() to build the SchemaDescriptor — except
signal registrations, whose live handler callables never cross into the
schema; only their `target`/`on` bitmask does (see `SignalRegistration`).
"""

from __future__ import annotations

import threading
from collections.abc import Callable
from typing import Any, NamedTuple


class SignalRegistration(NamedTuple):
    """One `@pylon.signal(target, on=...)` registration.

    `handler` is the live decorated callable itself — it never crosses into
    the Rust-side schema (only `target`/`on` do, as a bitmask on the
    matching `TypeDescriptor.signals`); it's consulted directly by the
    dispatch loop that drains the outbox a mutation's capture trigger
    writes to.
    """

    target: type
    on: int
    handler: Callable[..., Any]


_lock = threading.Lock()
_types: list[type] = []
_enums: list[type] = []
_custom_scalars: list[type] = []
_named_tuples: list[type] = []
_functions: list = []
_signals: list[SignalRegistration] = []

#: Bumped by every mutation below. Lets a consumer that derives something
#: expensive from the registered classes (`pylon.query.hydration_registry`)
#: memoize it and know exactly when that memo went stale, rather than
#: guessing from a length or an identity that can repeat.
_generation = 0


def generation() -> int:
    """Current registry generation — see `_generation`."""
    return _generation


def _bump() -> None:
    """Invalidate anything memoized against `generation()`. Call while
    holding `_lock`."""
    global _generation
    _generation += 1


def register_type(cls: type) -> None:
    with _lock:
        _types.append(cls)
        _bump()


def register_enum(cls: type) -> None:
    with _lock:
        _enums.append(cls)
        _bump()


def register_scalar(cls: type) -> None:
    with _lock:
        _custom_scalars.append(cls)


def register_named_tuple(cls: type) -> None:
    with _lock:
        _named_tuples.append(cls)
        _bump()


def register_function(func: object) -> None:
    with _lock:
        _functions.append(func)


def register_signal(registration: SignalRegistration) -> None:
    with _lock:
        _signals.append(registration)


def functions_snapshot() -> list:
    with _lock:
        return list(_functions)


def signals_snapshot() -> list[SignalRegistration]:
    with _lock:
        return list(_signals)


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
        _functions.clear()
        _signals.clear()
        _bump()
