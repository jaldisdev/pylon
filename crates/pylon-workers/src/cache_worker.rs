//! Native port of `pylon.cache.CacheInvalidationWorker` — LISTENs on
//! `pylon_cache_invalidate` and evicts matching cache entries. Unlike
//! `pylon.worker.IndexWorker` there's no outbox table to drain from: the
//! NOTIFY payload *is* the tag to invalidate (a schema-qualified table
//! name, written by the `_pylon.notify_cache_invalidate()` trigger), so
//! eviction happens directly from the notification callback.
//!
//! The Python version batches pending tags behind an `asyncio.Lock`-guarded
//! drain loop because its notification callback is synchronous but must
//! hand off to an async task to run the (conceptually async) eviction
//! without blocking the event loop. Here `PgListener`'s callback already
//! runs on a background task outside any event loop, and `Cache::invalidate`
//! is a plain synchronous call — so there's nothing to hand off to; each
//! notification is evicted immediately, with no separate drain step needed.
//! LMDB serializes writers regardless of how many transactions the work is
//! split across, so this isn't a behavior change, just fewer moving parts.

use std::path::Path;
use std::sync::Arc;

use pylon_cache::Cache;
use pylon_pgcon::PgListener;

use crate::error::Result;

pub const NOTIFY_CHANNEL: &str = "pylon_cache_invalidate";

pub struct CacheInvalidationWorker {
    // Held only to keep the listener's background task (and its dedicated
    // connection) alive for the worker's lifetime — never queried directly.
    _listener: PgListener,
    // Kept alongside (not just moved into the notification closure) so a
    // caller — tests, or a future `cache status` sharing this process —
    // can read the same handle back. LMDB refuses a second `Env::open` on
    // the same path within one process (`EnvAlreadyOpened`), so this is the
    // only way anything else in this process can observe the worker's
    // eviction effects deterministically.
    cache: Arc<Cache>,
}

impl CacheInvalidationWorker {
    /// Opens a dedicated LISTEN/NOTIFY connection against `dsn` and this
    /// worker's own LMDB handle at `cache_path` — LMDB supports safe
    /// concurrent multi-process access to one file, so this worker process
    /// evicting entries is immediately visible to every serving process
    /// mapping the same path (matches the old Python worker's own
    /// reasoning for opening its own handle rather than sharing one).
    pub async fn connect(dsn: &str, cache_path: &Path, max_size_mb: usize) -> Result<Self> {
        let cache = Arc::new(Cache::open(cache_path, max_size_mb)?);
        let cache_for_listener = cache.clone();
        let listener = PgListener::connect(dsn, move |n| {
            if let Err(e) = cache_for_listener.invalidate(&[n.payload().to_string()]) {
                eprintln!("CacheInvalidationWorker: eviction failed for tag {:?}: {e}", n.payload());
            }
        })
        .await?;
        listener.listen(NOTIFY_CHANNEL).await?;
        Ok(Self { _listener: listener, cache })
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Never returns — keeps the worker alive. Run alongside other workers
    /// (see `worker start`'s orchestration) rather than awaited alone; the
    /// caller is expected to cancel/drop this future to stop the worker.
    pub async fn run(&self) {
        std::future::pending::<()>().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dsn() -> String {
        std::env::var("PYLON_PGCON_TEST_DSN").unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5418/app".to_string())
    }

    /// Postgres NOTIFY channels are global to the database, not scoped to a
    /// single test — every `CacheInvalidationWorker` in this test binary
    /// LISTENs on the same `pylon_cache_invalidate` channel against the
    /// same real database, so tests running concurrently would otherwise
    /// evict each other's entries if they shared a tag name. A
    /// nanosecond-timestamp-suffixed tag keeps each test's NOTIFY isolated,
    /// the same pattern used elsewhere in this codebase's real-DB tests.
    fn unique_tag(prefix: &str) -> String {
        format!("{prefix}_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
    }

    #[tokio::test]
    #[ignore]
    async fn a_notify_on_the_channel_evicts_the_tagged_entry() {
        let dir = tempfile::tempdir().unwrap();
        let worker = CacheInvalidationWorker::connect(&test_dsn(), dir.path(), 10).await.unwrap();
        let tag = unique_tag("cache_worker_test_evict");
        // Seed one cache entry tagged `tag` via the worker's own handle —
        // LMDB refuses a second `Env::open` on the same path within one
        // process, so this is the only handle a test can safely use once
        // the worker is alive (see `CacheInvalidationWorker::cache`).
        worker.cache().put("k1", vec![pylon_value::CachedValue::I64(1)], vec![tag.clone()]).unwrap();
        assert!(worker.cache().get("k1").unwrap().is_some());

        let notifier = pylon_pgcon::PgPool::connect(&test_dsn(), 1).await.unwrap();
        notifier.query_raw(&format!("NOTIFY pylon_cache_invalidate, '{tag}'")).await.unwrap();

        // Eviction happens on the notification callback, asynchronously
        // relative to this test — poll briefly instead of assuming it's
        // already done the instant NOTIFY returns.
        let mut evicted = false;
        for _ in 0..50 {
            if worker.cache().get("k1").unwrap().is_none() {
                evicted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(evicted, "entry tagged {tag:?} should have been evicted");
    }

    #[tokio::test]
    #[ignore]
    async fn a_notify_on_an_unrelated_tag_leaves_the_entry_alone() {
        let dir = tempfile::tempdir().unwrap();
        let worker = CacheInvalidationWorker::connect(&test_dsn(), dir.path(), 10).await.unwrap();
        let tag = unique_tag("cache_worker_test_untouched");
        worker.cache().put("k1", vec![pylon_value::CachedValue::I64(1)], vec![tag]).unwrap();

        let notifier = pylon_pgcon::PgPool::connect(&test_dsn(), 1).await.unwrap();
        notifier.query_raw("NOTIFY pylon_cache_invalidate, 'public.other_table'").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(worker.cache().get("k1").unwrap().is_some());
    }
}
