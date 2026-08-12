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
        prometheus::opts!(
            "pylon_worker_jobs_processed_total",
            "Outbox rows successfully processed, by index kind"
        ),
        &["index_kind"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

pub static JOBS_FAILED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!(
            "pylon_worker_jobs_failed_total",
            "Outbox rows that failed and were scheduled for retry, by index kind"
        ),
        &["index_kind"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

/// Rows that exhausted every retry and are now parked as `Failed`.
///
/// Separate from `JOBS_FAILED`, which ticks on every failed attempt
/// including ones that will retry — so on its own it can't distinguish a
/// backend that's briefly flapping from work that has permanently given up.
/// This one only ever moves when something needs a human.
pub static JOBS_ABANDONED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!(
            "pylon_worker_jobs_abandoned_total",
            "Outbox rows that exhausted every retry and are now Failed, by index kind"
        ),
        &["index_kind"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

/// Rows sitting in the outbox right now, by index kind and status.
///
/// Sampled on scrape rather than updated on write, so it reports a real
/// queue depth even for an index kind whose worker never started — a
/// misconfigured `[search]`/`[models.*]` section otherwise accumulates
/// silently, with no worker to increment any counter.
pub static OUTBOX_DEPTH: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!(
            "pylon_index_outbox_depth",
            "Rows currently in _pylon.\"IndexOutbox\", by index kind and status"
        ),
        &["index_kind", "status"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
});

/// Age in seconds of the oldest unprocessed outbox row, by index kind.
/// Climbs without bound when no worker is consuming that kind.
pub static OUTBOX_OLDEST_PENDING_AGE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!(
            "pylon_index_outbox_oldest_pending_age_seconds",
            "Age of the oldest row not yet processed, by index kind"
        ),
        &["index_kind"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
});

pub static CACHE_INVALIDATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!(
            "pylon_cache_invalidations_total",
            "Cache entries evicted by tag, by outcome"
        ),
        &["outcome"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

pub static CACHE_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!("pylon_cache_requests_total", "Read-through cache lookups, by outcome"),
        &["outcome"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(counter.clone()))
        .unwrap();
    counter
});

pub static QUERIES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let counter = IntCounterVec::new(
        prometheus::opts!("pylon_queries_total", "PyQL queries, by pipeline stage (compile/execute) and outcome — a compile failure (bad PyQL, unknown type/field) never reaches the execute stage"),
        &["stage", "outcome"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(counter.clone()))
        .unwrap();
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
        prometheus::opts!(
            "pylon_pgcon_pool_size",
            "Connections currently established (idle + checked out), by connection name"
        ),
        &["connection"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
});

static POOL_AVAILABLE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!(
            "pylon_pgcon_pool_available",
            "Idle connections available for immediate checkout, by connection name"
        ),
        &["connection"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
});

static POOL_WAITING: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!(
            "pylon_pgcon_pool_waiting",
            "Callers currently blocked waiting for a connection, by connection name"
        ),
        &["connection"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(gauge.clone()))
        .unwrap();
    gauge
});

static POOL_MAX_SIZE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    let gauge = IntGaugeVec::new(
        prometheus::opts!(
            "pylon_pgcon_pool_max_size",
            "Configured pool capacity, by connection name"
        ),
        &["connection"],
    )
    .unwrap();
    prometheus::default_registry()
        .register(Box::new(gauge.clone()))
        .unwrap();
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
    POOL_AVAILABLE
        .with_label_values(&[connection])
        .set(status.available as i64);
    POOL_WAITING.with_label_values(&[connection]).set(status.waiting as i64);
    POOL_MAX_SIZE
        .with_label_values(&[connection])
        .set(status.max_size as i64);
}

/// Renders every metric registered so far (across every crate that shares
/// this process's `prometheus::default_registry()`) as Prometheus text
/// exposition format.
/// Never panics: `/metrics` is served from the same process as the
/// application, so an encoding fault here must degrade to an empty scrape
/// rather than take down the request path.
pub fn render() -> String {
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    if let Err(e) = TextEncoder::new().encode(&metric_families, &mut buffer) {
        eprintln!("metrics: failed to encode the registry: {e}");
        return String::new();
    }
    match String::from_utf8(buffer) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("metrics: encoder produced non-UTF-8 output: {e}");
            String::new()
        }
    }
}

/// Samples `_pylon."IndexOutbox"` into `OUTBOX_DEPTH` and
/// `OUTBOX_OLDEST_PENDING_AGE`. Called on scrape rather than on a timer, so
/// it costs nothing when nobody is looking.
///
/// Reports every `IndexKind` the enum declares, not just the ones with rows,
/// so a queue that is filling up with no worker to drain it reads as a
/// rising number rather than as an absent series.
pub const OUTBOX_DEPTH_SQL: &str = r#"
SELECT k.kind::text AS index_kind,
       s.status::text AS status,
       COALESCE(o.n, 0)::int8 AS depth,
       COALESCE(o.oldest_age, 0)::int8 AS oldest_age
FROM unnest(enum_range(NULL::_pylon."IndexKind")) AS k(kind)
CROSS JOIN unnest(enum_range(NULL::_pylon."IndexOutboxStatus")) AS s(status)
LEFT JOIN (
    SELECT index_kind, status, count(*) AS n,
           EXTRACT(EPOCH FROM (now() - min(enqueued_at))) AS oldest_age
    FROM _pylon."IndexOutbox"
    GROUP BY index_kind, status
) o ON o.index_kind = k.kind AND o.status = s.status
"#;

/// Applies one `OUTBOX_DEPTH_SQL` row to the gauges.
pub fn record_outbox_depth(index_kind: &str, status: &str, depth: i64, oldest_age: i64) {
    OUTBOX_DEPTH.with_label_values(&[index_kind, status]).set(depth);
    if status == "Pending" {
        OUTBOX_OLDEST_PENDING_AGE
            .with_label_values(&[index_kind])
            .set(oldest_age);
    }
}
