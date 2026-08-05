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

from __future__ import annotations

import dataclasses
import re
from typing import Any

RESERVED_WIRE_NAME_PREFIX = "pylon_"
"""Every Postgres channel Pylon's own internals ever LISTEN/NOTIFY on
(`pylon_index_queue`, `pylon_signal_queue`, `pylon_cache_invalidate` — see
`crates/pylon-core/src/stdlib/ddl.rs`) starts with this. A user `Channel`
landing on one of those exact names would silently share wire traffic with
whichever internal consumer is already listening on it, so the whole prefix
is reserved rather than just the three names in use today."""


class Channel:
    """A PostgreSQL pub/sub channel (`NOTIFY`/`LISTEN`), declared as a bound
    module-level value — not an annotation, since (unlike `Global`/`Alias`)
    it needs real instance state (an optional `name=` override).

    The payload can be:

    - A registered `@pylon.type`/`@pylon.interface` type — `notify()` sends
      the object, `listen()` decodes into that type.
    - A plain scalar (`str`, `uuid.UUID`, a registered custom scalar, ...).
    - An ad hoc named-field shape via `pylon.Object(...)`, passing each
      field's *type* as the keyword value rather than a data value — e.g.
      `pylon.Object(doc_id=uuid.UUID, score=float)`. Every field must be a
      scalar (same restriction as `Tuple`/`NamedTuple`).

    Usage::

        UserUpdates = pylon.Channel(User)
        SearchReady = pylon.Channel(pylon.Object(doc_id=uuid.UUID, score=float), name="search_ready")

    The Postgres channel identifier (the actual `NOTIFY`/`LISTEN` argument)
    is derived from the module + this variable's own name unless overridden
    by `name=` — see `_wire_name_for_channel`. Postgres channels have no
    schema namespacing at all (a flat, database-wide identifier space), so
    this is the one schema construct whose module is folded directly into
    its wire-visible name rather than kept as separate namespacing.
    """

    def __init__(self, payload_type: Any, *, name: str | None = None, description: str | None = None) -> None:
        self.payload_type = payload_type
        self.name_override = name
        self.description = description


@dataclasses.dataclass
class ChannelDescriptor:
    """Collected metadata for a single module-level Channel."""

    name: str
    module: str
    payload_type: Any
    description: str | None = None
    wire_name_override: str | None = None

    def __repr__(self) -> str:
        return f"ChannelDescriptor({self.name!r}, module={self.module!r}, payload_type={self.payload_type!r})"


def _to_snake_case(name: str) -> str:
    s1 = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s1).lower()


def wire_name_for_channel(c: ChannelDescriptor) -> str:
    """The actual Postgres NOTIFY/LISTEN channel identifier for *c*.

    `name=` is used verbatim (full control, no casing applied). Otherwise
    `{module}__{snake_case(variable_name)}` — the module has to be folded in
    here (unlike every other named schema construct) since Postgres channels
    aren't schema-namespaced at all.
    """
    if c.wire_name_override is not None:
        return c.wire_name_override
    return f"{c.module}__{_to_snake_case(c.name)}"


def _infer_module_name(module: Any) -> str:
    override = getattr(module, "__pylon_module__", None)
    if isinstance(override, str):
        return override
    module_path = getattr(module, "__name__", "default")
    return module_path.rpartition(".")[-1] or module_path


def collect_module_channels(module: Any) -> list[ChannelDescriptor]:
    """Scan a module's bound values (not annotations) for `Channel` instances.

    Unlike `Global`/`Alias` (annotation-based, found via `typing.get_type_hints`),
    a `Channel` is a real instantiated value assigned at module scope, so
    discovery scans `vars(module)` instead.
    """
    module_name = _infer_module_name(module)
    result: list[ChannelDescriptor] = []
    for name, value in vars(module).items():
        if name.startswith("_"):
            continue
        if not isinstance(value, Channel):
            continue
        result.append(
            ChannelDescriptor(
                name=name,
                module=module_name,
                payload_type=value.payload_type,
                description=value.description,
                wire_name_override=value.name_override,
            )
        )
    return result
