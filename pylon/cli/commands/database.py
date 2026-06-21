import asyncio

import asyncpg
import click

from ..config import _print_error, requires_config


@click.group()
def database() -> None:
    """Manage the Pylon database installation."""


@database.command()
@click.option(
    "--dry-run",
    is_flag=True,
    default=False,
    help="Print the generated SQL without applying it.",
)
@requires_config
@click.pass_context
def initialize(ctx: click.Context, dry_run: bool) -> None:
    """Install the _pylon schema and standard library functions.

    Generates CREATE OR REPLACE FUNCTION statements for every PylonFunction
    in the stdlib registry and executes them against the configured database.
    Safe to run multiple times — all statements are idempotent.
    """
    from pylon._core import export_stdlib

    sql = export_stdlib()

    if dry_run:
        click.echo(sql)
        return

    config = ctx.obj["config"]
    db = config.database

    if db.dsn is not None:
        pg_url = db.dsn.replace("pylon://", "postgres://", 1)
    else:
        pw_part = f":{db.password}@" if db.password else "@"
        pg_url = f"postgres://{db.user}{pw_part}{db.host}:{db.port}/{db.name}"

    async def apply() -> None:
        conn = await asyncpg.connect(pg_url)
        try:
            async with conn.transaction():
                await conn.execute(sql)
        finally:
            await conn.close()

    try:
        asyncio.run(apply())
        click.echo("_pylon schema initialized.")
    except asyncpg.PostgresError as exc:
        _print_error("database error", str(exc))
        ctx.exit(1)
