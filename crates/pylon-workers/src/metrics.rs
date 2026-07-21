//! Prometheus counters for the background workers, registered into the
//! `prometheus` crate's process-global default registry — every worker
//! (`index_worker::drain_once`, `CacheInvalidationWorker`) increments these
//! directly, and `render()` dumps the whole registry as Prometheus text
//! exposition format. `pylon-py` exposes `render()` to Python as
//! `render_prometheus_metrics()`, served at `pylon serve`'s `/metrics`
//! route (`pylon/server/asgi.py`) — since `pylon serve` now launches these
//! same workers in-process (see `build_worker_tasks` in asgi.py), one
//! scrape target sees both HTTP- and worker-side metrics with no separate
//! listener needed.

use std::sync::LazyLock;

use prometheus::{Encoder, IntCounterVec, TextEncoder};

pub static JOBS_PROCESSED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!("pylon_worker_jobs_processed_total", "Outbox rows successfully processed, by index kind"),
        &["index_kind"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(counter.clone())).unwrap();
    counter
});

pub static JOBS_FAILED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!("pylon_worker_jobs_failed_total", "Outbox rows that failed and were scheduled for retry, by index kind"),
        &["index_kind"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(counter.clone())).unwrap();
    counter
});

pub static CACHE_INVALIDATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!("pylon_cache_invalidations_total", "Cache entries evicted by tag, by outcome"),
        &["outcome"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(counter.clone())).unwrap();
    counter
});

/// Renders every metric registered so far (across every crate that shares
/// this process's `prometheus::default_registry()`) as Prometheus text
/// exposition format.
pub fn render() -> String {
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    TextEncoder::new().encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
