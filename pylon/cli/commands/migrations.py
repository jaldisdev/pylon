"""Migration commands — pylon migration <subcommand>."""

from __future__ import annotations

import asyncio
from pathlib import Path

import click

from ..config import _print_error, requires_config

# ── Helpers ───────────────────────────────────────────────────────────────────


def _migrations_dir(config) -> Path:
    return config.project.schema_dir / "migrations"


def _require_migrations_dir(ctx: click.Context, d: Path) -> None:
    d.mkdir(parents=True, exist_ok=True)


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
    """Compute the tip ID from applied _pylon."Migrations" rows (the one with no descendant).

    A healthy chain has exactly one such row, but a tracking table can end
    up with several orphaned single-node "tips" (e.g. leftover rows from
    migration files that were since deleted/regenerated without cleaning up
    the tracking table). When that happens, deterministically return the
    lexicographically smallest ID among them — `next(iter(tips))` on a
    `set()` difference used to pick one at random (Python's string hash
    randomization means iteration order varies per *process*, so `status`
    and `apply` — each a separate CLI invocation — could disagree on which
    tip is current). Mirrors `pylon_core::migrate::applied_tip`'s tie-break.
    """
    applied = [r for r in tracking if r.get("applied_at") is not None]
    if not applied:
        return None
    applied_ids = {r["id"] for r in applied}
    onto_targets = {r["onto"] for r in applied}
    tips = applied_ids - onto_targets
    return min(tips) if tips else None


def _next_seq(d: Path) -> int:
    existing = sorted(d.glob("[0-9][0-9][0-9][0-9][0-9]_*.sql"))
    return int(existing[-1].name[:5]) + 1 if existing else 1


async def _ensure_tracking_tables(conn) -> None:
    await conn.execute(
        'CREATE TABLE IF NOT EXISTS _pylon."Migrations" ('
        "    id          text        PRIMARY KEY,"
        "    onto        text        NOT NULL,"
        "    filename    text        NOT NULL,"
        "    db_state    jsonb       NULL,"
        "    applied_at  timestamptz NULL"
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
    rows = await conn.fetch('SELECT id, onto, filename, db_state, applied_at FROM _pylon."Migrations"')
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
    from pylon._core import (
        schema_to_db_state_json,
        pgcon_connect,
        migration_ensure_tracking_tables,
        migration_read_tracking,
        migration_applied_tip,
        migration_advisory_lock,
        migration_try_advisory_lock,
        migration_record_applied,
    )

    config = ctx.obj["config"]
    d = _migrations_dir(config)
    _require_migrations_dir(ctx, d)

    migrations = _load_migrations(d)
    chain = _ordered_chain(migrations)

    pool = await pgcon_connect(_pg_dsn(config), config.database.pool_max_size)
    await migration_ensure_tracking_tables(pool)

    # Session-level advisory lock (§9.3)
    if no_wait:
        lock = await migration_try_advisory_lock(pool)
        if lock is None:
            raise click.ClickException(
                "Another 'pylon migration apply' is already running (--no-wait)."
            )
    else:
        lock = await migration_advisory_lock(pool)

    try:
        tracking = await migration_read_tracking(pool)  # [(id, onto, applied), ...]
        applied_tip = migration_applied_tip(tracking)

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

        applied_ids = {id_ for id_, _onto, _applied in tracking}
        for m in pending:
            # §12 squash compatibility: if any of this migration's squashed
            # constituent IDs are already in the tracking table, the DB was
            # updated via the old (pre-squash) chain — backfill and skip DDL.
            if m.squashed and any(sid in applied_ids for sid in m.squashed):
                await migration_record_applied(pool, m.id, m.onto, m.filename)
                click.echo(f"  Backfilled {m.filename} (squash of already-applied migrations)")
                applied_ids.add(m.id)
                continue
            await _apply_one(pool, m, dev_mode)
            applied_ids.add(m.id)

        # Store a db_state snapshot on the tip row so the next `migration create`
        # has a correct baseline without needing to apply pending migrations first.
        tip = pending[-1]
        schema = _reload_schema(config)
        db_state_snapshot = schema_to_db_state_json(schema)
        await pool.execute(
            'UPDATE _pylon."Migrations" SET db_state = $1::jsonb WHERE id = $2',
            [db_state_snapshot, tip.id],
        )

    finally:
        await lock.unlock()


async def _apply_one(pool, m, dev_mode: bool) -> None:
    """Apply one migration via `pylon_core::migrate::apply_one` (advisory
    lock, per-step transactions, resumable progress, and dev-mode savepoint
    retry all run in Rust — see `crates/pylon-core/src/migrate.rs`)."""
    from pylon._core import migration_apply_one

    try:
        await migration_apply_one(pool, m, dev_mode)
    except ValueError as exc:
        raise click.ClickException(str(exc)) from exc

    click.echo(f"  Applied {m.filename}")


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
            from pylon._core import pgcon_connect, introspect_db_state
            pool = await pgcon_connect(_pg_dsn(config), 2)
            db_state = await introspect_db_state(pool)
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
    from pylon._core import diff_schema as _diff_schema, pgcon_connect, introspect_db_state

    schema = _reload_schema(config)

    pool = await pgcon_connect(_pg_dsn(config), 2)
    db_state = await introspect_db_state(pool)

    conn = await asyncpg.connect(_pg_dsn(config))
    try:
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

    from pylon._finalize import RESERVED_MODULE_NAMES
    for py_file in sorted(schema_dir.glob("*.py")):
        stem = py_file.stem
        if stem.startswith("_"):
            continue
        if stem in RESERVED_MODULE_NAMES:
            raise click.ClickException(
                f"'{stem}.py' is not a valid module name: '{stem}' is a reserved "
                f"PostgreSQL schema name. Use a different name."
            )
        importlib.import_module(stem)

    from pylon.schema._registry import snapshot, functions_snapshot, named_tuples_snapshot
    types, enums, custom_scalars = snapshot()
    globals_: list = []
    for py_file in sorted(schema_dir.glob("*.py")):
        stem = py_file.stem
        if not stem.startswith("_") and stem in sys.modules:
            globals_.extend(collect_module_globals(sys.modules[stem]))

    schema = walk(
        types,
        enums,
        custom_scalars,
        globals_,
        functions=functions_snapshot(),
        named_tuples=named_tuples_snapshot(),
    )
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


_RENAME_HELP = """\
  y   — accept rename (emit ALTER … RENAME)
  n   — reject (emit DROP + CREATE instead)
  l   — show the DDL statement(s) for this change
  c   — list all changes confirmed so far
  b   — go back to the previous question
  s   — stop rename prompts and write a migration from what's confirmed so far
  q   — quit without writing anything
  h/? — show this help"""


def _rename_prompt_loop(
    type_candidates: list,
    col_candidates: list,
) -> tuple[list[tuple], list[tuple], bool]:
    """Present each rename candidate interactively.

    Returns (confirmed_type_renames, confirmed_col_renames, quit_requested).
    quit_requested=True means the user chose 'q' — caller should abort.
    """
    # Build a flat list of prompt entries.
    entries: list[dict] = []
    for old_mod, old_table, new_mod, new_table, new_type_name, confidence in type_candidates:
        pct = int(confidence * 100)
        if old_mod == new_mod:
            ddl = f'ALTER TABLE "{old_mod}"."{old_table}" RENAME TO "{new_table}";'
        else:
            ddl = (
                f'ALTER TABLE "{old_mod}"."{old_table}" SET SCHEMA "{new_mod}";\n'
                f'ALTER TABLE "{new_mod}"."{old_table}" RENAME TO "{new_table}";'
            )
        entries.append({
            "question": f"did you rename type '{old_mod}::{old_table}' to '{new_type_name}' ({pct}%)?",
            "ddl": ddl,
            "kind": "type",
            "data": (old_mod, old_table, new_mod, new_table),
        })
    for mod, table, old_col, new_col, pg_type in col_candidates:
        entries.append({
            "question": (
                f"did you rename property '{old_col}' to '{new_col}' "
                f"on '{mod}::{table}' ({pg_type})?"
            ),
            "ddl": f'ALTER TABLE "{mod}"."{table}" RENAME COLUMN "{old_col}" TO "{new_col}";',
            "kind": "col",
            "data": (mod, table, old_col, new_col),
        })

    if not entries:
        return [], [], False

    decisions: list[bool | None] = [None] * len(entries)
    idx = 0

    while idx < len(entries):
        e = entries[idx]
        click.echo(f"\n{e['question']}")

        confirmed_ddls = [
            entries[i]["ddl"] for i in range(len(entries)) if decisions[i] is True
        ]

        while True:
            raw = click.prompt("", prompt_suffix="[y,n,l,c,b,s,q,?] ").strip().lower()
            if raw == "y":
                decisions[idx] = True
                idx += 1
                break
            elif raw == "n":
                decisions[idx] = False
                idx += 1
                break
            elif raw == "l":
                for line in e["ddl"].splitlines():
                    click.echo(f"  {line}")
            elif raw == "c":
                if confirmed_ddls:
                    click.echo("Confirmed so far:")
                    for ddl in confirmed_ddls:
                        for line in ddl.splitlines():
                            click.echo(f"  {line}")
                else:
                    click.echo("  (nothing confirmed yet)")
            elif raw == "b":
                if idx > 0:
                    idx -= 1
                    decisions[idx] = None
                else:
                    click.echo("  Already at the first question.")
                break  # re-enter outer loop at new idx
            elif raw == "s":
                # Stop prompting; write a migration from what's confirmed so far.
                idx = len(entries)
                break
            elif raw == "q":
                return [], [], True
            elif raw in ("h", "?"):
                click.echo(_RENAME_HELP)
            else:
                click.echo(f"  Unknown option. Enter y, n, l, c, b, s, q, or ?")

    confirmed_type: list[tuple] = []
    confirmed_col: list[tuple] = []
    for i, e in enumerate(entries):
        if decisions[i] is True:
            if e["kind"] == "type":
                confirmed_type.append(e["data"])
            else:
                confirmed_col.append(e["data"])
    return confirmed_type, confirmed_col, False


def _fill_prompt_loop(
    fill_candidates: list,
    is_interactive: bool,
    schema,
) -> list[tuple]:
    """Resolve fill expressions (PyQL) for columns being made NOT NULL.

    Returns a list of (module, table, column, sql_expr) tuples ready for
    `diff_schema_ops_with_renames_and_fills`. Raises `click.ClickException`
    in non-interactive mode when any fill cannot be auto-derived from the
    schema's declared default.
    """
    from pylon._core import compile_fill_expr

    fills: list[tuple] = []
    need_prompt: list[tuple] = []

    for module, table, column, pg_type, type_name, is_new_column, default_sql in fill_candidates:
        if default_sql is not None:
            fills.append((module, table, column, default_sql))
        else:
            need_prompt.append((module, table, column, pg_type, type_name, is_new_column))

    if not need_prompt:
        return fills

    if not is_interactive:
        lines = [
            "The following columns are being made NOT NULL but have no fill expression:"
        ]
        for module, table, column, pg_type, type_name, is_new_col in need_prompt:
            kind = "new column" if is_new_col else "existing column"
            lines.append(f"  {type_name}.{column} ({pg_type}, {kind})")
        lines.append(
            "Provide a fill expression via the schema's default= annotation, "
            "or run interactively."
        )
        raise click.ClickException("\n".join(lines))

    click.echo(
        "\nSome columns are being made NOT NULL and need a fill expression\n"
        "to backfill existing rows before the constraint is set.\n"
        "Enter a PyQL expression (e.g. 'No content', 0, .other_field).\n"
        "Press Ctrl+C to abort."
    )

    for module, table, column, pg_type, type_name, is_new_col in need_prompt:
        kind = "new column" if is_new_col else "existing column"
        click.echo(f"\n  {type_name}.{column}  ({pg_type}, {kind})")
        click.echo(f'  Table: "{module}"."{table}"')

        while True:
            expr_str = click.prompt("  fill_expr>", prompt_suffix=" ").strip()
            if not expr_str:
                click.echo("  Expression cannot be empty.")
                continue
            try:
                sql_expr = compile_fill_expr(type_name, expr_str, schema)
            except Exception as exc:
                click.echo(f"  Error: {exc}")
                continue
            fills.append((module, table, column, sql_expr))
            break

    return fills


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
        detect_type_renames as _core_detect_type_renames,
        detect_col_renames as _core_detect_col_renames,
        detect_fill_required as _core_detect_fill_required,
        diff_schema_ops_with_renames_and_fills as _core_diff_schema_ops_with_renames_and_fills,
        render_migration_file,
        compute_migration_short_id,
        db_state_from_json,
        pgcon_connect,
        introspect_db_state,
    )

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

        # Pending migrations → abort. All migrations must be applied before
        # creating a new one so the live database is the correct diff baseline.
        if applied_tip != chain_tip:
            pending_start = 0 if applied_tip is None else (chain_ids.index(applied_tip) + 1)
            pending = chain[pending_start:]
            if pending:
                names = ", ".join(m.filename for m in pending)
                raise click.ClickException(
                    f"{len(pending)} unapplied migration(s): {names}\n"
                    "Run 'pylon migration apply' first."
                )

        # Use the db_state snapshot from the tip row as the baseline — this is
        # what the schema looked like after the last migration was applied, which
        # is the correct baseline even when watch has run since then.
        # Fall back to live DB introspection only when no snapshot exists yet.
        tip_row = next((r for r in tracking if r["id"] == chain_tip), None)
        db_state_json = tip_row["db_state"] if tip_row else None
        if db_state_json is not None:
            db_state = db_state_from_json(db_state_json)
        else:
            pool = await pgcon_connect(_pg_dsn(config), 2)
            db_state = await introspect_db_state(pool)
    finally:
        await conn.close()

    # ── Rename detection (interactive) ────────────────────────────────────────
    confirmed_type_renames: list[tuple[str, str, str, str]] = []
    confirmed_col_renames: list[tuple[str, str, str, str]] = []

    is_interactive = not non_interactive and sys.stdout.isatty()

    if is_interactive:
        type_candidates = _core_detect_type_renames(schema, db_state)
        col_candidates = _core_detect_col_renames(schema, db_state)

        if type_candidates or col_candidates:
            confirmed_type_renames, confirmed_col_renames, quit_requested = (
                _rename_prompt_loop(type_candidates, col_candidates)
            )
            if quit_requested:
                raise click.ClickException("Aborted.")

    # ── Fill expression detection ─────────────────────────────────────────────
    fill_candidates = _core_detect_fill_required(schema, db_state)
    fills = _fill_prompt_loop(fill_candidates, is_interactive, schema)

    # ── Compute final diff ────────────────────────────────────────────────────
    if confirmed_type_renames or confirmed_col_renames or fills:
        ops = _core_diff_schema_ops_with_renames_and_fills(
            schema, db_state, confirmed_type_renames, confirmed_col_renames, fills
        )
    else:
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

    # Final write confirmation (always ask unless --non-interactive or no TTY).
    if is_interactive:
        click.echo()
        if not click.confirm("Write migration?"):
            raise click.ClickException("Aborted.")

    # Assemble migration body and write the file.
    body = _assemble_migration_body(ops)
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


# ── squash ────────────────────────────────────────────────────────────────────

@migration.command()
@click.option("--from", "from_id", default=None, metavar="ID",
              help="First migration in the range to squash (inclusive).")
@click.option("--to", "to_id", default=None, metavar="ID",
              help="Last migration in the range to squash (inclusive).")
@click.option("--count", type=int, default=None, metavar="N",
              help="Squash the last N migrations.")
@click.option("--dry-run", is_flag=True, default=False,
              help="Print the squashed body without modifying any files.")
@requires_config
@click.pass_context
def squash(
    ctx: click.Context,
    from_id: str | None,
    to_id: str | None,
    count: int | None,
    dry_run: bool,
) -> None:
    """Collapse a contiguous range of migrations into a single file.

    Uses an ephemeral shadow database to compute the net DDL, then replaces the
    constituent files with one squashed migration that records their IDs in its
    header (for tracking-table compatibility with databases that have already
    applied the old chain).

    Requires CREATEDB privilege on the target PostgreSQL server.
    """
    asyncio.run(_squash(ctx, from_id, to_id, count, dry_run))


async def _squash(
    ctx: click.Context,
    from_id: str | None,
    to_id: str | None,
    count: int | None,
    dry_run: bool,
) -> None:
    import secrets
    import asyncpg
    from pylon._core import (
        diff_states as _core_diff_states,
        render_migration_file,
        compute_migration_short_id,
        pgcon_connect,
        migration_ensure_tracking_tables,
        introspect_db_state,
    )

    config = ctx.obj["config"]
    d = _migrations_dir(config)
    _require_migrations_dir(ctx, d)

    migrations = _load_migrations(d)
    chain = _ordered_chain(migrations)

    if not chain:
        raise click.ClickException("No migrations to squash.")

    chain_ids = [m.id for m in chain]

    # ── Resolve the squash range ───────────────────────────────────────────────
    if count is not None:
        if from_id or to_id:
            raise click.UsageError("Use --count OR --from/--to, not both.")
        if count < 2:
            raise click.ClickException("--count must be at least 2.")
        if count > len(chain):
            raise click.ClickException(
                f"--count ({count}) exceeds chain length ({len(chain)})."
            )
        range_start = len(chain) - count
        range_end = len(chain) - 1
    elif from_id and to_id:
        if from_id not in chain_ids:
            raise click.ClickException(f"--from ID {from_id!r} not found in chain.")
        if to_id not in chain_ids:
            raise click.ClickException(f"--to ID {to_id!r} not found in chain.")
        range_start = chain_ids.index(from_id)
        range_end = chain_ids.index(to_id)
        if range_start >= range_end:
            raise click.ClickException("--from must precede --to in the chain.")
    else:
        raise click.UsageError("Specify --count or both --from and --to.")

    squash_range = chain[range_start : range_end + 1]
    squashed_ids = [m.id for m in squash_range]
    onto = chain[range_start - 1].id if range_start > 0 else "initial"

    click.echo(
        f"Squashing {len(squash_range)} migration(s) "
        f"({squash_range[0].short_id} … {squash_range[-1].short_id})…"
    )

    # ── Spin up ephemeral shadow database ──────────────────────────────────────
    dsn = _pg_dsn(config)
    shadow_name = f"_pylon_shadow_{secrets.token_hex(8)}"
    click.echo(f"Creating shadow database {shadow_name!r}…")

    conn = await asyncpg.connect(dsn)
    try:
        await conn.execute(f'CREATE DATABASE "{shadow_name}" TEMPLATE template0')
    except asyncpg.InsufficientPrivilegeError:
        await conn.close()
        raise click.ClickException(
            "Squash requires CREATEDB privilege on the PostgreSQL server."
        )
    finally:
        await conn.close()

    before_state = after_state = None
    try:
        shadow_dsn = _shadow_dsn(dsn, shadow_name)
        shadow_conn = await asyncpg.connect(shadow_dsn)
        # A small dedicated pool for `_apply_one`/`introspect_db_state` on
        # the same ephemeral shadow database — `shadow_conn` (asyncpg) is
        # only still needed for `CREATE SCHEMA` (Phase 13 territory).
        shadow_pool = await pgcon_connect(shadow_dsn, 2)
        try:
            await shadow_conn.execute("CREATE SCHEMA IF NOT EXISTS _pylon")
            await migration_ensure_tracking_tables(shadow_pool)

            # Apply migrations before the squash range to reach the "before" state.
            pre_range = chain[:range_start]
            for m in pre_range:
                await _apply_one(shadow_pool, m, False)

            before_state = await introspect_db_state(shadow_pool)

            # Apply the squash range to reach the "after" state.
            for m in squash_range:
                await _apply_one(shadow_pool, m, False)

            after_state = await introspect_db_state(shadow_pool)
        finally:
            await shadow_conn.close()
    finally:
        drop_conn = await asyncpg.connect(dsn)
        try:
            await drop_conn.execute(f'DROP DATABASE IF EXISTS "{shadow_name}"')
        finally:
            await drop_conn.close()
        click.echo(f"Dropped shadow database {shadow_name!r}.")

    # ── Compute the net DDL ────────────────────────────────────────────────────
    ops = _core_diff_states(before_state, after_state)

    if not ops:
        raise click.ClickException(
            "Squash range produces no net DDL changes — nothing to write."
        )

    body = _assemble_migration_body(ops)
    new_short_id = compute_migration_short_id(body)
    content = render_migration_file(onto, body, squashed_ids)

    # Sequence number = position of range_start in the final file list (1-indexed).
    seq = range_start + 1
    filename = f"{seq:05d}_{new_short_id}.sql"

    if dry_run:
        click.echo(f"\n-- would write: {d / filename}")
        click.echo(content)
        click.echo(
            f"\n-- would delete: {', '.join(m.filename for m in squash_range)}"
        )
        click.echo(
            f"-- files after {squash_range[-1].filename} would be renumbered from {seq + 1:05d}"
        )
        return

    # ── Write squashed file, delete constituents, renumber ────────────────────
    (d / filename).write_text(content)

    for m in squash_range:
        f = d / m.filename
        if f.exists():
            f.unlink()

    _renumber_migrations(d)

    click.echo(f"Squashed {len(squash_range)} migration(s) → {filename}")


def _shadow_dsn(dsn: str, shadow_name: str) -> str:
    """Return a copy of `dsn` with the database name replaced by `shadow_name`."""
    from urllib.parse import urlparse, urlunparse
    parsed = urlparse(dsn)
    # Path is /dbname (possibly with ?params appended in the query component).
    new_path = f"/{shadow_name}"
    return urlunparse(parsed._replace(path=new_path))


def _renumber_migrations(d: Path) -> None:
    """Rename all migration files so their sequence prefixes are contiguous from 1."""
    files = sorted(d.glob("[0-9][0-9][0-9][0-9][0-9]_*.sql"))
    for new_idx, path in enumerate(files, start=1):
        parts = path.stem.split("_", 1)  # ["00003", "m1abc123..."]
        new_stem = f"{new_idx:05d}_{parts[1]}"
        new_path = path.parent / f"{new_stem}.sql"
        if new_path != path:
            path.rename(new_path)
