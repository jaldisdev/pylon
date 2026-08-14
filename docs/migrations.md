# Migrations

Pylon's schema is Python code — but Python code has no effect on the database until it's turned into a migration and applied. `pylon migration create` diffs your compiled schema against the live database and writes a migration file; `pylon migration apply` runs it. This page covers the model behind that workflow; see [`cli.md`](cli.md#pylon-migration) for the bare flag reference.

## Migration files

Each migration is a plain `.sql` file under `<schema-dir>/migrations/`, named `<seq>_<short-id>[_<name>].sql` (e.g. `00003_m1a3f9bc_add_author_bio.sql`), with a two-line header:

```sql
-- migration: m1<38 hex chars>
-- onto: m1<38 hex chars> | initial

CREATE TABLE ...;
```

The **ID** is `m1` followed by the first 38 hex characters of `SHA-256(body)` — a content hash, not a random UUID, so two people independently writing the same net change get the same ID. **`onto`** names the parent migration's ID (or the literal `initial` for the first migration in a chain), so the on-disk files form a linked list regardless of filename ordering — the filename's numeric prefix is just for human readability and gets renumbered by `squash`.

A migration body with more than one statement can contain `-- pylon:step` markers, splitting it into separately-committed groups — see [Transaction boundaries](#transaction-boundaries-and-concurrently) below.

## The tracking table

Postgres itself remembers which migrations have been applied, in `_pylon."Migrations"`. Every `pylon migration apply` run:

0. Brings the internal `_pylon` schema up to date — creating the tracking tables on a database that has never seen Pylon, and adding anything a newer Pylon version introduced on one that has. Every migration command does this first, so an upgrade reaches the internal structures without a separate step.

1. Takes a session-level Postgres advisory lock, so concurrent `apply` invocations serialize instead of racing (`--no-wait` fails fast instead of blocking if another one already holds it).
2. Reads the tracking table's recorded tip and compares it against the on-disk chain. If the recorded tip isn't found in the chain at all, that's treated as **diverged history** and refused outright — Pylon won't guess which side is right.
3. Applies everything between the recorded tip and the chain tip (or `--to ID`), one migration at a time, each in its own transaction (or transaction-per-step, for a file with `-- pylon:step` markers).
4. Records a schema snapshot on the newly-applied tip row — this is what `pylon migration create`'s next run diffs against as its baseline, not a fresh live-database introspection, so it stays correct even if `watch` has touched the database since.

## Creating a migration

```bash
pylon migration create
```

Every unapplied migration must already be applied before you can create a new one — `create` needs the live database to actually be at the chain tip to diff against, otherwise the diff would be computed against a moving target.

The diff engine compares your compiled schema against the database's introspected structure and produces a sequence of steps: create/alter/drop table, add/drop/alter column, create/drop index, install/replace function, and so on. Two categories need special handling beyond a plain accept/reject:

### Rename detection

A dropped `Product` type plus a newly-added, structurally-similar `Item` type is genuinely ambiguous — was `Product` renamed to `Item`, or was `Product` deleted and `Item` created independently? The same ambiguity applies at the property level. Rename candidates are resolved **first**, interactively, one at a time:

```
did you rename object type 'shop::Product' to 'shop::Item'?
"yes" (or "y"): Confirm the prompt
"no" (or "n"): Reject the prompt; a rejected rename gets a fresh suggestion, ...
```

Rejecting a candidate doesn't just skip it — it's banned for the rest of this `create` run, and the diff engine searches again with that pairing excluded, typically converging on a plain drop+create instead. This is the one place in the whole flow with real ambiguity to search over; every other kind of step has no alternative interpretation, so a rejection there just excludes it.

### Fill expressions

A column being made `NOT NULL` needs a value for any row that doesn't already have one. If the property has a schema-declared `Default(...)`, that's used automatically. Otherwise, interactively, you're prompted for a one-off PyQL expression:

```
score (int8, existing column)
Table: "shop"."product"
fill_expr> 0
```

Non-interactively, a column needing a fill expression with no declared default fails the command outright rather than guessing.

### Everything else

Every other detected change is presented as its own accept/reject step, in dependency-safe order, showing the Python schema declaration that will apply (not raw DDL, by default — press `l` in `--expert` mode, or it's shown automatically outside expert mode, to see the actual SQL). `s` finalizes early with only what's been confirmed so far; `b` undoes the last accepted step and re-asks it; `q` aborts with nothing written.

Non-interactively (`--non-interactive`, or automatically whenever stdout isn't a TTY — e.g. in CI), every step is auto-accepted and every fill expression falls back to its schema-declared default, with a plain summary printed instead of a per-step prompt.

A required Postgres extension your schema now depends on (e.g. `pgvector`) is never asked about — it's a hard prerequisite, prepended to the migration unconditionally.

## Applying migrations

```bash
pylon migration apply
```

Runs every pending migration in chain order. `--to ID` stops early; `--dev-mode` treats DDL that `pylon migration watch` already pushed live as already-satisfied, only advancing the tracking pointer rather than re-running it.

### Transaction boundaries and `CONCURRENTLY`

Most migration steps are wrapped in one transaction per migration file — but `CREATE INDEX CONCURRENTLY` and similar statements can't run inside a transaction block at all. When a migration needs one of these, the diff engine emits a `-- pylon:step non-transactional` marker splitting the file into separately-applied groups: the transactional ones still get all-or-nothing semantics, the non-transactional group runs on its own outside any wrapper.

## Everyday commands

```bash
pylon migration status              # applied tip, pending count, chain validity
pylon migration status --dev-mode   # also reports "watch drift" — see below
pylon migration log --from-fs       # on-disk chain, oldest first
pylon migration log --from-db --newest-first --limit 10
```

## Local iteration: `watch`

```bash
pylon migration watch
```

Watches schema `.py` files for changes and applies the diff to the live database **immediately, with no migration file written** — the fastest inner loop for iterating on a schema locally. Run `pylon migration create` once you're happy with where the schema landed, to capture the accumulated changes as a real, reviewable migration file.

`pylon migration status --dev-mode` reports when the live database has diverged from the last recorded migration this way ("watch drift") — a nudge that you have uncommitted schema changes sitting live in the database, not yet captured as a file.

## Hand-editing: `--blank` and `rehash`

```bash
pylon migration create --blank
# ... edit the generated stub by hand ...
pylon migration rehash dbschema/migrations/00004_m1xxxxxx.sql
```

`--blank` skips diffing entirely and writes an empty, hand-editable stub — for a migration whose DDL you want to write yourself (e.g. a data-only migration with no schema change at all). Since the file's ID is a content hash of its body, editing it by hand invalidates the header — `rehash` recomputes the ID and renames the file to match. Only valid for the current chain tip, and only before that migration has been applied anywhere (once applied, its ID is a commitment other databases may already be keying off of).

## Collapsing history: `squash`

```bash
pylon migration squash --count 12
# or
pylon migration squash --from m1aaaa... --to m1bbbb...
```

Long-lived projects accumulate migrations faster than anyone wants to read through. `squash` collapses a contiguous range into one file containing only the **net** DDL — not the concatenation of the originals, the actual before/after diff. It computes this safely by spinning up a real, ephemeral shadow database (requires `CREATEDB` privilege): applying every migration before the range to reach the "before" state, applying the range itself to reach "after," diffing the two, then dropping the shadow database.

The squashed file's header records every migration ID it replaced. This matters for compatibility: a database that already applied the old, unsquashed chain is recognized as already up to date (backfilled silently) the next time `pylon migration apply` runs against it — it doesn't try to reapply DDL that's already there under the old file names.

## First-time setup

Against a fresh, empty database:

```bash
pylon migration create      # first migration, diffing against an empty db
pylon migration apply
```

No separate install step: both commands set up the `_pylon` schema themselves
before doing anything else. [`pylon database initialize`](cli.md#pylon-database-initialize)
does the same thing on its own, for provisioning a database ahead of the code
that will use it.

See [`pylon database`](cli.md#pylon-database) for `dump`/`restore`/`wipe`, and [Getting started](getting-started.md) for the full walkthrough from an empty project.
