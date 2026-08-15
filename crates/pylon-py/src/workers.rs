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

//! pyo3 bindings over `pylon-workers` — the cache-invalidation worker as a
//! coroutine Python can run alongside its own tasks.
//!
//! Cache invalidation is here, and the index workers are not, because of
//! where each one has to run rather than what it is written in. Invalidation
//! evicts from an LMDB environment on local disk, so it has to reach the
//! cache the process next to it actually reads — which means Python needs a
//! way to start it. The index workers claim outbox rows the database
//! arbitrates, so they can run anywhere, and `pylon-server` runs them.

use pyo3::exceptions::{PyNotImplementedError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use crate::PylonCacheError;
use crate::pgcon::pgcon_err;

fn workers_err(err: pylon_workers::Error) -> PyErr {
    match err {
        pylon_workers::Error::Pgcon(e) => pgcon_err(e),
        pylon_workers::Error::Cache(msg) => PylonCacheError::new_err(msg),
        pylon_workers::Error::Providers(e) => PyRuntimeError::new_err(format!("model provider request failed: {e}")),
        pylon_workers::Error::Decode(msg) => PyRuntimeError::new_err(msg),
        pylon_workers::Error::Schema(msg) => PyValueError::new_err(msg),
        pylon_workers::Error::Unsupported(msg) => PyNotImplementedError::new_err(msg),
        pylon_workers::Error::Http(e) => PyRuntimeError::new_err(format!("search backend request failed: {e}")),
    }
}

/// Connects a `CacheInvalidationWorker` (LISTEN `pylon_cache_invalidate`,
/// evict matching LMDB entries) and runs it until the returned coroutine is
/// cancelled — the Rust-native replacement for
/// `pylon.cache.CacheInvalidationWorker(conn).run()`. Opens its own LMDB
/// handle at `cache_path` — for a worker process alongside the application
/// whose cache it evicts from, which works because LMDB allows concurrent
/// access to one file across processes. A process that already has the
/// handle open via `cache_init` (an application running this worker inline)
/// must use `run_cache_invalidation_worker_shared` instead, since LMDB
/// refuses a second `Env::open` on the same path within one process.
#[pyfunction]
fn run_cache_invalidation_worker(
    py: Python<'_>,
    dsn: String,
    cache_path: String,
    max_size_mb: usize,
) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let worker =
            pylon_workers::CacheInvalidationWorker::connect(&dsn, std::path::Path::new(&cache_path), max_size_mb)
                .await
                .map_err(workers_err)?;
        worker.run().await;
        Ok(())
    })
}

/// Like `run_cache_invalidation_worker`, but attaches to the process-global
/// `Cache` handle `pylon.cache.init()` already opened (via `cache_init`,
/// `crate::cache`) instead of opening a second one — for an application
/// that runs its own read-through cache *and* this worker in one process
/// (`pylon.workers.run_workers`). Errors immediately if `cache_init` hasn't
/// run yet in this process, which `pylon.workers` turns into a message
/// naming the connect-first ordering that would have avoided it.
#[pyfunction]
fn run_cache_invalidation_worker_shared(py: Python<'_>, dsn: String) -> PyResult<Bound<'_, PyAny>> {
    let cache = crate::cache::shared_cache()
        .ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let worker = pylon_workers::CacheInvalidationWorker::connect_with_cache(&dsn, cache)
            .await
            .map_err(workers_err)?;
        worker.run().await;
        Ok(())
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(run_cache_invalidation_worker, m)?)?;
    m.add_function(wrap_pyfunction!(run_cache_invalidation_worker_shared, m)?)?;
    Ok(())
}
