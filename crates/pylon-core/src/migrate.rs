//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

//! Migration execution: advisory locking, tracking tables, resumable
//! per-step progress, and dev-mode savepoint retry. A direct Rust port of
//! `pylon.cli.commands.migrations`'s `_apply_one` (plus the tracking-table
//! helpers `_ensure_tracking_tables`/`_read_tracking`/`_applied_tip`/
//! `_record_applied` it and `_apply`'s outer loop share). The outer
//! `apply` loop itself (chain resolution, `--to` targeting, the squash-
//! backfill special case, `click.echo` progress output) stays in Python —
//! this module is the part that's actually "migration execution."

use crate::migration::{MigrationFile, parse_steps, verify_integrity};
use pylon_pgcon::{PgPool, PgTransaction};
use pylon_value::DecodedValue;

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(transparent)]
    Integrity(#[from] crate::migration::MigrationError),
    #[error(transparent)]
    Db(#[from] pylon_pgcon::Error),
    #[error(
        "migration {id} is already recorded as applied onto {recorded_onto}, but the file \
         being applied claims onto {new_onto} — two different migrations share one ID"
    )]
    IdCollision {
        id: String,
        recorded_onto: String,
        new_onto: String,
    },
}

pub type Result<T> = std::result::Result<T, MigrateError>;

/// Fixed session-level advisory-lock key used by `apply` — must match the
/// value the Python implementation used forever, since it's what makes
/// concurrent `apply` runs (across processes, even across old/new
/// implementations during a rollout) mutually exclusive.
pub const ADVISORY_LOCK_KEY: i64 = 7_461_999;

const DUPLICATE_OBJECT_CODES: [tokio_postgres::error::SqlState; 5] = [
    tokio_postgres::error::SqlState::DUPLICATE_TABLE,
    tokio_postgres::error::SqlState::DUPLICATE_COLUMN,
    tokio_postgres::error::SqlState::DUPLICATE_SCHEMA,
    tokio_postgres::error::SqlState::DUPLICATE_OBJECT,
    tokio_postgres::error::SqlState::DUPLICATE_DATABASE,
];

fn is_duplicate_object_error(err: &pylon_pgcon::Error) -> bool {
    err.sqlstate().is_some_and(|code| DUPLICATE_OBJECT_CODES.contains(code))
}

/// Brings the whole internal `_pylon` schema up to date — tracking tables,
/// the index/signal outboxes, the cache-invalidate trigger function, and the
/// stdlib functions.
///
/// Runs the *entire* `export_stdlib()` blob, not just the migration tracking
/// subset. Those blobs carry their own upgrade statements (`ADD COLUMN IF
/// NOT EXISTS`, `CREATE OR REPLACE FUNCTION`), so which ones a database
/// receives decides which internal changes ever reach it. Applying only the
/// tracking subset here meant `_pylon."Migrations".schema_state` arrived on
/// every database while `_pylon."IndexOutbox".claimed_at` reached only
/// databases that had been re-initialized — and the index workers on the
/// rest failed every claim against a column that was never added.
///
/// Every statement is idempotent, and `batch_execute` runs them in one
/// implicit transaction, so this is safe to call on every migration and
/// cheap enough at that frequency — it is a few dozen statements, not a
/// schema diff.
pub async fn ensure_internal_schema(pool: &PgPool) -> Result<()> {
    pool.batch_execute(&crate::stdlib::export_stdlib()).await?;
    Ok(())
}

/// How a database's internal schema relates to the one this build expects.
///
/// The classification is deliberately separate from what any caller *does*
/// about it: `pylon-server` refuses to start on `TooOld` while a client
/// raises, and both merely note `Behind`, but the rule for which is which
/// belongs in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalSchemaState {
    /// No `_pylon."Internal"` row — a database no migration has ever run
    /// against. Legal, and not a mismatch: callers should behave exactly as
    /// they did before this check existed rather than treat it as an error.
    Unmigrated,
    /// Older than `MIN_SUPPORTED_INTERNAL_VERSION`: this build cannot work
    /// against it. The only fix is `pylon migration apply`.
    TooOld { found: i32, required: i32 },
    /// Behind this build but still within its supported range — an upgrade
    /// is pending and everything works meanwhile.
    Behind { found: i32, current: i32 },
    /// Exactly what this build writes.
    Current,
    /// Written by a *newer* build. Reported, never fatal: this is what a
    /// rollback looks like, and failing closed would turn the recovery
    /// lever into a second outage.
    Newer { found: i32, current: i32 },
}

impl InternalSchemaState {
    /// Whether this build should refuse to proceed.
    pub fn is_fatal(&self) -> bool {
        matches!(self, InternalSchemaState::TooOld { .. })
    }

    /// A one-line explanation, or `None` when there is nothing to say.
    pub fn message(&self) -> Option<String> {
        match self {
            InternalSchemaState::Unmigrated | InternalSchemaState::Current => None,
            InternalSchemaState::TooOld { found, required } => Some(format!(
                "this database's internal schema (version {found}) is older than this \
                 version of Pylon supports (version {required}); run `pylon migration apply` \
                 to bring it up to date"
            )),
            InternalSchemaState::Behind { found, current } => Some(format!(
                "this database's internal schema is at version {found}, this version of \
                 Pylon writes version {current}; `pylon migration apply` will update it"
            )),
            InternalSchemaState::Newer { found, current } => Some(format!(
                "this database's internal schema (version {found}) was written by a newer \
                 version of Pylon than this one (version {current}); continuing, but this \
                 build may not understand everything it finds"
            )),
        }
    }
}

/// Reads `_pylon."Internal".version`, or `None` for a database that has no
/// such table — one no migration has ever run against.
pub async fn read_internal_version(pool: &PgPool) -> Result<Option<i32>> {
    let rows = match pool
        .query_typed(
            r#"SELECT (version) AS result FROM _pylon."Internal" WHERE singleton"#,
            &[],
            pool.types(),
        )
        .await
    {
        Ok(rows) => rows,
        // `42P01 undefined_table` is the never-migrated case, not a failure
        // — same graceful degradation `read_schema_snapshot`'s callers rely
        // on for `_pylon."Schema"`.
        Err(e) if e.sqlstate() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(match rows.into_iter().next() {
        Some(DecodedValue::I64(v)) => Some(v as i32),
        _ => None,
    })
}

/// Classifies this database against what this build expects — see
/// `InternalSchemaState`.
pub async fn check_internal_schema(pool: &PgPool) -> Result<InternalSchemaState> {
    use crate::stdlib::ddl::{INTERNAL_SCHEMA_VERSION, MIN_SUPPORTED_INTERNAL_VERSION};

    Ok(match read_internal_version(pool).await? {
        None => InternalSchemaState::Unmigrated,
        Some(found) if found < MIN_SUPPORTED_INTERNAL_VERSION => InternalSchemaState::TooOld {
            found,
            required: MIN_SUPPORTED_INTERNAL_VERSION,
        },
        Some(found) if found < INTERNAL_SCHEMA_VERSION => InternalSchemaState::Behind {
            found,
            current: INTERNAL_SCHEMA_VERSION,
        },
        Some(found) if found > INTERNAL_SCHEMA_VERSION => InternalSchemaState::Newer {
            found,
            current: INTERNAL_SCHEMA_VERSION,
        },
        Some(_) => InternalSchemaState::Current,
    })
}

/// Upserts the process-wide schema snapshot every client fetches at
/// startup instead of reading `.pylon/schema.json` — a single row (the
/// `singleton` PK/CHECK forces at most one), written both by `migration
/// apply` (the formal path) and `watch` (immediate dev-mode sync), since
/// either is a point where the live database's actual shape just changed.
/// A bare schema-file edit with neither applied has no effect here, by
/// design — clients keep seeing the last-applied/synced shape until one of
/// those two actually run.
pub async fn write_schema_snapshot(pool: &PgPool, snapshot_json: &str) -> Result<()> {
    pool.execute_typed(
        r#"INSERT INTO _pylon."Schema" (singleton, snapshot, updated_at) VALUES (true, $1::jsonb, now())
           ON CONFLICT (singleton) DO UPDATE SET snapshot = $1::jsonb, updated_at = now()"#,
        &[DecodedValue::Str(snapshot_json.to_string())],
    )
    .await?;
    Ok(())
}

/// Reads the current schema snapshot, or `None` if neither `migration
/// apply` nor `watch` has ever run against this database.
pub async fn read_schema_snapshot(pool: &PgPool) -> Result<Option<String>> {
    let rows = pool
        .query_typed(
            r#"SELECT (snapshot::text) AS result FROM _pylon."Schema" WHERE singleton"#,
            &[],
            pool.types(),
        )
        .await?;
    Ok(match rows.into_iter().next() {
        Some(DecodedValue::Str(s)) => Some(s),
        _ => None,
    })
}

/// One row of `_pylon."Migrations"` — covers both `apply`'s own tracking
/// logic (id/onto/applied, for computing the applied tip) and `migration
/// create`'s diff-baseline lookup (db_state and schema_state, the JSON
/// snapshots recorded on the tip row by `apply`). `filename` isn't read —
/// nothing in this codebase actually consults it once a row exists.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackingRow {
    pub id: String,
    pub onto: String,
    /// The `db_state` JSON snapshot (catalog/DDL-visible shape only),
    /// pre-rendered as text (`db_state::text`) rather than decoded as
    /// jsonb — callers (`db_state_from_json`) want the raw JSON string to
    /// re-parse, not an already-decoded value tree.
    pub db_state: Option<String>,
    /// The full `SchemaDescriptor` JSON as of this migration (everything
    /// `db_state` has, plus schema semantics with zero DDL footprint —
    /// `readonly`, rewrites, `Channel`s, ...). `migration create` diffs
    /// against *this* (the tip row's own recorded state), not against
    /// `_pylon."Schema"`, for the same reason `db_state` already does:
    /// `watch` may have pushed ad hoc changes straight to the live database
    /// without ever going through `migration create`, and those must still
    /// show up as a pending change here rather than silently being treated
    /// as already-baselined. Pre-rendered as text for the same reason as
    /// `db_state` — re-parse via `SchemaDescriptor.from_json`.
    pub schema_state: Option<String>,
    pub applied: bool,
}

pub async fn read_tracking(pool: &PgPool) -> Result<Vec<TrackingRow>> {
    let rows = pool
        .query_typed(
            r#"SELECT (id, onto, (db_state::text), (schema_state::text), (applied_at IS NOT NULL)) AS result FROM _pylon."Migrations""#,
            &[],
            pool.types(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let DecodedValue::Composite(fields) = row else {
                return None;
            };
            let [
                DecodedValue::Str(id),
                DecodedValue::Str(onto),
                db_state,
                schema_state,
                DecodedValue::Bool(applied),
            ] = <[DecodedValue; 5]>::try_from(fields).ok()?
            else {
                return None;
            };
            let db_state = match db_state {
                DecodedValue::Str(s) => Some(s),
                _ => None,
            };
            let schema_state = match schema_state {
                DecodedValue::Str(s) => Some(s),
                _ => None,
            };
            Some(TrackingRow {
                id,
                onto,
                db_state,
                schema_state,
                applied,
            })
        })
        .collect())
}

/// Computes the tip ID from applied tracking rows (the one with no
/// descendant) — a pure function, no I/O, mirroring `_applied_tip`.
///
/// A healthy chain has exactly one such row, but a tracking table can end
/// up with several orphaned single-node "tips" (e.g. leftover rows from
/// migration files that were since deleted/regenerated without cleaning up
/// the tracking table) — when that happens, this deterministically returns
/// the lexicographically smallest ID among them, rather than an arbitrary
/// one. The original Python implementation this ports picked from a
/// `set()` difference (`next(iter(tips))`), whose iteration order is
/// randomized per-process by Python's string hash randomization — a real,
/// pre-existing bug (not introduced by this port) that could make
/// `status`/`apply` disagree on which tip is current from one invocation
/// to the next, since each CLI command is a separate process.
pub fn applied_tip(tracking: &[TrackingRow]) -> Option<String> {
    let applied: Vec<&TrackingRow> = tracking.iter().filter(|r| r.applied).collect();
    if applied.is_empty() {
        return None;
    }
    let onto_targets: std::collections::HashSet<&str> = applied.iter().map(|r| r.onto.as_str()).collect();
    applied
        .iter()
        .filter(|r| !onto_targets.contains(r.id.as_str()))
        .map(|r| r.id.as_str())
        .min()
        .map(|s| s.to_string())
}

/// Blocks until the advisory lock is acquired (`pg_advisory_lock`), on a
/// connection the caller then holds for the lock's entire lifetime — an
/// advisory lock is released by an explicit unlock (or the session
/// ending), not by a transaction boundary or by returning to the pool, so
/// acquire and release must run on the *same* connection (see
/// `PgPool::connection`). Pass the returned handle to `advisory_unlock`
/// once `apply` is done; dropping it without unlocking first would leave
/// the lock held until that specific connection eventually closes.
pub async fn advisory_lock(pool: &PgPool) -> Result<pylon_pgcon::PgConnection> {
    let conn = pool.connection().await?;
    conn.batch_execute(&format!("SELECT pg_advisory_lock({ADVISORY_LOCK_KEY})"))
        .await?;
    Ok(conn)
}

/// Attempts to acquire the advisory lock without blocking
/// (`pg_try_advisory_lock`); `None` means another `apply` holds it —
/// same held-connection contract as `advisory_lock`.
pub async fn try_advisory_lock(pool: &PgPool) -> Result<Option<pylon_pgcon::PgConnection>> {
    let conn = pool.connection().await?;
    let rows = conn
        .query_typed(
            &format!("SELECT (pg_try_advisory_lock({ADVISORY_LOCK_KEY})) AS result"),
            &[],
            pool.types(),
        )
        .await?;
    Ok(if matches!(rows.first(), Some(DecodedValue::Bool(true))) {
        Some(conn)
    } else {
        None
    })
}

pub async fn advisory_unlock(conn: pylon_pgcon::PgConnection) -> Result<()> {
    conn.batch_execute(&format!("SELECT pg_advisory_unlock({ADVISORY_LOCK_KEY})"))
        .await?;
    Ok(())
}

/// Records a migration as applied.
///
/// The conflict path updates `onto` and `filename` too, not just
/// `applied_at`: re-applying the same migration rewrites them with identical
/// values (free), while leaving them stale would let the applied-tip walk
/// read an `onto` that doesn't match the migration the ID refers to.
/// `check_no_id_collision` runs first and rejects the case where they would
/// genuinely differ.
const RECORD_APPLIED_SQL: &str = r#"
    INSERT INTO _pylon."Migrations" (id, onto, filename, applied_at)
    VALUES ($1, $2, $3, now())
    ON CONFLICT (id) DO UPDATE
        SET applied_at = now(),
            onto = EXCLUDED.onto,
            filename = EXCLUDED.filename
"#;

/// Rejects recording `id` when a *different* migration is already tracked
/// under it — i.e. one whose parent isn't `onto`.
///
/// Under the `m2` ID format this can't arise from two same-bodied migrations
/// at different chain positions, since `onto` is hashed in. It remains
/// reachable for `m1`-era IDs (body-only hash), where exactly that collision
/// silently overwrote the first migration's tracking row and left the chain
/// walk reading a parent that no longer matched.
async fn check_no_id_collision(pool: &PgPool, id: &str, onto: &str) -> Result<()> {
    let rows = pool
        .query_typed(
            r#"SELECT (onto) AS result FROM _pylon."Migrations" WHERE id = $1"#,
            &[DecodedValue::Str(id.to_string())],
            pool.types(),
        )
        .await?;
    if let Some(DecodedValue::Str(existing_onto)) = rows.into_iter().next()
        && existing_onto != onto
    {
        return Err(MigrateError::IdCollision {
            id: id.to_string(),
            recorded_onto: existing_onto,
            new_onto: onto.to_string(),
        });
    }
    Ok(())
}

fn record_applied_params(id: &str, onto: &str, filename: &str) -> Vec<DecodedValue> {
    vec![
        DecodedValue::Str(id.to_string()),
        DecodedValue::Str(onto.to_string()),
        DecodedValue::Str(filename.to_string()),
    ]
}

/// Records a migration as applied without running its DDL — used both by
/// `apply_one`'s last step (inside its transaction, via `record_applied_in_tx`)
/// and by `apply`'s squash-backfill case (a migration whose squashed
/// constituent IDs are already applied under the old chain: no DDL to run,
/// just mark it applied so future `apply` runs see it as done).
pub async fn record_applied(pool: &PgPool, id: &str, onto: &str, filename: &str) -> Result<()> {
    check_no_id_collision(pool, id, onto).await?;
    pool.execute_typed(RECORD_APPLIED_SQL, &record_applied_params(id, onto, filename))
        .await?;
    Ok(())
}

async fn record_applied_in_tx(tx: &PgTransaction, id: &str, onto: &str, filename: &str) -> Result<()> {
    tx.execute_typed(RECORD_APPLIED_SQL, &record_applied_params(id, onto, filename))
        .await?;
    Ok(())
}

async fn read_progress(pool: &PgPool, id: &str) -> Result<Option<i64>> {
    let rows = pool
        .query_typed(
            r#"SELECT (step_index) AS result FROM _pylon."Progress" WHERE id = $1"#,
            &[DecodedValue::Str(id.to_string())],
            pool.types(),
        )
        .await?;
    Ok(match rows.into_iter().next() {
        Some(DecodedValue::I64(n)) => Some(n),
        _ => None,
    })
}

async fn record_progress(pool: &PgPool, id: &str, step_index: i64) -> Result<()> {
    pool.execute_typed(
        r#"INSERT INTO _pylon."Progress" (id, step_index) VALUES ($1, $2)
           ON CONFLICT (id) DO UPDATE SET step_index = $2, updated_at = now()"#,
        &[DecodedValue::Str(id.to_string()), DecodedValue::I64(step_index)],
    )
    .await?;
    Ok(())
}

async fn delete_progress(pool: &PgPool, id: &str) -> Result<()> {
    pool.execute_typed(
        r#"DELETE FROM _pylon."Progress" WHERE id = $1"#,
        &[DecodedValue::Str(id.to_string())],
    )
    .await?;
    Ok(())
}

/// Same upsert as `record_progress`, but on an open transaction so a step's
/// DDL and the progress row that claims it commit together — see
/// `apply_one`'s own note on why that atomicity matters.
async fn record_progress_in_tx(tx: &PgTransaction, id: &str, step_index: i64) -> Result<()> {
    tx.execute_typed(
        r#"INSERT INTO _pylon."Progress" (id, step_index) VALUES ($1, $2)
           ON CONFLICT (id) DO UPDATE SET step_index = $2, updated_at = now()"#,
        &[DecodedValue::Str(id.to_string()), DecodedValue::I64(step_index)],
    )
    .await?;
    Ok(())
}

async fn delete_progress_in_tx(tx: &PgTransaction, id: &str) -> Result<()> {
    tx.execute_typed(
        r#"DELETE FROM _pylon."Progress" WHERE id = $1"#,
        &[DecodedValue::Str(id.to_string())],
    )
    .await?;
    Ok(())
}

/// Before retrying a `CONCURRENTLY` step, drops any invalid index it left
/// behind from a prior failed attempt (a `CREATE INDEX CONCURRENTLY` that
/// errors partway leaves an unusable index rather than rolling back, since
/// it can't run inside a transaction).
async fn drop_invalid_concurrent_index(pool: &PgPool, sql: &str) -> Result<()> {
    let Some(index_name) = concurrent_index_name(sql) else {
        return Ok(());
    };
    let rows = pool
        .query_typed(
            "SELECT (1) AS result FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = $1 AND NOT i.indisvalid",
            &[DecodedValue::Str(index_name.clone())],
            pool.types(),
        )
        .await?;
    if !rows.is_empty() {
        pool.batch_execute(&format!("DROP INDEX CONCURRENTLY IF EXISTS \"{index_name}\""))
            .await?;
    }
    Ok(())
}

/// Extracts the index name from a `CREATE INDEX CONCURRENTLY [IF NOT
/// EXISTS] name ...` statement, case-insensitively — the exact shape
/// pylon-core always emits for non-transactional migration steps. `None`
/// if `sql` doesn't start with that form.
fn concurrent_index_name(sql: &str) -> Option<String> {
    let mut tokens = sql.split_whitespace();
    let matches_kw = |t: Option<&str>, expected: &str| t.is_some_and(|t| t.eq_ignore_ascii_case(expected));
    if !matches_kw(tokens.next(), "CREATE") {
        return None;
    }
    if !matches_kw(tokens.next(), "INDEX") {
        return None;
    }
    if !matches_kw(tokens.next(), "CONCURRENTLY") {
        return None;
    }
    let mut next = tokens.next()?;
    if next.eq_ignore_ascii_case("IF") {
        if !matches_kw(tokens.next(), "NOT") {
            return None;
        }
        if !matches_kw(tokens.next(), "EXISTS") {
            return None;
        }
        next = tokens.next()?;
    }
    let after_quote = next.strip_prefix('"').unwrap_or(next);
    let name: String = after_quote
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

const DEV_SAVEPOINT: &str = "pylon_dev";

/// Applies one migration's steps in order, resuming from recorded progress
/// if a prior run failed mid-migration. Verifies `m`'s integrity first
/// (re-hashes the body against its header ID).
///
/// `_pylon."Progress".step_index` is the index of the last step that
/// **completed**, so a resume starts at `step_index + 1`. For a transactional
/// step the progress row is written inside that step's own transaction, so
/// the two commit together — a crash mid-step rolls both back and the step is
/// retried rather than skipped. A non-transactional step (`CREATE INDEX
/// CONCURRENTLY`) has no transaction to join, so it records progress
/// immediately afterwards; a crash in that window just re-runs the step, and
/// `drop_invalid_concurrent_index` clears the half-built index first.
///
/// `dev_mode` rebases onto a database `watch` has already partly changed: it
/// runs each *statement* in its own savepoint and skips the individual ones
/// that fail with "already exists", leaving the rest of the step to apply
/// normally.
pub async fn apply_one(pool: &PgPool, m: &MigrationFile, dev_mode: bool) -> Result<()> {
    verify_integrity(m)?;
    // Checked up front rather than at the `record_applied` at the end, so a
    // colliding ID is rejected before any of this migration's DDL runs.
    check_no_id_collision(pool, &m.id, &m.onto).await?;

    let steps = parse_steps(&m.body);
    let resume_from = read_progress(pool, &m.id).await?.map(|i| i + 1).unwrap_or(0) as usize;
    let multi_step = steps.len() > 1;

    for (step_idx, (transactional, sql)) in steps.iter().enumerate() {
        if step_idx < resume_from {
            continue;
        }
        let sql = sql.trim();
        if sql.is_empty() {
            continue;
        }

        let is_last = step_idx == steps.len() - 1;

        if *transactional {
            let tx = pool.begin_default().await?;

            let step_result = if dev_mode {
                apply_statements_rebasing(&tx, sql).await
            } else {
                tx.batch_execute(sql).await
            };

            if let Err(e) = step_result {
                let _ = tx.rollback().await;
                return Err(e.into());
            }

            if is_last {
                record_applied_in_tx(&tx, &m.id, &m.onto, &m.filename).await?;
                if multi_step {
                    delete_progress_in_tx(&tx, &m.id).await?;
                }
            } else if multi_step {
                record_progress_in_tx(&tx, &m.id, step_idx as i64).await?;
            }
            tx.commit().await?;
        } else {
            drop_invalid_concurrent_index(pool, sql).await?;
            pool.batch_execute(sql).await?;
            if is_last {
                record_applied(pool, &m.id, &m.onto, &m.filename).await?;
                if multi_step {
                    delete_progress(pool, &m.id).await?;
                }
            } else if multi_step {
                record_progress(pool, &m.id, step_idx as i64).await?;
            }
        }
    }

    Ok(())
}

/// Runs one step's statements inside `tx`, each wrapped in its own savepoint,
/// skipping any that fail because the object already exists.
///
/// The savepoint has to be per statement rather than per step: rolling the
/// whole step back on the first duplicate would undo the statements before it
/// *and* skip the ones after it, while still reporting the step as applied.
async fn apply_statements_rebasing(tx: &PgTransaction, sql: &str) -> std::result::Result<(), pylon_pgcon::Error> {
    for stmt in crate::migration::split_statements(sql) {
        tx.savepoint(DEV_SAVEPOINT).await?;
        match tx.batch_execute(&stmt).await {
            Ok(()) => tx.release_savepoint(DEV_SAVEPOINT).await?,
            Err(e) if is_duplicate_object_error(&e) => tx.rollback_to_savepoint(DEV_SAVEPOINT).await?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod concurrent_index_name_tests {
    use super::concurrent_index_name;

    #[test]
    fn extracts_a_bare_index_name() {
        assert_eq!(
            concurrent_index_name("CREATE INDEX CONCURRENTLY idx_person_name ON \"public\".\"Person\" (name);"),
            Some("idx_person_name".to_string())
        );
    }

    #[test]
    fn extracts_a_quoted_index_name() {
        assert_eq!(
            concurrent_index_name("CREATE INDEX CONCURRENTLY \"idx_person_name\" ON \"public\".\"Person\" (name);"),
            Some("idx_person_name".to_string())
        );
    }

    #[test]
    fn handles_if_not_exists() {
        assert_eq!(
            concurrent_index_name("CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_x ON t (c);"),
            Some("idx_x".to_string())
        );
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(
            concurrent_index_name("create index concurrently idx_x on t (c);"),
            Some("idx_x".to_string())
        );
    }

    #[test]
    fn returns_none_for_unrelated_sql() {
        assert_eq!(concurrent_index_name("CREATE TABLE foo ();"), None);
        assert_eq!(concurrent_index_name("CREATE INDEX idx_x ON t (c);"), None); // not CONCURRENTLY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::render_file;

    fn test_dsn() -> String {
        std::env::var("PYLON_PGCON_TEST_DSN").expect("PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests")
    }

    async fn test_pool() -> PgPool {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        pool.batch_execute("CREATE SCHEMA IF NOT EXISTS _pylon").await.unwrap();
        ensure_internal_schema(&pool).await.unwrap();
        pool
    }

    fn make_migration(onto: &str, body: &str) -> MigrationFile {
        let content = render_file(onto, body, &[]);
        crate::migration::parse(&content, "test").unwrap()
    }

    /// A body with the leading blank-line separator `render_file`/`parse`
    /// expect, so the migration's computed ID matches what gets hashed.
    fn body(sql: &str) -> String {
        format!("\n{sql}\n")
    }

    fn unique_table_name(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("{prefix}_{nanos}")
    }

    /// Every test in this module runs against the same shared,
    /// non-isolated `_pylon."Migrations"` table (unlike the rest of the
    /// live-execution suite, which gets a fresh schema per test via
    /// `unique_module()` — there's no equivalent scoping for this
    /// process-wide tracking table). Any test that records a tracking row
    /// must delete it again here, or it permanently pollutes whatever
    /// database `PYLON_PGCON_TEST_DSN` points at (this defaults to the
    /// same DSN pylon-demo uses, and a stray row here can shadow a real
    /// project's actual migration tip).
    async fn cleanup_migration_row(pool: &PgPool, id: &str) {
        pool.execute_typed(
            r#"DELETE FROM _pylon."Migrations" WHERE id = $1"#,
            &[DecodedValue::Str(id.to_string())],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn ensure_internal_schema_is_idempotent() {
        let pool = test_pool().await;
        ensure_internal_schema(&pool).await.unwrap();
        ensure_internal_schema(&pool).await.unwrap();
    }

    #[test]
    fn a_database_this_build_cannot_work_against_is_fatal_and_names_the_fix() {
        let state = InternalSchemaState::TooOld { found: 1, required: 2 };
        assert!(state.is_fatal());
        let message = state.message().unwrap();
        assert!(message.contains("pylon migration apply"), "got: {message}");
    }

    #[test]
    fn a_newer_database_is_reported_but_never_fatal() {
        // A rollback looks exactly like this. Failing closed here would turn
        // the recovery lever into a second outage.
        let state = InternalSchemaState::Newer { found: 2, current: 1 };
        assert!(!state.is_fatal());
        assert!(state.message().is_some());
    }

    #[test]
    fn a_pending_upgrade_is_reported_but_not_fatal() {
        let state = InternalSchemaState::Behind { found: 1, current: 2 };
        assert!(!state.is_fatal());
        assert!(state.message().is_some());
    }

    #[test]
    fn an_unmigrated_or_current_database_says_nothing() {
        // A database no migration has run against is a legal state, not a
        // mismatch — callers must behave as they did before this existed.
        for state in [InternalSchemaState::Unmigrated, InternalSchemaState::Current] {
            assert!(!state.is_fatal());
            assert_eq!(state.message(), None, "{state:?} should be silent");
        }
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn a_freshly_ensured_database_reads_as_current() {
        let pool = test_pool().await;
        ensure_internal_schema(&pool).await.unwrap();
        assert_eq!(
            check_internal_schema(&pool).await.unwrap(),
            InternalSchemaState::Current
        );
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn a_database_without_the_marker_table_reads_as_unmigrated() {
        // Not an error: `read_internal_version` has to tell "never migrated"
        // apart from "migrated, and old", or every brand-new database would
        // fail the check it is supposed to pass.
        let pool = test_pool().await;
        ensure_internal_schema(&pool).await.unwrap();
        pool.batch_execute(r#"ALTER TABLE _pylon."Internal" RENAME TO "Internal_hidden";"#)
            .await
            .unwrap();
        let state = check_internal_schema(&pool).await;
        pool.batch_execute(r#"ALTER TABLE _pylon."Internal_hidden" RENAME TO "Internal";"#)
            .await
            .unwrap();
        assert_eq!(state.unwrap(), InternalSchemaState::Unmigrated);
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn ensure_internal_schema_repairs_a_database_missing_a_newer_column() {
        // The failure this whole change exists for: `claimed_at` was added
        // to `_pylon."IndexOutbox"` with an `ADD COLUMN IF NOT EXISTS`
        // alongside the worker lease, but that statement lived in a blob
        // only `database initialize` ever shipped. Databases upgraded via
        // `migration apply` never received it, and every index-worker claim
        // failed against a column that was never added.
        //
        // Dropping the column reproduces such a database exactly.
        let pool = test_pool().await;
        ensure_internal_schema(&pool).await.unwrap();
        pool.batch_execute(r#"ALTER TABLE _pylon."IndexOutbox" DROP COLUMN IF EXISTS claimed_at;"#)
            .await
            .unwrap();

        ensure_internal_schema(&pool).await.unwrap();

        let rows = pool
            .query_typed(
                "SELECT (count(*)) AS result FROM information_schema.columns \
                 WHERE table_schema = '_pylon' AND table_name = 'IndexOutbox' \
                 AND column_name = 'claimed_at'",
                &[],
                pool.types(),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.into_iter().next(),
            Some(DecodedValue::I64(1)),
            "claimed_at should have been restored"
        );
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn schema_snapshot_round_trips() {
        // `_pylon."Schema"` is a shared singleton row across this whole test
        // module's DSN (same non-isolation concern as `_pylon."Migrations"`
        // — see `cleanup_migration_row`'s doc comment), so save and restore
        // whatever was there before rather than leaving test data behind.
        let pool = test_pool().await;
        let previous = read_schema_snapshot(&pool).await.unwrap();

        write_schema_snapshot(&pool, r#"{"probe": "schema_snapshot_round_trips"}"#)
            .await
            .unwrap();
        let read_back = read_schema_snapshot(&pool).await.unwrap();
        assert_eq!(
            read_back.as_deref(),
            Some(r#"{"probe": "schema_snapshot_round_trips"}"#)
        );

        // Upsert overwrites in place, so a second write must still round-trip
        // (not silently keep the first value).
        write_schema_snapshot(&pool, r#"{"probe": "second_write"}"#)
            .await
            .unwrap();
        let read_back_2 = read_schema_snapshot(&pool).await.unwrap();
        assert_eq!(read_back_2.as_deref(), Some(r#"{"probe": "second_write"}"#));

        match previous {
            Some(prior) => write_schema_snapshot(&pool, &prior).await.unwrap(),
            None => pool.batch_execute(r#"DELETE FROM _pylon."Schema""#).await.unwrap(),
        }
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn applied_tip_is_none_with_no_applied_rows() {
        assert_eq!(applied_tip(&[]), None);
        let all_pending = vec![TrackingRow {
            id: "m1a".into(),
            onto: "initial".into(),
            db_state: None,
            schema_state: None,
            applied: false,
        }];
        assert_eq!(applied_tip(&all_pending), None);
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn applied_tip_is_the_row_with_no_descendant() {
        let tracking = vec![
            TrackingRow {
                id: "m1a".into(),
                onto: "initial".into(),
                db_state: None,
                schema_state: None,
                applied: true,
            },
            TrackingRow {
                id: "m1b".into(),
                onto: "m1a".into(),
                db_state: None,
                schema_state: None,
                applied: true,
            },
            TrackingRow {
                id: "m1c".into(),
                onto: "m1b".into(),
                db_state: None,
                schema_state: None,
                applied: false,
            }, // not applied yet
        ];
        assert_eq!(applied_tip(&tracking), Some("m1b".to_string()));
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn applied_tip_is_deterministic_with_multiple_orphaned_tips() {
        // A tracking table can end up with several unrelated single-node
        // "tips" (orphaned rows from deleted/regenerated migration files) —
        // must always return the same answer, not one that depends on
        // hash-map iteration order (see the doc comment on `applied_tip`).
        let tracking = vec![
            TrackingRow {
                id: "m1zzz".into(),
                onto: "initial".into(),
                db_state: None,
                schema_state: None,
                applied: true,
            },
            TrackingRow {
                id: "m1aaa".into(),
                onto: "initial".into(),
                db_state: None,
                schema_state: None,
                applied: true,
            },
            TrackingRow {
                id: "m1mmm".into(),
                onto: "initial".into(),
                db_state: None,
                schema_state: None,
                applied: true,
            },
        ];
        for _ in 0..20 {
            assert_eq!(applied_tip(&tracking), Some("m1aaa".to_string()));
        }
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn apply_one_runs_ddl_and_records_tracking_row() {
        let pool = test_pool().await;
        let table = unique_table_name("migrate_apply_test");
        let m = make_migration("initial", &body(&format!("CREATE TABLE {table} (id int8);")));

        apply_one(&pool, &m, false).await.unwrap();

        // DDL actually ran.
        let rows = pool
            .query_typed(
                &format!("SELECT (1) AS result FROM {table}"),
                &[],
                &pylon_pgcon::ExtensionOids::default(),
            )
            .await;
        assert!(rows.is_ok(), "table should exist after apply_one");

        // Tracking row recorded.
        let tracking = read_tracking(&pool).await.unwrap();
        let row = tracking
            .iter()
            .find(|r| r.id == m.id)
            .expect("tracking row for this migration");
        assert!(row.applied);
        assert_eq!(row.onto, "initial");
        assert_eq!(
            row.db_state, None,
            "db_state is only ever set separately, by `migration create`'s own UPDATE"
        );
        assert_eq!(
            row.schema_state, None,
            "schema_state is only ever set separately, by `apply`'s own UPDATE"
        );

        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn read_tracking_decodes_schema_state_as_raw_json_text() {
        let pool = test_pool().await;
        let m = make_migration("initial", &body("SELECT 1;"));
        record_applied(&pool, &m.id, &m.onto, &m.filename).await.unwrap();

        pool.execute_typed(
            r#"UPDATE _pylon."Migrations" SET schema_state = $1::jsonb WHERE id = $2"#,
            &[
                DecodedValue::Str(r#"{"types":[]}"#.to_string()),
                DecodedValue::Str(m.id.clone()),
            ],
        )
        .await
        .unwrap();

        let tracking = read_tracking(&pool).await.unwrap();
        let row = tracking.iter().find(|r| r.id == m.id).unwrap();
        // Raw text, not a decoded DecodedValue::Object tree — `SchemaDescriptor::from_json`
        // (the pyo3-exposed consumer) re-parses this string itself.
        assert_eq!(row.schema_state.as_deref(), Some(r#"{"types": []}"#));

        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn read_tracking_decodes_db_state_as_raw_json_text() {
        let pool = test_pool().await;
        let m = make_migration("initial", &body("SELECT 1;"));
        record_applied(&pool, &m.id, &m.onto, &m.filename).await.unwrap();

        pool.execute_typed(
            r#"UPDATE _pylon."Migrations" SET db_state = $1::jsonb WHERE id = $2"#,
            &[
                DecodedValue::Str(r#"{"schemas":["default"]}"#.to_string()),
                DecodedValue::Str(m.id.clone()),
            ],
        )
        .await
        .unwrap();

        let tracking = read_tracking(&pool).await.unwrap();
        let row = tracking.iter().find(|r| r.id == m.id).unwrap();
        // Raw text, not a decoded DecodedValue::Object tree — `db_state_from_json`
        // (the pyo3-exposed consumer) re-parses this string itself.
        assert_eq!(row.db_state.as_deref(), Some(r#"{"schemas": ["default"]}"#));

        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn apply_one_multi_step_clears_progress_after_completion() {
        let pool = test_pool().await;
        let t1 = unique_table_name("migrate_step1");
        let t2 = unique_table_name("migrate_step2");
        let m = make_migration(
            "initial",
            &format!("\nCREATE TABLE {t1} (id int8);\n-- pylon:step\nCREATE TABLE {t2} (id int8);\n"),
        );

        apply_one(&pool, &m, false).await.unwrap();

        for t in [&t1, &t2] {
            let rows = pool
                .query_typed(
                    &format!("SELECT (1) AS result FROM {t}"),
                    &[],
                    &pylon_pgcon::ExtensionOids::default(),
                )
                .await;
            assert!(rows.is_ok(), "table {t} should exist after apply_one");
        }

        let progress = pool
            .query_typed(
                r#"SELECT (1) AS result FROM _pylon."Progress" WHERE id = $1"#,
                &[DecodedValue::Str(m.id.clone())],
                &pylon_pgcon::ExtensionOids::default(),
            )
            .await
            .unwrap();
        assert!(
            progress.is_empty(),
            "progress row must be cleared after a successful multi-step apply"
        );

        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn apply_one_resumes_from_recorded_progress_skipping_earlier_steps() {
        let pool = test_pool().await;
        let t2 = unique_table_name("migrate_resume_step2");
        // Step 0 is intentionally invalid SQL — if apply_one didn't skip
        // it (via the pre-recorded progress row below), this test would
        // fail with a Postgres syntax error instead of succeeding.
        let m = make_migration(
            "initial",
            &format!("\nTHIS IS NOT VALID SQL;\n-- pylon:step\nCREATE TABLE {t2} (id int8);\n"),
        );

        // Simulate a prior run that got through step 0 already.
        record_progress(&pool, &m.id, 0).await.unwrap();

        apply_one(&pool, &m, false).await.unwrap();

        let rows = pool
            .query_typed(
                &format!("SELECT (1) AS result FROM {t2}"),
                &[],
                &pylon_pgcon::ExtensionOids::default(),
            )
            .await;
        assert!(rows.is_ok(), "step 1 should have run");

        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn apply_one_does_not_skip_the_step_it_failed_on_when_resumed() {
        let pool = test_pool().await;
        let t0 = unique_table_name("migrate_crash_step0");
        let t1 = unique_table_name("migrate_crash_step1");
        let t2 = unique_table_name("migrate_crash_step2");

        // Step 1 fails on the first run because `t1` doesn't exist yet. The
        // point of the test is what the *second* run does with step 1: it
        // must retry it, not treat it as already done.
        let m = make_migration(
            "initial",
            &format!(
                "\nCREATE TABLE {t0} (id int8);\n\
                 -- pylon:step\n\
                 INSERT INTO {t1} (id) VALUES (1);\n\
                 -- pylon:step\n\
                 CREATE TABLE {t2} (id int8);\n"
            ),
        );

        let first = apply_one(&pool, &m, false).await;
        assert!(first.is_err(), "step 1 should have failed on the first run");

        // Progress must name step 0 — the last step that actually committed.
        // Recording step 1 here is the bug: it would make the retry resume at
        // step 2 and skip the INSERT forever.
        assert_eq!(
            read_progress(&pool, &m.id).await.unwrap(),
            Some(0),
            "progress must record the last *completed* step, not the one being attempted"
        );

        // Make step 1 able to succeed, then resume.
        pool.batch_execute(&format!("CREATE TABLE {t1} (id int8);"))
            .await
            .unwrap();
        apply_one(&pool, &m, false).await.unwrap();

        let rows = pool
            .query_typed(
                &format!("SELECT (count(*)) AS result FROM {t1}"),
                &[],
                &pylon_pgcon::ExtensionOids::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.first(),
            Some(&DecodedValue::I64(1)),
            "step 1 must have been retried on resume, not skipped"
        );

        let t2_rows = pool
            .query_typed(
                &format!("SELECT (1) AS result FROM {t2}"),
                &[],
                &pylon_pgcon::ExtensionOids::default(),
            )
            .await;
        assert!(t2_rows.is_ok(), "step 2 should have run after the resumed step 1");

        for t in [&t0, &t1, &t2] {
            pool.batch_execute(&format!("DROP TABLE IF EXISTS {t}")).await.unwrap();
        }
        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn apply_one_dev_mode_skips_only_the_duplicate_statement_in_a_step() {
        let pool = test_pool().await;
        let before = unique_table_name("migrate_rebase_before");
        let existing = unique_table_name("migrate_rebase_existing");
        let after = unique_table_name("migrate_rebase_after");

        // `watch` already created the middle table out of band.
        pool.batch_execute(&format!("CREATE TABLE {existing} (id int8);"))
            .await
            .unwrap();

        // One step, three statements, only the middle one already applied.
        let m = make_migration(
            "initial",
            &body(&format!(
                "CREATE TABLE {before} (id int8);\n\
                 CREATE TABLE {existing} (id int8);\n\
                 CREATE TABLE {after} (id int8);"
            )),
        );

        apply_one(&pool, &m, true).await.unwrap();

        // Rolling back the whole step on the duplicate would drop `before`
        // and never reach `after`, while still recording the migration as
        // applied — the failure this test exists to catch.
        for t in [&before, &after] {
            let rows = pool
                .query_typed(
                    &format!("SELECT (1) AS result FROM {t}"),
                    &[],
                    &pylon_pgcon::ExtensionOids::default(),
                )
                .await;
            assert!(
                rows.is_ok(),
                "table {t} should exist — only the duplicate statement may be skipped"
            );
        }

        let tracking = read_tracking(&pool).await.unwrap();
        assert!(tracking.iter().any(|r| r.id == m.id && r.applied));

        for t in [&before, &existing, &after] {
            pool.batch_execute(&format!("DROP TABLE IF EXISTS {t}")).await.unwrap();
        }
        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn apply_one_dev_mode_swallows_a_duplicate_table_error() {
        let pool = test_pool().await;
        let table = unique_table_name("migrate_dev_mode_test");
        // Simulate `watch` having already applied this exact DDL out of band.
        pool.batch_execute(&format!("CREATE TABLE {table} (id int8);"))
            .await
            .unwrap();

        let m = make_migration("initial", &body(&format!("CREATE TABLE {table} (id int8);")));
        apply_one(&pool, &m, true).await.unwrap(); // dev_mode=true: must not error

        let tracking = read_tracking(&pool).await.unwrap();
        assert!(
            tracking.iter().any(|r| r.id == m.id && r.applied),
            "still recorded applied despite the swallowed error"
        );

        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn apply_one_without_dev_mode_propagates_a_duplicate_table_error() {
        let pool = test_pool().await;
        let table = unique_table_name("migrate_no_dev_mode_test");
        pool.batch_execute(&format!("CREATE TABLE {table} (id int8);"))
            .await
            .unwrap();

        let m = make_migration("initial", &body(&format!("CREATE TABLE {table} (id int8);")));
        let result = apply_one(&pool, &m, false).await; // dev_mode=false: must error
        assert!(result.is_err());
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn record_applied_standalone_marks_a_migration_applied_without_running_ddl() {
        let pool = test_pool().await;
        let m = make_migration("initial", &body("SELECT 1;"));

        record_applied(&pool, &m.id, &m.onto, &m.filename).await.unwrap();

        let tracking = read_tracking(&pool).await.unwrap();
        assert!(tracking.iter().any(|r| r.id == m.id && r.applied));

        cleanup_migration_row(&pool, &m.id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn advisory_lock_round_trips_and_blocks_a_concurrent_try_lock() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();

        let held = advisory_lock(&pool).await.unwrap();

        // A second, independent connection can't acquire the same key.
        let blocked = try_advisory_lock(&pool).await.unwrap();
        assert!(blocked.is_none(), "advisory lock should still be held");

        advisory_unlock(held).await.unwrap();

        // Now it's free again.
        let reacquired = try_advisory_lock(&pool).await.unwrap();
        assert!(reacquired.is_some());
        advisory_unlock(reacquired.unwrap()).await.unwrap();
    }
}
