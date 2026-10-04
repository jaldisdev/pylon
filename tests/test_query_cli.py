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

"""`pylon query` / the REPL against the database `-d` selected."""

import contextlib
from pathlib import Path

from click.testing import CliRunner

import pylon
from pylon.cli import commands
from pylon.cli.root import cli

PYLON_TOML = """
[project]
name = "selector"
schema-dir = "dbschema"

[database]
host = "base.example"
port = 5432
name = "base_db"
user = "u"
password = "p"

[database.other]
name = "other_db"
"""


def _write_project(root: Path) -> None:
    (root / 'pylon.toml').write_text(PYLON_TOML)
    (root / 'dbschema').mkdir()


def _capture_client(monkeypatch) -> list:
    """Replaces the client with one that only records the config it was
    handed, so the test asserts on the selection rather than on a connection
    nothing is listening for."""
    seen = []

    @contextlib.asynccontextmanager
    async def fake_client(config=None):
        seen.append(config)
        yield object()

    async def no_execute(*_args, **_kwargs):
        return None

    monkeypatch.setattr(commands.query, 'create_async_client', fake_client)
    monkeypatch.setattr(commands.query, '_execute', no_execute)
    monkeypatch.setattr(pylon, 'finalize', lambda *a, **k: None)
    return seen


def test_a_query_runs_against_the_named_connection(monkeypatch, tmp_path):
    seen = _capture_client(monkeypatch)
    _write_project(tmp_path)
    monkeypatch.chdir(tmp_path)

    result = CliRunner().invoke(cli, ['-d', 'other', 'query', 'select 1'])

    assert result.exit_code == 0, result.output
    assert [c.database.name for c in seen] == ['other_db']


def test_a_query_without_the_selector_runs_against_the_base_connection(monkeypatch, tmp_path):
    seen = _capture_client(monkeypatch)
    _write_project(tmp_path)
    monkeypatch.chdir(tmp_path)

    result = CliRunner().invoke(cli, ['query', 'select 1'])

    assert result.exit_code == 0, result.output
    assert [c.database.name for c in seen] == ['base_db']


def test_the_repl_runs_against_the_named_connection(monkeypatch, tmp_path):
    seen = []

    def fake_repl(*, as_json=False, project_name=None, config=None):
        seen.append(config)

    monkeypatch.setattr('pylon.cli.root.repl', fake_repl)
    _write_project(tmp_path)
    monkeypatch.chdir(tmp_path)

    result = CliRunner().invoke(cli, ['-d', 'other'])

    assert result.exit_code == 0, result.output
    assert [c.database.name for c in seen] == ['other_db']
