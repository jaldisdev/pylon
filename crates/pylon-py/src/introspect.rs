//! pyo3 binding over `pylon_core::introspect` — replaces
//! `pylon.schema._introspect.introspect_db_state`'s asyncpg-based
//! implementation with the Rust one.

use pyo3::prelude::*;

use pylon_core as core;

use crate::pgcon::{pgcon_err, PgconPool};
use crate::DbState;

/// Query pg_catalog and return a `DbState` describing the live database.
#[pyfunction]
fn introspect_db_state<'py>(py: Python<'py>, pool: &PgconPool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let state = core::introspect::introspect_db_state(&pool).await.map_err(pgcon_err)?;
        Ok(DbState { inner: state })
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(introspect_db_state, m)?)?;
    Ok(())
}
