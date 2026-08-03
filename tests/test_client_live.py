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

import pytest

import pylon.schema as pylon
from pylon.schema import Property, Readonly
from pylon.schema._registry import clear as clear_registry, snapshot
from pylon.schema._walker import walk

pytestmark = pytest.mark.live_db


def _dsn() -> str:
    dsn = os.environ.get("PYLON_PGCON_TEST_DSN")
    if not dsn:
        raise RuntimeError("PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests")
    return dsn


def _build_schema(types, enums, scalars):
    return walk(types, enums, scalars, [])


def test_readonly_change_has_no_effect_until_a_migration_applies_it(live_pool, unique_module):
    from pylon._core import export_schema, migration_ensure_tracking_tables, migration_write_schema_snapshot
    from pylon.client import Client
    from pylon.config import Config, DatabaseConfig
    from pylon.exceptions import QueryError
    from pylon.query import _set_schema

    module = unique_module("live_readonly_gap")
    cfg = Config(database=DatabaseConfig(dsn=_dsn()))

    async def run():
        clear_registry()

        @pylon.type(module=module, name="Widget")
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

        @pylon.type(module=module, name="Widget")
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
        with pytest.raises(QueryError, match="read-only"):
            await client3.execute(f"update {module}::Widget set {{ name := 'fourth' }}")
        await client3.aclose()

        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())
