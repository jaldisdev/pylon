from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

import pytest
from click.testing import CliRunner

from pylon.cli.commands.migrations import _next_seq, _pg_url, _short_hash, migrate
from pylon.config import DatabaseConfig, ProjectConfig, Config


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
# _short_hash
# ---------------------------------------------------------------------------


class TestShortHash:
    def test_prefix(self):
        assert _short_hash(b'hello').startswith('m1')

    def test_length(self):
        assert len(_short_hash(b'hello')) == 8  # 'm1' + 6 hex chars

    def test_deterministic(self):
        assert _short_hash(b'hello') == _short_hash(b'hello')

    def test_different_inputs_differ(self):
        assert _short_hash(b'a') != _short_hash(b'b')

    def test_hex_chars_only(self):
        result = _short_hash(b'test')
        assert all(c in '0123456789abcdefm1' for c in result)


# ---------------------------------------------------------------------------
# _pg_url
# ---------------------------------------------------------------------------


def _make_config(*, dsn=None, host=None, port=None, name=None, user=None, password=None):
    if dsn:
        db = DatabaseConfig(dsn=dsn)
    else:
        db = DatabaseConfig(host=host, port=port, name=name, user=user, password=password)
    return MagicMock(database=db)


class TestPgUrl:
    def test_dsn_replaces_scheme(self):
        config = _make_config(dsn='pylon://u:p@localhost:5432/mydb')
        assert _pg_url(config) == 'postgres://u:p@localhost:5432/mydb'

    def test_dsn_non_pylon_scheme_unchanged(self):
        config = _make_config(dsn='postgres://u:p@localhost:5432/mydb')
        assert _pg_url(config) == 'postgres://u:p@localhost:5432/mydb'

    def test_discrete_fields_with_password(self):
        config = _make_config(host='localhost', port=5432, name='mydb', user='u', password='p')
        assert _pg_url(config) == 'postgres://u:p@localhost:5432/mydb'

    def test_discrete_fields_without_password(self):
        config = _make_config(host='localhost', port=5432, name='mydb', user='u', password=None)
        assert _pg_url(config) == 'postgres://u@localhost:5432/mydb'


# ---------------------------------------------------------------------------
# migrate command (no Atlas / DB required)
# ---------------------------------------------------------------------------


def _make_ctx_obj(migrations_dir: Path):
    db = DatabaseConfig(host='localhost', port=5432, name='db', user='u', password='p')
    project = ProjectConfig(schema_dir=migrations_dir.parent)
    config = Config(database=db, project=project)
    return {'config': config}


class TestListCommand:
    def test_missing_migrations_dir_exits_nonzero(self, tmp_path):
        from pylon.cli.commands.migrations import list_migrations

        runner = CliRunner()
        result = runner.invoke(
            list_migrations,
            obj=_make_ctx_obj(tmp_path / 'migrations'),  # dir does not exist
        )

        assert result.exit_code != 0

    def test_missing_migrations_dir_error_message(self, tmp_path):
        from pylon.cli.commands.migrations import list_migrations

        runner = CliRunner()
        result = runner.invoke(
            list_migrations,
            obj=_make_ctx_obj(tmp_path / 'migrations'),
        )

        assert 'migrations directory not found' in result.output


class TestMigrateCommand:
    def test_dry_run_prints_sql(self, tmp_path):
        migrations_dir = tmp_path / 'migrations'
        migrations_dir.mkdir()
        sql = 'ALTER TABLE "foo" ADD COLUMN "bar" text;\n'
        (migrations_dir / '00001_m1abc123.sql').write_text(sql)

        runner = CliRunner()
        result = runner.invoke(
            migrate,
            ['00001_m1abc123.sql', '--dry-run'],
            obj=_make_ctx_obj(migrations_dir),
        )

        assert result.exit_code == 0
        assert sql.strip() in result.output

    def test_missing_file_exits_nonzero(self, tmp_path):
        migrations_dir = tmp_path / 'migrations'
        migrations_dir.mkdir()

        runner = CliRunner()
        result = runner.invoke(
            migrate,
            ['00099_m1missing.sql'],
            obj=_make_ctx_obj(migrations_dir),
        )

        assert result.exit_code != 0

    def test_missing_file_error_message(self, tmp_path):
        migrations_dir = tmp_path / 'migrations'
        migrations_dir.mkdir()

        runner = CliRunner()
        result = runner.invoke(
            migrate,
            ['00099_m1missing.sql'],
            obj=_make_ctx_obj(migrations_dir),
            catch_exceptions=False,
        )

        assert '00099_m1missing.sql' in result.output
