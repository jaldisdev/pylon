//! pyo3 bindings over `pylon-cache`.
//!
//! Thin glue only: encode/decode between Python's already-asyncpg-decoded
//! `record["result"]` values and `pylon_cache::CachedValue`, plus a single
//! process-global `Cache` handle. No shape/type knowledge is needed here —
//! the cache stores a structural mirror of whatever Python value it was
//! given and hands back an equivalent one, and the *existing* `_decode()`/
//! `_hydrate()` in `pylon/query.py` (driven by `CompiledQuery.shape`) does
//! the real interpretation on both the write and the read side.

use std::sync::{OnceLock, RwLock};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};

use pylon_cache::{cache_key as cache_key_impl, Cache, CachedValue};

use crate::PylonCacheError;

static PYLON_CACHE: OnceLock<RwLock<Option<Cache>>> = OnceLock::new();

fn cache_slot() -> &'static RwLock<Option<Cache>> {
    PYLON_CACHE.get_or_init(|| RwLock::new(None))
}

fn cache_err<E: std::fmt::Display>(e: E) -> PyErr {
    PylonCacheError::new_err(e.to_string())
}

/// Opens (or reopens) the process-global LMDB-backed cache at `path`.
#[pyfunction]
fn cache_init(path: &str, max_size_mb: usize) -> PyResult<()> {
    let cache = Cache::open(std::path::Path::new(path), max_size_mb).map_err(cache_err)?;
    *cache_slot().write().unwrap() = Some(cache);
    Ok(())
}

/// Returns the cached rows for `key` (each ready to feed directly into
/// `_decode(row, shape, registry)`), or `None` on a cache miss.
#[pyfunction]
fn cache_get<'py>(py: Python<'py>, key: &str) -> PyResult<Option<Bound<'py, PyList>>> {
    let guard = cache_slot().read().unwrap();
    let cache = guard.as_ref().ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
    let Some(entry) = cache.get(key).map_err(cache_err)? else {
        return Ok(None);
    };
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
    let cache = guard.as_ref().ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
    cache.put(key, rows, tags).map_err(cache_err)
}

/// Evicts every cache entry tagged with any of `tags`.
#[pyfunction]
fn cache_invalidate(tags: Vec<String>) -> PyResult<()> {
    let guard = cache_slot().read().unwrap();
    let cache = guard.as_ref().ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
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
    let cache = guard.as_ref().ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
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
    let cache = guard.as_ref().ok_or_else(|| PylonCacheError::new_err("cache not initialized; call cache_init() first"))?;
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

/// Encodes an already-asyncpg-decoded Python value into `CachedValue`.
/// Runtime-type-driven, not shape-driven — the shape is only consulted
/// later, by the unmodified `_decode()` on both the miss and the hit path.
fn py_to_cached(value: &Bound<'_, PyAny>) -> PyResult<CachedValue> {
    let py = value.py();

    if value.is_none() {
        return Ok(CachedValue::Null);
    }
    // Order matters: `bool` is a subclass of `int` in Python.
    if let Ok(b) = value.cast::<PyBool>() {
        return Ok(CachedValue::Bool(b.is_true()));
    }
    if let Ok(i) = value.cast::<PyInt>() {
        return Ok(CachedValue::I64(i.extract()?));
    }
    if let Ok(f) = value.cast::<PyFloat>() {
        return Ok(CachedValue::F64(f.extract()?));
    }
    if let Ok(s) = value.cast::<PyString>() {
        return Ok(CachedValue::Str(s.extract()?));
    }
    if let Ok(b) = value.cast::<PyBytes>() {
        return Ok(CachedValue::Bytes(b.as_bytes().to_vec()));
    }
    if value.is_instance(&py.import("uuid")?.getattr("UUID")?)? {
        let raw: Vec<u8> = value.getattr("bytes")?.extract()?;
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&raw);
        return Ok(CachedValue::Uuid(bytes));
    }
    if value.is_instance(&py.import("decimal")?.getattr("Decimal")?)? {
        return Ok(CachedValue::Decimal(value.str()?.extract()?));
    }
    if let Ok(d) = value.cast::<PyDict>() {
        let entries = d
            .iter()
            .map(|(k, v)| Ok((k.extract::<String>()?, py_to_cached(&v)?)))
            .collect::<PyResult<Vec<_>>>()?;
        return Ok(CachedValue::Object(entries));
    }
    // Postgres arrays (Python `list`) and composite/record tuples (asyncpg
    // `Record`, Python `tuple`) both just need positional reconstruction —
    // encode either as `CachedValue::Array`; the shape (applied later by
    // `_decode`) is what tells them apart on the read side.
    if let Ok(len) = value.len() {
        let items = (0..len).map(|i| py_to_cached(&value.get_item(i)?)).collect::<PyResult<Vec<_>>>()?;
        return Ok(CachedValue::Array(items));
    }
    Err(PyValueError::new_err(format!(
        "cannot cache a value of type {}",
        value.get_type().name()?
    )))
}

/// Reconstructs a Python value from `CachedValue`, structurally equivalent
/// to what asyncpg would have decoded — safe to feed into the existing
/// `_decode()`/`_hydrate()` exactly as if it came from a live query.
fn cached_to_py<'py>(py: Python<'py>, value: &CachedValue) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        CachedValue::Null => py.None().into_bound(py),
        CachedValue::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        CachedValue::I64(i) => PyInt::new(py, *i).into_any(),
        CachedValue::F64(f) => PyFloat::new(py, *f).into_any(),
        CachedValue::Str(s) => PyString::new(py, s).into_any(),
        CachedValue::Bytes(b) => PyBytes::new(py, b).into_any(),
        CachedValue::Uuid(bytes) => {
            // Passed as a hex string (not a `bytes` kwarg) to avoid needing
            // an extra crate just for keyword-argument construction here.
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            py.import("uuid")?.getattr("UUID")?.call1((hex,))?
        }
        CachedValue::Decimal(s) => py.import("decimal")?.getattr("Decimal")?.call1((s,))?,
        CachedValue::Array(items) => {
            let elems = items.iter().map(|v| cached_to_py(py, v)).collect::<PyResult<Vec<_>>>()?;
            PyTuple::new(py, elems)?.into_any()
        }
        CachedValue::Object(entries) => {
            let d = PyDict::new(py);
            for (k, v) in entries {
                d.set_item(k, cached_to_py(py, v)?)?;
            }
            d.into_any()
        }
    })
}
