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
        (tmp_path / 'atlas.sum').touch()
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
