"""Live-Postgres tests for the migration diff engine's Python-only pieces —
companion to `crates/pylon-core/tests/live_execution_migration_diff.rs`,
which covers the pure-Rust diff engine. The rename-rejection flow
(`Guidance`, `detect_type_renames`/`detect_col_renames`) is exercised here
instead, since it only exists on the Python side
(`pylon/cli/commands/migrations.py`'s `_rename_prompt_loop`).

Run with: `.venv/bin/pytest tests/test_migrations_live.py -m live_db`
"""

from __future__ import annotations

import asyncio

import pytest

import pylon.schema as pylon
from pylon.schema import MultiLink
from pylon.schema._registry import clear as clear_registry, signals_snapshot, snapshot
from pylon.schema._triggers import signal
from pylon.schema._walker import walk


def _core_has_new_api() -> bool:
    try:
        from pylon import _core

        return hasattr(_core, "Guidance")
    except ImportError:
        return False


requires_new_core = pytest.mark.skipif(not _core_has_new_api(), reason="Requires rebuilt pylon._core with Guidance")

pytestmark = [pytest.mark.live_db, requires_new_core]


def _build_schema(types, enums, scalars, signals=None):
    return walk(types, enums, scalars, [], signals=signals)


def test_reject_rename_falls_back_to_drop_create(live_pool, unique_module):
    from pylon._core import Guidance, detect_type_renames, diff_schema_steps_with_renames_and_fills, export_schema, introspect_db_state

    module = unique_module("live_pyreject_type")

    async def run():
        clear_registry()

        @pylon.type(module=module, name="Widget")
        class Widget:
            name: str

        schema_v1 = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema_v1))
        await live_pool.execute(f'INSERT INTO "{module}"."Widget" (name) VALUES ($1)', ["keep-me"])

        clear_registry()

        @pylon.type(module=module, name="Gadget")
        class Gadget:
            name: str

        schema_v2 = _build_schema(*snapshot())
        db_state = await introspect_db_state(live_pool)

        expected = (module, "Widget", module, "Gadget")

        candidates = detect_type_renames(schema_v2, db_state, Guidance())
        assert any(c[:4] == expected for c in candidates), candidates

        # Simulate the user answering "n" to the rename proposal.
        guidance = Guidance()
        guidance.ban_type_rename(*expected)
        remaining = detect_type_renames(schema_v2, db_state, guidance)
        assert not any(c[:4] == expected for c in remaining), remaining

        # With the rename banned, the diff should fall back to a plain
        # drop (Widget) + create (Gadget) instead.
        steps = diff_schema_steps_with_renames_and_fills(schema_v2, db_state, [], [], [])
        assert any(s.verb == "drop" and "Widget" in s.object_desc for s in steps), [s.object_desc for s in steps]
        assert any(s.verb == "create" and "Gadget" in s.object_desc for s in steps), [s.object_desc for s in steps]

        # `test_phantom_trigger_round_trip_via_python_entrypoints` does a
        # *full-database* introspection diff, which would otherwise see this
        # test's leftover schema as "should be dropped" too — clean up so
        # tests in this file don't interfere with each other.
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_confirmed_rename_applies_and_data_survives(live_pool, unique_module):
    from pylon._core import diff_schema_steps_with_renames_and_fills, export_schema, introspect_db_state

    module = unique_module("live_pyrename_type")

    async def run():
        clear_registry()

        @pylon.type(module=module, name="Widget")
        class Widget:
            name: str

        schema_v1 = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema_v1))
        await live_pool.execute(f'INSERT INTO "{module}"."Widget" (name) VALUES ($1)', ["keep-me"])

        clear_registry()

        @pylon.type(module=module, name="Gadget")
        class Gadget:
            name: str

        schema_v2 = _build_schema(*snapshot())
        db_state = await introspect_db_state(live_pool)

        # `diff_schema_steps_with_renames_and_fills` only *projects* the
        # rename onto its in-memory `current` copy so the rest of the diff
        # sees no further change for this table — it never emits the rename
        # DDL itself. The real `ALTER TABLE ... RENAME TO ...` is the
        # caller's own responsibility, exactly like
        # `_rename_prompt_loop` (`pylon/cli/commands/migrations.py`) builds
        # it directly rather than sourcing it from the diff engine's output.
        await live_pool.batch_execute(f'ALTER TABLE "{module}"."Widget" RENAME TO "Gadget";')

        type_renames = [(module, "Widget", module, "Gadget")]
        steps = diff_schema_steps_with_renames_and_fills(schema_v2, db_state, type_renames, [], [])
        for step in steps:
            for sql, _non_tx in step.resolved_ddl({}):
                await live_pool.batch_execute(sql)

        rows = await live_pool.query_named(f'SELECT name FROM "{module}"."Gadget" WHERE name = $1', ["keep-me"])
        assert len(rows) == 1, "renamed row should still be there with its original data"
        assert rows[0]["name"] == "keep-me"

        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_reject_col_rename_falls_back_to_drop_create(live_pool, unique_module):
    from pylon._core import Guidance, detect_col_renames, diff_schema_steps_with_renames_and_fills, export_schema, introspect_db_state

    module = unique_module("live_pyreject_col")

    async def run():
        clear_registry()

        @pylon.type(module=module, name="Widget")
        class Widget:
            name: str

        schema_v1 = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema_v1))

        clear_registry()

        @pylon.type(module=module, name="Widget")
        class WidgetV2:
            title: str

        schema_v2 = _build_schema(*snapshot())
        db_state = await introspect_db_state(live_pool)

        expected = (module, "Widget", "name", "title")

        candidates = detect_col_renames(schema_v2, db_state, Guidance())
        assert any(c[:4] == expected for c in candidates), candidates

        guidance = Guidance()
        guidance.ban_col_rename(*expected)
        remaining = detect_col_renames(schema_v2, db_state, guidance)
        assert not any(c[:4] == expected for c in remaining), remaining

        steps = diff_schema_steps_with_renames_and_fills(schema_v2, db_state, [], [], [])
        table_step = next((s for s in steps if s.kind == "table" and "Widget" in s.object_desc), None)
        assert table_step is not None, [s.object_desc for s in steps]
        assert table_step.verb == "alter"
        ddl_text = " ".join(sql for sql, _ in table_step.ddl).lower()
        assert "drop column" in ddl_text and "title" in ddl_text, ddl_text

        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_phantom_trigger_round_trip_via_python_entrypoints(live_pool, unique_module):
    from pylon._core import (
        db_state_from_json,
        diff_schema_steps_with_renames_and_fills,
        export_schema,
        introspect_db_state,
        schema_to_db_state_json,
    )

    module = unique_module("live_pytrigger")

    async def run():
        clear_registry()

        @pylon.type(module=module, name="Tag")
        class Tag:
            label: str

        @pylon.type(module=module, name="Widget")
        class Widget:
            name: str
            tags: MultiLink[Tag]

        @signal(Widget)
        async def _on_widget_change(old, new):
            pass

        types, enums, scalars = snapshot()
        schema = _build_schema(types, enums, scalars, signals=signals_snapshot())

        await live_pool.batch_execute(export_schema(schema))

        # Offline projection round trip — the exact JSON stored in
        # `_pylon."Migrations".db_state`, the code path that broke.
        baseline = db_state_from_json(schema_to_db_state_json(schema))
        offline_steps = diff_schema_steps_with_renames_and_fills(schema, baseline, [], [], [])
        assert not offline_steps, [s.object_desc for s in offline_steps]

        # Live introspection, via the same Python-facing entrypoint `pylon
        # migration create` actually calls.
        # NOTE: this is a *full-database* diff, so it only holds if every
        # other test in this file has already cleaned up its own schema by
        # the time this one runs (see each sibling test's own cleanup).
        live_state = await introspect_db_state(live_pool)
        live_steps = diff_schema_steps_with_renames_and_fills(schema, live_state, [], [], [])
        assert not live_steps, [s.object_desc for s in live_steps]

        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())
