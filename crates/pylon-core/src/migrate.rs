//! Migration execution: advisory locking, tracking tables, resumable
//! per-step progress, and dev-mode savepoint retry. A direct Rust port of
//! `pylon.cli.commands.migrations`'s `_apply_one` (plus the tracking-table
//! helpers `_ensure_tracking_tables`/`_read_tracking`/`_applied_tip`/
//! `_record_applied` it and `_apply`'s outer loop share). The outer
//! `apply` loop itself (chain resolution, `--to` targeting, the squash-
//! backfill special case, `click.echo` progress output) stays in Python —
//! this module is the part that's actually "migration execution."

use crate::migration::{parse_steps, verify_integrity, MigrationFile};
use pylon_pgcon::{PgPool, PgTransaction};
use pylon_value::CachedValue;

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(transparent)]
    Integrity(#[from] crate::migration::MigrationError),
    #[error(transparent)]
    Db(#[from] pylon_pgcon::Error),
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

pub async fn ensure_tracking_tables(pool: &PgPool) -> Result<()> {
    pool.batch_execute(
        r#"
        CREATE TABLE IF NOT EXISTS _pylon."Migrations" (
            id          text        PRIMARY KEY,
            onto        text        NOT NULL,
            filename    text        NOT NULL,
            db_state    jsonb       NULL,
            applied_at  timestamptz NULL
        );
        CREATE TABLE IF NOT EXISTS _pylon."Progress" (
            id          text        PRIMARY KEY,
            step_index  integer     NOT NULL,
            updated_at  timestamptz NOT NULL DEFAULT now()
        );
        "#,
    )
    .await?;
    Ok(())
}

/// One row of `_pylon."Migrations"` — just enough for `apply`'s own
/// tracking logic (computing the applied tip, deciding what's pending).
/// `filename`/`db_state` (needed by `migration create`'s diff baseline,
/// not migration execution) aren't read here.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackingRow {
    pub id: String,
    pub onto: String,
    pub applied: bool,
}

pub async fn read_tracking(pool: &PgPool) -> Result<Vec<TrackingRow>> {
    let rows = pool
        .query_typed(
            r#"SELECT (id, onto, (applied_at IS NOT NULL)) AS result FROM _pylon."Migrations""#,
            &[],
            &pylon_pgcon::ExtensionOids::default(),
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let CachedValue::Composite(fields) = row else { return None };
            let [CachedValue::Str(id), CachedValue::Str(onto), CachedValue::Bool(applied)] = <[CachedValue; 3]>::try_from(fields).ok()? else {
                return None;
            };
            Some(TrackingRow { id, onto, applied })
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
    applied.iter().filter(|r| !onto_targets.contains(r.id.as_str())).map(|r| r.id.as_str()).min().map(|s| s.to_string())
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
    conn.batch_execute(&format!("SELECT pg_advisory_lock({ADVISORY_LOCK_KEY})")).await?;
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
            &pylon_pgcon::ExtensionOids::default(),
        )
        .await?;
    Ok(if matches!(rows.first(), Some(CachedValue::Bool(true))) { Some(conn) } else { None })
}

pub async fn advisory_unlock(conn: pylon_pgcon::PgConnection) -> Result<()> {
    conn.batch_execute(&format!("SELECT pg_advisory_unlock({ADVISORY_LOCK_KEY})")).await?;
    Ok(())
}

const RECORD_APPLIED_SQL: &str = r#"
    INSERT INTO _pylon."Migrations" (id, onto, filename, applied_at)
    VALUES ($1, $2, $3, now())
    ON CONFLICT (id) DO UPDATE SET applied_at = now()
"#;

fn record_applied_params(id: &str, onto: &str, filename: &str) -> Vec<CachedValue> {
    vec![CachedValue::Str(id.to_string()), CachedValue::Str(onto.to_string()), CachedValue::Str(filename.to_string())]
}

/// Records a migration as applied without running its DDL — used both by
/// `apply_one`'s last step (inside its transaction, via `record_applied_in_tx`)
/// and by `apply`'s squash-backfill case (a migration whose squashed
/// constituent IDs are already applied under the old chain: no DDL to run,
/// just mark it applied so future `apply` runs see it as done).
pub async fn record_applied(pool: &PgPool, id: &str, onto: &str, filename: &str) -> Result<()> {
    pool.execute_typed(RECORD_APPLIED_SQL, &record_applied_params(id, onto, filename)).await?;
    Ok(())
}

async fn record_applied_in_tx(tx: &PgTransaction, id: &str, onto: &str, filename: &str) -> Result<()> {
    tx.execute_typed(RECORD_APPLIED_SQL, &record_applied_params(id, onto, filename)).await?;
    Ok(())
}

async fn read_progress(pool: &PgPool, id: &str) -> Result<Option<i64>> {
    let rows = pool
        .query_typed(
            r#"SELECT (step_index) AS result FROM _pylon."Progress" WHERE id = $1"#,
            &[CachedValue::Str(id.to_string())],
            &pylon_pgcon::ExtensionOids::default(),
        )
        .await?;
    Ok(match rows.into_iter().next() {
        Some(CachedValue::I64(n)) => Some(n),
        _ => None,
    })
}

async fn record_progress(pool: &PgPool, id: &str, step_index: i64) -> Result<()> {
    pool.execute_typed(
        r#"INSERT INTO _pylon."Progress" (id, step_index) VALUES ($1, $2)
           ON CONFLICT (id) DO UPDATE SET step_index = $2, updated_at = now()"#,
        &[CachedValue::Str(id.to_string()), CachedValue::I64(step_index)],
    )
    .await?;
    Ok(())
}

async fn delete_progress(pool: &PgPool, id: &str) -> Result<()> {
    pool.execute_typed(r#"DELETE FROM _pylon."Progress" WHERE id = $1"#, &[CachedValue::Str(id.to_string())]).await?;
    Ok(())
}

async fn delete_progress_in_tx(tx: &PgTransaction, id: &str) -> Result<()> {
    tx.execute_typed(r#"DELETE FROM _pylon."Progress" WHERE id = $1"#, &[CachedValue::Str(id.to_string())]).await?;
    Ok(())
}

/// Before retrying a `CONCURRENTLY` step, drops any invalid index it left
/// behind from a prior failed attempt (a `CREATE INDEX CONCURRENTLY` that
/// errors partway leaves an unusable index rather than rolling back, since
/// it can't run inside a transaction).
async fn drop_invalid_concurrent_index(pool: &PgPool, sql: &str) -> Result<()> {
    let Some(index_name) = concurrent_index_name(sql) else { return Ok(()) };
    let rows = pool
        .query_typed(
            "SELECT (1) AS result FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = $1 AND NOT i.indisvalid",
            &[CachedValue::Str(index_name.clone())],
            &pylon_pgcon::ExtensionOids::default(),
        )
        .await?;
    if !rows.is_empty() {
        pool.batch_execute(&format!("DROP INDEX CONCURRENTLY IF EXISTS \"{index_name}\"")).await?;
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
    let name: String = after_quote.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

const DEV_SAVEPOINT: &str = "pylon_dev";

/// Applies one migration's steps in order, resuming from recorded progress
/// if a prior run failed mid-migration. Verifies `m`'s integrity first
/// (re-hashes the body against its header ID), matching `_apply_one`'s own
/// `verify_migration(m)` call. `dev_mode` runs each transactional step
/// inside a savepoint and swallows "already exists" errors (structure
/// `watch` already applied), matching `_apply_one`'s nested-transaction
/// rebase behavior exactly.
pub async fn apply_one(pool: &PgPool, m: &MigrationFile, dev_mode: bool) -> Result<()> {
    verify_integrity(m)?;

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

        if multi_step {
            record_progress(pool, &m.id, step_idx as i64).await?;
        }

        let is_last = step_idx == steps.len() - 1;

        if *transactional {
            let tx = pool.begin_default().await?;

            let step_result = if dev_mode {
                tx.savepoint(DEV_SAVEPOINT).await?;
                match tx.batch_execute(sql).await {
                    Ok(()) => tx.release_savepoint(DEV_SAVEPOINT).await,
                    Err(e) if is_duplicate_object_error(&e) => tx.rollback_to_savepoint(DEV_SAVEPOINT).await,
                    Err(e) => Err(e),
                }
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
            }
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
        assert_eq!(concurrent_index_name("create index concurrently idx_x on t (c);"), Some("idx_x".to_string()));
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
        std::env::var("PYLON_PGCON_TEST_DSN").unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5418/app".to_string())
    }

    async fn test_pool() -> PgPool {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        pool.batch_execute("CREATE SCHEMA IF NOT EXISTS _pylon").await.unwrap();
        ensure_tracking_tables(&pool).await.unwrap();
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

    #[tokio::test]
    #[ignore]
    async fn ensure_tracking_tables_is_idempotent() {
        let pool = test_pool().await;
        ensure_tracking_tables(&pool).await.unwrap();
        ensure_tracking_tables(&pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn applied_tip_is_none_with_no_applied_rows() {
        assert_eq!(applied_tip(&[]), None);
        let all_pending = vec![TrackingRow { id: "m1a".into(), onto: "initial".into(), applied: false }];
        assert_eq!(applied_tip(&all_pending), None);
    }

    #[tokio::test]
    #[ignore]
    async fn applied_tip_is_the_row_with_no_descendant() {
        let tracking = vec![
            TrackingRow { id: "m1a".into(), onto: "initial".into(), applied: true },
            TrackingRow { id: "m1b".into(), onto: "m1a".into(), applied: true },
            TrackingRow { id: "m1c".into(), onto: "m1b".into(), applied: false }, // not applied yet
        ];
        assert_eq!(applied_tip(&tracking), Some("m1b".to_string()));
    }

    #[tokio::test]
    #[ignore]
    async fn applied_tip_is_deterministic_with_multiple_orphaned_tips() {
        // A tracking table can end up with several unrelated single-node
        // "tips" (orphaned rows from deleted/regenerated migration files) —
        // must always return the same answer, not one that depends on
        // hash-map iteration order (see the doc comment on `applied_tip`).
        let tracking = vec![
            TrackingRow { id: "m1zzz".into(), onto: "initial".into(), applied: true },
            TrackingRow { id: "m1aaa".into(), onto: "initial".into(), applied: true },
            TrackingRow { id: "m1mmm".into(), onto: "initial".into(), applied: true },
        ];
        for _ in 0..20 {
            assert_eq!(applied_tip(&tracking), Some("m1aaa".to_string()));
        }
    }

    #[tokio::test]
    #[ignore]
    async fn apply_one_runs_ddl_and_records_tracking_row() {
        let pool = test_pool().await;
        let table = unique_table_name("migrate_apply_test");
        let m = make_migration("initial", &body(&format!("CREATE TABLE {table} (id int8);")));

        apply_one(&pool, &m, false).await.unwrap();

        // DDL actually ran.
        let rows =
            pool.query_typed(&format!("SELECT (1) AS result FROM {table}"), &[], &pylon_pgcon::ExtensionOids::default()).await;
        assert!(rows.is_ok(), "table should exist after apply_one");

        // Tracking row recorded.
        let tracking = read_tracking(&pool).await.unwrap();
        let row = tracking.iter().find(|r| r.id == m.id).expect("tracking row for this migration");
        assert!(row.applied);
        assert_eq!(row.onto, "initial");
    }

    #[tokio::test]
    #[ignore]
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
            let rows =
                pool.query_typed(&format!("SELECT (1) AS result FROM {t}"), &[], &pylon_pgcon::ExtensionOids::default()).await;
            assert!(rows.is_ok(), "table {t} should exist after apply_one");
        }

        let progress =
            pool.query_typed(r#"SELECT (1) AS result FROM _pylon."Progress" WHERE id = $1"#, &[CachedValue::Str(m.id.clone())], &pylon_pgcon::ExtensionOids::default())
                .await
                .unwrap();
        assert!(progress.is_empty(), "progress row must be cleared after a successful multi-step apply");
    }

    #[tokio::test]
    #[ignore]
    async fn apply_one_resumes_from_recorded_progress_skipping_earlier_steps() {
        let pool = test_pool().await;
        let t2 = unique_table_name("migrate_resume_step2");
        // Step 0 is intentionally invalid SQL — if apply_one didn't skip
        // it (via the pre-recorded progress row below), this test would
        // fail with a Postgres syntax error instead of succeeding.
        let m = make_migration("initial", &format!("\nTHIS IS NOT VALID SQL;\n-- pylon:step\nCREATE TABLE {t2} (id int8);\n"));

        // Simulate a prior run that got through step 0 already.
        record_progress(&pool, &m.id, 0).await.unwrap();

        apply_one(&pool, &m, false).await.unwrap();

        let rows =
            pool.query_typed(&format!("SELECT (1) AS result FROM {t2}"), &[], &pylon_pgcon::ExtensionOids::default()).await;
        assert!(rows.is_ok(), "step 1 should have run");
    }

    #[tokio::test]
    #[ignore]
    async fn apply_one_dev_mode_swallows_a_duplicate_table_error() {
        let pool = test_pool().await;
        let table = unique_table_name("migrate_dev_mode_test");
        // Simulate `watch` having already applied this exact DDL out of band.
        pool.batch_execute(&format!("CREATE TABLE {table} (id int8);")).await.unwrap();

        let m = make_migration("initial", &body(&format!("CREATE TABLE {table} (id int8);")));
        apply_one(&pool, &m, true).await.unwrap(); // dev_mode=true: must not error

        let tracking = read_tracking(&pool).await.unwrap();
        assert!(tracking.iter().any(|r| r.id == m.id && r.applied), "still recorded applied despite the swallowed error");
    }

    #[tokio::test]
    #[ignore]
    async fn apply_one_without_dev_mode_propagates_a_duplicate_table_error() {
        let pool = test_pool().await;
        let table = unique_table_name("migrate_no_dev_mode_test");
        pool.batch_execute(&format!("CREATE TABLE {table} (id int8);")).await.unwrap();

        let m = make_migration("initial", &body(&format!("CREATE TABLE {table} (id int8);")));
        let result = apply_one(&pool, &m, false).await; // dev_mode=false: must error
        assert!(result.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn record_applied_standalone_marks_a_migration_applied_without_running_ddl() {
        let pool = test_pool().await;
        let m = make_migration("initial", &body("SELECT 1;"));

        record_applied(&pool, &m.id, &m.onto, &m.filename).await.unwrap();

        let tracking = read_tracking(&pool).await.unwrap();
        assert!(tracking.iter().any(|r| r.id == m.id && r.applied));
    }

    #[tokio::test]
    #[ignore]
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
