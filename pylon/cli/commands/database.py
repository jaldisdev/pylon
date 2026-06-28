from __future__ import annotations

import asyncio
import os
import subprocess
import sys
from pathlib import Path

import asyncpg
import click

from ..config import _print_error, requires_config


@click.group()
def database() -> None:
    """Manage the Pylon database installation."""


# ── helpers ────────────────────────────────────────────────────────────────────


def _pg_dsn(db) -> str:
    """Build a postgresql:// DSN from a DatabaseConfig."""
    if db.dsn:
        return db.dsn.replace("pylon://", "postgresql://", 1)
    pw = f":{db.password}" if db.password else ""
    return f"postgresql://{db.user}{pw}@{db.host}:{db.port}/{db.name}"


def _pg_env(db) -> dict[str, str]:
    """Return an env dict with PGPASSWORD set when needed."""
    env = os.environ.copy()
    if not db.dsn and db.password:
        env["PGPASSWORD"] = db.password
    return env


_SYSTEM_SCHEMAS = frozenset({"information_schema", "public", "_pylon"})


async def _user_schemas(conn) -> list[str]:
    rows = await conn.fetch(
        """
        SELECT schema_name
        FROM information_schema.schemata
        WHERE schema_name NOT IN ('information_schema', 'public', '_pylon')
          AND schema_name NOT LIKE 'pg_%'
        ORDER BY schema_name
        """
    )
    return [r["schema_name"] for r in rows]


# ── initialize ─────────────────────────────────────────────────────────────────


@database.command()
@click.option("--dry-run", is_flag=True, default=False,
              help="Print the generated SQL without applying it.")
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

    async def apply() -> None:
        conn = await asyncpg.connect(_pg_dsn(config.database))
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


# ── dump ───────────────────────────────────────────────────────────────────────


@database.command()
@click.argument("file")
@click.option("--format", "fmt", default="custom",
              type=click.Choice(["custom", "plain"], case_sensitive=False),
              help="Dump format: 'custom' (pg_restore) or 'plain' (SQL). Default: custom.")
@requires_config
@click.pass_context
def dump(ctx: click.Context, file: str, fmt: str) -> None:
    """Create a database backup using pg_dump.

    FILE is the destination path for the backup, e.g. backup.dump or backup.sql.
    The backup includes all schemas, functions, and data.
    """
    db = ctx.obj["config"].database

    pg_fmt = "--format=plain" if fmt == "plain" else "--format=custom"
    cmd = ["pg_dump", pg_fmt, f"--file={file}", _pg_dsn(db)]

    click.echo(f"Dumping database to {file!r} …")
    result = subprocess.run(cmd, env=_pg_env(db))
    if result.returncode != 0:
        _print_error("pg_dump failed", f"Exit code {result.returncode}")
        ctx.exit(result.returncode)
    else:
        click.echo(f"Backup written to {file!r}.")


# ── restore ────────────────────────────────────────────────────────────────────


@database.command()
@click.argument("file", type=click.Path(exists=True, dir_okay=False))
@requires_config
@click.pass_context
def restore(ctx: click.Context, file: str) -> None:
    """Restore a database backup created by 'database dump'.

    For .dump files (custom format) pg_restore is used.
    For .sql files (plain format) psql is used.
    """
    db = ctx.obj["config"].database
    dsn = _pg_dsn(db)
    env = _pg_env(db)
    path = Path(file)

    if path.suffix == ".sql":
        cmd = ["psql", dsn, f"--file={file}"]
        tool = "psql"
    else:
        cmd = ["pg_restore", "--format=custom", f"--dbname={dsn}", file]
        tool = "pg_restore"

    click.echo(f"Restoring from {file!r} using {tool} …")
    result = subprocess.run(cmd, env=env)
    if result.returncode != 0:
        _print_error(f"{tool} failed", f"Exit code {result.returncode}")
        ctx.exit(result.returncode)
    else:
        click.echo("Restore complete.")


# ── wipe ───────────────────────────────────────────────────────────────────────


@database.command()
@click.option("--force", is_flag=True, default=False,
              help="Skip the confirmation prompt.")
@requires_config
@click.pass_context
def wipe(ctx: click.Context, force: bool) -> None:
    """Delete all data and reset the user schema.

    Drops every non-system schema (preserving _pylon, public, pg_* and
    information_schema), then re-runs 'database initialize' and
    'migration migrate' to restore the schema from scratch.

    The database itself is NOT dropped.
    """
    db = ctx.obj["config"].database
    dbname = db.name or "pylon"

    if not force:
        click.confirm(
            f"This will wipe ALL data in '{dbname}' and reset the schema. Continue?",
            abort=True,
        )

    async def do_wipe() -> list[str]:
        conn = await asyncpg.connect(_pg_dsn(db))
        try:
            schemas = await _user_schemas(conn)
            async with conn.transaction():
                for schema in schemas:
                    await conn.execute(f'DROP SCHEMA "{schema}" CASCADE')
                    await conn.execute(f'CREATE SCHEMA "{schema}"')
            return schemas
        finally:
            await conn.close()

    try:
        dropped = asyncio.run(do_wipe())
    except asyncpg.PostgresError as exc:
        _print_error("database error", str(exc))
        ctx.exit(1)
        return

    if dropped:
        click.echo(f"Wiped schemas: {', '.join(dropped)}")
    else:
        click.echo("No user schemas found — nothing to wipe.")

    # Re-initialize _pylon stdlib.
    ctx.invoke(initialize)

    # Re-apply migrations.
    from .migrations import migration
    migrate_cmd = migration.commands.get("migrate")  # type: ignore[union-attr]
    if migrate_cmd:
        ctx.invoke(migrate_cmd)
