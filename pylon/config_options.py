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
