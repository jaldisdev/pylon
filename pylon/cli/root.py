import sys

import click

from pylon.config import load_config

from .banner import _BOLD_RED, _RESET
from .commands.database import database
from .commands.migrations import migration
from .commands.query import repl
from .commands.version import version
from .config import NO_CONFIG_HINT, _print_error, requires_config


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
@click.pass_context
def cli(ctx: click.Context) -> None:
    """Pylon — async PostgreSQL mapper and PyQL query engine.

    Run without a subcommand to start an interactive PyQL session.
    """
    ctx.ensure_object(dict)

    try:
        ctx.obj["config"] = load_config()
    except (FileNotFoundError, KeyError, ValueError):
        ctx.obj["config"] = None

    if ctx.invoked_subcommand is None:
        if ctx.obj["config"] is None:
            _print_error("no pylon.toml found", NO_CONFIG_HINT)
            ctx.exit(1)
        repl()


# --- groups -------------------------------------------------------------------

cli.add_command(database)
cli.add_command(migration)


# --- top-level commands -------------------------------------------------------

cli.add_command(version)


# --- shortcuts ----------------------------------------------------------------


@cli.command("migrate", short_help="Shortcut for `pylon migration migrate`.")
@click.argument("migration_name", required=False)
@click.option(
    "--dry-run",
    is_flag=True,
    default=False,
    help="Preview SQL without applying.",
)
@click.pass_context
def migrate_shortcut(ctx: click.Context, migration_name: str | None, dry_run: bool) -> None:
    """Shortcut for `pylon migration migrate`."""
    ctx.invoke(migration.commands["migrate"], migration_name=migration_name, dry_run=dry_run)  # type: ignore[index]
