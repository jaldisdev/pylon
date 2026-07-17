//! pyo3 async bindings over `pylon-pgcon` — the real query/execute
//! surface. Parameters convert Python -> `CachedValue` synchronously,
//! before entering the async block (the GIL is already held there, at the
//! top of a `#[pyfunction]` call); results convert the other way only
//! *after* the future resolves, via `PyCachedValue`'s `IntoPyObject` impl,
//! which `pyo3_async_runtimes::tokio::future_into_py` calls once it has
//! re-acquired the GIL — so no Python object ever needs to cross the
//! `.await` inside these futures.

use std::sync::{Arc, OnceLock, RwLock};

use pyo3::prelude::*;
use tokio::sync::Mutex as AsyncMutex;

use pylon_pgcon::{ExtensionOids, PgPool, PgTransaction};
use pylon_value::CachedValue;

use crate::pgvalue::{cached_to_py, py_to_cached};
use crate::PylonPgconError;

static PYLON_PGCON: OnceLock<RwLock<Option<PgPool>>> = OnceLock::new();

fn pgcon_slot() -> &'static RwLock<Option<PgPool>> {
    PYLON_PGCON.get_or_init(|| RwLock::new(None))
}

/// Maps a `pylon-pgcon` error to the real `pylon.exceptions.*` class the
/// old asyncpg-based `client.py` already raised for the same situation —
/// so once `Client` is wired onto this driver (a later phase), no further
/// exception translation is needed in Python, and user code catching
/// `except pylon.exceptions.TransactionSerializationError` (etc.) keeps
/// working unchanged. Classifies by SQLSTATE exactly like asyncpg's own
/// typed exceptions do (`asyncpg.SerializationError.sqlstate == "40001"`,
/// `asyncpg.DeadlockDetectedError.sqlstate == "40P01"`); anything else —
/// including every other constraint violation — becomes the same generic
/// `QueryError` `_fmt_pg_error` already produces for those today (the
/// hierarchy is preserved as-is in this pass, not redesigned).
fn pgcon_err(err: pylon_pgcon::Error) -> PyErr {
    use tokio_postgres::error::SqlState;

    let class_name = match err.sqlstate() {
        Some(code) if *code == SqlState::T_R_SERIALIZATION_FAILURE => "TransactionSerializationError",
        Some(code) if *code == SqlState::T_R_DEADLOCK_DETECTED => "TransactionDeadlockError",
        _ => "QueryError",
    };
    Python::attach(|py| {
        let cls = py
            .import("pylon.exceptions")
            .and_then(|m| m.getattr(class_name))
            .expect("pylon.exceptions must define the core exception hierarchy");
        match cls.call1((err.to_string(),)) {
            Ok(instance) => PyErr::from_value(instance),
            Err(construct_err) => construct_err,
        }
    })
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

/// Wraps a `CachedValue` so it can be returned from an async future body
/// and converted to a Python object by `future_into_py` after the GIL is
/// reacquired — the same job `cached_to_py` does, just reachable through
/// `IntoPyObject` instead of called directly (orphan rules mean we can't
/// impl `IntoPyObject` for `pylon_value::CachedValue` itself here).
struct PyCachedValue(CachedValue);

impl<'py> IntoPyObject<'py> for PyCachedValue {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        cached_to_py(py, &self.0)
    }
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

/// Runs `sql` with positional `params` (`$1, $2, ...`, matching
/// `CompiledQuery.param_names`'s own convention) and returns the decoded
/// `result` column of every row, each ready to feed into
/// `pylon.query.deserialize()` unmodified — structurally identical to
/// what a cache hit already reconstructs today.
#[pyfunction]
fn pgcon_query<'py>(py: Python<'py>, sql: String, params: Vec<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
    let pool = cloned_pool()?;
    let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let rows = pool.query_typed(&sql, &cached_params, &ExtensionOids::default()).await.map_err(pgcon_err)?;
        Ok(rows.into_iter().map(PyCachedValue).collect::<Vec<_>>())
    })
}

/// Runs `sql` with positional `params` and discards the result, returning
/// the number of rows affected — for `INSERT`/`UPDATE`/`DELETE` with no
/// `RETURNING` clause to decode.
#[pyfunction]
fn pgcon_execute<'py>(py: Python<'py>, sql: String, params: Vec<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
    let pool = cloned_pool()?;
    let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        pool.execute_typed(&sql, &cached_params).await.map_err(pgcon_err)
    })
}

/// One explicit transaction on a connection checked out of the pool.
/// Wraps its `PgTransaction` in an `Arc<tokio::sync::Mutex<..>>` (not a
/// `std::sync::Mutex` — the guard needs to be held across `.await` inside
/// each async method's future, which a std guard can't do since it isn't
/// `Send`) so `query`/`execute`/`commit`/`rollback` can each be called as
/// independent async methods from Python while still serializing access to
/// the single underlying connection. `commit`/`rollback` `.take()` the
/// `Option`, so a second call on an already-closed transaction fails
/// cleanly instead of reusing a consumed connection.
#[pyclass(module = "pylon._core")]
struct PgconTransaction {
    inner: Arc<AsyncMutex<Option<PgTransaction>>>,
}

fn closed_tx_err() -> PyErr {
    PylonPgconError::new_err("transaction already committed or rolled back")
}

#[pymethods]
impl PgconTransaction {
    fn query<'py>(&self, py: Python<'py>, sql: String, params: Vec<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let guard = inner.lock().await;
            let tx = guard.as_ref().ok_or_else(closed_tx_err)?;
            let rows = tx.query_typed(&sql, &cached_params, &ExtensionOids::default()).await.map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyCachedValue).collect::<Vec<_>>())
        })
    }

    fn execute<'py>(&self, py: Python<'py>, sql: String, params: Vec<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let guard = inner.lock().await;
            let tx = guard.as_ref().ok_or_else(closed_tx_err)?;
            tx.execute_typed(&sql, &cached_params).await.map_err(pgcon_err)
        })
    }

    fn commit<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let tx = inner.lock().await.take().ok_or_else(closed_tx_err)?;
            tx.commit().await.map_err(pgcon_err)
        })
    }

    fn rollback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let tx = inner.lock().await.take().ok_or_else(closed_tx_err)?;
            tx.rollback().await.map_err(pgcon_err)
        })
    }
}

/// Starts an explicit transaction on a fresh pooled connection. `isolation`
/// is one of `"read_uncommitted"`, `"read_committed"`, `"repeatable_read"`,
/// `"serializable"` — the same values `AsyncTransaction`/`client.transaction()`
/// already accept today.
#[pyfunction]
fn pgcon_transaction(py: Python<'_>, isolation: String) -> PyResult<Bound<'_, PyAny>> {
    let pool = cloned_pool()?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let tx = pool.begin(&isolation).await.map_err(pgcon_err)?;
        Ok(PgconTransaction { inner: Arc::new(AsyncMutex::new(Some(tx))) })
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pgcon_connect, m)?)?;
    m.add_function(wrap_pyfunction!(pgcon_query, m)?)?;
    m.add_function(wrap_pyfunction!(pgcon_execute, m)?)?;
    m.add_function(wrap_pyfunction!(pgcon_transaction, m)?)?;
    m.add_class::<PgconTransaction>()?;
    Ok(())
}
