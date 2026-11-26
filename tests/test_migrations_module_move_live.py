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

"""Live-Postgres tests for moving types between modules through `pylon
migration create`/`apply`: rows, junction rows and enum values survive, the
triggers and functions follow, and nothing is left behind in the old module.

Every diff against the live database here is narrowed to this test's own
modules: the test database is shared, and whatever other suites left in it
is not this test's business.

Run with: `.venv/bin/pytest tests/test_migrations_module_move_live.py -m live_db`
"""

from __future__ import annotations

import asyncio
import re
import shutil
from dataclasses import dataclass
from pathlib import Path

import pytest

pytestmark = [pytest.mark.live_db]


@dataclass
class Project:
    root: Path
    family: str
    people: str
    audit: str
    obj: dict

    @property
    def migrations(self) -> Path:
        return self.root / 'migrations'

    def write_family(self, module: str, parent: str = 'Parent', status: str = 'ParentStatus') -> None:
        """`Parent` (an enum, a check, a multi-link, a rewrite, an audit
        trigger, an interface link) and `Child`, plus a set-returning function,
        all in `module`, with the parent type and its enum named `parent` and
        `status`. The file stays the same, so `lazy` references keep working
        whichever module it declares."""
        # Rewritten within the same second at the same size, a stale `.pyc`
        # would still pass for current.
        shutil.rmtree(self.root / '__pycache__', ignore_errors=True)
        (self.root / f'{self.family}.py').write_text(
            f"""from __future__ import annotations
from typing import Annotated
import pylon

__pylon_module__ = {module!r}

@pylon.enum('Draft', 'Live', 'Archived')
class {status}(pylon.Enum):
    pass

@pylon.type
class Child:
    label: pylon.Str
    rank: pylon.Property[pylon.Int32, pylon.Default(0)]

@pylon.type
class {parent}:
    title: pylon.Property[pylon.Str, pylon.MaxLen(64)]
    status: pylon.Property[{status}, pylon.Default({status}.Draft)]
    deleted_at: pylon.DateTime | None
    deleted: pylon.Property[pylon.Bool, pylon.Rewrite(pylon.On.Insert | pylon.On.Update, "EXISTS (.deleted_at)")] | None
    created_by: pylon.Link[
        Annotated["Account", pylon.lazy(".{self.people}")], pylon.OnDelete(pylon.Target, pylon.Allow)
    ] | None
    children: pylon.MultiLink[
        Annotated["Child", pylon.lazy(".{self.family}")], pylon.OnDelete(pylon.Source, pylon.DeleteTarget)
    ]
    pylon.Trigger(
        on=pylon.On.Insert,
        timing=pylon.Timing.After,
        handler="insert {self.audit}::Audit {{ action := 'insert', type := __new__.__type__, item := __new__.id }}",
    )

@pylon.function(volatility=pylon.Volatility.Stable)
def ranked_children(min_rank: pylon.Int32) -> set[Annotated["Child", pylon.lazy(".{self.family}")]]:
    \"\"\"
    select {module}::Child filter .rank >= min_rank
    \"\"\"
"""
        )

    def write_people(self) -> None:
        (self.root / f'{self.people}.py').write_text(
            f"""from __future__ import annotations
import pylon

__pylon_module__ = {self.people!r}

@pylon.interface
class Account:
    name: pylon.Str

@pylon.type
class Person(Account):
    email: pylon.Str | None
"""
        )
        (self.root / f'{self.audit}.py').write_text(
            f"""from __future__ import annotations
import pylon

__pylon_module__ = {self.audit!r}

@pylon.type
class Audit:
    action: pylon.Str
    item: pylon.UUID
    type: pylon.Str
"""
        )

    def invoke(self, command: str, *args: str):
        from click.testing import CliRunner

        from pylon.cli.commands.migrations import migration

        return CliRunner().invoke(migration.commands[command], list(args), obj=self.obj)

    def create_first(self):
        """The first `create` has no recorded baseline and diffs the whole
        live database, which in the shared test database holds every other
        suite's tables, with rows; dropping them has to be allowed."""
        return self.invoke('create', '--non-interactive', '--allow-unsafe')

    def migration_ids(self) -> list[str]:
        text = '\n'.join(f.read_text() for f in sorted(self.migrations.glob('*.sql')))
        return re.findall(r'^-- migration: (\S+)$', text, re.MULTILINE)


@pytest.fixture
def project(live_pool, tmp_path, unique_module):
    from conftest import live_db_dsn

    from pylon.config import Config, DatabaseConfig, ProjectConfig

    names = {kind: unique_module(f'live_move_{kind}') for kind in ('a', 'b', 'family', 'people', 'audit')}
    (tmp_path / 'migrations').mkdir()
    config = Config(database=DatabaseConfig(dsn=live_db_dsn()), project=ProjectConfig(schema_dir=tmp_path))
    p = Project(tmp_path, names['family'], names['people'], names['audit'], {'config': config})
    p.module_a, p.module_b = names['a'], names['b']
    p.write_people()
    p.write_family(p.module_a)
    try:
        yield p
    finally:
        ids = p.migration_ids()

        async def cleanup() -> None:
            if ids:
                await live_pool.execute('DELETE FROM _pylon."Migrations" WHERE id = ANY($1)', [ids])
            for module in (p.module_a, p.module_b, p.people, p.audit):
                await live_pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE;')

        asyncio.run(cleanup())


def seed(live_pool, module: str, people: str) -> None:
    async def run() -> None:
        await live_pool.batch_execute(
            f"""
            INSERT INTO "{people}"."Person"(name) VALUES ('ann'), ('bob');
            INSERT INTO "{module}"."Child"(label, rank) SELECT 'c' || g, g FROM generate_series(1, 6) g;
            INSERT INTO "{module}"."Parent"(title, status, created_by_id)
              SELECT 'p' || g, (ARRAY['Draft', 'Live', 'Archived'])[g]::"{module}"."ParentStatus",
                     (SELECT id FROM "{people}"."Person" WHERE name = 'ann')
              FROM generate_series(1, 3) g;
            INSERT INTO "{module}"."Parent.children"(source, target)
              SELECT p.id, c.id FROM "{module}"."Parent" p JOIN "{module}"."Child" c
              ON c.rank % 3 = substr(p.title, 2)::int % 3;
            """
        )

    asyncio.run(run())


def checksums(live_pool, module: str, names: dict[str, str] | None = None) -> dict[str, tuple]:
    """Row count and content checksum per table, keyed by its original name;
    `names` maps one to what it is called now."""
    names = names or {}

    async def run() -> dict[str, tuple]:
        out = {}
        for key in ('Parent', 'Child', 'Parent.children'):
            table = names.get(key, key)
            rows = await live_pool.query_named(
                "SELECT count(*) AS n, md5(string_agg(t::text, ',' ORDER BY t::text)) AS sum "
                f'FROM "{module}"."{table}" t',
                [],
            )
            out[key] = (rows[0]['n'], rows[0]['sum'])
        return out

    return asyncio.run(run())


def live_steps(live_pool, project: Project) -> list[str]:
    """Every step a diff of the live database against the schema files
    proposes for this project's own modules."""
    from pylon._core import diff_schema_steps_with_renames_and_fills, introspect_db_state

    from pylon.cli.commands.migrations import _reload_schema

    schema = _reload_schema(project.obj['config'])

    async def run():
        return await introspect_db_state(live_pool, schema)

    state = asyncio.run(run())
    steps = diff_schema_steps_with_renames_and_fills(schema, state, [], [], [])
    mine = (project.module_a, project.module_b, project.people, project.audit)
    return [
        sql
        for step in steps
        for sql, _ in step.resolved_ddl({})
        if any(f"'{m}::" in step.object_desc or f'"{m}"' in sql for m in mine)
    ]


def leftovers_in(live_pool, old_module: str) -> dict[str, list]:
    async def run() -> dict[str, list]:
        stale = await live_pool.query_named(
            'SELECT p.proname FROM pg_proc p WHERE p.prosrc LIKE $1',
            [f'%"{old_module}".%'],
        )
        orphans = await live_pool.query_named(
            """SELECT n.nspname || '.' || p.proname AS name FROM pg_proc p
               JOIN pg_namespace n ON n.oid = p.pronamespace
               WHERE p.prorettype = 'trigger'::regtype AND n.nspname = $1
                 AND NOT EXISTS (SELECT 1 FROM pg_trigger t WHERE t.tgfoid = p.oid)""",
            [old_module],
        )
        functions = await live_pool.query_named(
            'SELECT p.proname FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1',
            [old_module],
        )
        return {
            'stale': [r['proname'] for r in stale],
            'orphans': [r['name'] for r in orphans],
            'functions': [r['proname'] for r in functions],
        }

    return asyncio.run(run())


def test_a_type_moved_to_another_module_keeps_its_rows(live_pool, project):
    result = project.create_first()
    assert result.exit_code == 0, result.output
    assert project.invoke('apply').exit_code == 0
    seed(live_pool, project.module_a, project.people)
    before = checksums(live_pool, project.module_a)
    # Unchanged and live: nothing to do.
    assert live_steps(live_pool, project) == []

    project.write_family(project.module_b)
    result = project.invoke('create', '--non-interactive')
    assert result.exit_code == 0, result.output
    assert f"Moving object type '{project.module_a}::Parent' to module '{project.module_b}'" in result.output
    body = sorted(project.migrations.glob('*.sql'))[-1].read_text()
    assert 'DROP TABLE' not in body and 'DROP TYPE' not in body, body
    result = project.invoke('apply')
    assert result.exit_code == 0, result.output

    assert checksums(live_pool, project.module_b) == before
    assert leftovers_in(live_pool, project.module_a) == {'stale': [], 'orphans': [], 'functions': []}

    async def audit_types() -> set[str]:
        rows = await live_pool.query_named(f'SELECT DISTINCT type FROM "{project.audit}"."Audit"', [])
        return {r['type'] for r in rows}

    assert asyncio.run(audit_types()) == {f'{project.module_b}::Parent'}

    # The rewrite, the audit trigger and the interface link run the moved
    # functions; a stale one would still write to the old module.
    async def exercise() -> list:
        await live_pool.batch_execute(
            f"""INSERT INTO "{project.module_b}"."Parent"(title, deleted_at) VALUES ('p4', now());
                DELETE FROM "{project.people}"."Person" WHERE name = 'ann';"""
        )
        return await live_pool.query_named(
            f"""SELECT (SELECT deleted FROM "{project.module_b}"."Parent" WHERE title = 'p4') AS deleted,
                       (SELECT count(*) FROM "{project.module_b}"."Parent" WHERE created_by_id IS NOT NULL) AS linked,
                       (SELECT count(*) FROM "{project.module_b}".ranked_children(3)) AS ranked""",
            [],
        )

    assert asyncio.run(exercise()) == [{'deleted': True, 'linked': 0, 'ranked': 4}]

    result = project.invoke('create', '--non-interactive')
    assert 'No schema changes detected' in result.output, result.output
    assert live_steps(live_pool, project) == []


def test_a_hand_written_move_is_finished_by_the_next_create(live_pool, project):
    from pylon._core import migration_read_tracking

    assert project.create_first().exit_code == 0
    assert project.invoke('apply').exit_code == 0
    seed(live_pool, project.module_a, project.people)

    project.write_family(project.module_b)
    result = project.invoke('create', '--blank', '--name', 'move')
    assert result.exit_code == 0, result.output
    blank = next(project.migrations.glob('*_move.sql'))
    a, b = project.module_a, project.module_b
    header = ''.join(blank.read_text().splitlines(keepends=True)[:2])
    blank.write_text(
        header + f'\nCREATE SCHEMA IF NOT EXISTS "{b}";\n'
        f'ALTER TABLE "{a}"."Child" SET SCHEMA "{b}";\n'
        f'ALTER TABLE "{a}"."Parent" SET SCHEMA "{b}";\n'
        f'ALTER TABLE "{a}"."Parent.children" SET SCHEMA "{b}";\n'
        f'ALTER TYPE "{a}"."ParentStatus" SET SCHEMA "{b}";\n'
    )
    assert project.invoke('rehash', str(blank)).exit_code == 0
    result = project.invoke('apply')
    assert result.exit_code == 0, result.output
    assert 'diffs against the live database' in result.output, result.output

    # The files do not say what a hand-written migration reached.
    async def tip_baseline():
        tracking = await migration_read_tracking(live_pool)
        return next(
            db_state for id_, _onto, db_state, _schema, _applied in tracking if id_ == project.migration_ids()[-1]
        )

    assert asyncio.run(tip_baseline()) is None

    steps = '\n'.join(live_steps(live_pool, project))
    assert f'DROP FUNCTION IF EXISTS "{a}"."Parent_rw_ins_' in steps, steps
    assert f'"{a}"."_ifl_Parent_created_by_' in steps, steps
    assert f"n.nspname = '{a}' AND p.proname = 'ranked_children'" in steps, steps


def test_non_interactive_create_refuses_to_drop_a_table_holding_rows(live_pool, project):
    assert project.create_first().exit_code == 0
    assert project.invoke('apply').exit_code == 0
    seed(live_pool, project.module_a, project.people)

    # Replaced by a type that shares nothing with it but `id`: nothing to
    # read as a rename.
    family = project.root / f'{project.family}.py'
    replaced = (
        family.read_text()
        .replace('class Child:', 'class Kid:')
        .replace('"Child"', '"Kid"')
        .replace('::Child', '::Kid')
        .replace('    label: pylon.Str', '    caption: pylon.Str')
        .replace('    rank: pylon.Property', '    level: pylon.Property')
        .replace('.rank >=', '.level >=')
    )
    shutil.rmtree(project.root / '__pycache__', ignore_errors=True)
    family.write_text(replaced)
    result = project.invoke('create', '--non-interactive', '--dry-run')
    assert result.exit_code == 1, result.output
    assert f'still hold rows: {project.module_a}::Child' in result.output, result.output

    result = project.invoke('create', '--non-interactive', '--dry-run', '--allow-unsafe')
    assert result.exit_code == 0, result.output
    assert f'DROP TABLE IF EXISTS "{project.module_a}"."Child" CASCADE;' in result.output, result.output


def test_a_type_moved_and_renamed_keeps_its_rows(live_pool, project):
    """Moved to another module and renamed in the same migration, along with
    the enum one of its columns has — which used to read as one type dropped
    and another created, for the table and the enum alike."""
    result = project.create_first()
    assert result.exit_code == 0, result.output
    assert project.invoke('apply').exit_code == 0
    seed(live_pool, project.module_a, project.people)
    before = checksums(live_pool, project.module_a)

    a, b = project.module_a, project.module_b
    project.write_family(b, parent='Holder', status='HolderPhase')
    result = project.invoke('create', '--non-interactive')
    assert result.exit_code == 0, result.output
    assert f"Renaming object type '{a}::Parent' to '{b}::Holder'" in result.output, result.output
    body = sorted(project.migrations.glob('*.sql'))[-1].read_text()
    assert 'DROP TABLE' not in body and 'DROP TYPE' not in body, body
    assert f'ALTER TYPE "{a}"."ParentStatus" RENAME TO "HolderPhase";' in body, body
    assert 'ADD CONSTRAINT' not in body, body
    result = project.invoke('apply')
    assert result.exit_code == 0, result.output

    after = checksums(live_pool, b, {'Parent': 'Holder', 'Parent.children': 'Holder.children'})
    assert after == before

    async def leftovers() -> list:
        named_after_parent = await live_pool.query_named(
            """SELECT conname AS name FROM pg_constraint
               WHERE connamespace = $1::text::regnamespace AND conname LIKE 'Parent%'
               UNION ALL SELECT relname FROM pg_class
               WHERE relnamespace = $1::text::regnamespace AND relname LIKE 'Parent%'""",
            [b],
        )
        old_audit_names = await live_pool.query_named(
            f'SELECT DISTINCT type AS name FROM "{project.audit}"."Audit" WHERE type <> $1',
            [f'{b}::Holder'],
        )
        return named_after_parent + old_audit_names

    assert asyncio.run(leftovers()) == []
    assert leftovers_in(live_pool, a) == {'stale': [], 'orphans': [], 'functions': []}
    assert 'No schema changes detected' in project.invoke('create', '--non-interactive').output
    assert live_steps(live_pool, project) == []
