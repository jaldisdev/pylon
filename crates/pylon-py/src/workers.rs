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

//! pyo3 bindings over `pylon-workers` — native replacements for
//! `pylon.worker`/`pylon.cache`'s LISTEN/NOTIFY-driven background workers.
//! Each binding here is a coroutine with the same call shape its Python
//! predecessor had (one awaitable to run alongside the others via
//! `asyncio.gather` in `pylon worker start`), so the CLI wiring doesn't
//! need to change until the dedicated re-architecture phase.

use std::collections::HashMap;
use std::time::Duration;

use pyo3::exceptions::{PyNotImplementedError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use pylon_workers::ProviderConfig;

use crate::pgcon::pgcon_err;
use crate::{PylonCacheError, SchemaDescriptor};

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
/// handle at `cache_path`; for a process (like `pylon serve`) that already
/// has one open via `cache_init`, use `run_cache_invalidation_worker_shared`
/// instead — LMDB refuses a second `Env::open` on the same path within one
/// process.
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
/// `crate::cache`) instead of opening a second one — for `pylon serve`,
/// which runs its own read-through cache *and* this worker in one process.
/// Errors immediately if `cache_init` hasn't run yet in this process.
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

/// Runs the native `VectorIndexWorker` claim/embed/write loop until the
/// returned coroutine is cancelled — the Rust-native replacement for
/// `VectorIndexWorker(conn, schema=schema, providers=providers).run()`.
/// `providers` is `[(type_name, index_name, api_style, api_url, model,
/// api_key), ...]` — the already-resolved `[models.*]` entry for each
/// `(type_name, index_name)` pair `_build_providers` would have looked up;
/// `pylon.toml` parsing itself stays in Python.
#[pyfunction]
#[pyo3(signature = (dsn, schema, providers, batch_size=50, poll_interval_secs=30.0))]
fn run_vector_worker<'py>(
    py: Python<'py>,
    dsn: String,
    schema: &SchemaDescriptor,
    providers: Vec<(String, Option<String>, String, String, String, Option<String>)>,
    batch_size: i64,
    poll_interval_secs: f64,
) -> PyResult<Bound<'py, PyAny>> {
    let schema = schema.inner.clone();
    let provider_map: HashMap<(String, Option<String>), ProviderConfig> = providers
        .into_iter()
        .map(|(type_name, index_name, api_style, api_url, model, api_key)| {
            (
                (type_name, index_name),
                ProviderConfig {
                    api_style,
                    api_url,
                    model,
                    api_key,
                },
            )
        })
        .collect();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let worker = pylon_workers::VectorIndexWorker::new(schema, provider_map).map_err(workers_err)?;
        pylon_workers::index_worker::run(&dsn, batch_size, Duration::from_secs_f64(poll_interval_secs), worker)
            .await
            .map_err(workers_err)
    })
}

/// Runs the native `MeilisearchIndexWorker` (`SearchIndexWorker<MeilisearchClient>`)
/// claim/fetch/index loop until the returned coroutine is cancelled — the
/// Rust-native replacement for `MeilisearchWorker(conn, schema=schema,
/// client=client).run()`.
#[pyfunction]
#[pyo3(signature = (dsn, schema, base_url, api_key=None, batch_size=50, poll_interval_secs=30.0, timeout_secs=10.0))]
fn run_meilisearch_worker<'py>(
    py: Python<'py>,
    dsn: String,
    schema: &SchemaDescriptor,
    base_url: String,
    api_key: Option<String>,
    batch_size: i64,
    poll_interval_secs: f64,
    timeout_secs: f64,
) -> PyResult<Bound<'py, PyAny>> {
    let schema = schema.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let client =
            pylon_workers::MeilisearchClient::new(&base_url, api_key.as_deref(), Duration::from_secs_f64(timeout_secs))
                .map_err(workers_err)?;
        let worker = pylon_workers::SearchIndexWorker::new(schema, client, "Meilisearch");
        pylon_workers::index_worker::run(&dsn, batch_size, Duration::from_secs_f64(poll_interval_secs), worker)
            .await
            .map_err(workers_err)
    })
}

/// Runs the native `OpenSearchIndexWorker` (`SearchIndexWorker<OpenSearchClient>`)
/// claim/fetch/index loop until the returned coroutine is cancelled — the
/// Rust-native replacement for `OpenSearchWorker(conn, schema=schema,
/// client=client).run()`.
#[pyfunction]
#[pyo3(signature = (dsn, schema, base_url, user=None, password=None, batch_size=50, poll_interval_secs=30.0, timeout_secs=10.0))]
fn run_opensearch_worker<'py>(
    py: Python<'py>,
    dsn: String,
    schema: &SchemaDescriptor,
    base_url: String,
    user: Option<String>,
    password: Option<String>,
    batch_size: i64,
    poll_interval_secs: f64,
    timeout_secs: f64,
) -> PyResult<Bound<'py, PyAny>> {
    let schema = schema.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let auth = user.as_deref().zip(password.as_deref());
        let client = pylon_workers::OpenSearchClient::new(&base_url, auth, Duration::from_secs_f64(timeout_secs))
            .map_err(workers_err)?;
        let worker = pylon_workers::SearchIndexWorker::new(schema, client, "OpenSearch");
        pylon_workers::index_worker::run(&dsn, batch_size, Duration::from_secs_f64(poll_interval_secs), worker)
            .await
            .map_err(workers_err)
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(run_cache_invalidation_worker, m)?)?;
    m.add_function(wrap_pyfunction!(run_cache_invalidation_worker_shared, m)?)?;
    m.add_function(wrap_pyfunction!(run_vector_worker, m)?)?;
    m.add_function(wrap_pyfunction!(run_meilisearch_worker, m)?)?;
    m.add_function(wrap_pyfunction!(run_opensearch_worker, m)?)?;
    Ok(())
}
