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
from pathlib import Path

import click

from pylon.exceptions import PylonError

from ..config import _print_error, requires_config


@click.group()
def database() -> None:
    """Manage the Pylon database installation."""


# ── helpers ────────────────────────────────────────────────────────────────────


def _pg_dsn(db) -> str:
    """Build a postgresql:// DSN from a DatabaseConfig."""
    if db.dsn:
        return db.dsn.replace('pylon://', 'postgresql://', 1)
    pw = f':{db.password}' if db.password else ''
    return f'postgresql://{db.user}{pw}@{db.host}:{db.port}/{db.name}'


def _pg_env(db) -> dict[str, str]:
    """Return an env dict with PGPASSWORD set when needed."""
    env = os.environ.copy()
    if not db.dsn and db.password:
        env['PGPASSWORD'] = db.password
    return env


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


async def _public_drops(pool) -> list[str]:
    """DROP statements for the `default` module's contents.

    Object by object rather than `DROP SCHEMA public`: `public` also holds
    whatever `CREATE EXTENSION` put there, plus grants belonging to the
    database itself.
    """
    return await pool.query(
        """
        SELECT (stmt) AS result
        FROM (
            -- A partition, and a sequence owned by an identity column, go
            -- with their parent table ('a'/'i'); 'e' is extension-owned.
            SELECT 1 AS phase, c.relname AS name, format(
                'DROP %s IF EXISTS %s CASCADE;',
                CASE c.relkind
                    WHEN 'v' THEN 'VIEW'
                    WHEN 'm' THEN 'MATERIALIZED VIEW'
                    WHEN 'S' THEN 'SEQUENCE'
                    WHEN 'f' THEN 'FOREIGN TABLE'
                    ELSE 'TABLE'
                END,
                format('%I.%I', n.nspname, c.relname)
            ) AS stmt
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public'
              AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
              AND NOT c.relispartition
              AND NOT EXISTS (
                  SELECT 1 FROM pg_depend d
                  WHERE d.objid = c.oid
                    AND d.classid = 'pg_class'::regclass
                    AND d.objsubid = 0
                    AND d.deptype IN ('e', 'a', 'i')
              )

            UNION ALL

            SELECT 2, p.proname, format(
                'DROP %s IF EXISTS %s CASCADE;',
                CASE p.prokind
                    WHEN 'p' THEN 'PROCEDURE'
                    WHEN 'a' THEN 'AGGREGATE'
                    ELSE 'FUNCTION'
                END,
                format('%I.%I(%s)', n.nspname, p.proname, pg_get_function_identity_arguments(p.oid))
            )
            FROM pg_proc p
            JOIN pg_namespace n ON n.oid = p.pronamespace
            WHERE n.nspname = 'public'
              AND NOT EXISTS (
                  SELECT 1 FROM pg_depend d
                  WHERE d.objid = p.oid
                    AND d.classid = 'pg_proc'::regclass
                    AND d.deptype = 'e'
              )

            UNION ALL

            -- An array type, a multirange and a table's row type are not
            -- independent objects, so they are skipped.
            SELECT 3, t.typname, format(
                'DROP %s IF EXISTS %s CASCADE;',
                CASE t.typtype WHEN 'd' THEN 'DOMAIN' ELSE 'TYPE' END,
                format('%I.%I', n.nspname, t.typname)
            )
            FROM pg_type t
            JOIN pg_namespace n ON n.oid = t.typnamespace
            WHERE n.nspname = 'public'
              AND t.typtype IN ('e', 'd', 'r', 'c')
              AND (
                  t.typrelid = 0
                  OR (SELECT c.relkind FROM pg_class c WHERE c.oid = t.typrelid) = 'c'
              )
              AND NOT EXISTS (SELECT 1 FROM pg_type el WHERE el.typarray = t.oid)
              AND NOT EXISTS (
                  SELECT 1 FROM pg_depend d
                  WHERE d.objid = t.oid
                    AND d.classid = 'pg_type'::regclass
                    AND d.deptype IN ('e', 'i')
              )
        ) AS drops
        ORDER BY phase, name
        """,
        [],
    )


_INTERNAL_TABLES = ('Migrations', 'Progress', 'Schema', 'IndexOutbox', 'SignalOutbox')


async def _internal_tables(pool) -> list[str]:
    """Which of `_INTERNAL_TABLES` this database actually has."""
    present = await pool.query(
        """
        SELECT (c.relname) AS result
        FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = '_pylon' AND c.relkind = 'r'
        """,
        [],
    )
    return [name for name in _INTERNAL_TABLES if name in present]


# ── initialize ─────────────────────────────────────────────────────────────────


@database.command()
@click.option('--dry-run', is_flag=True, default=False, help='Print the generated SQL without applying it.')
@requires_config
@click.pass_context
def initialize(ctx: click.Context, dry_run: bool) -> None:
    """Install or update the internal _pylon schema.

    Creates the migration tracking tables, the index and signal outboxes, and
    every standard-library function, and brings an existing _pylon schema up
    to date. Safe to run multiple times — all statements are idempotent.

    Not a required step: every migration command does this before it runs, so
    an ordinary project never needs it and an upgrade picks up new internal
    structures on the next migration. Use it to provision a database as its
    own step, or with --dry-run to read the DDL that would be applied.
    """
    from pylon._core import export_stdlib

    sql = export_stdlib()

    if dry_run:
        click.echo(sql)
        return

    config = ctx.obj['config']

    async def apply() -> None:
        from pylon._core import pgcon_connect

        pool = await pgcon_connect(_pg_dsn(config.database), 2)
        # `batch_execute` runs the whole multi-statement blob via the simple
        # query protocol, which Postgres itself wraps in an implicit
        # transaction (atomic all-or-nothing) — no explicit BEGIN needed.
        await pool.batch_execute(sql)

    try:
        asyncio.run(apply())
        click.echo('_pylon schema initialized.')
    except PylonError as exc:
        _print_error('database error', str(exc))
        ctx.exit(1)


# ── dump ───────────────────────────────────────────────────────────────────────


@database.command()
@click.argument('file')
@click.option(
    '--format',
    'fmt',
    default='custom',
    type=click.Choice(['custom', 'plain'], case_sensitive=False),
    help="Dump format: 'custom' (pg_restore) or 'plain' (SQL). Default: custom.",
)
@requires_config
@click.pass_context
def dump(ctx: click.Context, file: str, fmt: str) -> None:
    """Create a database backup using pg_dump.

    FILE is the destination path for the backup, e.g. backup.dump or backup.sql.
    The backup includes all schemas, functions, and data.
    """
    db = ctx.obj['config'].database

    pg_fmt = '--format=plain' if fmt == 'plain' else '--format=custom'
    cmd = ['pg_dump', pg_fmt, f'--file={file}', _pg_dsn(db)]

    click.echo(f'Dumping database to {file!r} …')
    result = subprocess.run(cmd, env=_pg_env(db))
    if result.returncode != 0:
        _print_error('pg_dump failed', f'Exit code {result.returncode}')
        ctx.exit(result.returncode)
    else:
        click.echo(f'Backup written to {file!r}.')


# ── restore ────────────────────────────────────────────────────────────────────


@database.command()
@click.argument('file', type=click.Path(exists=True, dir_okay=False))
@requires_config
@click.pass_context
def restore(ctx: click.Context, file: str) -> None:
    """Restore a database backup created by 'database dump'.

    For .dump files (custom format) pg_restore is used.
    For .sql files (plain format) psql is used.
    """
    db = ctx.obj['config'].database
    dsn = _pg_dsn(db)
    env = _pg_env(db)
    path = Path(file)

    if path.suffix == '.sql':
        cmd = ['psql', dsn, f'--file={file}']
        tool = 'psql'
    else:
        cmd = ['pg_restore', '--format=custom', f'--dbname={dsn}', file]
        tool = 'pg_restore'

    click.echo(f'Restoring from {file!r} using {tool} …')
    result = subprocess.run(cmd, env=env)
    if result.returncode != 0:
        _print_error(f'{tool} failed', f'Exit code {result.returncode}')
        ctx.exit(result.returncode)
    else:
        click.echo('Restore complete.')


# ── wipe ───────────────────────────────────────────────────────────────────────


@database.command()
@click.option('--force', is_flag=True, default=False, help='Skip the confirmation prompt.')
@requires_config
@click.pass_context
def wipe(ctx: click.Context, force: bool) -> None:
    """Destroy all database contents.

    Drops every module (including default), clears migration history and the
    stored schema. Leaves extension objects and the database itself.
    """
    db = ctx.obj['config'].database
    dbname = db.name or 'pylon'

    if not force:
        click.confirm(
            f"This will destroy all data in '{dbname}'. Continue?",
            abort=True,
        )

    async def do_wipe() -> None:
        from pylon._core import pgcon_connect

        pool = await pgcon_connect(_pg_dsn(db), 2)
        schemas = await _user_schemas(pool)

        # One `batch_execute` call — Postgres's simple query protocol wraps
        # the whole multi-statement blob in an implicit transaction, same
        # atomicity as the old explicit `async with conn.transaction():`.
        statements = [f'DROP SCHEMA "{schema}" CASCADE;' for schema in schemas]
        statements.extend(await _public_drops(pool))
        statements.extend(f'DELETE FROM _pylon."{table}";' for table in await _internal_tables(pool))
        if statements:
            await pool.batch_execute('\n'.join(statements))

    try:
        asyncio.run(do_wipe())
    except PylonError as exc:
        _print_error('database error', str(exc))
        ctx.exit(1)
        return

    click.echo('Database wiped.')
