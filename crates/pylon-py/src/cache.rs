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

//! pyo3 bindings over `pylon-cache`.
//!
//! Thin glue only: encode/decode between Python's already-asyncpg-decoded
//! `record["result"]` values and `pylon_cache::DecodedValue`, plus a single
//! process-global `Cache` handle. No shape/type knowledge is needed here —
//! the cache stores a structural mirror of whatever Python value it was
//! given and hands back an equivalent one, and the *existing* `_decode()`/
//! `_hydrate()` in `pylon/query.py` (driven by `CompiledQuery.shape`) does
//! the real interpretation on both the write and the read side.

use std::sync::{Arc, OnceLock, RwLock};

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use pylon_cache::{Cache, cache_key as cache_key_impl};

use crate::PylonCacheError;
use crate::pgvalue::{cached_to_py, py_to_cached};

static PYLON_CACHE: OnceLock<RwLock<Option<Arc<Cache>>>> = OnceLock::new();

fn cache_slot() -> &'static RwLock<Option<Arc<Cache>>> {
    PYLON_CACHE.get_or_init(|| RwLock::new(None))
}

fn cache_err<E: std::fmt::Display>(e: E) -> PyErr {
    PylonCacheError::new_err(e.to_string())
}

/// The process-global `Cache` handle `cache_init` opened, if any — shared
/// with `workers::run_cache_invalidation_worker_shared` so `pylon serve`
/// (which now also runs the cache-invalidation worker in-process, see
/// `pylon/server/asgi.py`) evicts through the *same* open LMDB environment
/// its own read-through cache uses, rather than a second `Cache::open` on
/// the same path — LMDB refuses that within one process.
pub(crate) fn shared_cache() -> Option<Arc<Cache>> {
    cache_slot().read().unwrap().clone()
}

/// Opens (or reopens) the process-global LMDB-backed cache at `path`.
#[pyfunction]
fn cache_init(path: &str, max_size_mb: usize) -> PyResult<()> {
    let cache = Cache::open(std::path::Path::new(path), max_size_mb).map_err(cache_err)?;
    *cache_slot().write().unwrap() = Some(Arc::new(cache));
    Ok(())
}

/// Returns the cached rows for `key` (each ready to feed directly into
/// `_decode(row, shape, registry)`), or `None` on a cache miss. The single
/// choke point every read-through cache lookup goes through — both
/// `pylon.cache.get` and `.get_json` call this same primitive — so this is
/// where the hit/miss counters (`pylon_workers::metrics::CACHE_REQUESTS`)
/// live, rather than duplicated in each Python caller.
#[pyfunction]
fn cache_get<'py>(py: Python<'py>, key: &str) -> PyResult<Option<Bound<'py, PyList>>> {
    let guard = cache_slot().read().unwrap();
    let cache = guard
        .as_ref()
        .ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
    let Some(entry) = cache.get(key).map_err(cache_err)? else {
        pylon_workers::metrics::CACHE_REQUESTS
            .with_label_values(&["miss"])
            .inc();
        return Ok(None);
    };
    pylon_workers::metrics::CACHE_REQUESTS.with_label_values(&["hit"]).inc();
    let rows = entry
        .rows
        .iter()
        .map(|row| cached_to_py(py, row))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(Some(PyList::new(py, rows)?))
}

/// Caches `rows` (each `record["result"]` from asyncpg, already decoded)
/// under `key`, tagged with `tags` for later invalidation.
#[pyfunction]
fn cache_put(key: &str, tags: Vec<String>, rows: Vec<Bound<'_, PyAny>>) -> PyResult<()> {
    let rows = rows.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
    let guard = cache_slot().read().unwrap();
    let cache = guard
        .as_ref()
        .ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
    cache.put(key, rows, tags).map_err(cache_err)
}

/// Evicts every cache entry tagged with any of `tags`.
#[pyfunction]
fn cache_invalidate(tags: Vec<String>) -> PyResult<()> {
    let guard = cache_slot().read().unwrap();
    let cache = guard
        .as_ref()
        .ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
    cache.invalidate(&tags).map_err(cache_err)
}

/// `sha256(sql) + bound parameter values`, hex-encoded — the single source
/// of truth for cache-key derivation (see `pylon_cache::store::cache_key`),
/// so Python never re-implements the hashing scheme independently.
#[pyfunction]
fn cache_key(sql: &str, params: Vec<Bound<'_, PyAny>>) -> PyResult<String> {
    let params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
    cache_key_impl(sql, &params).map_err(cache_err)
}

/// Current cache size — `{"entry_count": int, "used_bytes": int}` — for the
/// `pylon cache status` CLI command.
#[pyfunction]
fn cache_stat<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
    let guard = cache_slot().read().unwrap();
    let cache = guard
        .as_ref()
        .ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
    let stats = cache.stat().map_err(cache_err)?;
    let d = PyDict::new(py);
    d.set_item("entry_count", stats.entry_count)?;
    d.set_item("used_bytes", stats.used_bytes)?;
    Ok(d)
}

/// Evicts every cache entry — for the `pylon cache purge` CLI command.
#[pyfunction]
fn cache_clear() -> PyResult<()> {
    let guard = cache_slot().read().unwrap();
    let cache = guard
        .as_ref()
        .ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
    cache.clear().map_err(cache_err)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(cache_init, m)?)?;
    m.add_function(wrap_pyfunction!(cache_get, m)?)?;
    m.add_function(wrap_pyfunction!(cache_put, m)?)?;
    m.add_function(wrap_pyfunction!(cache_invalidate, m)?)?;
    m.add_function(wrap_pyfunction!(cache_key, m)?)?;
    m.add_function(wrap_pyfunction!(cache_stat, m)?)?;
    m.add_function(wrap_pyfunction!(cache_clear, m)?)?;
    Ok(())
}
