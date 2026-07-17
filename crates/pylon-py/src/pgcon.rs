//! pyo3 async bindings over `pylon-pgcon`.
//!
//! This is driver-migration phase 3: prove the async bridge itself works
//! end to end (persistent tokio runtime, real asyncio awaitables, a pool
//! that survives across calls) before building the real
//! `CachedValue`-decoding query path in a later phase. `pgcon_query_scalar_i64`
//! is a narrow validation surface, not the eventual public API.

use std::sync::{OnceLock, RwLock};

use pyo3::prelude::*;

use pylon_pgcon::PgPool;

use crate::PylonPgconError;

static PYLON_PGCON: OnceLock<RwLock<Option<PgPool>>> = OnceLock::new();

fn pgcon_slot() -> &'static RwLock<Option<PgPool>> {
    PYLON_PGCON.get_or_init(|| RwLock::new(None))
}

fn pgcon_err<E: std::fmt::Display>(e: E) -> PyErr {
    PylonPgconError::new_err(e.to_string())
}

/// Clones the process-global pool out from under its lock. A
/// `RwLockReadGuard` isn't `Send`, so it can't be held across the `.await`
/// inside `future_into_py`'s future — `PgPool` itself is cheaply cloneable
/// (an `Arc`-backed handle), so callers take an owned copy instead.
fn cloned_pool() -> PyResult<PgPool> {
    pgcon_slot()
        .read()
        .unwrap()
        .clone()
        .ok_or_else(|| PylonPgconError::new_err("pgcon not connected; call pgcon_connect() first"))
}

/// Connects (or reconnects) the process-global connection pool.
#[pyfunction]
fn pgcon_connect(py: Python<'_>, dsn: String, max_size: usize) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let pool = PgPool::connect(&dsn, max_size).await.map_err(pgcon_err)?;
        *pgcon_slot().write().unwrap() = Some(pool);
        Ok(())
    })
}

/// Column 0 of every row as `int` — validation-only surface for the pyo3
/// async boundary; superseded by real `CachedValue`-based decoding once
/// that phase lands.
#[pyfunction]
fn pgcon_query_scalar_i64(py: Python<'_>, sql: String) -> PyResult<Bound<'_, PyAny>> {
    let pool = cloned_pool()?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        pool.query_scalar_i64(&sql).await.map_err(pgcon_err)
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pgcon_connect, m)?)?;
    m.add_function(wrap_pyfunction!(pgcon_query_scalar_i64, m)?)?;
    Ok(())
}
