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
import os
import types as _types

import pytest

import pylon.schema as pylon
from pylon.schema import Property, Readonly
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import snapshot
from pylon.schema._walker import walk

pytestmark = pytest.mark.live_db


def _dsn() -> str:
    dsn = os.environ.get('PYLON_PGCON_TEST_DSN')
    if not dsn:
        raise RuntimeError('PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests')
    return dsn


def _build_schema(types, enums, scalars, channels=None):
    return walk(types, enums, scalars, [], channels=channels)


def test_readonly_change_has_no_effect_until_a_migration_applies_it(live_pool, unique_module):
    from pylon._core import export_schema, migration_ensure_tracking_tables, migration_write_schema_snapshot

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
        await migration_ensure_tracking_tables(live_pool)
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
    from pylon._core import export_schema, migration_ensure_tracking_tables, migration_write_schema_snapshot

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
        await migration_ensure_tracking_tables(live_pool)
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

    from pylon._core import export_schema, migration_ensure_tracking_tables, migration_write_schema_snapshot

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
        await migration_ensure_tracking_tables(live_pool)
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
    from pylon._core import export_schema, migration_ensure_tracking_tables, migration_write_schema_snapshot

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
        await migration_ensure_tracking_tables(live_pool)
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
    from pylon._core import export_schema, migration_ensure_tracking_tables, migration_write_schema_snapshot

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
        await migration_ensure_tracking_tables(live_pool)
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
