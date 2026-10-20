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
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import signals_snapshot, snapshot
from pylon.schema._triggers import signal
from pylon.schema._walker import walk

# A stale `pylon._core` is caught once, for the whole run, in conftest.py's
# `pytest_configure`.
pytestmark = [pytest.mark.live_db]


def _build_schema(types, enums, scalars, signals=None):
    return walk(types, enums, scalars, [], signals=signals)


def test_reject_rename_falls_back_to_drop_create(live_pool, unique_module):
    from pylon._core import (
        Guidance,
        detect_type_renames,
        diff_schema_steps_with_renames_and_fills,
        export_schema,
        introspect_db_state,
    )

    module = unique_module('live_pyreject_type')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str

        schema_v1 = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema_v1))
        await live_pool.execute(f'INSERT INTO "{module}"."Widget" (name) VALUES ($1)', ['keep-me'])

        clear_registry()

        @pylon.type(module=module, name='Gadget')
        class Gadget:
            name: str

        schema_v2 = _build_schema(*snapshot())
        db_state = await introspect_db_state(live_pool)

        expected = (module, 'Widget', module, 'Gadget')

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
        assert any(s.verb == 'drop' and 'Widget' in s.object_desc for s in steps), [s.object_desc for s in steps]
        assert any(s.verb == 'create' and 'Gadget' in s.object_desc for s in steps), [s.object_desc for s in steps]

        # `test_phantom_trigger_round_trip_via_python_entrypoints` does a
        # *full-database* introspection diff, which would otherwise see this
        # test's leftover schema as "should be dropped" too — clean up so
        # tests in this file don't interfere with each other.
        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_confirmed_rename_applies_and_data_survives(live_pool, unique_module):
    from pylon._core import diff_schema_steps_with_renames_and_fills, export_schema, introspect_db_state

    module = unique_module('live_pyrename_type')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str

        schema_v1 = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema_v1))
        await live_pool.execute(f'INSERT INTO "{module}"."Widget" (name) VALUES ($1)', ['keep-me'])

        clear_registry()

        @pylon.type(module=module, name='Gadget')
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

        type_renames = [(module, 'Widget', module, 'Gadget')]
        steps = diff_schema_steps_with_renames_and_fills(schema_v2, db_state, type_renames, [], [])
        for step in steps:
            for sql, _non_tx in step.resolved_ddl({}):
                await live_pool.batch_execute(sql)

        rows = await live_pool.query_named(f'SELECT name FROM "{module}"."Gadget" WHERE name = $1', ['keep-me'])
        assert len(rows) == 1, 'renamed row should still be there with its original data'
        assert rows[0]['name'] == 'keep-me'

        await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

    asyncio.run(run())


def test_reject_col_rename_falls_back_to_drop_create(live_pool, unique_module):
    from pylon._core import (
        Guidance,
        detect_col_renames,
        diff_schema_steps_with_renames_and_fills,
        export_schema,
        introspect_db_state,
    )

    module = unique_module('live_pyreject_col')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Widget')
        class Widget:
            name: str

        schema_v1 = _build_schema(*snapshot())
        await live_pool.batch_execute(export_schema(schema_v1))

        clear_registry()

        @pylon.type(module=module, name='Widget')
        class WidgetV2:
            title: str

        schema_v2 = _build_schema(*snapshot())
        db_state = await introspect_db_state(live_pool)

        expected = (module, 'Widget', 'name', 'title')

        candidates = detect_col_renames(schema_v2, db_state, Guidance())
        assert any(c[:4] == expected for c in candidates), candidates

        guidance = Guidance()
        guidance.ban_col_rename(*expected)
        remaining = detect_col_renames(schema_v2, db_state, guidance)
        assert not any(c[:4] == expected for c in remaining), remaining

        steps = diff_schema_steps_with_renames_and_fills(schema_v2, db_state, [], [], [])
        table_step = next((s for s in steps if s.kind == 'table' and 'Widget' in s.object_desc), None)
        assert table_step is not None, [s.object_desc for s in steps]
        assert table_step.verb == 'alter'
        ddl_text = ' '.join(sql for sql, _ in table_step.ddl).lower()
        assert 'drop column' in ddl_text and 'title' in ddl_text, ddl_text

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

    module = unique_module('live_pytrigger')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Tag')
        class Tag:
            label: str

        @pylon.type(module=module, name='Widget')
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


def test_readonly_only_change_is_captured_by_migration_create(live_pool, tmp_path, unique_module):
    """A schema change with zero physical DDL footprint (flipping `Readonly`
    on an existing property, here) used to be structurally invisible to
    `migration create`/`apply`: the diff engine only ever compares against
    live-catalog introspection, which has nothing to see for a change like
    this, so it printed "No schema changes detected" and never wrote a
    migration file at all — not "doesn't need one," genuinely could never
    produce one. This drives the whole CLI flow (`migration create` twice,
    `migration apply` twice) end to end against a real database, proving
    the second `create` now produces a migration and the second `apply`
    updates `_pylon."Schema"` to reflect the flip.
    """
    import json
    import re

    from click.testing import CliRunner
    from conftest import live_db_dsn

    from pylon.cli.commands.migrations import migration
    from pylon.config import Config, DatabaseConfig, ProjectConfig

    module = unique_module('live_content_only')
    (tmp_path / 'migrations').mkdir()
    schema_file = tmp_path / 'widget_schema.py'

    def write_schema(readonly: bool) -> None:
        constraint = ', pylon.Readonly' if readonly else ''
        schema_file.write_text(
            'import pylon.schema as pylon\n\n'
            f"@pylon.type(module={module!r}, name='Widget')\n"
            'class Widget:\n'
            f'    name: pylon.Property[str{constraint}]\n'
        )

    write_schema(readonly=False)

    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=tmp_path),
    )
    obj = {'config': config}
    runner = CliRunner()
    migrations_dir = tmp_path / 'migrations'

    try:
        # First migration: real DDL (creates the table). Baseline, not the
        # behavior under test.
        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        first_files = sorted(migrations_dir.glob('*.sql'))
        assert len(first_files) == 1, result.output

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        # Flip `name` to readonly — the live table's columns don't change at
        # all, so the DDL diff is empty. Before the fix this made `create`
        # a silent no-op forever.
        write_schema(readonly=True)

        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        assert 'No schema changes detected' not in result.output, result.output

        second_files = sorted(migrations_dir.glob('*.sql'))
        assert len(second_files) == 2, [f.name for f in second_files]
        new_file = next(f for f in second_files if f not in first_files)
        assert 'non-DDL schema change' in new_file.read_text()

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        # `_pylon."Schema"` must now reflect the readonly flip — parsed as
        # raw JSON rather than through the pyo3 SchemaDescriptor object
        # model, which doesn't expose a `properties` getter.
        async def check() -> None:
            from pylon._core import migration_read_schema_snapshot

            snapshot_json = await migration_read_schema_snapshot(live_pool)
            assert snapshot_json is not None
            snapshot = json.loads(snapshot_json)
            widget = next(t for t in snapshot['types'] if t['name'] == 'Widget' and t['module'] == module)
            name_prop = next(p for p in widget['properties'] if p['name'] == 'name')
            assert name_prop['is_readonly'], 'readonly-only change never made it into _pylon."Schema"'

        asyncio.run(check())
    finally:
        # `_pylon."Migrations"` is a shared singleton table across this
        # whole test DB (same non-isolation concern noted throughout this
        # file's Rust counterpart) — dropping only the module's own Postgres
        # schema would leave this test's tracking rows behind, which the
        # *next* `create`/`apply` run anywhere would immediately trip over
        # as a "history has diverged" chain conflict against its own empty
        # on-disk chain.
        migration_ids = re.findall(
            r'^-- migration: (\S+)$',
            '\n'.join(f.read_text() for f in migrations_dir.glob('*.sql')),
            re.MULTILINE,
        )

        async def cleanup() -> None:
            if migration_ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [migration_ids])
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

        asyncio.run(cleanup())


def test_reload_schema_collects_aliases(live_pool, tmp_path, unique_module):
    """`_reload_schema()` (used by `migration create`/`apply`/`watch`) never
    collected `Alias` declarations at all, unlike `pylon.finalize()`'s own
    schema-building path — so any `Alias` a project declared would silently
    vanish from every migration and from `_pylon."Schema"` the moment a
    migration was ever applied through the normal CLI flow, even though
    `pylon.finalize()` (the in-process path) saw it fine. This proves a
    single `create` + `apply` now round-trips an Alias into `_pylon."Schema"`.
    """
    import json
    import re

    from click.testing import CliRunner
    from conftest import live_db_dsn

    from pylon.cli.commands.migrations import migration
    from pylon.config import Config, DatabaseConfig, ProjectConfig

    module = unique_module('live_alias_reload')
    (tmp_path / 'migrations').mkdir()
    # A filename distinct from other tests in this file matters here, not
    # just for tidiness: `_reload_schema` imports by module *name*
    # (`importlib.import_module(stem)`), and `sys.modules` caches by that
    # same name regardless of which `tmp_path` it came from — a later test
    # reusing "widget_schema" would silently get an already-cached stale
    # module object from an earlier test's different tmp_path instead of
    # importing this file at all (confirmed while writing this test). Not a
    # real production concern since a real project's schema_dir is stable
    # across invocations, but it bites two tests in the same process that
    # happen to pick the same schema filename.
    (tmp_path / 'alias_widget_schema.py').write_text(
        'import pylon.schema as pylon\n\n'
        f"@pylon.type(module={module!r}, name='Widget')\n"
        'class Widget:\n'
        '    name: str\n\n'
        'published_widgets: pylon.Alias["select Widget filter .name = \'keep\'"]\n'
    )

    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=tmp_path),
    )
    obj = {'config': config}
    runner = CliRunner()
    migrations_dir = tmp_path / 'migrations'

    try:
        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        async def check() -> None:
            from pylon._core import migration_read_schema_snapshot

            snapshot_json = await migration_read_schema_snapshot(live_pool)
            assert snapshot_json is not None
            snapshot = json.loads(snapshot_json)
            # An Alias's own Pylon module comes from the schema *file's*
            # module (its filename stem, absent a `__pylon_module__`
            # override) — not from the `module=` kwarg on any type declared
            # in it, so this doesn't match `module` (the unique() value used
            # for Widget's own type-level module).
            alias = next(
                (a for a in snapshot['aliases'] if a['name'] == 'published_widgets'),
                None,
            )
            assert alias is not None, snapshot['aliases']
            assert alias['expr'] == "select Widget filter .name = 'keep'"

        asyncio.run(check())
    finally:
        migration_ids = re.findall(
            r'^-- migration: (\S+)$',
            '\n'.join(f.read_text() for f in migrations_dir.glob('*.sql')),
            re.MULTILINE,
        )

        async def cleanup() -> None:
            if migration_ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [migration_ids])
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

        asyncio.run(cleanup())


def test_channel_addition_is_captured_by_migration_create(live_pool, tmp_path, unique_module):
    """A `Channel` has zero physical DDL footprint (no table, no column, no
    catalog object at all — Postgres NOTIFY/LISTEN channels aren't backed by
    anything) — exactly the kind of change the `schema_content_changed` fix
    exists to catch. Proves the full CLI flow end to end: a schema whose
    *only* change is adding a Channel still produces a migration through
    `create`, and `apply` still syncs `_pylon."Schema"` to include it.
    """
    import json
    import re

    from click.testing import CliRunner
    from conftest import live_db_dsn

    from pylon.cli.commands.migrations import migration
    from pylon.config import Config, DatabaseConfig, ProjectConfig

    module = unique_module('live_channel_reload')
    (tmp_path / 'migrations').mkdir()
    schema_file = tmp_path / 'channel_widget_schema.py'

    def write_schema(with_channel: bool) -> None:
        channel_line = '\nUserUpdates = pylon.Channel(Widget)\n' if with_channel else ''
        schema_file.write_text(
            'import pylon.schema as pylon\n\n'
            f"@pylon.type(module={module!r}, name='Widget')\n"
            'class Widget:\n'
            '    name: str\n'
            f'{channel_line}'
        )

    write_schema(with_channel=False)

    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=tmp_path),
    )
    obj = {'config': config}
    runner = CliRunner()
    migrations_dir = tmp_path / 'migrations'

    try:
        # First migration: real DDL (creates the table), no Channel yet.
        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        first_files = sorted(migrations_dir.glob('*.sql'))
        assert len(first_files) == 1, result.output

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        # Add the Channel — zero DDL footprint, so the diff is empty. Before
        # the schema_content_changed fix this made `create` a silent no-op.
        write_schema(with_channel=True)

        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        assert 'No schema changes detected' not in result.output, result.output

        second_files = sorted(migrations_dir.glob('*.sql'))
        assert len(second_files) == 2, [f.name for f in second_files]
        new_file = next(f for f in second_files if f not in first_files)
        assert 'non-DDL schema change' in new_file.read_text()

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        async def check() -> None:
            from pylon._core import migration_read_schema_snapshot

            snapshot_json = await migration_read_schema_snapshot(live_pool)
            assert snapshot_json is not None
            snapshot = json.loads(snapshot_json)
            channel = next(
                (c for c in snapshot['channels'] if c['name'] == 'UserUpdates'),
                None,
            )
            assert channel is not None, snapshot['channels']
            assert channel['payload'] == {'Type': f'{module}::Widget'}

        asyncio.run(check())
    finally:
        migration_ids = re.findall(
            r'^-- migration: (\S+)$',
            '\n'.join(f.read_text() for f in migrations_dir.glob('*.sql')),
            re.MULTILINE,
        )

        async def cleanup() -> None:
            if migration_ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [migration_ids])
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

        asyncio.run(cleanup())


@pytest.mark.parametrize('direction', ['grown', 'shrunk'])
def test_baseline_survives_schema_edits_between_create_and_apply(live_pool, tmp_path, unique_module, direction):
    """`create` refuses to run while anything is pending, so editing the
    schema between creating a migration and applying it is the normal way to
    work. `apply` used to derive the tip's diff baseline from whatever the
    schema files said when it ran, so those edits got recorded as already
    live and the next `create` diffed against a database that never existed:
    a grown schema lost the delta ("No schema changes detected"), a shrunk
    one re-emitted statements the previous migration already carried.
    """
    import re

    from click.testing import CliRunner
    from conftest import live_db_dsn

    from pylon.cli.commands.migrations import migration
    from pylon.config import Config, DatabaseConfig, ProjectConfig

    module = unique_module(f'live_baseline_{direction}')
    (tmp_path / 'migrations').mkdir()
    schema_file = tmp_path / f'baseline_{direction}_schema.py'

    def write_schema(with_gadget: bool) -> None:
        gadget = (
            f'\n@pylon.type(module={module!r}, name="Gadget")\nclass Gadget:\n    label: pylon.Property[str]\n'
            if with_gadget
            else ''
        )
        schema_file.write_text(
            'import pylon.schema as pylon\n\n'
            f'@pylon.type(module={module!r}, name="Widget")\n'
            'class Widget:\n'
            '    name: pylon.Property[str]\n' + gadget
        )

    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=tmp_path),
    )
    obj = {'config': config}
    runner = CliRunner()
    migrations_dir = tmp_path / 'migrations'

    # "grown" creates without Gadget and then adds it; "shrunk" is the mirror.
    created_with_gadget = direction == 'shrunk'

    try:
        write_schema(with_gadget=created_with_gadget)
        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        first_files = sorted(migrations_dir.glob('*.sql'))
        assert len(first_files) == 1, result.output

        # The edit `apply` used to mistake for the state it had just reached.
        write_schema(with_gadget=not created_with_gadget)

        # Creating on top of a pending migration stays refused, as in Gel.
        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 1, result.output
        assert 'unapplied migration(s)' in result.output, result.output

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        if direction == 'grown':
            result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
            assert result.exit_code == 0, result.output
            assert 'No schema changes detected' not in result.output, result.output
            second_files = sorted(migrations_dir.glob('*.sql'))
            assert len(second_files) == 2, [f.name for f in second_files]
            new_file = next(f for f in second_files if f not in first_files)
            assert 'Gadget' in new_file.read_text(), new_file.read_text()
        else:
            # Gadget is back in the files and really is in the database, so
            # there is nothing left to migrate.
            write_schema(with_gadget=True)
            result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
            assert result.exit_code == 0, result.output
            assert 'No schema changes detected' in result.output, result.output
            assert sorted(migrations_dir.glob('*.sql')) == first_files, result.output
    finally:
        migration_ids = re.findall(
            r'^-- migration: (\S+)$',
            '\n'.join(f.read_text() for f in migrations_dir.glob('*.sql')),
            re.MULTILINE,
        )

        async def cleanup() -> None:
            if migration_ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [migration_ids])
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

        asyncio.run(cleanup())


def test_migration_log_marks_a_created_but_unapplied_migration_as_pending(live_pool, tmp_path, unique_module):
    """`create` records a tracking row before the migration is applied, so
    `log --from-db` sees one with a NULL `applied_at`. It used to print that
    straight through as `None`.
    """
    import re

    from click.testing import CliRunner
    from conftest import live_db_dsn

    from pylon.cli.commands.migrations import migration
    from pylon.config import Config, DatabaseConfig, ProjectConfig

    module = unique_module('live_log_pending')
    (tmp_path / 'migrations').mkdir()
    (tmp_path / 'log_pending_schema.py').write_text(
        'import pylon.schema as pylon\n\n'
        f'@pylon.type(module={module!r}, name="Widget")\n'
        'class Widget:\n'
        '    name: pylon.Property[str]\n'
    )

    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=tmp_path),
    )
    obj = {'config': config}
    runner = CliRunner()
    migrations_dir = tmp_path / 'migrations'

    try:
        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output

        result = runner.invoke(migration.commands['log'], ['--from-db'], obj=obj)
        assert result.exit_code == 0, result.output
        assert '(pending)' in result.output, result.output
        assert 'None' not in result.output, result.output

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        result = runner.invoke(migration.commands['log'], ['--from-db'], obj=obj)
        assert result.exit_code == 0, result.output
        assert '(pending)' not in result.output, result.output
        assert 'UTC' in result.output, result.output
    finally:
        migration_ids = re.findall(
            r'^-- migration: (\S+)$',
            '\n'.join(f.read_text() for f in migrations_dir.glob('*.sql')),
            re.MULTILINE,
        )

        async def cleanup() -> None:
            if migration_ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [migration_ids])
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

        asyncio.run(cleanup())


def test_apply_rebases_onto_watch_drift_without_being_told_to(live_pool, tmp_path, unique_module):
    """`watch` pushes DDL straight to the database, so an `apply` that re-runs
    it has to tolerate objects that already exist. That used to depend on the
    operator remembering `--dev-mode` — without it the first duplicate object
    aborted the step with a bare Postgres error. `watch` now records itself
    as the last writer of `_pylon."Schema"`, which `apply` reads.
    """
    import re

    from click.testing import CliRunner
    from conftest import live_db_dsn

    from pylon.cli.commands.migrations import _sync_once, migration
    from pylon.config import Config, DatabaseConfig, ProjectConfig

    module = unique_module('live_watch_drift')
    (tmp_path / 'migrations').mkdir()
    (tmp_path / 'watch_drift_schema.py').write_text(
        'import pylon.schema as pylon\n\n'
        f'@pylon.type(module={module!r}, name="Widget")\n'
        'class Widget:\n'
        '    name: pylon.Property[str]\n'
    )

    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=tmp_path),
    )
    obj = {'config': config}
    runner = CliRunner()
    migrations_dir = tmp_path / 'migrations'

    try:
        asyncio.run(_sync_once(config))

        result = runner.invoke(migration.commands['status'], [], obj=obj)
        assert result.exit_code == 0, result.output
        assert "'watch' last wrote" in result.output, result.output

        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output

        # No --dev-mode: the marker is what has to carry it.
        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output
        assert 'rebasing onto it' in result.output, result.output

        # Applying rewrites the marker as its own, or every later apply would
        # keep rebasing over drift that is long since recorded.
        result = runner.invoke(migration.commands['status'], [], obj=obj)
        assert result.exit_code == 0, result.output
        assert "'watch' last wrote" not in result.output, result.output
    finally:
        migration_ids = re.findall(
            r'^-- migration: (\S+)$',
            '\n'.join(f.read_text() for f in migrations_dir.glob('*.sql')),
            re.MULTILINE,
        )

        async def cleanup() -> None:
            if migration_ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [migration_ids])
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

        asyncio.run(cleanup())


def test_multilink_added_to_an_interface_applies_without_hand_reordering(live_pool, tmp_path, unique_module):
    """Adding a multi-link to an interface with two concrete implementors
    produced a migration that could not be applied: the junction target
    foreign keys rendered ahead of the `CREATE TABLE` statements for the
    junction tables they constrain, so `apply` died on `relation
    "<module>"."Report.tags" does not exist` and the file had to be
    hand-reordered and `migration rehash`-ed before it would run.

    Drives the real `create`/`apply` CLI flow, since the ordering bug only
    showed up in a *second* migration against a database where the
    implementors' tables already exist — diffing the same schema against an
    empty database happened to order correctly.
    """
    import re

    from click.testing import CliRunner
    from conftest import live_db_dsn

    from pylon.cli.commands.migrations import migration
    from pylon.config import Config, DatabaseConfig, ProjectConfig

    module = unique_module('live_iface_multilink')
    # The link target lives in a *different* module on purpose: that is what
    # makes the junction's target FK cross Postgres schemas, which is why it
    # can't be folded into the junction's own CREATE TABLE the way the
    # source-side reference is.
    target_module = unique_module('live_iface_tags')
    (tmp_path / 'migrations').mkdir()
    schema_file = tmp_path / 'perspective_schema.py'

    def write_schema(with_tags: bool) -> None:
        tags_line = '    tags: pylon.MultiLink[Tag, pylon.OnDelete(pylon.Target, pylon.Allow)]\n' if with_tags else ''
        schema_file.write_text(
            'import pylon.schema as pylon\n\n'
            f"@pylon.type(module={target_module!r}, name='Tag')\n"
            'class Tag:\n'
            '    label: str\n\n'
            f"@pylon.interface(module={module!r}, name='Perspective')\n"
            'class Perspective:\n'
            '    title: str\n'
            f'{tags_line}\n'
            f"@pylon.type(module={module!r}, name='Report')\n"
            'class Report(Perspective):\n'
            '    pages: int\n\n'
            f"@pylon.type(module={module!r}, name='Brief')\n"
            'class Brief(Perspective):\n'
            '    summary: str\n'
        )

    write_schema(with_tags=False)

    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=tmp_path),
    )
    obj = {'config': config}
    runner = CliRunner()
    migrations_dir = tmp_path / 'migrations'

    try:
        # Baseline: the two implementors' tables and the interface's view.
        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        first_files = sorted(migrations_dir.glob('*.sql'))
        assert len(first_files) == 1, result.output

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        # The change under test: every implementor gains its own junction
        # table, and the interface gains a view over them.
        write_schema(with_tags=True)

        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        second_files = sorted(migrations_dir.glob('*.sql'))
        assert len(second_files) == 2, [f.name for f in second_files]

        body = next(f for f in second_files if f not in first_files).read_text()
        for owner in ('Report', 'Brief'):
            create_at = body.index(f'CREATE TABLE IF NOT EXISTS "{module}"."{owner}.tags"')
            fk_at = body.index(f'ADD CONSTRAINT "{owner}_tags_target_fkey"')
            assert create_at < fk_at, f'{owner}.tags constrained before it is created:\n{body}'

        # The acceptance criterion: no hand-editing, no `migration rehash`.
        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output
    finally:
        migration_ids = re.findall(
            r'^-- migration: (\S+)$',
            '\n'.join(f.read_text() for f in migrations_dir.glob('*.sql')),
            re.MULTILINE,
        )

        async def cleanup() -> None:
            if migration_ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [migration_ids])
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{target_module}" CASCADE;')

        asyncio.run(cleanup())


def test_interface_pointer_changing_kind_applies_without_hand_reordering(live_pool, tmp_path, unique_module):
    """Changing a pointer's kind on an interface with two implementors
    produced a migration that could not be applied.

    An interface is materialised as a view selecting its pointers out of
    every implementor's table, and `required_components` going from a
    property to a multi-link drops that column from both of them. The
    planner emitted the column drops with the view still standing, so
    `apply` died on the first statement with "cannot drop column
    required_components of table ... because other objects depend on it",
    and then went on to re-`CREATE VIEW` a view it had never dropped. The
    file had to be hand-edited and `migration rehash`-ed to get through.

    Drives the real `create`/`apply` CLI flow and keeps a row in each
    implementor: the generated statements are each well-formed on their own
    and only their order is wrong, so a test that merely read the SQL would
    have passed throughout.
    """
    import re

    from click.testing import CliRunner
    from conftest import live_db_dsn

    from pylon.cli.commands.migrations import migration
    from pylon.config import Config, DatabaseConfig, ProjectConfig

    module = unique_module('live_iface_kindchange')
    target_module = unique_module('live_iface_components')
    (tmp_path / 'migrations').mkdir()
    schema_file = tmp_path / 'preset_schema.py'
    implementors = ('SystemBriefPreset', 'CustomBriefPreset')

    def write_schema(*, as_multilink: bool) -> None:
        # Before: a scalar array property, plus a second multi-link that the
        # change also takes away — so the migration drops one link view
        # while rebuilding the object view, both against the same tables.
        if as_multilink:
            pointers = '    required_components: pylon.MultiLink[Component]\n'
        else:
            pointers = (
                '    required_components: pylon.Array[pylon.Str] | None\n'
                '    required_addons: pylon.MultiLink[Component]\n'
            )
        schema_file.write_text(
            'import pylon.schema as pylon\n\n'
            f"@pylon.type(module={target_module!r}, name='Component')\n"
            'class Component:\n'
            '    label: str\n\n'
            f"@pylon.interface(module={module!r}, name='BriefPreset')\n"
            'class BriefPreset:\n'
            '    title: str\n'
            f'{pointers}\n'
            f"@pylon.type(module={module!r}, name='SystemBriefPreset')\n"
            'class SystemBriefPreset(BriefPreset):\n'
            '    builtin: bool\n\n'
            f"@pylon.type(module={module!r}, name='CustomBriefPreset')\n"
            'class CustomBriefPreset(BriefPreset):\n'
            '    owner: str\n'
        )

    write_schema(as_multilink=False)

    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=tmp_path),
    )
    obj = {'config': config}
    runner = CliRunner()
    migrations_dir = tmp_path / 'migrations'

    try:
        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        first_files = sorted(migrations_dir.glob('*.sql'))
        assert len(first_files) == 1, result.output

        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        async def seed() -> None:
            await live_pool.execute(
                f'INSERT INTO "{module}"."SystemBriefPreset" (title, builtin, required_components) VALUES ($1, $2, $3)',
                ['system', True, ['a']],
            )
            await live_pool.execute(
                f'INSERT INTO "{module}"."CustomBriefPreset" (title, owner, required_components) VALUES ($1, $2, $3)',
                ['custom', 'someone', ['b']],
            )

        asyncio.run(seed())

        write_schema(as_multilink=True)

        result = runner.invoke(migration.commands['create'], ['--non-interactive'], obj=obj)
        assert result.exit_code == 0, result.output
        second_files = sorted(migrations_dir.glob('*.sql'))
        assert len(second_files) == 2, [f.name for f in second_files]

        body = next(f for f in second_files if f not in first_files).read_text()
        rebuild_at = body.index(f'CREATE VIEW "{module}"."BriefPreset"')
        for owner in implementors:
            drop_column_at = body.index(f'ALTER TABLE "{module}"."{owner}" DROP COLUMN IF EXISTS')
            teardown_at = body.rindex(f'DROP VIEW IF EXISTS "{module}"."BriefPreset";', 0, drop_column_at)
            assert teardown_at < drop_column_at < rebuild_at, (
                f"{owner}'s column drop must sit between the view's teardown and its rebuild:\n{body}"
            )

        # The acceptance criterion: no hand-editing, no `migration rehash`.
        result = runner.invoke(migration.commands['apply'], [], obj=obj)
        assert result.exit_code == 0, result.output

        async def check() -> None:
            # The interface is queryable again, over both implementors, and
            # its rows survived the rebuild.
            titles = await live_pool.query_named(f'SELECT title FROM "{module}"."BriefPreset" ORDER BY title', [])
            assert [r['title'] for r in titles] == ['custom', 'system']
            # The pointer really is a multi-link now, with the interface's
            # own link view standing over both implementors' junctions.
            rows = await live_pool.query_named(
                f'SELECT source, target FROM "{module}"."BriefPreset.required_components"', []
            )
            assert rows == []
            gone = await live_pool.query_named(
                'SELECT table_name FROM information_schema.views WHERE table_schema = $1 AND table_name = $2',
                [module, 'BriefPreset.required_addons'],
            )
            assert gone == [], 'the dropped multi-link left its link view behind'

        asyncio.run(check())
    finally:
        migration_ids = re.findall(
            r'^-- migration: (\S+)$',
            '\n'.join(f.read_text() for f in migrations_dir.glob('*.sql')),
            re.MULTILINE,
        )

        async def cleanup() -> None:
            if migration_ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [migration_ids])
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')
            await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{target_module}" CASCADE;')

        asyncio.run(cleanup())
