import click

from pylon.config import load_config

from .commands.database import database
from .commands.migrations import migration
from .commands.query import repl
from .commands.version import version
from .config import NO_CONFIG_HINT, _print_error, requires_config


@click.group(invoke_without_command=True)
@click.pass_context
def cli(ctx: click.Context) -> None:
    """Pylon — async PostgreSQL mapper and PyQL query engine.

    Run without a subcommand to start an interactive PyQL session.
    """
    ctx.ensure_object(dict)

    try:
        ctx.obj["config"] = load_config()
    except FileNotFoundError:
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
@click.option(
    "--dry-run",
    is_flag=True,
    default=False,
    help="Preview SQL without applying.",
)
@click.pass_context
def migrate_shortcut(ctx: click.Context, dry_run: bool) -> None:
    """Shortcut for `pylon migration migrate`."""
    ctx.invoke(migration.commands["migrate"], dry_run=dry_run)  # type: ignore[index]
