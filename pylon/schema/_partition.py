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
from typing import Literal

from . import _collector

PartitionInterval = Literal['daily', 'weekly', 'monthly', 'yearly']

#: The intervals range partitioning is offered on. Range partitioning works
#: on any orderable column, but automatic maintenance ("create the next few
#: ranges, drop the ones past retention") only has a meaning against time.
_VALID_INTERVALS: tuple[str, ...] = ('daily', 'weekly', 'monthly', 'yearly')


class Partition:
    """Declare a type's table as range-partitioned on a time property.

    Partitions are created and dropped by ``pg_partman``, driven by the
    ``PartitionMaintenanceWorker`` that ``pylon serve`` starts automatically
    for any schema containing a ``Partition``.

    Usage::

        @pylon.type
        class Event(pylon.BaseObject):
            occurred_at: Property[pylon.DateTime]
            payload: Property[pylon.JSON]

            pylon.Partition('occurred_at', interval='monthly', retention=12)

    A type may declare **at most one** ``Partition``: a table has exactly one
    partition key, so a second declaration contradicts the first rather than
    refining it.

    Args:
        pointer: The property to partition on. Must be a **required**
            (non-optional) ``DateTime``/``LocalDateTime``/``LocalDate``
            property of this type. PostgreSQL requires the partition key to
            be part of the primary key and never null, so Pylon adds it to
            the primary key for you and rejects an optional property.
        interval: How wide each partition is.
        premake: How many future partitions to keep ready. A write landing in
            a range that doesn't exist yet fails, so this is the margin
            against maintenance falling behind.
        retention: Drop partitions older than this many intervals — e.g.
            ``interval='monthly', retention=12`` keeps a rolling year.
            ``None`` (the default) keeps everything: the alternative deletes
            data on a schedule, which should never be what you get by
            accident.
    """

    __slots__ = ('interval', 'pointer', 'premake', 'retention')

    def __init__(
        self,
        pointer: str,
        *,
        interval: PartitionInterval = 'monthly',
        premake: int = 4,
        retention: int | None = None,
    ) -> None:
        if interval not in _VALID_INTERVALS:
            raise ValueError(f'Partition: interval must be one of {", ".join(_VALID_INTERVALS)}, got {interval!r}')
        if premake < 1:
            raise ValueError(
                f'Partition: premake must be at least 1, got {premake} — with no future partitions '
                f'pre-created, the first write past the current range fails'
            )
        if retention is not None and retention < 1:
            raise ValueError(f'Partition: retention must be at least 1 interval when set, got {retention}')

        self.pointer = pointer
        self.interval = interval
        self.premake = premake
        self.retention = retention
        # Picked up by the enclosing @pylon.type decorator, the same way
        # Index/Exclusive/Trigger class-body expressions are.
        _collector.register(self)

    def __repr__(self) -> str:
        parts = [repr(self.pointer), f'interval={self.interval!r}', f'premake={self.premake}']
        if self.retention is not None:
            parts.append(f'retention={self.retention}')
        return f'Partition({", ".join(parts)})'


@dataclasses.dataclass
class PartitionDescriptor:
    """Collected metadata for one type's `Partition`, handed to the Rust
    schema builder."""

    pointer: str
    interval: str
    premake: int
    retention: int | None = None
