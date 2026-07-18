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

use pylon_pgcon::{ExtensionOids, PgListener};
use pylon_value::CachedValue;

use crate::error::{Error, Result};

const NOTIFY_CHANNEL: &str = "pylon_index_queue";

const CLAIM_BATCH_SQL: &str = r#"
UPDATE _pylon."IndexOutbox"
SET status = 'Processing'
WHERE id IN (
    SELECT id FROM _pylon."IndexOutbox"
    WHERE index_kind = $1::_pylon."IndexKind"
      AND status = 'Pending'
      AND (next_attempt IS NULL OR next_attempt <= now())
    ORDER BY enqueued_at
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
RETURNING id, object_id, type_name, index_name, attempts
"#;

const MARK_DONE_SQL: &str = r#"DELETE FROM _pylon."IndexOutbox" WHERE id = ANY($1::uuid[])"#;

const MARK_FAILED_SQL: &str = r#"
UPDATE _pylon."IndexOutbox"
SET status = CASE WHEN attempts >= 5
                  THEN 'Failed'::_pylon."IndexOutboxStatus"
                  ELSE 'Pending'::_pylon."IndexOutboxStatus"
             END,
    attempts = attempts + 1,
    next_attempt = now() + (30 * 2^LEAST(attempts, 4) || ' seconds')::interval
WHERE id = ANY($1::uuid[])
"#;

/// One claimed `_pylon."IndexOutbox"` row. `attempts` isn't carried —
/// nothing downstream of `claim_batch` reads it, matching the Python
/// worker's own claimed-row usage.
#[derive(Debug, Clone)]
pub struct ClaimedRow {
    pub id: [u8; 16],
    pub object_id: [u8; 16],
    pub type_name: String,
    pub index_name: Option<String>,
}

/// Implemented by each index kind (`VectorIndexWorker`, and later the
/// Meilisearch/OpenSearch workers) — mirrors `IndexWorker.process_batch`.
pub trait BatchProcessor: Send + Sync {
    /// The `index_kind` value stored in `_pylon."IndexOutbox"` for this
    /// worker (`"Vector"`, `"OpenSearch"`, `"Meilisearch"`).
    fn index_kind(&self) -> &'static str;

    fn process_batch(&self, listener: &PgListener, rows: &[ClaimedRow]) -> impl std::future::Future<Output = Result<()>> + Send;
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
        let ids: Vec<CachedValue> = rows.iter().map(|r| CachedValue::Uuid(r.id)).collect();

        match processor.process_batch(listener, &rows).await {
            Ok(()) => {
                if let Err(e) = listener.execute_typed(MARK_DONE_SQL, &[CachedValue::Array(ids)]).await {
                    eprintln!("IndexWorker({}): mark_done failed: {e}", processor.index_kind());
                }
            }
            Err(e) => {
                eprintln!("IndexWorker({}): batch failed, scheduling retry: {e}", processor.index_kind());
                if let Err(e) = listener.execute_typed(MARK_FAILED_SQL, &[CachedValue::Array(ids)]).await {
                    eprintln!("IndexWorker({}): mark_failed failed: {e}", processor.index_kind());
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
            &[CachedValue::Str(index_kind.to_string()), CachedValue::I64(limit)],
            &ExtensionOids::default(),
        )
        .await?;
    rows.iter().map(decode_claimed_row).collect()
}

fn decode_claimed_row(value: &CachedValue) -> Result<ClaimedRow> {
    let CachedValue::Object(fields) = value else {
        return Err(Error::Decode("claim_batch: expected a named-column row".into()));
    };
    let field = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);

    let id = match field("id") {
        Some(CachedValue::Uuid(b)) => *b,
        _ => return Err(Error::Decode("claim_batch: missing/invalid 'id'".into())),
    };
    let object_id = match field("object_id") {
        Some(CachedValue::Uuid(b)) => *b,
        _ => return Err(Error::Decode("claim_batch: missing/invalid 'object_id'".into())),
    };
    let type_name = match field("type_name") {
        Some(CachedValue::Str(s)) => s.clone(),
        _ => return Err(Error::Decode("claim_batch: missing/invalid 'type_name'".into())),
    };
    let index_name = match field("index_name") {
        Some(CachedValue::Str(s)) => Some(s.clone()),
        Some(CachedValue::Null) | None => None,
        _ => return Err(Error::Decode("claim_batch: invalid 'index_name'".into())),
    };

    Ok(ClaimedRow { id, object_id, type_name, index_name })
}
