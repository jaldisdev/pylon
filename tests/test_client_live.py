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

"""Live-Postgres tests for `pylon.client.Client`'s own schema-installation
behavior — specifically the guarantee added alongside
`pylon.client._install_migrated_schema`: a schema change with no physical
DDL footprint (e.g. a property's `readonly` flag, enforced only by the Rust
compiler consulting `SchemaDescriptor`, never a real Postgres constraint)
must have no effect on query compilation until a migration actually writes
it to `_pylon."Schema"` — even if `pylon.finalize()` (or, as simulated here,
a direct `pylon.query._set_schema()` call standing in for it) already
installed the *new* declaration as the process-level singleton.

Companion to `crates/pylon-client/tests/live_execution_basic.rs`, which
covers the Rust `pylon-client` crate's own (always-DB-only) schema fetch —
this file is specifically about the Python package, since `pylon.finalize()`
builds its schema from whatever `.py` files are on disk with no DB
consultation at all, unlike the Rust client.

Run with: `.venv/bin/pytest tests/test_client_live.py -m live_db`
"""

from __future__ import annotations

import asyncio
import datetime
import decimal
import os
import types as _types
import uuid as _uuid

import pytest

import pylon.schema as pylon
from pylon.datatypes import NamedTupleValue
from pylon.schema import Link, Property, Readonly
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import snapshot
from pylon.schema._walker import walk

pytestmark = pytest.mark.live_db


def _dsn() -> str:
    dsn = os.environ.get('PYLON_PGCON_TEST_DSN')
    if not dsn:
        raise RuntimeError('PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests')
    return dsn


def _build_schema(types, enums, scalars, channels=None, named_tuples=None):
    return walk(types, enums, scalars, [], channels=channels, named_tuples=named_tuples)


def test_readonly_change_has_no_effect_until_a_migration_applies_it(live_pool, unique_module):
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.exceptions import QueryError
    from pylon.query import _set_schema

    module = unique_module('live_readonly_gap')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class WidgetV1:
            name: str

        schema_v1 = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema_v1))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema_v1.to_json())

        # A freshly-connected client sees the just-migrated (non-readonly)
        # schema — the update succeeds.
        client1 = Client(cfg)
        await client1.ensure_connected()
        await client1.execute(f"insert {module}::Widget {{ name := 'first' }}")
        await client1.execute(f"update {module}::Widget set {{ name := 'second' }}")
        await client1.aclose()

        # Redeclare the SAME property as readonly — zero physical DDL
        # footprint, purely a compiler-side flag — but do NOT write a new
        # migration snapshot (no `migration apply` has run against this).
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class WidgetV2:
            name: Property[str, Readonly]

        schema_v2 = _build_schema(*snapshot())

        # Simulate an app process where `pylon.finalize()` already picked
        # up the new (readonly) declaration — e.g. schema.py was edited and
        # the process restarted — well before anyone ran `pylon migration
        # apply`. This is exactly what `pylon.finalize()` itself would do;
        # calling `_set_schema` directly here just skips redoing the
        # file-import machinery for a schema this test already built.
        _set_schema(schema_v2)

        # A brand-new client, freshly connected: `ensure_connected()` must
        # overwrite the singleton above with the database's actually-
        # migrated (still non-readonly) schema — the update must still
        # succeed, proving the un-migrated readonly change had zero effect.
        client2 = Client(cfg)
        await client2.ensure_connected()
        await client2.execute(f"update {module}::Widget set {{ name := 'third' }}")
        await client2.aclose()

        # Now actually apply the migration: write the v2 snapshot.
        await migration_write_schema_snapshot(live_pool, schema_v2.to_json())

        # A third fresh client now sees the migrated (readonly) schema —
        # the same update must be rejected.
        client3 = Client(cfg)
        await client3.ensure_connected()
        with pytest.raises(QueryError, match='read-only'):
            await client3.execute(f"update {module}::Widget set {{ name := 'fourth' }}")
        await client3.aclose()

        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


async def _first_payload(gen, trigger_factory=None, *, timeout: float = 20.0, retry_every: float = 0.25):
    """Start consuming *gen*, fire *trigger_factory* until a notification
    arrives, and return the first yielded payload.

    `client.listen()` is an async *generator* — calling it builds a generator
    object but runs none of its body (including the `add_listener()` call
    that actually registers `LISTEN` with Postgres) until something first
    drives it via `__anext__()`/`async for`. Handing `gen.__anext__()` to
    `asyncio.create_task()` only *schedules* that; it doesn't run any of it
    synchronously. And there is no way to observe from out here whether
    another backend's `LISTEN` has registered — Postgres doesn't expose one
    session's subscriptions to another. A notification sent before
    registration completes is simply never delivered to anyone.

    This used to sleep a fixed 2s and fire once, which is a race by
    construction: 0.1s flaked under full-suite load, 0.5s flaked on a loaded
    machine, and 2s flaked in CI. Rather than widen that margin a fourth
    time, *keep firing* — `trigger_factory` is called repeatedly until the
    listener actually yields. Whichever notification lands first is the one
    the test sees, so registration timing stops mattering at all. A genuine
    regression (the subscription never arriving) still fails loudly, now via
    the overall deadline.

    `trigger_factory` must be a zero-argument callable returning a fresh
    awaitable each call — a bare coroutine can only be awaited once. It must
    also be safe to run more than once; the callers here insert rows whose
    resulting notification payload is identical every time.
    """
    task = asyncio.create_task(gen.__anext__())
    if trigger_factory is None:
        return await asyncio.wait_for(task, timeout=timeout)

    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    try:
        while True:
            await trigger_factory()
            done, _pending = await asyncio.wait({task}, timeout=retry_every)
            if done:
                return task.result()
            if loop.time() >= deadline:
                raise TimeoutError(f'no notification arrived within {timeout}s')
    finally:
        if not task.done():
            task.cancel()


def test_client_listen_decodes_scalar_channel_payload(live_pool, unique_module):
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.schema._channels import Channel, collect_module_channels
    from pylon.schema._triggers import On, Timing, Trigger

    module = unique_module('live_listen_scalar')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str
            Trigger(on=On.Insert, timing=Timing.After, handler='select notify(Pings, __new__.name)')

        channels_module = _types.ModuleType('live_listen_scalar_channels')
        channels_module.__pylon_module__ = module
        channels_module.Pings = Channel(str)
        channels = collect_module_channels(channels_module)

        schema = _build_schema(*snapshot(), channels=channels)
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        payload = await _first_payload(
            client.listen('Pings'),
            lambda: client.execute(f"insert {module}::Widget {{ name := 'gadget' }}"),
        )
        assert payload == 'gadget'

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_client_listen_decodes_type_channel_payload_as_the_rows_id(live_pool, unique_module):
    import uuid

    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.schema._channels import Channel, collect_module_channels
    from pylon.schema._triggers import On, Timing, Trigger

    module = unique_module('live_listen_type')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str
            Trigger(on=On.Insert, timing=Timing.After, handler='select notify(WidgetUpdates, __new__)')

        channels_module = _types.ModuleType('live_listen_type_channels')
        channels_module.__pylon_module__ = module
        channels_module.WidgetUpdates = Channel(Widget)
        channels = collect_module_channels(channels_module)

        schema = _build_schema(*snapshot(), channels=channels)
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        payload = await _first_payload(
            client.listen('WidgetUpdates'),
            lambda: client.execute(f"insert {module}::Widget {{ name := 'gadget' }}"),
        )
        assert isinstance(payload, uuid.UUID)

        # `_first_payload` fires its trigger until one notification lands, so
        # there may be several Widgets by now — the guarantee under test is
        # that the payload is a real inserted row's id, not which one.
        rows = await client.query(f"select {module}::Widget {{ id }} filter .name = 'gadget'")
        assert payload in {r.id for r in rows}

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_client_listen_decodes_object_channel_payload(live_pool, unique_module):
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.datatypes import Object as PylonObject
    from pylon.schema._channels import Channel, collect_module_channels
    from pylon.schema._triggers import On, Timing, Trigger

    module = unique_module('live_listen_object')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str
            score: float
            Trigger(
                on=On.Insert,
                timing=Timing.After,
                handler='select notify(WidgetReady, { name := __new__.name, score := __new__.score })',
            )

        channels_module = _types.ModuleType('live_listen_object_channels')
        channels_module.__pylon_module__ = module
        channels_module.WidgetReady = Channel(PylonObject(name=str, score=float))
        channels = collect_module_channels(channels_module)

        schema = _build_schema(*snapshot(), channels=channels)
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        payload = await _first_payload(
            client.listen('WidgetReady'),
            lambda: client.execute(f"insert {module}::Widget {{ name := 'gadget', score := 0.75 }}"),
        )
        assert isinstance(payload, PylonObject)
        assert payload.name == 'gadget'
        assert payload.score == 0.75

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_client_listen_raises_on_malformed_payload(live_pool, unique_module):
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.exceptions import QueryError
    from pylon.schema._channels import Channel, collect_module_channels, wire_name_for_channel

    module = unique_module('live_listen_malformed')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str

        channels_module = _types.ModuleType('live_listen_malformed_channels')
        channels_module.__pylon_module__ = module
        channels_module.Ids = Channel(__import__('uuid').UUID)
        channels = collect_module_channels(channels_module)
        wire_name = wire_name_for_channel(channels[0])

        schema = _build_schema(*snapshot(), channels=channels)
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        # Bypass notify()'s own compile-time shape validation entirely —
        # a raw NOTIFY with text that isn't a valid uuid, exactly the kind
        # of mismatch `listen()` must raise on rather than silently drop.
        async def send_bad_payload():
            await live_pool.execute(f"SELECT pg_notify('{wire_name}', 'not-a-uuid')", [])

        # `_first_payload` keeps firing until a notification lands, so the
        # outer retry loop this test used to need is gone.
        with pytest.raises(QueryError, match="doesn't match its declared shape"):
            await _first_payload(client.listen('Ids'), send_bad_payload)

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_rollback_discards_the_transactions_writes(live_pool, unique_module):
    """`raise Rollback` leaves the block quietly and leaves no rows behind.

    The point of the exception is a transaction that can write, read its own
    writes, and then vanish — so both halves are asserted here: the row is
    visible to the transaction that inserted it, and gone once the block
    exits. The attempt counter guards the other half of the contract: a
    deliberate abort is not a retriable failure, so the loop must run the
    body exactly once and then fall through instead of re-running it.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.exceptions import Rollback

    module = unique_module('live_tx_rollback')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str

        schema = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        attempts = 0
        async for tx in client.transaction():
            async with tx:
                attempts += 1
                await tx.execute(f"insert {module}::Widget {{ name := 'ghost' }}")
                assert await tx.query(f'select {module}::Widget {{ name }}')
                raise Rollback

        assert attempts == 1
        assert await client.query(f'select {module}::Widget {{ name }}') == []

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_a_decimal_survives_the_round_trip_whatever_its_precision(live_pool, unique_module):
    """Every digit a `Decimal` carries reaches the column and comes back.

    `numeric` is arbitrary precision, and so is the string `DecodedValue::Decimal`
    carries it as, but the wire codec used to route both through a 96-bit,
    28-decimal-place carrier. A value that overflowed that came back rescaled by
    a power of ten rather than rounded or refused: `Decimal(0.0000004)` — 65
    significant digits, because a float has no exact decimal form — arrived as
    3.9999999999999998, ten million times over, silently. Sub-cent costs billed
    from a float are exactly the shape that hits, so the assertion is on the
    value, not merely on the query succeeding.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig

    module = unique_module('live_decimal_precision')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    # Each pairs what a caller holds with what it must still be afterwards.
    # `Decimal(float)` is spelled out rather than computed so the expectation
    # cannot drift with it.
    values = [
        decimal.Decimal(0.0000004),  # noqa: RUF032 — a caller building one from a float is the case under test
        decimal.Decimal('0.00000039999999999999998189924473035450347424557548947632312774658203125'),
        decimal.Decimal('1.2222222222222222222222222222222E-7'),
        decimal.Decimal('1.2222222222222222222222222222222E+40'),
        decimal.Decimal('1E+29'),
        decimal.Decimal('12.50'),
        decimal.Decimal('-9999.001'),
        decimal.Decimal('0'),
        decimal.Decimal('-0.0000004'),
    ]

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Cost')
        class Cost:
            amount: pylon.Decimal

        schema = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        for value in values:
            # As a parameter, which is the path the encoder is on.
            assert await client.query_single('select <decimal>$value;', value=value) == value, value

            # And through a real column, so the scale that reached Postgres is
            # the scale that comes back out of it.
            written = await client.query_required_single(
                f'select (insert {module}::Cost {{ amount := <decimal>$value }}) {{ amount }};',
                value=value,
            )
            assert written.amount == value, value

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_an_array_of_named_tuples_round_trips_as_the_type_it_declares(live_pool, unique_module):
    """A `tuple<…>` inside an `array<…>` keeps its member shape end to end.

    Both halves used to be lost. The column for an `Array[SomeNamedTuple]`
    came out `jsonb` rather than `jsonb[]`, because resolving the nominal
    marker to jsonb dropped the array with it; and the read described the
    property as a plain scalar, so every element came back as the raw decoded
    jsonb — `headers[0]["value"]` worked and `headers[0].value` raised. Both
    the nominal form (hydrates to its registered class) and the structural one
    (hydrates to a named-tuple value) are checked, since they travel through
    different halves of the shape resolution.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig

    module = unique_module('live_array_of_tuples')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.named_tuple
        class Point(pylon.NamedTuple):
            x: pylon.Float64
            y: pylon.Float64

        # The decorator reads the module off the file that defines the class,
        # which would put every run's Point in the same one.
        Point.__pylon_module__ = module

        @pylon.type(module=module, name='Route')
        class Route:
            label: pylon.Str
            waypoints: pylon.Array[Point] | None
            headers: pylon.Array[pylon.Tuple[('name', pylon.Str), ('value', pylon.Str)]] | None

        schema = _build_schema(*snapshot(), named_tuples=[Point])
        ddl = export_schema(schema)
        # Each element is the composite type its declaration asks for, so
        # every member keeps its own Postgres type. A declared named tuple is
        # named for itself; a structural one for its content.
        assert f'"waypoints" "{module}"."Point_t"[]' in ddl, ddl
        assert '"headers" "' in ddl and '"."t_' in ddl, ddl

        await live_pool.batch_execute(ddl)
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        written = await client.query_required_single(
            f"""select (insert {module}::Route {{
                  label := <str>$label,
                  waypoints := <array<{module}::Point>>$waypoints,
                  headers := <array<tuple<name: str, value: str>>>$headers
                }}) {{ label, waypoints, headers }};""",
            label='home',
            waypoints=[{'x': 1.0, 'y': 2.0}, {'x': 3.0, 'y': 4.0}],
            headers=[{'name': 'X-Foo', 'value': 'bar'}],
        )

        waypoints = list(written.waypoints)
        assert all(isinstance(point, Point) for point in waypoints), waypoints
        assert [(point.x, point.y) for point in waypoints] == [(1.0, 2.0), (3.0, 4.0)]

        headers = list(written.headers)
        assert all(isinstance(header, NamedTupleValue) for header in headers), headers
        # A named tuple is a tuple: reachable by name *and* by position.
        assert [(header.name, header.value) for header in headers] == [('X-Foo', 'bar')]
        assert tuple(headers[0]) == ('X-Foo', 'bar')

        # A bare path select reads the same property without a shape around it.
        [bare] = await client.query(f'select {module}::Route.headers;')
        assert [(header.name, header.value) for header in bare] == [('X-Foo', 'bar')]

        # And so does one bound in a `with`, which reads the same column.
        [bound] = await client.query(f'with entries := {module}::Route.headers select entries;')
        assert [(header.name, header.value) for header in bound] == [('X-Foo', 'bar')]

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_a_path_to_an_optional_property_yields_only_the_rows_that_have_one(live_pool, unique_module):
    """An empty is nothing, not a NULL — a path reaching one yields no value.

    `select Webhook.headers` over two webhooks, one of them without headers,
    used to be two results, the second `None`. A shape is the opposite case
    and still reads the unset one as `None`: there the object is the result.
    Aggregated into an array the same empty was a NULL element, which
    Postgres refuses outright for an array-typed one ("cannot accumulate null
    arrays"), so that form failed rather than merely differing.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig

    module = unique_module('live_empty_results')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Team')
        class Team:
            name: pylon.Str

        @pylon.type(module=module, name='Member')
        class Member:
            name: pylon.Str
            nickname: pylon.Str | None
            tags: pylon.Array[pylon.Str] | None
            team: Link[Team]

        schema = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        await client.execute(f"insert {module}::Team {{ name := 'red' }};")
        await client.execute(
            f"""insert {module}::Member {{
                  name := 'with', nickname := 'nick', tags := ['a'],
                  team := (select {module}::Team filter .name = 'red' limit 1)
                }};"""
        )
        await client.execute(
            f"""insert {module}::Member {{
                  name := 'without',
                  team := (select {module}::Team filter .name = 'red' limit 1)
                }};"""
        )

        # The property the second row leaves unset yields one value, not two.
        assert await client.query(f'select {module}::Member.nickname;') == ['nick']
        assert await client.query(f'select {module}::Member.tags;') == [['a']]
        # A required one still yields every row.
        assert sorted(await client.query(f'select {module}::Member.name;')) == ['with', 'without']
        # Bound in a `with`, the set is the same set.
        assert await client.query(f'with names := {module}::Member.nickname select names;') == ['nick']

        # A shape keeps it: the object is the result there, and it has no
        # nickname — which is what `None` says.
        rows = await client.query(f'select {module}::Member {{ name, nickname }} order by .name;')
        assert [(row.name, row.nickname) for row in rows] == [('with', 'nick'), ('without', None)]

        # Aggregated into an array, an empty contributes no element — and for
        # the array-typed property, Postgres would refuse a NULL one.
        team = await client.query_required_single(
            f"""select {module}::Team {{
                  nicknames := .<team[is {module}::Member].nickname,
                  names := .<team[is {module}::Member].name
                }} limit 1;"""
        )
        assert list(team.nicknames) == ['nick']
        assert sorted(team.names) == ['with', 'without']

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_a_set_of_array_valued_properties_reads_back_as_the_arrays_it_holds(live_pool, unique_module):
    """`Team { tag_sets := .<team[is Member].tags }` — a set of arrays.

    PostgreSQL has no array of arrays, so accumulating one member's `text[]`
    beside another's failed outright: "cannot accumulate arrays of different
    dimensionality" (and "cannot accumulate null arrays" where one was
    unset). Each element travels as a record of one field now, which keeps
    every element's own column type — a `uuid` stays a `UUID`, a tuple stays
    a named tuple — rather than flattening the whole thing through jsonb.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig

    module = unique_module('live_array_sets')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.enum('RED', 'BLUE')
        class Colour(pylon.Enum):
            pass

        Colour.__pylon_module__ = module

        @pylon.type(module=module, name='Crew')
        class Crew:
            name: pylon.Str

        @pylon.type(module=module, name='Hand')
        class Hand:
            name: pylon.Str
            tags: pylon.Array[pylon.Str] | None
            colours: pylon.Array[Colour] | None
            keys: pylon.Array[pylon.UUID] | None
            headers: pylon.Array[pylon.Tuple[('name', pylon.Str), ('value', pylon.Str)]] | None
            crew: Link[Crew]

        schema = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        await client.execute(f"insert {module}::Crew {{ name := 'red' }};")
        key = _uuid.UUID(int=7)
        # Arrays of different lengths, and one row with none at all: the
        # three shapes that each failed differently before.
        for name, tags, headers in (
            ('one', ['a'], [('X-One', '1')]),
            ('two', ['b', 'c'], [('X-Two', '2'), ('X-Three', '3')]),
            ('none', None, None),
        ):
            await client.execute(
                f"""insert {module}::Hand {{
                      name := <str>$name,
                      tags := <optional array<str>>$tags,
                      colours := <optional array<{module}::Colour>>$colours,
                      keys := <optional array<uuid>>$keys,
                      headers := <optional array<tuple<name: str, value: str>>>$headers,
                      crew := (select {module}::Crew filter .name = 'red' limit 1)
                    }};""",
                name=name,
                tags=tags,
                colours=['RED', 'BLUE'] if tags else None,
                keys=[key] if tags else None,
                headers=headers,
            )

        crew = await client.query_required_single(
            f"""select {module}::Crew {{
                  tag_sets := .<crew[is {module}::Hand].tags,
                  colour_sets := .<crew[is {module}::Hand].colours,
                  key_sets := .<crew[is {module}::Hand].keys,
                  header_sets := .<crew[is {module}::Hand].headers
                }} limit 1;"""
        )

        assert sorted(list(entry) for entry in crew.tag_sets) == [['a'], ['b', 'c']]
        # An enum array keeps its members, not their labels.
        assert [list(entry) for entry in crew.colour_sets] == [[Colour.RED, Colour.BLUE]] * 2
        # And a uuid array keeps `UUID`s — what a flattening through jsonb
        # would have turned into strings.
        assert [list(entry) for entry in crew.key_sets] == [[key], [key]]
        header_sets = sorted(
            [[(header.name, header.value) for header in entry] for entry in crew.header_sets],
            key=len,
        )
        assert header_sets == [[('X-One', '1')], [('X-Two', '2'), ('X-Three', '3')]]

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_a_decimal_inside_a_tuple_keeps_every_digit(live_pool, unique_module):
    """A tuple travels as jsonb, which has one number type and no scale.

    `12.3400` came back as `12.34` and a value wider than a float as the
    nearest one, because the number was parsed into an `f64` on its way out
    of jsonb. The digits survive now, and the member's own declaration is
    what says to build a decimal from them — a `float64` member beside it
    still reads as a float.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig

    module = unique_module('live_decimal_members')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))
    exact = decimal.Decimal('0.00000039999999999999998189924473035450347424557548947632312774658203125')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Line')
        class Line:
            label: pylon.Str
            price: pylon.Tuple[('amount', pylon.Decimal), ('rate', pylon.Float64)] | None

        schema = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        for amount in (decimal.Decimal('12.3400'), exact):
            written = await client.query_required_single(
                f"""select (insert {module}::Line {{
                      label := 'l',
                      price := (amount := <decimal>$amount, rate := <float64>$rate)
                    }}) {{ price }};""",
                amount=amount,
                rate=0.5,
            )
            # The scale is part of the value: `12.3400` is not `12.34`.
            assert written.price.amount == amount
            assert str(written.price.amount) == str(amount)
            assert isinstance(written.price.amount, decimal.Decimal)
            # The float member beside it is still a float.
            assert written.price.rate == 0.5
            assert isinstance(written.price.rate, float)
            await client.execute(f'delete {module}::Line;')

        # And read back out of the column, not just off the insert.
        await client.execute(
            f"""insert {module}::Line {{
                  label := 'l', price := (amount := <decimal>$amount, rate := 1.0)
                }};""",
            amount=exact,
        )
        [row] = await client.query(f'select {module}::Line {{ price }};')
        assert row.price.amount == exact
        [bare] = await client.query(f'select {module}::Line.price;')
        assert bare.amount == exact

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_a_tuple_parameter_takes_the_shapes_a_caller_holds_it_in(live_pool, unique_module):
    """A tuple-typed parameter binds from a tuple, a class instance or a dict.

    jsonb keys a tuple's named members, and a caller holds the value
    positionally as often as by name — a plain `("X-Foo", "bar")`, an
    instance of the `@pylon.named_tuple` class, a `NamedTupleValue` read back
    from an earlier query. Only the cast knows the member names, so all of
    those used to be refused outright (`cannot bind a composite value as a
    query parameter`) and a dict was the only form that worked, which is what
    made saving a webhook's headers impossible.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.exceptions import InvalidParameterTypeError

    module = unique_module('live_tuple_params')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.named_tuple
        class Point(pylon.NamedTuple):
            x: pylon.Float64
            y: pylon.Float64

        Point.__pylon_module__ = module

        @pylon.type(module=module, name='Route')
        class Route:
            label: pylon.Str
            origin: Point | None
            waypoints: pylon.Array[Point] | None
            headers: pylon.Array[pylon.Tuple[('name', pylon.Str), ('value', pylon.Str)]] | None

        schema = _build_schema(*snapshot(), named_tuples=[Point])
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        # Every form of the same value, read back as the one thing it means.
        for origin, waypoints, headers in (
            ({'x': 1.0, 'y': 2.0}, [{'x': 1.0, 'y': 2.0}], [{'name': 'X-Foo', 'value': 'bar'}]),
            ((1.0, 2.0), [(1.0, 2.0)], [('X-Foo', 'bar')]),
            (Point(x=1.0, y=2.0), [Point(x=1.0, y=2.0)], [NamedTupleValue(name='X-Foo', value='bar')]),
        ):
            written = await client.query_required_single(
                f"""select (insert {module}::Route {{
                      label := <str>$label,
                      origin := <{module}::Point>$origin,
                      waypoints := <array<{module}::Point>>$waypoints,
                      headers := <array<tuple<name: str, value: str>>>$headers
                    }}) {{ origin, waypoints, headers }};""",
                label='home',
                origin=origin,
                waypoints=waypoints,
                headers=headers,
            )
            assert written.origin == Point(x=1.0, y=2.0)
            assert [(p.x, p.y) for p in written.waypoints] == [(1.0, 2.0)]
            assert [(h.name, h.value) for h in written.headers] == [('X-Foo', 'bar')]

        # `save()` writes the same columns with no hand-written cast, so it
        # has to supply one itself.
        route = Route(label='saved', origin=Point(x=7.0, y=7.5), waypoints=[(8.0, 8.5)], headers=None)
        await client.save(route)
        saved = await client.query_required_single(
            f"select {module}::Route {{ origin, waypoints }} filter .label = 'saved' limit 1;"
        )
        assert saved.origin == Point(x=7.0, y=7.5)
        assert [(p.x, p.y) for p in saved.waypoints] == [(8.0, 8.5)]

        route.origin = (3.5, 4.5)
        await client.save(route)
        updated = await client.query_required_single(
            f"select {module}::Route {{ origin }} filter .label = 'saved' limit 1;"
        )
        assert updated.origin == Point(x=3.5, y=4.5)

        # An all-unnamed tuple is a jsonb *array*, not an object — the one
        # tuple shape that has no keys to be read by.
        assert await client.query_single('select <tuple<str, bool>>$pair;', pair=('left', True)) == ('left', True)

        # Arity and type are reported against the argument, before execution,
        # rather than left to come back as a jsonb the database can't read.
        with pytest.raises(InvalidParameterTypeError, match='expected 2 elements'):
            await client.query_single(
                'select <array<tuple<name: str, value: str>>>$headers;',
                headers=[('X-Foo', 'bar', 'extra')],
            )
        with pytest.raises(InvalidParameterTypeError, match="got type 'int'"):
            await client.query_single(
                'select <array<tuple<name: str, value: str>>>$headers;',
                headers=[42],
            )
        # The refusal must leave the connection usable, not poison it.
        assert await client.query_single('select 1;') == 1

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_a_month_bearing_duration_survives_the_round_trip(live_pool, unique_module):
    """`cal::date_duration` and `cal::relative_duration` carry months, and a
    `datetime.timedelta` cannot.

    A month is not a fixed span of time without a date to count it from, so the
    decoder used to refuse a month-bearing interval outright — which left every
    `cal::to_date_duration(months := ...)` unreadable from Python even though
    Postgres had computed it. `RelativeDuration` keeps the three components
    apart, in both directions.
    """
    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.datatypes import RelativeDuration

    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        client = Client(cfg)
        await client.ensure_connected()

        assert await client.query_single('select cal::to_date_duration(years := 1, months := 2, days := 3);') == (
            RelativeDuration(months=14, days=3)
        )
        # Months of 0 stays a timedelta — that is what `std::duration` is.
        assert await client.query_single('select <duration>$d;', d=datetime.timedelta(hours=2)) == (
            datetime.timedelta(hours=2)
        )
        # And as a parameter, which is the encoder's side of the same value.
        given = RelativeDuration(months=14, days=3)
        assert await client.query_single('select <cal::relative_duration>$d;', d=given) == given

        await client.aclose()

    asyncio.run(run())


def test_several_statements_written_together_run_from_every_entry_point(live_pool, unique_module):
    """A script is a script whichever method is handed it.

    Only `Client.query` used to take one; `query_single`, `execute`, the JSON
    variants and *every* method on a transaction rejected the second statement
    at parse time ("unexpected 'select' after the end of the query"), so
    freshening two tables before a probe had to be split into separate calls
    for no reason the caller could see. The value of a script is its last
    statement's rows, which is what each of these reports.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.exceptions import Rollback

    module = unique_module('live_script_entry_points')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str

        schema = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        # Two statements, the second one's rows being the script's value.
        script = (
            f'insert {module}::Widget {{ name := <str>$name }};\n'
            f'select {module}::Widget {{ name }} filter .name = <str>$name;'
        )

        assert [w.name for w in await client.query(script, name='a')] == ['a']
        assert (await client.query_single(script, name='b')).name == 'b'
        assert '"name": "c"' in (await client.query_json(script, name='c')).replace("'", '"')
        assert '"name": "d"' in (await client.query_single_json(script, name='d')).replace("'", '"')
        await client.execute(script, name='e')

        # Every one of them wrote, so the insert really ran each time.
        assert len(await client.query(f'select {module}::Widget {{ name }}')) == 5

        # And the same on a transaction, which runs the statements on the
        # caller's own transaction rather than opening one of its own.
        async for tx in client.transaction():
            async with tx:
                assert [w.name for w in await tx.query(script, name='f')] == ['f']
                assert (await tx.query_single(script, name='g')).name == 'g'
                assert '"name": "h"' in (await tx.query_json(script, name='h')).replace("'", '"')
                assert '"name": "i"' in (await tx.query_single_json(script, name='i')).replace("'", '"')
                await tx.execute(script, name='j')
                assert len(await tx.query(f'select {module}::Widget {{ name }}')) == 10
                raise Rollback

        # The transaction's five are gone; the client's five remain.
        assert len(await client.query(f'select {module}::Widget {{ name }}')) == 5

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_a_string_for_a_non_text_parameter_is_refused_without_losing_the_transaction(live_pool, unique_module):
    """A wrong-typed argument is the client's to reject, not the server's.

    The raw UTF-8 used to be written into whatever binary slot the parameter
    had, so Postgres answered "insufficient data left in message" -- from the
    server, which aborted the open transaction along with it, turning one bad
    argument into the loss of everything the block had done.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.exceptions import Rollback

    module = unique_module('live_wrong_param_type')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str

        schema = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        async for tx in client.transaction():
            async with tx:
                await tx.execute(f"insert {module}::Widget {{ name := 'kept' }}")
                with pytest.raises(Exception) as refused:
                    await tx.query_single(
                        'select <int32>(duration_to_seconds(<duration>$span) * 1000);', span='25 days'
                    )
                assert 'interval' in str(refused.value), refused.value
                # The write before it is still there, so the transaction was
                # never aborted by the refusal.
                assert len(await tx.query(f'select {module}::Widget {{ name }}')) == 1
                raise Rollback

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


@pytest.mark.live_db
def test_a_member_read_out_of_a_stored_tuple_is_the_type_it_declares(live_pool, unique_module):
    """A tuple's column is a composite type, so `.price.amount` is a field of
    that row — a jsonb `->` would be an operator the type does not have.

    The member arrives as the type it was declared with, which is what makes
    a `decimal` member a `Decimal` with its scale rather than whatever a
    number parsed out of jsonb became. Reaching a member that isn't declared
    is a compile error now: a composite has no absent key to hand back.
    """
    from pylon._core import export_schema, migration_ensure_internal_schema, migration_write_schema_snapshot

    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.exceptions import InvalidQueryError

    module = unique_module('live_tuple_members')
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.named_tuple
        class Money(pylon.NamedTuple):
            amount: pylon.Decimal
            currency: pylon.Str

        Money.__pylon_module__ = module

        @pylon.type(module=module, name='Line')
        class Line:
            label: pylon.Str
            price: Money | None
            nested: pylon.Tuple[('deep', pylon.Tuple[('amount', pylon.Decimal)]), ('note', pylon.Str)] | None

        schema = _build_schema(*snapshot(), named_tuples=[Money])
        await live_pool.batch_execute(export_schema(schema))
        await migration_ensure_internal_schema(live_pool)
        await migration_write_schema_snapshot(live_pool, schema.to_json())

        client = Client(cfg)
        await client.ensure_connected()

        await client.execute(
            f"""insert {module}::Line {{
                  label := 'a',
                  price := (amount := <decimal>'12.3400', currency := 'EUR'),
                  nested := (deep := (amount := <decimal>'9.9900'), note := 'n')
                }};"""
        )
        # One member empty, and `nested` absent altogether.
        await client.execute(
            f"""insert {module}::Line {{
                  label := 'b', price := (amount := <decimal>'1.0000', currency := {{}})
                }};"""
        )

        amounts = await client.query(f'select {module}::Line.price.amount order by {module}::Line.label;')
        assert amounts == [decimal.Decimal('12.3400'), decimal.Decimal('1.0000')]
        assert all(isinstance(a, decimal.Decimal) for a in amounts)
        # Every digit, which is the whole reason the column has a type.
        assert [str(a) for a in amounts] == ['12.3400', '1.0000']

        # An empty member is nothing, not a None standing in for one.
        assert await client.query(f'select {module}::Line.price.currency;') == ['EUR']
        # And a member of a tuple that is itself unset is nothing either.
        assert await client.query(f'select {module}::Line.nested.deep.amount;') == [decimal.Decimal('9.9900')]

        # The tuple itself is still there, though — `IS NOT NULL` on a
        # composite asks whether *every* member is set, which would have
        # dropped this row for having one empty member.
        prices = await client.query(f'select {module}::Line.price order by {module}::Line.label;')
        assert [(p.amount, p.currency) for p in prices] == [
            (decimal.Decimal('12.3400'), 'EUR'),
            (decimal.Decimal('1.0000'), None),
        ]

        with pytest.raises(InvalidQueryError) as refused:
            await client.query(f'select {module}::Line.price.nope;')
        assert "'nope' is not a member" in str(refused.value), refused.value
        assert 'amount, currency' in str(refused.value), refused.value

        await client.aclose()
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())
