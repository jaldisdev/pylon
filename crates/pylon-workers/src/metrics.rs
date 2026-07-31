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

use prometheus::{Encoder, IntCounterVec, IntGaugeVec, TextEncoder};

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

pub static CACHE_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!("pylon_cache_requests_total", "Read-through cache lookups, by outcome"),
        &["outcome"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(counter.clone())).unwrap();
    counter
});

pub static QUERIES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!("pylon_queries_total", "PyQL queries, by pipeline stage (compile/execute) and outcome — a compile failure (bad PyQL, unknown type/field) never reaches the execute stage"),
        &["stage", "outcome"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(counter.clone())).unwrap();
    counter
});

/// Increments `QUERIES{stage="execute"}` for `outcome` — "success" or
/// "error" depending on `result`. Generic over `T`/`E` so every
/// `*_compiled` pgcon binding (`crates/pylon-py/src/pgcon.rs`) can call
/// this the same way regardless of what it returns, right after the
/// `.await` and before `map_err` consumes the `Result`.
pub fn record_query_result<T, E>(result: &Result<T, E>) {
    let outcome = if result.is_ok() { "success" } else { "error" };
    QUERIES.with_label_values(&["execute", outcome]).inc();
}

/// Increments `QUERIES{stage="compile"}` for `outcome` — called from
/// `pylon.client._compile_and_resolve`/`_compile_and_bind` (the actual
/// query-serving compile step; not every `pylon.query.compile()` caller,
/// e.g. the LSP, counts as a "served" query).
pub fn record_compile_result(success: bool) {
    let outcome = if success { "success" } else { "error" };
    QUERIES.with_label_values(&["compile", outcome]).inc();
}

static POOL_SIZE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!("pylon_pgcon_pool_size", "Connections currently established (idle + checked out), by connection name"),
        &["connection"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(gauge.clone())).unwrap();
    gauge
});

static POOL_AVAILABLE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!("pylon_pgcon_pool_available", "Idle connections available for immediate checkout, by connection name"),
        &["connection"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(gauge.clone())).unwrap();
    gauge
});

static POOL_WAITING: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!("pylon_pgcon_pool_waiting", "Callers currently blocked waiting for a connection, by connection name"),
        &["connection"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(gauge.clone())).unwrap();
    gauge
});

static POOL_MAX_SIZE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!("pylon_pgcon_pool_max_size", "Configured pool capacity, by connection name"),
        &["connection"],
    )
    .unwrap();
    prometheus::default_registry().register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// Samples a pgcon pool's current connection accounting into the pool
/// gauges above, labeled by `connection` (e.g. "default", or a named
/// `[connections.*]` entry — see `pylon-server`'s `AppState::clients`).
/// Gauges, not counters: called fresh on every `/metrics` scrape (from
/// `AppState::record_pool_metrics`) rather than updated as pool events
/// happen, since deadpool doesn't expose checkout/return as observable
/// events, only a point-in-time `status()` snapshot.
pub fn record_pool_status(connection: &str, status: &pylon_pgcon::PoolStatus) {
    POOL_SIZE.with_label_values(&[connection]).set(status.size as i64);
    POOL_AVAILABLE.with_label_values(&[connection]).set(status.available as i64);
    POOL_WAITING.with_label_values(&[connection]).set(status.waiting as i64);
    POOL_MAX_SIZE.with_label_values(&[connection]).set(status.max_size as i64);
}

/// Renders every metric registered so far (across every crate that shares
/// this process's `prometheus::default_registry()`) as Prometheus text
/// exposition format.
pub fn render() -> String {
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    TextEncoder::new().encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
