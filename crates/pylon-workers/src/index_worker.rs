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

//! Shared claim/drain/mark-done/mark-failed machinery for `_pylon."IndexOutbox"`
//! consumers — native port of `pylon.worker.IndexWorker`'s base class,
//! reused by `VectorIndexWorker` and (later) the Meilisearch/OpenSearch
//! workers, exactly like the Python base class is.
//!
//! Python's `_drain` needs an explicit `asyncio.Lock`-guarded reentrancy
//! check because its NOTIFY callback fires an independent async task
//! (`asyncio.ensure_future(self._drain())`) that could genuinely overlap
//! with an already-running drain or the poll-interval's own drain. Here the
//! NOTIFY callback only calls `Notify::notify_one()` (a cheap, synchronous
//! wake) and a single background task owns the actual drain loop —
//! `tokio::select!` between the wake and the poll-interval timer, always
//! sequential. That removes the possibility of a concurrent drain
//! entirely, so no lock is needed; not a behavior change, just fewer
//! moving parts (same reasoning as `CacheInvalidationWorker`'s notify path).

use std::sync::Arc;
use std::time::Duration;

use pylon_pgcon::PgListener;
use pylon_value::DecodedValue;

use crate::error::{Error, Result};

const NOTIFY_CHANNEL: &str = "pylon_index_queue";

/// How long a row may sit in `Processing` before another drain reclaims it.
///
/// A worker that dies mid-batch (OOM, eviction, SIGKILL) leaves its claimed
/// rows in `Processing` with nothing left to move them out again, and
/// `claim_batch` only looks at `Pending` — so without this they would never
/// be indexed and nothing would report it. Deliberately much longer than a
/// healthy batch takes, so it reclaims only genuinely abandoned work.
pub const PROCESSING_LEASE: Duration = Duration::from_secs(300);

/// Attempts allowed before a row is parked as `Failed`.
pub const MAX_ATTEMPTS: i64 = 5;

/// Claims up to `$2` rows for this index kind.
///
/// Picks up both never-started work (`Pending`, due) and work abandoned by a
/// dead worker (`Processing` past `$3`, the lease). `claimed_at` is stamped
/// on every claim so the lease is measured from when *this* worker took the
/// row, not from when it was first enqueued.
const CLAIM_BATCH_SQL: &str = r#"
UPDATE _pylon."IndexOutbox"
SET status = 'Processing', claimed_at = now()
WHERE id IN (
    SELECT id FROM _pylon."IndexOutbox"
    WHERE index_kind = $1::_pylon."IndexKind"
      AND (
        (status = 'Pending' AND (next_attempt IS NULL OR next_attempt <= now()))
        OR (status = 'Processing' AND claimed_at IS NOT NULL
            AND claimed_at < now() - ($3::text || ' seconds')::interval)
      )
    ORDER BY enqueued_at
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
RETURNING id, object_id, type_name, index_name, operation, attempts
"#;

const MARK_DONE_SQL: &str = r#"DELETE FROM _pylon."IndexOutbox" WHERE id = ANY($1::uuid[])"#;

/// Records a failed attempt and either schedules a retry or parks the row.
///
/// `attempts + 1` is the count *including* this failure, so comparing that
/// (rather than the pre-update `attempts`) against `MAX_ATTEMPTS` is what
/// makes the fifth failure terminal instead of the sixth. Backoff is still
/// keyed off the pre-update value: 30s, 60s, 120s, 240s, then capped.
const MARK_FAILED_SQL: &str = r#"
UPDATE _pylon."IndexOutbox"
SET status = CASE WHEN attempts + 1 >= $2::int
                  THEN 'Failed'::_pylon."IndexOutboxStatus"
                  ELSE 'Pending'::_pylon."IndexOutboxStatus"
             END,
    attempts = attempts + 1,
    claimed_at = NULL,
    next_attempt = now() + (30 * 2^LEAST(attempts, 3) || ' seconds')::interval
WHERE id = ANY($1::uuid[])
RETURNING (status = 'Failed'::_pylon."IndexOutboxStatus") AS terminal
"#;

/// What a claimed row asks the worker to do with its object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Build/refresh the object's document or embedding.
    Index,
    /// Remove the object from the index — it no longer exists.
    Delete,
}

impl Operation {
    fn from_column(s: &str) -> Self {
        // Anything unrecognised is treated as an index request, matching the
        // column's own `DEFAULT 'index'`.
        if s.eq_ignore_ascii_case("delete") {
            Operation::Delete
        } else {
            Operation::Index
        }
    }
}

/// One claimed `_pylon."IndexOutbox"` row. `attempts` isn't carried —
/// nothing downstream of `claim_batch` reads it.
#[derive(Debug, Clone)]
pub struct ClaimedRow {
    pub id: [u8; 16],
    pub object_id: [u8; 16],
    pub type_name: String,
    pub index_name: Option<String>,
    /// Whether this row is an upsert or a removal. Enqueued by the mutation
    /// trigger; a delete must reach the external index or the object stays
    /// searchable after it's gone.
    pub operation: Operation,
}

/// Implemented by each index kind (`VectorIndexWorker`, and later the
/// Meilisearch/OpenSearch workers) — mirrors `IndexWorker.process_batch`.
pub trait BatchProcessor: Send + Sync {
    /// The `index_kind` value stored in `_pylon."IndexOutbox"` for this
    /// worker (`"Vector"`, `"OpenSearch"`, `"Meilisearch"`).
    fn index_kind(&self) -> &'static str;

    fn process_batch(
        &self,
        listener: &PgListener,
        rows: &[ClaimedRow],
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// Starts the claim/drain loop for `processor` on a dedicated LISTEN/NOTIFY
/// connection — same lifecycle shape as `IndexWorker.run()`: an initial
/// drain, then wait for either a NOTIFY or `poll_interval` to elapse,
/// repeating forever. The returned `PgListener` is also what
/// `process_batch` implementations run their own SQL against (fetch,
/// write), exactly like Python's `self._conn` is shared between listening
/// and querying.
pub async fn run<P>(dsn: &str, batch_size: i64, poll_interval: Duration, processor: P) -> Result<()>
where
    P: BatchProcessor + 'static,
{
    let notify = Arc::new(tokio::sync::Notify::new());
    let notify_for_listener = notify.clone();
    let listener = Arc::new(PgListener::connect(dsn, move |_n| notify_for_listener.notify_one()).await?);
    listener.listen(NOTIFY_CHANNEL).await?;

    loop {
        drain_once(&listener, batch_size, &processor).await;
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }
}

async fn drain_once<P: BatchProcessor>(listener: &PgListener, batch_size: i64, processor: &P) {
    loop {
        let rows = match claim_batch(listener, processor.index_kind(), batch_size).await {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("IndexWorker({}): claim_batch failed: {e}", processor.index_kind());
                return;
            }
        };
        if rows.is_empty() {
            break;
        }
        let short_batch = rows.len() < batch_size as usize;
        let ids: Vec<DecodedValue> = rows.iter().map(|r| DecodedValue::Uuid(r.id)).collect();

        match processor.process_batch(listener, &rows).await {
            Ok(()) => {
                crate::metrics::JOBS_PROCESSED
                    .with_label_values(&[processor.index_kind()])
                    .inc_by(rows.len() as u64);
                if let Err(e) = listener.execute_typed(MARK_DONE_SQL, &[DecodedValue::Array(ids)]).await {
                    eprintln!("IndexWorker({}): mark_done failed: {e}", processor.index_kind());
                }
            }
            Err(e) => {
                crate::metrics::JOBS_FAILED
                    .with_label_values(&[processor.index_kind()])
                    .inc_by(rows.len() as u64);
                eprintln!(
                    "IndexWorker({}): batch failed, scheduling retry: {e}",
                    processor.index_kind()
                );
                match mark_failed(listener, ids, processor.index_kind()).await {
                    Ok(0) => {}
                    Ok(terminal) => {
                        // Distinct from JOBS_FAILED, which counts every
                        // failed attempt including ones that will retry.
                        // This counts rows that have given up for good and
                        // now need someone to look at them.
                        crate::metrics::JOBS_ABANDONED
                            .with_label_values(&[processor.index_kind()])
                            .inc_by(terminal);
                        eprintln!(
                            "IndexWorker({}): {terminal} row(s) exhausted {MAX_ATTEMPTS} attempts and are now Failed — \
                             inspect with `pylon worker failed` and requeue with `pylon worker retry`",
                            processor.index_kind()
                        );
                    }
                    Err(e) => {
                        eprintln!("IndexWorker({}): mark_failed failed: {e}", processor.index_kind());
                    }
                }
            }
        }

        if short_batch {
            break;
        }
    }
}

async fn claim_batch(listener: &PgListener, index_kind: &str, limit: i64) -> Result<Vec<ClaimedRow>> {
    let rows = listener
        .query_typed_named(
            CLAIM_BATCH_SQL,
            &[
                DecodedValue::Str(index_kind.to_string()),
                DecodedValue::I64(limit),
                // Bound as text and cast in SQL: `$3 || ' seconds'` makes
                // Postgres report this parameter as `text`, and an integer
                // written into a text slot arrives as raw binary bytes.
                DecodedValue::Str(PROCESSING_LEASE.as_secs().to_string()),
            ],
            listener.types(),
        )
        .await?;
    rows.iter().map(decode_claimed_row).collect()
}

/// Applies `MARK_FAILED_SQL` and reports how many of those rows are now
/// terminally `Failed` rather than scheduled for another attempt.
async fn mark_failed(listener: &PgListener, ids: Vec<DecodedValue>, index_kind: &str) -> Result<u64> {
    let rows = listener
        .query_typed_named(
            MARK_FAILED_SQL,
            &[DecodedValue::Array(ids), DecodedValue::I64(MAX_ATTEMPTS)],
            listener.types(),
        )
        .await?;
    let _ = index_kind;
    Ok(rows
        .iter()
        .filter(|row| {
            let DecodedValue::Object(fields) = row else {
                return false;
            };
            matches!(
                fields.iter().find(|(k, _)| k == "terminal").map(|(_, v)| v),
                Some(DecodedValue::Bool(true))
            )
        })
        .count() as u64)
}

fn decode_claimed_row(value: &DecodedValue) -> Result<ClaimedRow> {
    let DecodedValue::Object(fields) = value else {
        return Err(Error::Decode("claim_batch: expected a named-column row".into()));
    };
    let field = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);

    let id = match field("id") {
        Some(DecodedValue::Uuid(b)) => *b,
        _ => return Err(Error::Decode("claim_batch: missing/invalid 'id'".into())),
    };
    let object_id = match field("object_id") {
        Some(DecodedValue::Uuid(b)) => *b,
        _ => return Err(Error::Decode("claim_batch: missing/invalid 'object_id'".into())),
    };
    let type_name = match field("type_name") {
        Some(DecodedValue::Str(s)) => s.clone(),
        _ => return Err(Error::Decode("claim_batch: missing/invalid 'type_name'".into())),
    };
    let index_name = match field("index_name") {
        Some(DecodedValue::Str(s)) => Some(s.clone()),
        Some(DecodedValue::Null) | None => None,
        _ => return Err(Error::Decode("claim_batch: invalid 'index_name'".into())),
    };

    let operation = match field("operation") {
        Some(DecodedValue::Str(s)) => Operation::from_column(s),
        // The column has a NOT NULL default, so absence only happens on a
        // database predating it — treat that as the default it would carry.
        Some(DecodedValue::Null) | None => Operation::Index,
        _ => return Err(Error::Decode("claim_batch: invalid 'operation'".into())),
    };

    Ok(ClaimedRow {
        id,
        object_id,
        type_name,
        index_name,
        operation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_parses_the_delete_marker() {
        assert_eq!(Operation::from_column("delete"), Operation::Delete);
        assert_eq!(Operation::from_column("DELETE"), Operation::Delete);
    }

    #[test]
    fn operation_defaults_to_index_for_anything_else() {
        assert_eq!(Operation::from_column("index"), Operation::Index);
        assert_eq!(Operation::from_column(""), Operation::Index);
        assert_eq!(Operation::from_column("something-new"), Operation::Index);
    }

    #[test]
    fn claim_sql_selects_the_operation_column() {
        // The gap this closes: `operation` was never in RETURNING, so every
        // claimed row looked like an index request and deletes never reached
        // the external index.
        assert!(CLAIM_BATCH_SQL.contains("operation"));
    }

    #[test]
    fn claim_sql_also_reclaims_expired_processing_rows() {
        assert!(CLAIM_BATCH_SQL.contains("status = 'Processing'"));
        assert!(CLAIM_BATCH_SQL.contains("claimed_at"));
    }

    #[test]
    fn mark_failed_counts_the_attempt_being_recorded() {
        // `attempts >= 5` (the pre-update value) parks the row on the sixth
        // failure; `attempts + 1 >= 5` parks it on the fifth, which is what
        // MAX_ATTEMPTS says.
        assert!(MARK_FAILED_SQL.contains("attempts + 1 >= $2::int"));
    }

    // ── Live-Postgres tests ───────────────────────────────────────────────

    fn test_dsn() -> String {
        std::env::var("PYLON_PGCON_TEST_DSN").expect("PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests")
    }

    fn unique_type_name(prefix: &str) -> String {
        format!(
            "{prefix}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// Bootstraps the outbox table and returns a listener onto it.
    async fn outbox_listener() -> PgListener {
        let listener = PgListener::connect(&test_dsn(), |_| {}).await.unwrap();
        listener
            .batch_execute("CREATE SCHEMA IF NOT EXISTS _pylon")
            .await
            .unwrap();
        listener
            .batch_execute(pylon_core::stdlib::ddl::INDEX_OUTBOX_DDL)
            .await
            .unwrap();
        listener
    }

    /// Inserts one outbox row. Values are interpolated rather than bound:
    /// they're all test-controlled literals here, and binding against the
    /// enum-typed `status` column would drag this helper into the driver's
    /// parameter-encoding rules, which is not what these tests are about.
    async fn enqueue(listener: &PgListener, type_name: &str, operation: &str, status: &str, claimed_ago_secs: i64) {
        let claimed_at = if claimed_ago_secs == 0 {
            "NULL".to_string()
        } else {
            format!("now() - interval '{claimed_ago_secs} seconds'")
        };
        listener
            .batch_execute(&format!(
                r#"INSERT INTO _pylon."IndexOutbox"
                   (object_id, type_name, index_kind, operation, status, claimed_at)
                   VALUES (gen_random_uuid(), '{type_name}', 'Vector', '{operation}', '{status}', {claimed_at})"#
            ))
            .await
            .unwrap();
    }

    async fn cleanup(listener: &PgListener, type_name: &str) {
        listener
            .batch_execute(&format!(
                r#"DELETE FROM _pylon."IndexOutbox" WHERE type_name = '{type_name}'"#
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn claim_batch_surfaces_the_delete_operation() {
        let listener = outbox_listener().await;
        let type_name = unique_type_name("outbox_op");
        enqueue(&listener, &type_name, "delete", "Pending", 0).await;

        let rows = claim_batch(&listener, "Vector", 10).await.unwrap();
        let row = rows.iter().find(|r| r.type_name == type_name).expect("row not claimed");
        assert_eq!(
            row.operation,
            Operation::Delete,
            "a delete must survive the claim, or the object stays in the index forever"
        );

        cleanup(&listener, &type_name).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn claim_batch_reclaims_a_row_abandoned_past_its_lease() {
        let listener = outbox_listener().await;
        let stale = unique_type_name("outbox_stale");
        let fresh = unique_type_name("outbox_fresh");

        // A worker died holding this one.
        enqueue(
            &listener,
            &stale,
            "index",
            "Processing",
            PROCESSING_LEASE.as_secs() as i64 + 60,
        )
        .await;
        // Another worker is legitimately working on this one right now.
        enqueue(&listener, &fresh, "index", "Processing", 1).await;

        let rows = claim_batch(&listener, "Vector", 50).await.unwrap();
        assert!(
            rows.iter().any(|r| r.type_name == stale),
            "a row abandoned past the lease must be reclaimed, not stranded in Processing"
        );
        assert!(
            !rows.iter().any(|r| r.type_name == fresh),
            "a row still inside its lease belongs to the worker holding it"
        );

        cleanup(&listener, &stale).await;
        cleanup(&listener, &fresh).await;
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn a_row_is_parked_after_exactly_max_attempts_failures() {
        let listener = outbox_listener().await;
        let type_name = unique_type_name("outbox_attempts");
        enqueue(&listener, &type_name, "index", "Pending", 0).await;

        let claimed = claim_batch(&listener, "Vector", 50).await.unwrap();
        let id = claimed
            .iter()
            .find(|r| r.type_name == type_name)
            .expect("row not claimed")
            .id;

        // Failures 1..MAX_ATTEMPTS-1 must keep the row retryable.
        for attempt in 1..MAX_ATTEMPTS {
            let terminal = mark_failed(&listener, vec![DecodedValue::Uuid(id)], "Vector")
                .await
                .unwrap();
            assert_eq!(terminal, 0, "row went terminal early, on attempt {attempt}");
        }

        // The MAX_ATTEMPTS'th failure is the terminal one.
        let terminal = mark_failed(&listener, vec![DecodedValue::Uuid(id)], "Vector")
            .await
            .unwrap();
        assert_eq!(
            terminal, 1,
            "row should be Failed after exactly {MAX_ATTEMPTS} attempts"
        );

        cleanup(&listener, &type_name).await;
    }
}
