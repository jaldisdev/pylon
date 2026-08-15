# CLI reference

The `pylon` command is installed as a console script (`pylon.cli:main`). Every subcommand except `pylon version` and `pylon completion` requires a `pylon.toml` — the CLI walks up from the current directory to find one (see [`config.md`](config.md)).

## Global options

```
pylon [-d/--database NAME] [COMMAND]
```

| Flag | Description |
|---|---|
| `-d`, `--database NAME` | Use a named connection from `[database.NAME]` in `pylon.toml` instead of the base `[database]` block. Shell-completes against your configured connection names. |

Running `pylon` with **no subcommand** starts an interactive PyQL session (the REPL) — equivalent to typing a query straight after connecting, no separate `pylon repl` command needed.

## `pylon` (REPL)

```bash
pylon
```

Calls `pylon.finalize()`, connects via `create_async_client()`, and drops you into a multiline prompt. A statement runs when the buffer ends with `;` (or starts with `\`) and you press Enter — otherwise Enter just inserts a newline, so multi-line queries are natural to type.

Special commands (backslash-prefixed, no trailing `;`):

| Command | Effect |
|---|---|
| `\help` | Show in-session help. |
| `\quit` | Exit (same as Ctrl-D). |
| `set global name := expression;` | Evaluate `expression` (via `select expression`) and bind it as a session global for the rest of the REPL session — resolves `name` against a declared [`Global`](schema/globals-and-aliases.md) if one matches, otherwise treats it as already-qualified or defaults to the `default` module. |

Query history persists per-project at `~/.pylon/history/<project-name>` (or `default` if the project has no `[project] name`).

## `pylon query`

```bash
pylon query "select Person { name, age };"
```

One-shot query execution — compiles, runs, prints the result, and exits. No REPL session, no history.

| Flag | Description |
|---|---|
| `--json` | Print results as JSON instead of the REPL's table-ish display. |

## `pylon migration`

Migration file lifecycle. See [Migrations](migrations.md) for the full workflow this drives — this section is the flag reference. `pylon migrate` is a shortcut for `pylon migration apply`.

### `pylon migration create`

Diffs the compiled schema against the live database (or writes a blank stub) and writes a new file under `<schema-dir>/migrations/`.

| Flag | Description |
|---|---|
| `--blank` | Skip diffing entirely; write a hand-editable stub. Run `pylon migration rehash` after editing it. |
| `--name SLUG` | Append a label to the generated filename. |
| `--dry-run` | Print the file content instead of writing it. |
| `--non-interactive` | Skip the confirmation prompts; auto-accepts every step (also the default automatically when stdout isn't a TTY). Any column that needs an explicit fill expression and has no schema-declared default fails the command instead of prompting. |
| `--expert` | Terser prompts — hides the DDL/Python-declaration preview by default; press `l` during a prompt to reveal it anyway. |

When interactive (a TTY, and `--non-interactive` not given), each detected change is confirmed one at a time. Rename detection runs first (object-type renames, then property renames) since it's the one place the diff is genuinely ambiguous — everything else is presented as an accept/reject step. At every prompt:

| Key | Action |
|---|---|
| `y` / `yes` | Confirm this step. |
| `n` / `no` | Reject it. A rejected rename gets a fresh alternative suggestion (typically drop+create); a rejected ordinary step is simply left out. |
| `l` / `list` | (expert mode) Show the DDL for this step. |
| `c` / `confirmed` | List every step already confirmed so far in this run. |
| `b` / `back` | Undo the most recently accepted step and re-ask it. |
| `s` / `stop` | Finalize the migration now, keeping only what's been confirmed. |
| `q` / `quit` | Abort without writing anything. |

A column being made `NOT NULL` that has no schema-declared default and no existing rows already satisfying it prompts for a one-off PyQL fill expression (e.g. `'unknown'`, `0`, `.other_field`) to backfill existing rows.

### `pylon migration apply`

Applies every migration between the database's recorded tip and the on-disk chain tip (or `--to`), under a session advisory lock so concurrent `apply` runs serialize instead of racing.

| Flag | Description |
|---|---|
| `--to ID` | Stop after applying this migration ID (shell-completes against on-disk IDs). |
| `--dev-mode` | Skip DDL already applied live via `pylon migration watch`; just advance the tracking pointer to match. |
| `--no-wait` | Fail immediately instead of blocking if another `apply` already holds the lock. |

### `pylon migration status`

Prints the on-disk chain tip, the database's recorded applied tip, and the list of pending migrations (or a chain-divergence warning if the recorded tip isn't in the on-disk chain at all).

| Flag | Description |
|---|---|
| `--dev-mode` | Also report "watch drift" — schema changes visible live in the database (via `pylon migration watch`) that haven't yet been captured as an actual migration file. |

### `pylon migration log`

Prints migration history, oldest-first by default.

| Flag | Description |
|---|---|
| `--from-fs` | Read from the on-disk chain (mutually exclusive with `--from-db`; one of the two is required). |
| `--from-db` | Read from the database's tracking table instead — shows what's actually been applied and when. |
| `--newest-first` | Reverse the order. |
| `--limit N` | Cap the number of entries shown. |

### `pylon migration watch`

```bash
pylon migration watch
```

No flags. Watches every `.py` file under `schema-dir` for changes; on each change, recompiles the schema, diffs it against the live database, and applies the resulting DDL immediately — no migration file is written. Meant for local development iteration; run `pylon migration create` afterward to capture the accumulated changes as a real, reviewable migration. A required Postgres extension (e.g. `pgvector`) is enabled automatically as a prerequisite, not asked about.

### `pylon migration rehash FILE`

Recomputes a hand-edited migration's content hash (its ID) and rewrites the file's header line and filename to match. Only valid for the current chain tip, and only before it's been applied anywhere — the ID is a commitment other databases key off of once applied.

### `pylon migration squash`

Collapses a contiguous range of on-disk migrations into one file, computing the *net* DDL via an ephemeral shadow database (requires `CREATEDB` privilege on the target Postgres server — the shadow database is created, the pre-range and in-range migrations are applied to it in turn to compute a before/after diff, then it's dropped).

| Flag | Description |
|---|---|
| `--from ID` | First migration in the range (inclusive); pairs with `--to`. |
| `--to ID` | Last migration in the range (inclusive); pairs with `--from`. |
| `--count N` | Squash the last N migrations instead of an explicit range. Mutually exclusive with `--from`/`--to`. Must be at least 2. |
| `--dry-run` | Print the squashed body without touching any files. |

The squashed file's header records every constituent migration's original ID, so a database that already applied the old, unsquashed chain is still recognized as up to date (backfilled, not reapplied) the next time `pylon migration apply` runs against it.

## `pylon database`

Whole-database operations.

### `pylon database initialize`

```bash
pylon database initialize
```

Installs the internal `_pylon` schema — the migration tracking tables, the
index and signal outboxes, and every standard-library function — and brings an
existing one up to date. Idempotent, safe to re-run.

**Not a required step.** Every migration command (`create`, `apply`, `status`,
`watch`, `squash`) does the same thing before it runs, so an ordinary project
never needs this and an upgrade picks up new internal structures on the next
migration. Reach for it when you want the database prepared as its own step —
provisioning in CI or a container entrypoint, ahead of the code that will use
it — or with `--dry-run` to read the DDL Pylon would apply.

| Flag | Description |
|---|---|
| `--dry-run` | Print the generated SQL without executing it. |

### `pylon database dump FILE`

Wraps `pg_dump`.

| Flag | Description |
|---|---|
| `--format {custom,plain}` | `custom` (the default) produces a `pg_restore`-only archive; `plain` produces a plain `.sql` file. |

### `pylon database restore FILE`

Wraps `pg_restore` (for a `custom`-format dump) or `psql` (for a `.sql` file) — selected automatically from the file extension.

### `pylon database wipe`

Drops every user-defined module (as a Postgres schema, `CASCADE`) and clears the migration tracking table. **Does not** drop the database itself.

| Flag | Description |
|---|---|
| `--force` | Skip the interactive confirmation prompt. |

## `pylon cache`

Inspects/manages the on-disk LMDB query-result cache (see [`config.md`](config.md#cache) for `[cache]` settings). Opens its own handle at `[cache].path` regardless of whether `[cache].enabled` is set — inspection works even with caching toggled off.

### `pylon cache status`

Prints the cache path, whether it's enabled, entry count, and bytes used.

### `pylon cache purge`

Evicts every entry.

| Flag | Description |
|---|---|
| `--yes` | Skip the confirmation prompt. |

## `pylon worker`

### `pylon worker start`

```bash
pylon worker start
```

Starts the two background workers that have to run in a Python process, and runs until interrupted: the [signal dispatcher](schema/signals.md), because a `@pylon.signal` handler is a live Python callable that exists nowhere else, and cache invalidation, because it evicts from an LMDB file on local disk and so has to reach the cache a nearby process actually reads.

Vector and search indexing are not here. They claim outbox rows the database arbitrates, so they can run anywhere, and they run in [`pylon-server`](server.md) — including under `--no-http`, for a worker-only container. If your schema declares a `VectorIndex` or `SearchIndex`, this command logs a line at startup saying so, since nothing it starts will drain those rows.

| Flag | Description |
|---|---|
| `--batch-size N` | SignalOutbox rows claimed per polling cycle (default 50). |
| `--poll-interval SECONDS` | Seconds between polls when idle (default 30). |
| `--disable-cache-worker` | Skip the cache-invalidation worker. |
| `--disable-signal-dispatcher` | Skip the signal dispatcher. |
| `--log-level {DEBUG,INFO,WARNING,ERROR}` | Logging verbosity (default `INFO`). |

The two `--disable-*` flags split even this pair across processes. The reason to disable one is that some *other* process is covering it, and for the cache-invalidation worker that is a stronger claim than it looks: it evicts from the LMDB environment it opens at `[cache].path`, and the cache has no expiry, so it only keeps a cache correct if it can reach that exact directory. A worker on another machine or in another container, pointed at its own empty copy of the path, evicts nothing anyone reads while every reader of the real cache keeps serving the write it never saw. Either share the directory (a second process or container on the same volume) or run the worker inside the application process ([`pylon.workers`](client/python.md#running-workers-in-process)).

The signal dispatcher has no such constraint — one anywhere in the deployment is enough. So the usual split is a cache invalidator per application and a dispatcher once, centrally:

```bash
pylon worker start --disable-signal-dispatcher   # beside each application, sharing its cache directory
pylon worker start --disable-cache-worker        # once, anywhere
```

Exiting with nothing to do is an error, so a flag combination that disables everything fails loudly instead of idling.

## `pylon info`

```bash
pylon info
```

No flags. Prints the resolved Python interpreter, the located `pylon.toml` path, the configured schema directory, the PyQL version pin (if set), and the target database connection (password masked) — a quick sanity check for "what is this CLI actually pointed at right now."

## `pylon version`

```bash
pylon version
```

Prints the installed `pylon` package version. Works without a `pylon.toml`.

## `pylon completion [SHELL]`

```bash
pylon completion bash    # or zsh / fish
```

Prints a shell completion script for `bash`, `zsh`, or `fish`. Run with no argument for install-instruction text:

```bash
eval "$(pylon completion bash)"    >> ~/.bashrc
eval "$(pylon completion zsh)"     >> ~/.zshrc
pylon completion fish | source     >> ~/.config/fish/config.fish
```
