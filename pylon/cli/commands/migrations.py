"""Migration commands — pylon migration <subcommand>."""

from __future__ import annotations

import asyncio
import re
from pathlib import Path

import click

from ..config import _print_error, requires_config

# ── Helpers ───────────────────────────────────────────────────────────────────

_ADVISORY_LOCK_KEY = 7_461_999  # fixed session-level advisory-lock key for apply


def _migrations_dir(config) -> Path:
    return config.project.schema_dir / "migrations"


def _require_migrations_dir(ctx: click.Context, d: Path) -> None:
    if not d.is_dir():
        _print_error(
            "migrations directory not found",
            f"Expected at {d}. Create it with: mkdir -p {d}",
        )
        ctx.exit(1)


def _pg_dsn(config) -> str:
    db = config.database
    if db.dsn is not None:
        return db.dsn.replace("pylon://", "postgresql://", 1)
    pw = f":{db.password}" if db.password else ""
    return f"postgresql://{db.user}{pw}@{db.host}:{db.port}/{db.name}"


def _load_migrations(d: Path) -> list:
    """Parse all migration files in `d`, returned unsorted."""
    from pylon._core import parse_migration

    files = sorted(d.glob("[0-9][0-9][0-9][0-9][0-9]_*.sql"))
    result = []
    for f in files:
        content = f.read_text()
        try:
            m = parse_migration(content, f.stem)
        except ValueError as exc:
            raise click.ClickException(f"Bad migration file {f.name}: {exc}") from exc
        result.append(m)
    return result


def _ordered_chain(migrations: list) -> list:
    """Validate migrations and return them in chain order (oldest first)."""
    from pylon._core import validate_migration_chain

    if not migrations:
        return []
    try:
        ordered_ids = validate_migration_chain(migrations)
    except ValueError as exc:
        raise click.ClickException(f"Migration chain error: {exc}") from exc
    by_id = {m.id: m for m in migrations}
    return [by_id[mid] for mid in ordered_ids]


def _applied_tip(tracking: list[dict]) -> str | None:
    """Compute the tip ID from _pylon."Migrations" rows (the one with no descendant)."""
    if not tracking:
        return None
    applied_ids = {r["id"] for r in tracking}
    onto_targets = {r["onto"] for r in tracking}
    tips = applied_ids - onto_targets
    return next(iter(tips)) if tips else None


def _next_seq(d: Path) -> int:
    existing = sorted(d.glob("[0-9][0-9][0-9][0-9][0-9]_*.sql"))
    return int(existing[-1].name[:5]) + 1 if existing else 1


def _parse_steps(body: str) -> list[tuple[bool, str]]:
    """Split a migration body on -- pylon:step markers into (transactional, sql) pairs."""
    steps: list[tuple[bool, str]] = []
    current_transactional = True
    current_parts: list[str] = []

    for line in body.splitlines(keepends=True):
        stripped = line.strip()
        if stripped == "-- pylon:step":
            steps.append((current_transactional, "".join(current_parts)))
            current_transactional = True
            current_parts = []
        elif stripped == "-- pylon:step non-transactional":
            steps.append((current_transactional, "".join(current_parts)))
            current_transactional = False
            current_parts = []
        else:
            current_parts.append(line)

    steps.append((current_transactional, "".join(current_parts)))
    return steps


async def _ensure_tracking_tables(conn) -> None:
    await conn.execute(
        'CREATE TABLE IF NOT EXISTS _pylon."Migrations" ('
        "    id          text        PRIMARY KEY,"
        "    onto        text        NOT NULL,"
        "    filename    text        NOT NULL,"
        "    applied_at  timestamptz NOT NULL DEFAULT now()"
        ");"
    )
    await conn.execute(
        'CREATE TABLE IF NOT EXISTS _pylon."Progress" ('
        "    id          text        PRIMARY KEY,"
        "    step_index  integer     NOT NULL,"
        "    updated_at  timestamptz NOT NULL DEFAULT now()"
        ");"
    )


async def _read_tracking(conn) -> list[dict]:
    rows = await conn.fetch('SELECT id, onto, filename FROM _pylon."Migrations"')
    return [dict(r) for r in rows]


# ── CLI group ─────────────────────────────────────────────────────────────────

@click.group()
def migration() -> None:
    """Manage Pylon schema migrations."""


# ── apply ─────────────────────────────────────────────────────────────────────

@migration.command()
@click.option("--to", "to_id", default=None, metavar="ID",
              help="Stop applying at this migration ID.")
@click.option("--dev-mode", is_flag=True, default=False,
              help="Skip DDL already applied via 'watch'; just move the tracking pointer.")
@click.option("--no-wait", is_flag=True, default=False,
              help="Fail immediately if another 'apply' holds the lock.")
@requires_config
@click.pass_context
def apply(ctx: click.Context, to_id: str | None, dev_mode: bool, no_wait: bool) -> None:
    """Apply pending migrations to the database."""
    asyncio.run(_apply(ctx, to_id, dev_mode, no_wait))


async def _apply(
    ctx: click.Context,
    to_id: str | None,
    dev_mode: bool,
    no_wait: bool,
) -> None:
    import asyncpg
    from pylon._core import verify_migration

    config = ctx.obj["config"]
    d = _migrations_dir(config)
    _require_migrations_dir(ctx, d)

    migrations = _load_migrations(d)
    chain = _ordered_chain(migrations)

    conn = await asyncpg.connect(_pg_dsn(config))
    try:
        await _ensure_tracking_tables(conn)

        # Session-level advisory lock (§9.3)
        if no_wait:
            acquired = await conn.fetchval(
                "SELECT pg_try_advisory_lock($1)", _ADVISORY_LOCK_KEY
            )
            if not acquired:
                raise click.ClickException(
                    "Another 'pylon migration apply' is already running (--no-wait)."
                )
        else:
            await conn.execute("SELECT pg_advisory_lock($1)", _ADVISORY_LOCK_KEY)

        try:
            tracking = await _read_tracking(conn)
            applied_tip = _applied_tip(tracking)

            if not chain:
                click.echo("No migrations found.")
                return

            chain_ids = [m.id for m in chain]

            if applied_tip is None:
                pending_start = 0
            elif applied_tip in chain_ids:
                pending_start = chain_ids.index(applied_tip) + 1
            else:
                raise click.ClickException(
                    f"Database tip {applied_tip!r} not found in on-disk chain — "
                    "history has diverged. Resolve manually."
                )

            pending = chain[pending_start:]
            if to_id is not None:
                if to_id not in chain_ids:
                    raise click.ClickException(f"--to target {to_id!r} not in chain.")
                stop_idx = chain_ids.index(to_id)
                if stop_idx < pending_start:
                    raise click.ClickException(f"--to target {to_id!r} is already applied.")
                pending = chain[pending_start : stop_idx + 1]

            if not pending:
                click.echo("Already up to date.")
                return

            for m in pending:
                await _apply_one(conn, m, dev_mode, verify_migration)

        finally:
            await conn.execute("SELECT pg_advisory_unlock($1)", _ADVISORY_LOCK_KEY)
    finally:
        await conn.close()


async def _apply_one(conn, m, dev_mode: bool, verify_migration) -> None:
    try:
        verify_migration(m)
    except ValueError as exc:
        raise click.ClickException(str(exc))

    steps = _parse_steps(m.body)

    # Resume from recorded progress if a prior run failed mid-migration (§9.1)
    progress_row = await conn.fetchrow(
        'SELECT step_index FROM _pylon."Progress" WHERE id = $1', m.id
    )
    resume_from = (progress_row["step_index"] + 1) if progress_row else 0
    multi_step = len(steps) > 1

    for step_idx, (transactional, sql) in enumerate(steps):
        if step_idx < resume_from:
            continue
        sql = sql.strip()
        if not sql:
            continue

        if multi_step:
            await conn.execute(
                'INSERT INTO _pylon."Progress" (id, step_index) VALUES ($1, $2) '
                "ON CONFLICT (id) DO UPDATE SET step_index = $2, updated_at = now()",
                m.id, step_idx,
            )

        is_last = step_idx == len(steps) - 1

        if transactional:
            async with conn.transaction():
                if not dev_mode:
                    await conn.execute(sql)
                if is_last:
                    await _record_applied(conn, m)
                    if multi_step:
                        await conn.execute(
                            'DELETE FROM _pylon."Progress" WHERE id = $1', m.id
                        )
        else:
            if not dev_mode:
                await _drop_invalid_concurrent_index(conn, sql)
                await conn.execute(sql)
            if is_last:
                await _record_applied(conn, m)
                if multi_step:
                    await conn.execute(
                        'DELETE FROM _pylon."Progress" WHERE id = $1', m.id
                    )

    click.echo(f"  Applied {m.filename}")


async def _record_applied(conn, m) -> None:
    await conn.execute(
        'INSERT INTO _pylon."Migrations" (id, onto, filename) VALUES ($1, $2, $3)',
        m.id, m.onto, m.filename,
    )


async def _drop_invalid_concurrent_index(conn, sql: str) -> None:
    """Before retrying a CONCURRENTLY step, drop any invalid index it left behind (§9.1)."""
    match = re.search(
        r'CREATE\s+INDEX\s+CONCURRENTLY\s+(?:IF\s+NOT\s+EXISTS\s+)?"?(\w+)"?',
        sql, re.IGNORECASE,
    )
    if not match:
        return
    index_name = match.group(1)
    invalid = await conn.fetchval(
        "SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid "
        "WHERE c.relname = $1 AND NOT i.indisvalid",
        index_name,
    )
    if invalid:
        await conn.execute(f'DROP INDEX CONCURRENTLY IF EXISTS "{index_name}"')


# ── status ────────────────────────────────────────────────────────────────────

@migration.command()
@click.option("--dev-mode", is_flag=True, default=False,
              help="Also report whether the live DB has drifted ahead of the recorded tip (watch drift).")
@requires_config
@click.pass_context
def status(ctx: click.Context, dev_mode: bool) -> None:
    """Show applied tip, pending migrations, and chain validity."""
    asyncio.run(_status(ctx, dev_mode))


async def _status(ctx: click.Context, dev_mode: bool) -> None:
    import asyncpg

    config = ctx.obj["config"]
    d = _migrations_dir(config)
    _require_migrations_dir(ctx, d)

    migrations = _load_migrations(d)
    chain = _ordered_chain(migrations)

    conn = await asyncpg.connect(_pg_dsn(config))
    try:
        await _ensure_tracking_tables(conn)
        tracking = await _read_tracking(conn)
        if dev_mode:
            from pylon.schema._introspect import introspect_db_state
            db_state = await introspect_db_state(conn)
        else:
            db_state = None
    finally:
        await conn.close()

    applied_tip = _applied_tip(tracking)

    if not chain:
        click.echo("No migrations on disk.")
        if tracking:
            click.echo(f"Database tip: {applied_tip} (no files — orphaned?)")
        return

    chain_ids = [m.id for m in chain]
    fs_tip = chain[-1].id

    click.echo(f"Chain tip (disk):  {fs_tip}")
    click.echo(f"Applied tip (db):  {applied_tip or '(none)'}")

    if applied_tip is None:
        pending = chain
    elif applied_tip in chain_ids:
        pending = chain[chain_ids.index(applied_tip) + 1:]
    else:
        click.echo("\n⚠  Database tip not found in on-disk chain — history has diverged.")
        return

    if pending:
        click.echo(f"\n{len(pending)} pending migration(s):")
        for m in pending:
            click.echo(f"  {m.filename}")
    else:
        click.echo("\nDatabase is up to date.")

    # dev-mode: report whether watch has applied changes beyond the recorded tip.
    if dev_mode and db_state is not None and not pending:
        from pylon._core import diff_schema as _core_diff_schema
        schema = _reload_schema(config)
        ops = _core_diff_schema(schema, db_state)
        if ops:
            click.echo(f"\n⚠  Live database has {len(ops)} change(s) not yet in a migration (watch drift):")
            for sql in ops:
                click.echo(f"  {sql.splitlines()[0]}")
            click.echo("\nRun 'pylon migration create' to record them, then 'pylon migration apply --dev-mode'.")
        else:
            click.echo("\nLive database matches compiled schema — no watch drift.")


# ── log ───────────────────────────────────────────────────────────────────────

@migration.command("log")
@click.option("--from-fs", "source", flag_value="fs", help="Walk on-disk chain.")
@click.option("--from-db", "source", flag_value="db", help="Read tracking table.")
@click.option("--newest-first", is_flag=True, default=False)
@click.option("--limit", type=int, default=None, metavar="N")
@requires_config
@click.pass_context
def log_cmd(
    ctx: click.Context,
    source: str | None,
    newest_first: bool,
    limit: int | None,
) -> None:
    """Print migration history from files (--from-fs) or database (--from-db)."""
    if source is None:
        raise click.UsageError("Specify --from-fs or --from-db.")
    asyncio.run(_log(ctx, source, newest_first, limit))


async def _log(
    ctx: click.Context,
    source: str,
    newest_first: bool,
    limit: int | None,
) -> None:
    import asyncpg

    config = ctx.obj["config"]
    d = _migrations_dir(config)

    if source == "fs":
        _require_migrations_dir(ctx, d)
        chain = _ordered_chain(_load_migrations(d))
        entries = [{"id": m.id, "onto": m.onto, "ref": m.filename} for m in chain]
    else:
        conn = await asyncpg.connect(_pg_dsn(config))
        try:
            await _ensure_tracking_tables(conn)
            rows = await conn.fetch(
                'SELECT id, onto, filename, applied_at '
                'FROM _pylon."Migrations" ORDER BY applied_at'
            )
        finally:
            await conn.close()
        entries = [
            {"id": r["id"], "onto": r["onto"],
             "ref": r["applied_at"].strftime("%Y-%m-%d %H:%M:%S UTC")}
            for r in rows
        ]

    if newest_first:
        entries = list(reversed(entries))
    if limit is not None:
        entries = entries[:limit]

    if not entries:
        click.echo("No migrations.")
        return

    for e in entries:
        click.echo(f"{e['id']}  onto={e['onto']}  {e['ref']}")


# ── watch ────────────────────────────────────────────────────────────────────


@migration.command()
@requires_config
@click.pass_context
def watch(ctx: click.Context) -> None:
    """Live-sync a dev database on schema file changes (no migration files written).

    Watches Python schema source files for changes, recompiles the schema,
    diffs it against the live database, and applies DDL immediately.
    Run 'pylon migration create' afterward to record the changes as a migration.
    """
    asyncio.run(_watch(ctx))


async def _watch(ctx: click.Context) -> None:
    import asyncpg
    from watchfiles import awatch

    config = ctx.obj["config"]
    schema_dir = config.project.schema_dir

    click.echo(f"Watching {schema_dir} for changes… (Ctrl+C to stop)")

    # Initial sync
    await _sync_once(config)

    async for changes in awatch(str(schema_dir)):
        py_changes = [p for _, p in changes if p.endswith(".py")]
        if py_changes:
            click.echo(f"\nDetected change: {', '.join(py_changes)}")
            await _sync_once(config)


async def _sync_once(config) -> None:
    """Recompile schema, introspect DB, diff, apply."""
    import asyncpg
    from pylon._core import diff_schema as _diff_schema
    from pylon.schema._introspect import introspect_db_state

    schema = _reload_schema(config)

    conn = await asyncpg.connect(_pg_dsn(config))
    try:
        db_state = await introspect_db_state(conn)
        ops = _diff_schema(schema, db_state)

        if not ops:
            click.echo("Schema up to date.")
            return

        async with conn.transaction():
            for sql in ops:
                await conn.execute(sql)

        click.echo(f"Applied {len(ops)} DDL statement(s):")
        for sql in ops:
            # Print first line of each statement as a brief summary
            first_line = sql.splitlines()[0]
            click.echo(f"  {first_line}")
    finally:
        await conn.close()


def _reload_schema(config):
    """Clear the registry, re-import schema modules, return a fresh SchemaDescriptor."""
    import sys
    import importlib
    from pylon.schema._registry import clear as _clear_registry
    from pylon.schema._walker import walk
    from pylon.schema._globals import collect_module_globals

    schema_dir = config.project.schema_dir

    # Remove previously loaded schema modules from sys.modules so they re-run.
    schema_dir_str = str(schema_dir)
    to_remove = [
        name for name, mod in sys.modules.items()
        if hasattr(mod, "__file__") and mod.__file__ and
        mod.__file__.startswith(schema_dir_str) and not name.startswith("_")
    ]
    for name in to_remove:
        del sys.modules[name]

    _clear_registry()

    # Ensure schema_dir is on sys.path
    if schema_dir_str not in sys.path:
        sys.path.insert(0, schema_dir_str)

    for py_file in sorted(schema_dir.glob("*.py")):
        stem = py_file.stem
        if not stem.startswith("_"):
            importlib.import_module(stem)

    from pylon.schema._registry import snapshot, functions_snapshot
    types, enums, custom_scalars = snapshot()
    globals_: list = []
    for py_file in sorted(schema_dir.glob("*.py")):
        stem = py_file.stem
        if not stem.startswith("_") and stem in sys.modules:
            globals_.extend(collect_module_globals(sys.modules[stem]))

    schema = walk(types, enums, custom_scalars, globals_, functions=functions_snapshot())
    return schema


# ── create ────────────────────────────────────────────────────────────────────

@migration.command()
@click.option("--blank", is_flag=True, default=False,
              help="Write a hand-editable stub without diffing the database.")
@click.option("--name", default=None, metavar="SLUG",
              help="Optional label appended to the filename.")
@click.option("--dry-run", is_flag=True, default=False,
              help="Print the file content without writing it.")
@click.option("--non-interactive", "non_interactive", is_flag=True, default=False,
              help="Skip the confirmation prompt (also the default when stdout is not a TTY).")
@requires_config
@click.pass_context
def create(
    ctx: click.Context,
    blank: bool,
    name: str | None,
    dry_run: bool,
    non_interactive: bool,
) -> None:
    """Generate a new migration file.

    Diffs the compiled schema against the live database and writes a migration
    file for any pending changes. Use --blank to skip diffing and write a
    hand-editable stub instead.
    """
    if blank:
        _create_blank(ctx, name, dry_run)
    else:
        asyncio.run(_create_from_diff(ctx, name, dry_run, non_interactive))


def _create_blank(ctx: click.Context, name: str | None, dry_run: bool) -> None:
    from pylon._core import blank_migration_body, render_migration_file, compute_migration_short_id

    config = ctx.obj["config"]
    d = _migrations_dir(config)
    _require_migrations_dir(ctx, d)

    chain = _ordered_chain(_load_migrations(d))
    onto = chain[-1].id if chain else "initial"

    body = blank_migration_body()
    content = render_migration_file(onto, body)
    short_id = compute_migration_short_id(body)

    seq = _next_seq(d)
    stem = f"{seq:05d}_{short_id}"
    if name:
        stem = f"{stem}_{name}"
    filename = f"{stem}.sql"

    if dry_run:
        click.echo(f"-- would write: {d / filename}")
        click.echo(content)
        return

    (d / filename).write_text(content)
    click.echo(f"Created {filename}")
    click.echo("Edit the file, then run 'pylon migration rehash' to recompute its ID.")


async def _create_from_diff(
    ctx: click.Context,
    name: str | None,
    dry_run: bool,
    non_interactive: bool,
) -> None:
    import sys
    import asyncpg
    from pylon._core import (
        diff_schema_ops as _core_diff_schema_ops,
        render_migration_file,
        compute_migration_short_id,
        verify_migration,
    )
    from pylon.schema._introspect import introspect_db_state

    config = ctx.obj["config"]
    d = _migrations_dir(config)
    _require_migrations_dir(ctx, d)

    # Compile the target schema from Python source files.
    schema = _reload_schema(config)

    # Load and validate the on-disk migration chain.
    migrations = _load_migrations(d)
    chain = _ordered_chain(migrations)
    chain_tip = chain[-1].id if chain else "initial"

    conn = await asyncpg.connect(_pg_dsn(config))
    try:
        await _ensure_tracking_tables(conn)
        tracking = await _read_tracking(conn)
        applied_tip = _applied_tip(tracking)
        chain_ids = [m.id for m in chain]

        # Diverged history → abort.
        if applied_tip is not None and applied_tip not in chain_ids:
            raise click.ClickException(
                f"Database tip {applied_tip!r} not found in on-disk chain — "
                "history has diverged. Resolve manually."
            )

        # Behind → apply pending migrations first so the diff baseline is current.
        if applied_tip != chain_tip:
            pending_start = 0 if applied_tip is None else (chain_ids.index(applied_tip) + 1)
            pending = chain[pending_start:]
            if pending:
                click.echo(f"Applying {len(pending)} pending migration(s) before diffing…")
                await conn.execute("SELECT pg_advisory_lock($1)", _ADVISORY_LOCK_KEY)
                try:
                    for m in pending:
                        await _apply_one(conn, m, False, verify_migration)
                finally:
                    await conn.execute("SELECT pg_advisory_unlock($1)", _ADVISORY_LOCK_KEY)

        # Introspect the live database (now at the chain tip).
        db_state = await introspect_db_state(conn)
    finally:
        await conn.close()

    # Compute the diff — returns [(sql, non_transactional), ...].
    ops = _core_diff_schema_ops(schema, db_state)

    if not ops:
        click.echo("No schema changes detected.")
        return

    # Show proposed changes.
    has_concurrent = any(nt for _, nt in ops)
    click.echo(f"\n{len(ops)} proposed change(s):")
    for sql, non_tx in ops:
        marker = " [CONCURRENTLY]" if non_tx else ""
        click.echo(f"  {sql.splitlines()[0]}{marker}")
    if has_concurrent:
        click.echo("\n  Note: CONCURRENTLY statements run outside a transaction wrapper.")

    # Confirm unless --non-interactive or not a TTY.
    if not non_interactive and sys.stdout.isatty():
        click.echo()
        if not click.confirm("Write migration?"):
            raise click.ClickException("Aborted.")

    # Assemble migration body with -- pylon:step markers between transactional
    # and non-transactional groups.
    body = _assemble_migration_body(ops)

    # Write the file.
    onto = chain_tip
    content = render_migration_file(onto, body)
    short_id = compute_migration_short_id(body)

    seq = _next_seq(d)
    stem = f"{seq:05d}_{short_id}"
    if name:
        stem = f"{stem}_{name}"
    filename = f"{stem}.sql"

    if dry_run:
        click.echo(f"\n-- would write: {d / filename}")
        click.echo(content)
        return

    (d / filename).write_text(content)
    click.echo(f"\nCreated {filename}")


def _assemble_migration_body(ops: list[tuple[str, bool]]) -> str:
    """Build a migration body string from (sql, non_transactional) pairs.

    Consecutive ops with the same transactional status are grouped. Between
    groups a -- pylon:step or -- pylon:step non-transactional marker is emitted
    so the apply command knows where transaction boundaries fall.
    """
    if not ops:
        return ""

    # Group consecutive ops by their non_transactional flag.
    groups: list[tuple[bool, list[str]]] = []  # [(non_transactional, [sql, ...]), ...]
    for sql, non_tx in ops:
        if groups and groups[-1][0] == non_tx:
            groups[-1][1].append(sql)
        else:
            groups.append((non_tx, [sql]))

    parts: list[str] = []
    for i, (non_tx, sqls) in enumerate(groups):
        if i > 0:
            # Insert the step marker that signals the start of this group.
            if non_tx:
                parts.append("-- pylon:step non-transactional")
            else:
                parts.append("-- pylon:step")
        parts.append("\n".join(sqls))

    return "\n" + "\n\n".join(parts) + "\n"


# ── rehash ────────────────────────────────────────────────────────────────────

@migration.command()
@click.argument("file", type=click.Path(exists=True, dir_okay=False, path_type=Path))
@requires_config
@click.pass_context
def rehash(ctx: click.Context, file: Path) -> None:
    """Recompute a hand-edited migration's ID and update its header and filename.

    Only valid for the chain tip that hasn't been applied to any database yet.
    """
    from pylon._core import compute_migration_id, compute_migration_short_id, parse_migration

    config = ctx.obj["config"]
    d = _migrations_dir(config)
    _require_migrations_dir(ctx, d)

    content = file.read_text()
    m = parse_migration(content, file.stem)

    # Guard: must be chain tip
    chain = _ordered_chain(_load_migrations(d))
    if chain and chain[-1].id != m.id:
        raise click.ClickException(
            f"{file.name} is not the chain tip — rehash is only valid for the tip."
        )

    new_id = compute_migration_id(m.body)
    if new_id == m.id:
        click.echo("ID is already correct — nothing to do.")
        return

    new_short = compute_migration_short_id(m.body)

    # Rewrite the migration: line in the header
    lines = content.splitlines(keepends=True)
    lines[0] = f"-- migration: {new_id}\n"
    file.write_text("".join(lines))

    # Rename the file: keep seq + optional name suffix, replace short_id
    stem = file.stem  # e.g. 00001_m1abc123_my_slug
    parts = stem.split("_", 2)  # ["00001", "m1abc123", "my_slug"] or ["00001", "m1abc123"]
    new_stem = f"{parts[0]}_{new_short}"
    if len(parts) > 2:
        new_stem = f"{new_stem}_{parts[2]}"
    new_path = file.parent / f"{new_stem}.sql"
    file.rename(new_path)

    click.echo(f"Rehashed: {file.name} → {new_path.name}")
    click.echo(f"  old ID: {m.id}")
    click.echo(f"  new ID: {new_id}")
