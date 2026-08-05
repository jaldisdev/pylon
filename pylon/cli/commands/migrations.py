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


def _next_seq(d: Path) -> int:
    existing = sorted(d.glob("[0-9][0-9][0-9][0-9][0-9]_*.sql"))
    return int(existing[-1].name[:5]) + 1 if existing else 1


def _complete_migration_id(ctx: click.Context, param: click.Parameter, incomplete: str) -> list[str]:
    """Shell-completion callback: on-disk migration IDs matching the prefix."""
    try:
        from pylon.config import load_config

        config = load_config()
        migrations = _load_migrations(_migrations_dir(config))
    except Exception:
        return []
    return [m.id for m in migrations if m.id.startswith(incomplete)]


# ── CLI group ─────────────────────────────────────────────────────────────────

@click.group()
def migration() -> None:
    """Manage Pylon schema migrations."""


# ── apply ─────────────────────────────────────────────────────────────────────

@migration.command()
@click.option("--to", "to_id", default=None, metavar="ID",
              shell_complete=_complete_migration_id,
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
        migration_write_schema_snapshot,
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
        tracking = await migration_read_tracking(pool)  # [(id, onto, db_state, schema_state, applied), ...]
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

        applied_ids = {id_ for id_, _onto, _db_state, _schema_state, _applied in tracking}
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

        # Store a db_state + schema_state snapshot on the tip row so the next
        # `migration create` has a correct baseline without needing to apply
        # pending migrations first. schema_state is the full descriptor
        # (including schema semantics with zero DDL footprint — readonly,
        # rewrites, channels, ...) that db_state alone can't represent.
        tip = pending[-1]
        schema = _reload_schema(config)
        db_state_snapshot = schema_to_db_state_json(schema)
        await pool.execute(
            'UPDATE _pylon."Migrations" SET db_state = $1::jsonb, schema_state = $2::jsonb WHERE id = $3',
            [db_state_snapshot, schema.to_json(), tip.id],
        )

        # Update the schema snapshot every client fetches at startup — this
        # migration just changed what the live database actually looks like,
        # so clients should see it now, not whatever the schema files say
        # (those have no effect until migrated, by design).
        await migration_write_schema_snapshot(pool, schema.to_json())

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
    from pylon._core import (
        pgcon_connect,
        migration_ensure_tracking_tables,
        migration_read_tracking,
        migration_applied_tip,
        introspect_db_state,
    )

    config = ctx.obj["config"]
    d = _migrations_dir(config)
    _require_migrations_dir(ctx, d)

    migrations = _load_migrations(d)
    chain = _ordered_chain(migrations)

    pool = await pgcon_connect(_pg_dsn(config), 2)
    await migration_ensure_tracking_tables(pool)
    tracking = await migration_read_tracking(pool)  # [(id, onto, db_state, schema_state, applied), ...]
    db_state = await introspect_db_state(pool) if dev_mode else None

    applied_tip = migration_applied_tip(tracking)

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
        from pylon._core import (
            diff_schema as _core_diff_schema,
            schema_content_changed as _core_schema_content_changed,
            SchemaDescriptor,
        )
        schema = _reload_schema(config)
        ops = _core_diff_schema(schema, db_state)

        # DDL diffs alone miss schema semantics with zero physical footprint
        # (readonly, rewrites, channels, ...) — compare full schema content
        # against the tip row's own recorded schema_state too, so drift in
        # those constructs is reported here just as reliably as DDL drift.
        tip_row = next((r for r in tracking if r[0] == applied_tip), None)
        schema_state_json = tip_row[3] if tip_row else None
        previous_schema = SchemaDescriptor.from_json(schema_state_json) if schema_state_json is not None else None
        content_changed = _core_schema_content_changed(schema, previous_schema)

        if ops or content_changed:
            click.echo("\n⚠  Live database has drift not yet in a migration:")
            if ops:
                click.echo(f"  {len(ops)} DDL change(s):")
                for sql in ops:
                    click.echo(f"    {sql.splitlines()[0]}")
            if content_changed:
                click.echo("  Non-DDL schema change(s) (e.g. readonly/rewrites/channels) not yet recorded.")
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
    config = ctx.obj["config"]
    d = _migrations_dir(config)

    if source == "fs":
        _require_migrations_dir(ctx, d)
        chain = _ordered_chain(_load_migrations(d))
        entries = [{"id": m.id, "onto": m.onto, "ref": m.filename} for m in chain]
    else:
        from pylon._core import pgcon_connect, migration_ensure_tracking_tables

        pool = await pgcon_connect(_pg_dsn(config), 2)
        await migration_ensure_tracking_tables(pool)
        rows = await pool.query_named(
            """
            SELECT id, onto,
                   (to_char(applied_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') || ' UTC') AS applied_at
            FROM _pylon."Migrations" ORDER BY applied_at
            """,
            [],
        )
        entries = [{"id": r["id"], "onto": r["onto"], "ref": r["applied_at"]} for r in rows]

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
    from pylon._core import (
        SchemaDescriptor,
        diff_schema as _diff_schema,
        missing_extension_ddl as _core_missing_extension_ddl,
        schema_content_changed as _core_schema_content_changed,
        pgcon_connect,
        introspect_db_state,
        migration_ensure_tracking_tables,
        migration_read_schema_snapshot,
        migration_write_schema_snapshot,
    )

    schema = _reload_schema(config)

    pool = await pgcon_connect(_pg_dsn(config), 2)
    db_state = await introspect_db_state(pool)

    ops = _diff_schema(schema, db_state)

    # A required Postgres extension (e.g. pgvector) is a hard prerequisite,
    # not something to ask about — always ensure it's enabled before the
    # rest of the DDL that needs it.
    ops = _core_missing_extension_ddl(schema, db_state) + ops

    await migration_ensure_tracking_tables(pool)

    # DDL-visible changes (`ops` above) aren't the whole story: schema
    # semantics with zero physical DDL footprint (`readonly`, rewrites,
    # pub/sub `Channel`s, ...) never show up in `ops` no matter what, since
    # there's no column/constraint/catalog object to introspect and diff
    # against. Compare full schema content against the last-synced snapshot
    # too, so a content-only change still results in `_pylon."Schema"`
    # getting updated instead of silently going unsynced forever.
    previous_json = await migration_read_schema_snapshot(pool)
    previous = SchemaDescriptor.from_json(previous_json) if previous_json is not None else None
    content_changed = _core_schema_content_changed(schema, previous)

    if not ops and not content_changed:
        click.echo("Schema up to date.")
        return

    if ops:
        # One `batch_execute` call — Postgres's simple query protocol wraps the
        # whole multi-statement blob in an implicit transaction, same atomicity
        # as the old explicit `async with conn.transaction():`.
        await pool.batch_execute("\n".join(ops))

        click.echo(f"Applied {len(ops)} DDL statement(s):")
        for sql in ops:
            # Print first line of each statement as a brief summary
            first_line = sql.splitlines()[0]
            click.echo(f"  {first_line}")
    else:
        click.echo("Applied schema changes with no DDL footprint (e.g. readonly/rewrites/channels).")

    # A dev-mode sync just changed the live database the same way a real
    # migration would — clients should be able to pick that up immediately
    # too, not just once a formal `migration apply` eventually records it.
    await migration_write_schema_snapshot(pool, schema.to_json())


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

    from pylon.schema._aliases import collect_module_aliases
    from pylon.schema._registry import snapshot, functions_snapshot, named_tuples_snapshot, signals_snapshot
    types, enums, custom_scalars = snapshot()
    globals_: list = []
    # `_finalize.finalize()` (the in-process path) has always collected
    # aliases here too — this reload path (migration create/apply/watch)
    # never did, so any `Alias` a project declared would silently vanish
    # from every migration and from `_pylon."Schema"` the moment one was
    # ever applied, even though `pylon.finalize()` itself saw it fine.
    aliases_: list = []
    for py_file in sorted(schema_dir.glob("*.py")):
        stem = py_file.stem
        if not stem.startswith("_") and stem in sys.modules:
            globals_.extend(collect_module_globals(sys.modules[stem]))
            aliases_.extend(collect_module_aliases(sys.modules[stem]))

    schema = walk(
        types,
        enums,
        custom_scalars,
        globals_,
        functions=functions_snapshot(),
        aliases=aliases_,
        named_tuples=named_tuples_snapshot(),
        signals=signals_snapshot(),
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
@click.option("--expert", is_flag=True, default=False,
              help="Terser prompts: hide DDL/schema previews by default (press 'l' to reveal).")
@requires_config
@click.pass_context
def create(
    ctx: click.Context,
    blank: bool,
    name: str | None,
    dry_run: bool,
    non_interactive: bool,
    expert: bool,
) -> None:
    """Generate a new migration file.

    Diffs the compiled schema against the live database and writes a migration
    file for any pending changes. Use --blank to skip diffing and write a
    hand-editable stub instead.
    """
    if blank:
        _create_blank(ctx, name, dry_run)
    else:
        asyncio.run(_create_from_diff(ctx, name, dry_run, non_interactive, expert))


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


_ACTIONS: list[tuple[str, tuple[str, str], str]] = [
    ("y", ("y", "yes"), "Confirm the prompt"),
    ("n", ("n", "no"),
     "Reject the prompt; a rejected rename gets a fresh suggestion, anything else is left out of this migration"),
    ("c", ("c", "confirmed"), "List already confirmed SQL statements for the current migration"),
    ("b", ("b", "back"), "Go back a step by reverting the latest accepted statement(s)"),
    ("s", ("s", "stop"), "Stop and finalize the migration with only the currently accepted changes"),
    ("q", ("q", "quit"), "Quit without saving any changes"),
]

_ACTIONS_EXPERT: list[tuple[str, tuple[str, str], str]] = [
    ("y", ("y", "yes"), 'Confirm the prompt ("l" to see the DDL statement(s))'),
    ("n", ("n", "no"),
     "Reject the prompt; a rejected rename gets a fresh suggestion, anything else is left out of this migration"),
    ("l", ("l", "list"), "List the DDL statement(s) for this step"),
    ("c", ("c", "confirmed"), "List already confirmed SQL statements for the current migration"),
    ("b", ("b", "back"), "Go back a step by reverting the latest accepted statement(s)"),
    ("s", ("s", "stop"), "Stop and finalize the migration with only the currently accepted changes"),
    ("q", ("q", "quit"), "Quit without saving any changes"),
]


def _print_ddl(ddl: list[str]) -> None:
    for sql in ddl:
        for line in sql.splitlines():
            click.echo(f"    {line}")


def _ask_action(
    prompt_text: str,
    ddl: list[str],
    confirmed_so_far: list[str],
    expert: bool,
    python_snippet: str | None = None,
) -> str:
    """Render one step (prompt, optional DDL + schema preview, action menu)
    and return a validated action key: y, n, b, s, or q. "l" and "c" are
    handled here — they print and re-loop rather than returning.
    """
    choices = _ACTIONS_EXPERT if expert else _ACTIONS
    hint = ",".join(key for key, _, _ in choices) + ",?"

    click.echo(f"\n{prompt_text}")
    if not expert:
        click.echo()
        if python_snippet is not None:
            # The Python declaration reads far better than PyQL's compiled
            # SQL (deeply nested CTEs for anything non-trivial) — show only
            # one, not both. Raw DDL is still one "l" away for anyone who
            # wants to double check exactly what will run.
            click.echo("If so, the following schema declaration will apply:")
            click.echo()
            for line in python_snippet.splitlines():
                click.echo(f"    {line}")
        else:
            click.echo("If so, the following DDL statement(s) will be applied:")
            click.echo()
            _print_ddl(ddl)
        click.echo()
        click.echo("Select an action:")
        click.echo()
        for _, aliases, help_text in choices:
            click.echo(f'"{aliases[1]}" (or "{aliases[0]}"): {help_text}')
        click.echo()

    question = "Which action do you want to take?" if not expert else ""

    while True:
        raw = click.prompt(question, prompt_suffix=f" [{hint}] ").strip().lower()
        matched = next((key for key, aliases, _ in choices if raw in aliases), None)
        if matched == "l":
            click.echo("The following DDL statement(s) will be applied:")
            _print_ddl(ddl)
        elif matched == "c":
            if confirmed_so_far:
                click.echo("Confirmed so far:")
                _print_ddl(confirmed_so_far)
            else:
                click.echo("  (nothing confirmed yet)")
        elif matched is not None:
            return matched
        elif raw in ("h", "?"):
            for _, aliases, help_text in choices:
                click.echo(f'"{aliases[1]}" (or "{aliases[0]}"): {help_text}')
        else:
            click.echo(f"  Unknown option. Enter one of: {hint}")


def _rename_ddl(type_renames: list[tuple], col_renames: list[tuple]) -> list[tuple[str, bool]]:
    """Render confirmed rename tuples back to (sql, non_transactional) pairs
    — used both by the rename loop's own "confirmed"/"stop" display and to
    assemble the final migration body alongside the general diff's DDL.
    """
    ops: list[tuple[str, bool]] = []
    for old_mod, old_table, new_mod, new_table in type_renames:
        if old_mod == new_mod:
            ops.append((f'ALTER TABLE "{old_mod}"."{old_table}" RENAME TO "{new_table}";', False))
        else:
            ops.append((f'ALTER TABLE "{old_mod}"."{old_table}" SET SCHEMA "{new_mod}";', False))
            ops.append((f'ALTER TABLE "{new_mod}"."{old_table}" RENAME TO "{new_table}";', False))
    for module, table, old_col, new_col in col_renames:
        ops.append((f'ALTER TABLE "{module}"."{table}" RENAME COLUMN "{old_col}" TO "{new_col}";', False))
    return ops


def _rename_prompt_loop(
    schema,
    db_state,
    expert: bool,
) -> tuple[list[tuple], list[tuple], bool, bool]:
    """Interactively resolve rename candidates one at a time.

    Rejecting a candidate bans it for the rest of this invocation (via
    `Guidance`) and re-diffs, so the next question proposes a different
    explanation (typically drop + create) instead of asking about the same
    pair again — the one place Pylon's diff engine has real ambiguity to
    search over; a rejected non-rename step has no alternative to search
    for and is simply excluded (see `_migration_prompt_loop`).

    Returns (confirmed_type_renames, confirmed_col_renames, quit_requested, stopped).
    """
    from pylon._core import Guidance, detect_type_renames as _detect_type, detect_col_renames as _detect_col

    guidance = Guidance()
    confirmed_type: list[tuple] = []
    confirmed_col: list[tuple] = []
    # Accepted decisions only, in order — "back" undoes the most recent one.
    # A rejection isn't undoable via "back" either, matching how a plain
    # step's "no" isn't reversible (nothing was recorded to undo).
    history: list[tuple[str, tuple]] = []

    while True:
        type_candidates = [
            c for c in _detect_type(schema, db_state, guidance)
            if (c[0], c[1], c[2], c[3]) not in confirmed_type
        ]
        col_candidates = [
            c for c in _detect_col(schema, db_state, guidance)
            if (c[0], c[1], c[2], c[3]) not in confirmed_col
        ]
        if not type_candidates and not col_candidates:
            return confirmed_type, confirmed_col, False, False

        if type_candidates:
            old_mod, old_table, new_mod, new_table, new_type_name, _confidence = type_candidates[0]
            if old_mod == new_mod:
                ddl = [f'ALTER TABLE "{old_mod}"."{old_table}" RENAME TO "{new_table}";']
            else:
                ddl = [
                    f'ALTER TABLE "{old_mod}"."{old_table}" SET SCHEMA "{new_mod}";',
                    f'ALTER TABLE "{new_mod}"."{old_table}" RENAME TO "{new_table}";',
                ]
            prompt = f"did you rename object type '{old_mod}::{old_table}' to '{new_type_name}'?"
            kind, data = "type", (old_mod, old_table, new_mod, new_table)
        else:
            module, table, old_col, new_col, _pg_type = col_candidates[0]
            ddl = [f'ALTER TABLE "{module}"."{table}" RENAME COLUMN "{old_col}" TO "{new_col}";']
            prompt = f"did you rename property '{old_col}' of object type '{module}::{table}' to '{new_col}'?"
            kind, data = "col", (module, table, old_col, new_col)

        confirmed_so_far = [sql for sql, _ in _rename_ddl(confirmed_type, confirmed_col)]
        action = _ask_action(prompt, ddl, confirmed_so_far, expert)

        if action == "y":
            (confirmed_type if kind == "type" else confirmed_col).append(data)
            history.append((kind, data))
        elif action == "n":
            if kind == "type":
                guidance.ban_type_rename(*data)
            else:
                guidance.ban_col_rename(*data)
        elif action == "b":
            if not history:
                click.echo("  Already at the first question.")
                continue
            prev_kind, prev_data = history.pop()
            (confirmed_type if prev_kind == "type" else confirmed_col).remove(prev_data)
        elif action == "s":
            return confirmed_type, confirmed_col, False, True
        elif action == "q":
            return confirmed_type, confirmed_col, True, False


def _resolve_required_input(step, schema) -> dict[str, str]:
    """Prompt for each of `step.required_input`'s expressions, reusing
    `_fill_prompt_loop`'s "PyQL expression, compiled via `compile_fill_expr`"
    UX. An empty response accepts that input's own default expression as-is
    (it's already valid SQL, not a PyQL string, so it needs no compiling).
    """
    from pylon._core import compile_fill_expr

    overrides: dict[str, str] = {}
    for placeholder, prompt_text, default_expr, type_name in step.required_input:
        click.echo(f"\n{prompt_text}.")
        click.echo("If left blank, the migration will use the default expression:")
        click.echo()
        click.echo(f"    {default_expr}")
        click.echo()
        while True:
            expr_str = click.prompt(f"PyQL expression {placeholder!r}", prompt_suffix="> ").strip()
            if not expr_str:
                overrides[placeholder] = default_expr
                break
            try:
                overrides[placeholder] = compile_fill_expr(type_name, expr_str, schema)
            except Exception as exc:
                click.echo(f"  Error: {exc}")
                continue
            break
    return overrides


def _reorder_for_presentation(steps: list, schema) -> list[int]:
    """Returns a permutation of `range(len(steps))` for the order steps are
    *asked about* — an interface's "view" step moves to right before the
    first of its implementors' "table" steps, since a user thinks of "did
    you create the Account concept" as preceding "did you create
    Individual", even though the interface's own DDL (a view selecting from
    its implementors) still has to be *assembled* afterward. Callers must
    keep using `steps`' original order — not this one — when assembling the
    final migration body; only the walk order changes.
    """
    first_table_step_for_interface: dict[str, int] = {}
    for i, step in enumerate(steps):
        if step.kind != "table":
            continue
        for iface in step.implements(schema):
            first_table_step_for_interface.setdefault(iface, i)

    view_insert_before: dict[int, int] = {}
    for i, step in enumerate(steps):
        if step.kind != "view":
            continue
        qname = step.qualified_name(schema)
        if qname in first_table_step_for_interface:
            view_insert_before[i] = first_table_step_for_interface[qname]

    if not view_insert_before:
        return list(range(len(steps)))

    order: list[int] = []
    for i in range(len(steps)):
        if i in view_insert_before:
            continue  # placed just before its target index below instead
        for view_i, target_i in view_insert_before.items():
            if target_i == i:
                order.append(view_i)
        order.append(i)
    return order


def _migration_prompt_loop(steps: list, schema, expert: bool) -> tuple[list[tuple[str, bool]], bool]:
    """Interactively walk each general create/alter/drop step one at a time.

    Returns (confirmed, quit_requested). `confirmed` is a list of
    (sql, non_transactional) tuples, in `steps`' original (dependency-safe)
    order — the same shape `_assemble_migration_body` already expects —
    with any `required_input` placeholders already resolved (see
    `_resolve_required_input`). Questions themselves are asked in
    `_reorder_for_presentation`'s order, which may differ.
    """
    order = _reorder_for_presentation(steps, schema)
    decisions: list[str | None] = [None] * len(steps)
    resolved: dict[int, list[tuple[str, bool]]] = {}
    pos = 0

    while pos < len(order):
        idx = order[pos]
        step = steps[idx]
        ddl = [sql for sql, _ in step.ddl]
        confirmed_so_far = [
            sql for i in range(len(steps)) if decisions[i] == "y" for sql, _ in resolved[i]
        ]

        action = _ask_action(step.prompt, ddl, confirmed_so_far, expert, step.python_snippet(schema))

        if action == "y":
            overrides = _resolve_required_input(step, schema) if step.required_input else {}
            resolved[idx] = step.resolved_ddl(overrides)
            decisions[idx] = "y"
            pos += 1
        elif action == "n":
            decisions[idx] = "n"
            pos += 1
        elif action == "b":
            if pos == 0:
                click.echo("  Already at the first question.")
                continue
            pos -= 1
            decisions[order[pos]] = None
            resolved.pop(order[pos], None)
        elif action == "s":
            break
        elif action == "q":
            return [], True

    confirmed: list[tuple[str, bool]] = []
    for i in range(len(steps)):
        if decisions[i] == "y":
            confirmed.extend(resolved[i])
    return confirmed, False


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
    expert: bool,
) -> None:
    import sys
    from pylon._core import (
        diff_schema_steps_with_renames_and_fills as _core_diff_schema_steps_with_renames_and_fills,
        missing_extension_ddl as _core_missing_extension_ddl,
        detect_fill_required as _core_detect_fill_required,
        schema_content_changed as _core_schema_content_changed,
        render_migration_file,
        compute_migration_short_id,
        db_state_from_json,
        SchemaDescriptor,
        pgcon_connect,
        introspect_db_state,
        migration_ensure_tracking_tables,
        migration_read_tracking,
        migration_applied_tip,
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

    pool = await pgcon_connect(_pg_dsn(config), 2)
    await migration_ensure_tracking_tables(pool)
    tracking = await migration_read_tracking(pool)  # [(id, onto, db_state, schema_state, applied), ...]
    applied_tip = migration_applied_tip(tracking)
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
    tip_row = next((r for r in tracking if r[0] == chain_tip), None)
    db_state_json = tip_row[2] if tip_row else None
    if db_state_json is not None:
        db_state = db_state_from_json(db_state_json)
    else:
        db_state = await introspect_db_state(pool)

    # Schema semantics with zero physical DDL footprint (readonly, rewrites,
    # channels, ...) never show up in the DDL diff above no matter what, so
    # they need their own content comparison against the same tip-row
    # baseline (not against live `_pylon."Schema"`, for the same reason
    # db_state above isn't live-introspected: watch may have already pushed
    # ad hoc changes straight to the database without ever going through
    # `migration create`, and those must still show up as pending here).
    schema_state_json = tip_row[3] if tip_row else None
    previous_schema = SchemaDescriptor.from_json(schema_state_json) if schema_state_json is not None else None
    content_changed = _core_schema_content_changed(schema, previous_schema)

    confirmed_type_renames: list[tuple[str, str, str, str]] = []
    confirmed_col_renames: list[tuple[str, str, str, str]] = []
    is_interactive = not non_interactive and sys.stdout.isatty()
    stop_early = False

    if is_interactive:
        if not expert:
            click.echo("Running in interactive mode.")
            click.echo("HINT: Use `--expert` to run with less detailed prompts, or `--non-interactive`")
            click.echo("      to attempt to apply migrations without user input.")

        # ── Rename detection ───────────────────────────────────────────────
        confirmed_type_renames, confirmed_col_renames, quit_requested, stop_early = (
            _rename_prompt_loop(schema, db_state, expert)
        )
        if quit_requested:
            raise click.ClickException("Aborted.")

    if stop_early:
        # "stop" during rename prompts: finalize with only what's been
        # confirmed so far, skipping fill detection and the general diff
        # entirely — same semantics as "stop" anywhere else in the flow.
        ops = _rename_ddl(confirmed_type_renames, confirmed_col_renames)
    else:
        # ── Fill expression detection ──────────────────────────────────────
        fill_candidates = _core_detect_fill_required(schema, db_state)
        fills = _fill_prompt_loop(fill_candidates, is_interactive, schema)

        steps = _core_diff_schema_steps_with_renames_and_fills(
            schema, db_state, confirmed_type_renames, confirmed_col_renames, fills
        )

        if is_interactive:
            confirmed_ddl, quit_requested = _migration_prompt_loop(steps, schema, expert)
            if quit_requested:
                raise click.ClickException("Aborted.")
            ops = _rename_ddl(confirmed_type_renames, confirmed_col_renames) + confirmed_ddl
        else:
            # Non-interactive: auto-accept every step (any required_input
            # placeholder resolves to its own default expression).
            ops = _rename_ddl(confirmed_type_renames, confirmed_col_renames)
            for step in steps:
                ops.extend(step.resolved_ddl({}))

    # A required Postgres extension (e.g. pgvector) isn't a design decision
    # to confirm/reject — it's a hard prerequisite the rest of the DDL can't
    # succeed without — so it's prepended unconditionally rather than routed
    # through the per-step confirmation flow.
    ext_ddl = _core_missing_extension_ddl(schema, db_state)
    if ext_ddl:
        ops = [(sql, False) for sql in ext_ddl] + ops

    if not ops:
        if content_changed:
            # A schema change with zero physical DDL footprint (readonly,
            # rewrites, channels, ...) — there's nothing to diff into a DDL
            # step, but it still needs a migration file, or it can never be
            # recorded/applied at all. This isn't a design decision to
            # confirm/reject (same reasoning as ext_ddl above), so it's
            # included unconditionally rather than routed through a prompt.
            # The body carries a content fingerprint so its hash — and
            # therefore this migration's ID — is unique to the actual
            # change, not a fixed empty string every such migration would
            # otherwise collide on.
            ops = [(_non_ddl_change_marker_sql(schema), False)]
            click.echo("Non-DDL schema change detected (e.g. readonly/rewrite/channel) — recording a migration with no DDL to sync it.")
        else:
            click.echo("No schema changes detected.")
            return

    # Non-interactive mode never got a per-step preview — show one summary
    # before writing, one line per grouped step (not per raw DDL statement,
    # matching the readability the interactive loop already has). Interactive
    # mode already confirmed everything step by step, so there's nothing
    # left to re-display.
    if not is_interactive and not stop_early and steps:
        click.echo(f"\n{len(steps)} change(s):")
        for step in steps:
            snippet = step.python_snippet(schema)
            if snippet is not None:
                click.echo()
                for line in snippet.splitlines():
                    click.echo(f"  {line}")
            else:
                # No snippet for this step kind yet (e.g. modules) — fall
                # back to the question text.
                click.echo(f"  {step.prompt}")
        if any(nt for _, nt in ops):
            click.echo("\n  Note: some changes use CONCURRENTLY statements that run outside a transaction wrapper.")

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


def _non_ddl_change_marker_sql(schema) -> str:
    """A SQL-comment-only op standing in for a migration with no DDL.

    Used when `schema_content_changed` finds a real change but the DDL diff
    is empty (readonly/rewrites/channels/... — schema semantics with no
    physical Postgres footprint). A migration file's ID is the hash of its
    body, so a literal empty body would make every such migration collide
    on the same ID; embedding a fingerprint of the resulting schema content
    keeps each one's ID (and rendered file) unique to its actual change,
    while still executing as a harmless no-op when applied.
    """
    import hashlib
    content_hash = hashlib.sha256(schema.to_json().encode()).hexdigest()[:16]
    return (
        "-- pylon: non-DDL schema change (e.g. readonly/rewrite/channel) — nothing to run.\n"
        f"-- content fingerprint: {content_hash}\n"
        "-- Applying this migration re-syncs the stored schema snapshot to match."
    )


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
              shell_complete=_complete_migration_id,
              help="First migration in the range to squash (inclusive).")
@click.option("--to", "to_id", default=None, metavar="ID",
              shell_complete=_complete_migration_id,
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
    from pylon._core import (
        diff_states as _core_diff_states,
        render_migration_file,
        compute_migration_short_id,
        pgcon_connect,
        migration_ensure_tracking_tables,
        introspect_db_state,
    )
    from pylon.exceptions import QueryError

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

    admin_pool = await pgcon_connect(dsn, 2)
    try:
        await admin_pool.execute(f'CREATE DATABASE "{shadow_name}" TEMPLATE template0', [])
    except QueryError as exc:
        if getattr(exc, "sqlstate", None) == "42501":  # insufficient_privilege
            raise click.ClickException(
                "Squash requires CREATEDB privilege on the PostgreSQL server."
            ) from exc
        raise

    before_state = after_state = None
    try:
        shadow_dsn = _shadow_dsn(dsn, shadow_name)
        shadow_pool = await pgcon_connect(shadow_dsn, 2)
        await shadow_pool.execute("CREATE SCHEMA IF NOT EXISTS _pylon", [])
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
        await admin_pool.execute(f'DROP DATABASE IF EXISTS "{shadow_name}"', [])
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
