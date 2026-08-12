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

"""Process-wide runtime index of `@pylon.signal` handlers.

Built once by `pylon.finalize()` from `_registry.signals_snapshot()` and
installed as a module-level singleton here — deliberately separate from
`SchemaDescriptor`/`pylon.query`'s schema singleton, since only the
`target`/`on` bitmask crosses into Rust (see `_registry.SignalRegistration`);
the live handler callables stay Python-only and are looked up directly from
this index by whatever process ends up dispatching signals (see
`pylon.signals`).
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from ._triggers import On

_index: dict[str, dict[On, list[Callable[..., Any]]]] = {}


def build_index(registrations: list[Any]) -> dict[str, dict[On, list[Callable[..., Any]]]]:
    """Build a fresh index from a `_registry.signals_snapshot()` list.

    Keyed by the target type's own qualified name (`module::Name`, read
    directly off `__pylon_config__` — no separate class-to-qname map
    needed), then by each individual `On` flag a handler was registered
    for (a combined `on=On.Insert | On.Delete` registration appears under
    both `On.Insert` and `On.Delete`, so `handlers_for` can look up by a
    single flag without re-decomposing the bitmask itself).
    """
    index: dict[str, dict[On, list[Callable[..., Any]]]] = {}
    for reg in registrations:
        cfg = reg.target.__pylon_config__
        qname = f'{cfg.module}::{cfg.name}'
        by_op = index.setdefault(qname, {})
        for flag in On:
            if reg.on & flag:
                by_op.setdefault(flag, []).append(reg.handler)
    return index


def _set_index(index: dict[str, dict[On, list[Callable[..., Any]]]]) -> None:
    global _index
    _index = index


def handlers_for(qualified_type_name: str, on: On) -> list[Callable[..., Any]]:
    """Every handler registered for `qualified_type_name` under exactly
    the single flag `on` (call once per flag, not with a combined mask)."""
    return list(_index.get(qualified_type_name, {}).get(on, []))
