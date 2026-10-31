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

from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock

from click.testing import CliRunner

from pylon.cli.commands.migrations import _next_seq, _pg_dsn
from pylon.config import Config, DatabaseConfig, ProjectConfig

# ---------------------------------------------------------------------------
# _next_seq
# ---------------------------------------------------------------------------


class TestNextSeq:
    def test_empty_dir(self, tmp_path):
        assert _next_seq(tmp_path) == 1

    def test_single_file(self, tmp_path):
        (tmp_path / '00001_m1abc123.sql').touch()
        assert _next_seq(tmp_path) == 2

    def test_multiple_files(self, tmp_path):
        (tmp_path / '00001_m1abc123.sql').touch()
        (tmp_path / '00002_m1def456.sql').touch()
        (tmp_path / '00003_m1ghi789.sql').touch()
        assert _next_seq(tmp_path) == 4

    def test_ignores_non_matching_files(self, tmp_path):
        (tmp_path / 'notes.txt').touch()
        (tmp_path / 'README.md').touch()
        assert _next_seq(tmp_path) == 1

    def test_sequence_is_based_on_highest(self, tmp_path):
        # Gaps in sequence should not affect the result.
        (tmp_path / '00001_m1abc123.sql').touch()
        (tmp_path / '00005_m1xyz999.sql').touch()
        assert _next_seq(tmp_path) == 6


# ---------------------------------------------------------------------------
# _pg_dsn
# ---------------------------------------------------------------------------


def _make_config(*, dsn=None, host=None, port=None, name=None, user=None, password=None):
    if dsn:
        db = DatabaseConfig(dsn=dsn)
    else:
        db = DatabaseConfig(host=host, port=port, name=name, user=user, password=password)
    return MagicMock(database=db)


class TestPgDsn:
    def test_dsn_replaces_pylon_scheme(self):
        config = _make_config(dsn='pylon://u:p@localhost:5432/mydb')
        assert _pg_dsn(config) == 'postgresql://u:p@localhost:5432/mydb'

    def test_dsn_postgresql_scheme_unchanged(self):
        config = _make_config(dsn='postgresql://u:p@localhost:5432/mydb')
        assert _pg_dsn(config) == 'postgresql://u:p@localhost:5432/mydb'

    def test_dsn_postgres_scheme_unchanged(self):
        config = _make_config(dsn='postgres://u:p@localhost:5432/mydb')
        assert _pg_dsn(config) == 'postgres://u:p@localhost:5432/mydb'

    def test_discrete_fields_with_password(self):
        config = _make_config(host='localhost', port=5432, name='mydb', user='u', password='p')
        assert _pg_dsn(config) == 'postgresql://u:p@localhost:5432/mydb'

    def test_discrete_fields_without_password(self):
        config = _make_config(host='localhost', port=5432, name='mydb', user='u', password=None)
        assert _pg_dsn(config) == 'postgresql://u@localhost:5432/mydb'


# ---------------------------------------------------------------------------
# _require_migrations_dir: auto-creates on first use
# ---------------------------------------------------------------------------


def _make_ctx_obj(migrations_dir: Path):
    db = DatabaseConfig(host='localhost', port=5432, name='db', user='u', password='p')
    project = ProjectConfig(schema_dir=migrations_dir.parent)
    config = Config(database=db, project=project)
    return {'config': config}


class TestMigrationsDirAutoCreate:
    def test_status_creates_dir_if_missing(self, tmp_path):
        from pylon.cli.commands.migrations import migration

        migrations_dir = tmp_path / 'migrations'
        assert not migrations_dir.exists()

        runner = CliRunner()
        # status requires a DB connection, so it will fail — but the dir
        # should be created before that happens.
        runner.invoke(
            migration.commands['status'],
            obj=_make_ctx_obj(migrations_dir),
        )

        assert migrations_dir.is_dir()


# ---------------------------------------------------------------------------
# _place_applied_tip: reading a database's tip through squashed headers
# ---------------------------------------------------------------------------


def _fake_migration(id_: str, onto: str, filename: str, squashed=()):
    """Stand-in for `pylon._core.MigrationFile` — the resolver reads four
    attributes off it and nothing else."""
    return SimpleNamespace(id=id_, onto=onto, filename=filename, squashed=list(squashed))


class TestPlaceAppliedTip:
    """A squash deletes the files it replaces, so a database that applied the
    pre-squash chain has a tip that names no file. Every command used to read
    that as a diverged history and refuse: `apply` could never reach its own
    backfill, `status` reported a correct database as broken, and `create`
    would not write a migration at all.
    """

    def _chain(self):
        return [
            _fake_migration('m1aaa', 'initial', '00001_m1aaa'),
            _fake_migration('m1sss', 'm1aaa', '00002_m1sss', squashed=['m1bbb', 'm1ccc', 'm1ddd']),
            _fake_migration('m1eee', 'm1sss', '00003_m1eee'),
        ]

    def test_no_tip_at_all(self):
        from pylon.cli.commands.migrations import _place_applied_tip

        position = _place_applied_tip(None, self._chain())
        assert position == (-1, None, None)

    def test_tip_on_disk(self):
        from pylon.cli.commands.migrations import _place_applied_tip

        position = _place_applied_tip('m1aaa', self._chain())
        assert position.applied_through == 0
        assert position.absorbed_by is None
        assert position.problem is None

    def test_tip_is_the_last_migration_a_squash_replaced(self):
        from pylon.cli.commands.migrations import _place_applied_tip

        # The whole replaced range ran, so the database is at the squashed
        # migration's own state — it wants that migration's tracking row,
        # not its DDL.
        position = _place_applied_tip('m1ddd', self._chain())
        assert position.applied_through == 1
        assert position.absorbed_by.filename == '00002_m1sss'
        assert position.problem is None

    def test_tip_partway_into_a_replaced_range_is_refused(self):
        from pylon.cli.commands.migrations import _place_applied_tip

        # Only part of the range ran. The squashed migration's DDL starts
        # from the state before the range, so neither running nor recording
        # it is right, and saying so beats either.
        position = _place_applied_tip('m1ccc', self._chain())
        assert position.absorbed_by is None
        assert '2 migration(s) into the range of 3' in position.problem
        assert '00002_m1sss' in position.problem

    def test_unknown_tip_is_still_a_diverged_history(self):
        from pylon.cli.commands.migrations import _place_applied_tip

        position = _place_applied_tip('m1zzz', self._chain())
        assert position.applied_through == -1
        assert 'history has diverged' in position.problem

    def test_a_tip_absorbed_by_the_last_file_leaves_nothing_pending(self):
        from pylon.cli.commands.migrations import _place_applied_tip

        chain = [
            _fake_migration('m1aaa', 'initial', '00001_m1aaa'),
            _fake_migration('m1sss', 'm1aaa', '00002_m1sss', squashed=['m1bbb', 'm1ccc']),
        ]
        position = _place_applied_tip('m1ccc', chain)
        assert chain[position.applied_through + 1 :] == []


# ---------------------------------------------------------------------------
# squash: the range has to end at the chain tip
# ---------------------------------------------------------------------------


def _write_chain(migrations_dir: Path, count: int) -> list[str]:
    """Write `count` linked migration files and return their IDs."""
    from pylon._core import compute_migration_id, compute_migration_short_id, render_migration_file

    migrations_dir.mkdir(parents=True, exist_ok=True)
    ids: list[str] = []
    onto = 'initial'
    for seq in range(1, count + 1):
        body = f'\nSELECT {seq};\n'
        (migrations_dir / f'{seq:05d}_{compute_migration_short_id(body, onto)}.sql').write_text(
            render_migration_file(onto, body)
        )
        onto = compute_migration_id(body, onto)
        ids.append(onto)
    return ids


class TestSquashRangeMustEndAtTip:
    """Squashing a middle range left the migration *after* the range naming a
    file the squash had just deleted. Nothing rewrote that link — doing so
    would change that migration's ID, and every ID after it, which is the
    history other databases have already recorded — so every later command
    failed with a chain conflict. The range is refused up front instead.
    """

    def test_a_middle_range_is_refused(self, tmp_path):
        from pylon.cli.commands.migrations import migration

        migrations_dir = tmp_path / 'migrations'
        ids = _write_chain(migrations_dir, 4)

        result = CliRunner().invoke(
            migration.commands['squash'],
            ['--from', ids[1], '--to', ids[2]],
            obj=_make_ctx_obj(migrations_dir),
        )

        assert result.exit_code == 1, result.output
        assert 'has to end at the chain tip' in result.output, result.output
        # Refused before anything was touched.
        assert len(list(migrations_dir.glob('*.sql'))) == 4

    def test_a_range_through_the_tip_is_accepted(self, tmp_path):
        from pylon.cli.commands.migrations import migration

        migrations_dir = tmp_path / 'migrations'
        ids = _write_chain(migrations_dir, 4)

        result = CliRunner().invoke(
            migration.commands['squash'],
            ['--from', ids[1], '--to', ids[3]],
            obj=_make_ctx_obj(migrations_dir),
        )

        # Gets past range resolution and on to the shadow database, which is
        # where this test's unreachable DSN stops it.
        assert 'has to end at the chain tip' not in result.output, result.output
        assert 'Squashing 3 migration(s)' in result.output, result.output
