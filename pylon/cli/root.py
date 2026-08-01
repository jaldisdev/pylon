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

import dataclasses
import sys

import click

from pylon.config import load_config

from .banner import _BOLD_RED, _RESET
from .commands.cache import cache
from .commands.completion import completion_cmd
from .commands.database import database
from .commands.info import info_cmd
from .commands.migrations import migration
from .commands.query import query_cmd, repl
from .commands.version import version
from .commands.worker import worker
from .config import NO_CONFIG_HINT, _print_error, requires_config


def _complete_db_name(ctx: click.Context, param: click.Parameter, incomplete: str) -> list[str]:
    """Shell-completion callback: named [database.<name>] connections from pylon.toml."""
    try:
        config = load_config()
    except Exception:
        return []
    return [k for k in config.connections if k != "default" and k.startswith(incomplete)]


def main() -> None:
    try:
        cli(standalone_mode=False)
    except click.exceptions.Exit as e:
        sys.exit(e.exit_code)
    except click.ClickException as e:
        e.show()
        sys.exit(e.exit_code)
    except Exception as e:
        click.echo(f"{_BOLD_RED}error:{_RESET} {e}", err=True)
        sys.exit(1)


@click.group(invoke_without_command=True)
@click.option(
    "-d", "--database", "db_name",
    default=None, metavar="NAME",
    shell_complete=_complete_db_name,
    help="Named database connection from pylon.toml (e.g. -d staging).",
)
@click.pass_context
def cli(ctx: click.Context, db_name: str | None) -> None:
    """Pylon — async PostgreSQL mapper and PyQL query engine.

    Run without a subcommand to start an interactive PyQL session.
    """
    ctx.ensure_object(dict)

    try:
        config = load_config()
    except (FileNotFoundError, KeyError, ValueError):
        config = None

    if config is not None and db_name is not None:
        db = config.connections.get(db_name)
        if db is None:
            available = ", ".join(k for k in config.connections if k != "default")
            _print_error(
                f"connection {db_name!r} not found in pylon.toml",
                f"Available: {available}" if available else "No named connections defined.",
            )
            ctx.obj["config"] = None
            ctx.exit(1)
            return
        config = dataclasses.replace(config, database=db)

    ctx.obj["config"] = config

    if ctx.invoked_subcommand is None:
        if ctx.obj["config"] is None:
            _print_error("no pylon.toml found", NO_CONFIG_HINT)
            ctx.exit(1)
        cfg = ctx.obj["config"]
        project_name = cfg.project.name if cfg and cfg.project else None
        repl(project_name=project_name)


# --- groups -------------------------------------------------------------------

cli.add_command(cache)
cli.add_command(database)
cli.add_command(migration)
cli.add_command(worker)


# --- top-level commands -------------------------------------------------------

cli.add_command(version)
cli.add_command(query_cmd)
cli.add_command(info_cmd)
cli.add_command(completion_cmd)


# --- shortcuts ----------------------------------------------------------------


@cli.command("migrate", short_help="Shortcut for `pylon migration apply`.")
@click.pass_context
def migrate_shortcut(ctx: click.Context) -> None:
    """Shortcut for `pylon migration apply`."""
    ctx.invoke(migration.commands["apply"])  # type: ignore[index]
