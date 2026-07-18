//! pyo3 bindings over `pylon-workers` — native replacements for
//! `pylon.worker`/`pylon.cache`'s LISTEN/NOTIFY-driven background workers.
//! Each binding here is a coroutine with the same call shape its Python
//! predecessor had (one awaitable to run alongside the others via
//! `asyncio.gather` in `pylon worker start`), so the CLI wiring doesn't
//! need to change until the dedicated re-architecture phase.

use pyo3::prelude::*;

use crate::pgcon::pgcon_err;
use crate::PylonCacheError;

fn workers_err(err: pylon_workers::Error) -> PyErr {
    match err {
        pylon_workers::Error::Pgcon(e) => pgcon_err(e),
        pylon_workers::Error::Cache(msg) => PylonCacheError::new_err(msg),
    }
}

/// Connects a `CacheInvalidationWorker` (LISTEN `pylon_cache_invalidate`,
/// evict matching LMDB entries) and runs it until the returned coroutine is
/// cancelled — the Rust-native replacement for
/// `pylon.cache.CacheInvalidationWorker(conn).run()`.
#[pyfunction]
fn run_cache_invalidation_worker(py: Python<'_>, dsn: String, cache_path: String, max_size_mb: usize) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let worker = pylon_workers::CacheInvalidationWorker::connect(&dsn, std::path::Path::new(&cache_path), max_size_mb)
            .await
            .map_err(workers_err)?;
        worker.run().await;
        Ok(())
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(run_cache_invalidation_worker, m)?)?;
    Ok(())
}
