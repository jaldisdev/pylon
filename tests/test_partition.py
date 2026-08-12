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

import itertools

import pytest

import pylon.schema as pylon
from pylon.schema import DateTime, Partition, Property
from pylon.schema._walker import SchemaError, walk

_counter = itertools.count()


def _module() -> str:
    """Each test declares its own types, and the schema registry is process
    wide — a fresh module name per test keeps them from colliding."""
    return f'parttest{next(_counter)}'


@pytest.fixture(autouse=True)
def _drain_collector():
    """Class-body expressions (`Index`, `Trigger`, `Partition`, ...) register
    themselves into a process-global collector on construction, which the
    enclosing `@pylon.type` then drains. Constructing one *outside* a class
    body — as the constructor tests below do — leaves it in that collector,
    where the next decorated class would pick it up. Draining on both sides
    keeps each test independent.
    """
    from pylon.schema import _collector

    _collector.drain()
    yield
    _collector.drain()


# ---------------------------------------------------------------------------
# Partition constructor validation
# ---------------------------------------------------------------------------


class TestPartitionConstructor:
    def test_defaults(self):
        p = Partition('occurred_at')
        assert p.pointer == 'occurred_at'
        assert p.interval == 'monthly'
        assert p.premake == 4
        # Retention defaults to keeping everything: the alternative deletes
        # data on a schedule, which must never be the accidental default.
        assert p.retention is None

    @pytest.mark.parametrize('interval', ['daily', 'weekly', 'monthly', 'yearly'])
    def test_accepts_every_supported_interval(self, interval):
        assert Partition('t', interval=interval).interval == interval

    def test_rejects_an_unknown_interval(self):
        with pytest.raises(ValueError, match='interval must be one of'):
            Partition('t', interval='hourly')

    def test_rejects_premake_below_one(self):
        # With no future partitions, the first write past the current range
        # fails outright.
        with pytest.raises(ValueError, match='premake must be at least 1'):
            Partition('t', premake=0)

    def test_rejects_non_positive_retention(self):
        with pytest.raises(ValueError, match='retention must be at least 1'):
            Partition('t', retention=0)

    def test_repr_omits_retention_when_unset(self):
        assert repr(Partition('t')) == "Partition('t', interval='monthly', premake=4)"
        assert 'retention=12' in repr(Partition('t', retention=12))


# ---------------------------------------------------------------------------
# Declaration inside a type
# ---------------------------------------------------------------------------


class TestPartitionDeclaration:
    def test_collected_onto_the_type(self):
        mod = _module()

        @pylon.type(module=mod, name='Event')
        class Event:
            occurred_at: Property[DateTime]
            Partition('occurred_at', interval='daily', premake=7, retention=30)

        part = Event.__pylon_config__.partition
        assert part is not None
        assert (part.pointer, part.interval, part.premake, part.retention) == ('occurred_at', 'daily', 7, 30)

    def test_absent_by_default(self):
        mod = _module()

        @pylon.type(module=mod, name='Plain')
        class Plain:
            occurred_at: Property[DateTime]

        assert Plain.__pylon_config__.partition is None

    def test_at_most_one_per_type(self):
        # A table has exactly one partition key, so a second declaration
        # contradicts the first rather than refining it.
        mod = _module()
        with pytest.raises(ValueError, match='at most one Partition'):

            @pylon.type(module=mod, name='Twice')
            class Twice:
                a: Property[DateTime]
                b: Property[DateTime]
                Partition('a')
                Partition('b')

    def test_rejected_on_an_abstract_type(self):
        mod = _module()
        with pytest.raises(ValueError, match='abstract'):

            @pylon.abstract(module=mod)
            class Timestamped:
                occurred_at: Property[DateTime]
                Partition('occurred_at')


# ---------------------------------------------------------------------------
# Walker: descriptor construction and pointer checks
# ---------------------------------------------------------------------------


class TestPartitionWalk:
    def test_reaches_the_schema_descriptor(self):
        mod = _module()

        @pylon.type(module=mod, name='Event')
        class Event:
            occurred_at: Property[DateTime]
            Partition('occurred_at', interval='monthly', retention=12)

        schema = walk([Event], [], [], [])
        part = schema.types[0].partition
        assert part is not None
        assert part.pointer == 'occurred_at'
        assert part.interval == 'monthly'
        assert part.retention == 12

    def test_unknown_pointer_is_rejected(self):
        mod = _module()

        @pylon.type(module=mod, name='Event')
        class Event:
            occurred_at: Property[DateTime]
            Partition('nope')

        with pytest.raises(SchemaError, match='not a property of this type'):
            walk([Event], [], [], [])

    def test_optional_pointer_is_rejected(self):
        # PostgreSQL has no range for a NULL partition key to land in.
        mod = _module()

        @pylon.type(module=mod, name='Event')
        class Event:
            occurred_at: Property[DateTime] | None
            Partition('occurred_at')

        with pytest.raises(SchemaError, match='can never be empty'):
            walk([Event], [], [], [])


# ---------------------------------------------------------------------------
# Generated DDL
# ---------------------------------------------------------------------------


class TestPartitionDDL:
    def _ddl(self, **kwargs) -> str:
        from pylon._core import export_schema

        mod = _module()

        @pylon.type(module=mod, name='Event')
        class Event:
            occurred_at: Property[DateTime]
            Partition('occurred_at', **kwargs)

        return export_schema(walk([Event], [], [], []))

    def test_declares_range_partitioning(self):
        assert 'PARTITION BY RANGE ("occurred_at")' in self._ddl()

    def test_partition_key_joins_the_primary_key(self):
        assert 'PRIMARY KEY ("id", "occurred_at")' in self._ddl()

    def test_registers_with_partman_idempotently(self):
        ddl = self._ddl(interval='daily', premake=7)
        assert 'partman.create_parent(' in ddl
        assert 'IF NOT EXISTS (SELECT 1 FROM partman.part_config' in ddl
        assert "p_interval := '1 day'" in ddl
        assert 'p_premake := 7' in ddl

    def test_retention_is_applied_and_cleared(self):
        assert "SET retention = '12 months'" in self._ddl(retention=12)
        assert 'SET retention = NULL' in self._ddl()
