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

import asyncio
import os
import subprocess
import sys
from pathlib import Path

import click

from ..config import _print_error, requires_config
from pylon.exceptions import PylonError


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


async def _user_schemas(pool) -> list[str]:
    return await pool.query(
        """
        SELECT (schema_name) AS result
        FROM information_schema.schemata
        WHERE schema_name NOT IN ('information_schema', 'public', '_pylon')
          AND schema_name NOT LIKE 'pg_%'
        ORDER BY schema_name
        """,
        [],
    )


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
        from pylon._core import pgcon_connect

        pool = await pgcon_connect(_pg_dsn(config.database), 2)
        # `batch_execute` runs the whole multi-statement blob via the simple
        # query protocol, which Postgres itself wraps in an implicit
        # transaction (atomic all-or-nothing) — no explicit BEGIN needed.
        await pool.batch_execute(sql)

    try:
        asyncio.run(apply())
        click.echo("_pylon schema initialized.")
    except PylonError as exc:
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
    """Destroy all database contents.

    Drops all user-defined modules and clears migration history.
    The database itself is NOT dropped.
    """
    db = ctx.obj["config"].database
    dbname = db.name or "pylon"

    if not force:
        click.confirm(
            f"This will destroy all data in '{dbname}'. Continue?",
            abort=True,
        )

    async def do_wipe() -> None:
        from pylon._core import pgcon_connect

        pool = await pgcon_connect(_pg_dsn(db), 2)
        schemas = await _user_schemas(pool)
        tracking_rows = await pool.query(
            "SELECT (to_regclass('_pylon.\"Migrations\"')) AS result", []
        )
        has_tracking = tracking_rows[0] if tracking_rows else None

        # One `batch_execute` call — Postgres's simple query protocol wraps
        # the whole multi-statement blob in an implicit transaction, same
        # atomicity as the old explicit `async with conn.transaction():`.
        statements = [f'DROP SCHEMA "{schema}" CASCADE;' for schema in schemas]
        if has_tracking:
            statements.append('DELETE FROM _pylon."Migrations";')
            statements.append('DELETE FROM _pylon."Progress";')
        if statements:
            await pool.batch_execute("\n".join(statements))

    try:
        asyncio.run(do_wipe())
    except PylonError as exc:
        _print_error("database error", str(exc))
        ctx.exit(1)
        return

    click.echo("Database wiped.")
