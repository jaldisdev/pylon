"""Registry of known Pylon session config options. Exposed to clients via
``Client.with_config()`` and to the frontend via ``/api/config-options``
(``pylon/server/asgi.py``), so the globals/config modal can render a toggle
plus a value editor for each option without hardcoding the list in the UI.

Adding a new option: add an entry here, thread its resolved value from
``Client.with_config()`` through ``pylon.query.compile()``'s matching kwarg,
and consume it in pylon-core's ``SessionConfig``
(``crates/pylon-core/src/ir/mod.rs``).
"""

from __future__ import annotations

import dataclasses
from typing import Any


@dataclasses.dataclass(frozen=True)
class ConfigOptionSpec:
    name: str
    type_name: str  # "bool" for now — the only type pylon-core's SessionConfig knows.
    default: Any


CONFIG_OPTIONS: list[ConfigOptionSpec] = [
    ConfigOptionSpec(
        name="allow_user_specified_id",
        type_name="bool",
        default=False,
    ),
]

CONFIG_OPTIONS_BY_NAME: dict[str, ConfigOptionSpec] = {c.name: c for c in CONFIG_OPTIONS}
