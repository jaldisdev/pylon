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

//! pyo3 binding over `pylon_core::introspect` — replaces
//! `pylon.schema._introspect.introspect_db_state`'s Python
//! implementation with the Rust one.

use pyo3::prelude::*;

use pylon_core as core;

use crate::pgcon::{PgconPool, pgcon_err};
use crate::{DbDump, DbState};

/// Query pg_catalog and return a `DbState` describing the live database.
#[pyfunction]
fn introspect_db_state<'py>(py: Python<'py>, pool: &PgconPool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let state = core::introspect::introspect_db_state(&pool).await.map_err(pgcon_err)?;
        Ok(DbState { inner: state })
    })
}

/// Query pg_catalog and return a `DbDump`: the whole structure of the live
/// database, every object's own DDL included. For `pylon migration squash`,
/// which has no compiled schema to render anything from — see
/// `pylon_core::diff::dump`.
#[pyfunction]
fn introspect_db_dump<'py>(py: Python<'py>, pool: &PgconPool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let dump = core::introspect::introspect_db_dump(&pool).await.map_err(pgcon_err)?;
        Ok(DbDump { inner: dump })
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(introspect_db_state, m)?)?;
    m.add_function(wrap_pyfunction!(introspect_db_dump, m)?)?;
    Ok(())
}
