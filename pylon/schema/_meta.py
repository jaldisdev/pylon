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
from typing import Any

MISSING = dataclasses.MISSING


@dataclasses.dataclass
class PointerMeta:
    """Metadata for a single property/link/multilink/computed pointer on a Pylon type."""

    name: str
    kind: str  # 'property' | 'link' | 'multilink' | 'computed'
    scalar_type: Any  # pylon scalar class, or raw Python type for shorthands
    nullable: bool
    constraints: list[Any]
    default: Any  # dataclasses.MISSING or a concrete default value
    default_factory: Any  # dataclasses.MISSING or a zero-argument callable
    description: str | None = None
    # link / multilink
    link_target: Any = None  # the linked Pylon type
    through: Any = None  # intermediate type for MultiLink with link properties
    # computed
    expression: str | None = None
    # mutation rewrites declared inside Property[T, Rewrite(...)]
    rewrites: list[Any] = dataclasses.field(default_factory=list)
    # deletion policies declared inside Link[T, OnDelete(...)] or MultiLink[T, OnDelete(...)]
    on_delete: list[Any] = dataclasses.field(default_factory=list)
    # transpiler-level read-only flag; does not affect PostgreSQL
    is_readonly: bool = False


@dataclasses.dataclass
class PylonConfig:
    """Schema metadata attached to every Pylon type as __pylon_config__."""

    module: str
    name: str
    table: str
    abstract: bool
    materialized: bool
    pointers: dict[str, PointerMeta] = dataclasses.field(default_factory=dict)
    constraints: list[Any] = dataclasses.field(default_factory=list)
    indexes: list[Any] = dataclasses.field(default_factory=list)
    vector_indexes: list[Any] = dataclasses.field(default_factory=list)
    #: At most one `Partition` per type; `None` for an ordinary table.
    partition: Any = None
    search_indexes: list[Any] = dataclasses.field(default_factory=list)
    triggers: list[Any] = dataclasses.field(default_factory=list)
    description: str | None = None
    junction: bool = False
