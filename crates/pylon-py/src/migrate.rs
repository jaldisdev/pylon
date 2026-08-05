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

//! pyo3 bindings over `pylon_core::migrate` — the Rust-executed half of
//! `pylon migration apply` (advisory lock, tracking tables, per-migration
//! step execution). `pylon.cli.commands.migrations`'s Python `_apply` still
//! owns the outer loop (chain resolution, `--to` targeting, `click.echo`
//! progress output) and calls these primitives per migration, the same
//! shape `pgcon.rs`'s bindings already established for plain queries.

use std::sync::Arc;

use pyo3::prelude::*;
use tokio::sync::Mutex as AsyncMutex;

use pylon_core::migrate as core_migrate;

use crate::pgcon::{pgcon_err, PgconPool};
use crate::MigrationFile;

fn migrate_err(err: core_migrate::MigrateError) -> PyErr {
    match err {
        core_migrate::MigrateError::Integrity(e) => pyo3::exceptions::PyValueError::new_err(e.to_string()),
        core_migrate::MigrateError::Db(e) => pgcon_err(e),
    }
}

#[pyfunction]
fn migration_ensure_tracking_tables<'py>(py: Python<'py>, pool: &PgconPool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move { core_migrate::ensure_tracking_tables(&pool).await.map_err(migrate_err) })
}

/// Returns every `_pylon."Migrations"` row as `(id, onto, db_state,
/// schema_state, applied)` tuples — `applied` is `applied_at IS NOT NULL`;
/// `db_state` and `schema_state` are raw JSON snapshot text (or `None`),
/// ready for `db_state_from_json`/`SchemaDescriptor.from_json`
/// respectively. `filename` isn't included — nothing in this codebase
/// reads it once a row exists.
#[pyfunction]
fn migration_read_tracking<'py>(py: Python<'py>, pool: &PgconPool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let tracking = core_migrate::read_tracking(&pool).await.map_err(migrate_err)?;
        Ok(tracking.into_iter().map(|r| (r.id, r.onto, r.db_state, r.schema_state, r.applied)).collect::<Vec<_>>())
    })
}

/// Computes the tip ID from `(id, onto, db_state, schema_state, applied)`
/// tracking rows (the one with no descendant) — a pure function, no I/O.
#[pyfunction]
fn migration_applied_tip(tracking: Vec<(String, String, Option<String>, Option<String>, bool)>) -> Option<String> {
    let rows: Vec<core_migrate::TrackingRow> = tracking
        .into_iter()
        .map(|(id, onto, db_state, schema_state, applied)| core_migrate::TrackingRow { id, onto, db_state, schema_state, applied })
        .collect();
    core_migrate::applied_tip(&rows)
}

/// Records a migration as applied without running its DDL — the
/// squash-backfill case in `apply`'s outer loop (a migration whose
/// squashed constituent IDs are already applied under the old chain).
#[pyfunction]
fn migration_record_applied<'py>(py: Python<'py>, pool: &PgconPool, id: String, onto: String, filename: String) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        core_migrate::record_applied(&pool, &id, &onto, &filename).await.map_err(migrate_err)
    })
}

/// Applies one migration's steps in order (resuming from recorded progress,
/// running dev-mode savepoint retry when `dev_mode` is set) — see
/// `pylon_core::migrate::apply_one`.
#[pyfunction]
fn migration_apply_one<'py>(py: Python<'py>, pool: &PgconPool, m: &MigrationFile, dev_mode: bool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    let migration = m.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move { core_migrate::apply_one(&pool, &migration, dev_mode).await.map_err(migrate_err) })
}

/// A held advisory-lock connection — see `pylon_core::migrate::advisory_lock`
/// for why the lock and its release must share one connection. `unlock`
/// takes the inner connection, so a second call fails cleanly instead of
/// reusing an already-released handle.
#[pyclass(module = "pylon._core")]
struct MigrationLock {
    inner: Arc<AsyncMutex<Option<pylon_pgcon::PgConnection>>>,
}

#[pymethods]
impl MigrationLock {
    fn unlock<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let conn = inner.lock().await.take().ok_or_else(|| {
                pyo3::exceptions::PyRuntimeError::new_err("migration lock already released")
            })?;
            core_migrate::advisory_unlock(conn).await.map_err(migrate_err)
        })
    }
}

/// Blocks until the migration advisory lock is acquired.
#[pyfunction]
fn migration_advisory_lock<'py>(py: Python<'py>, pool: &PgconPool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let conn = core_migrate::advisory_lock(&pool).await.map_err(migrate_err)?;
        Ok(MigrationLock { inner: Arc::new(AsyncMutex::new(Some(conn))) })
    })
}

/// Attempts to acquire the migration advisory lock without blocking;
/// returns `None` if another `apply` already holds it.
#[pyfunction]
fn migration_try_advisory_lock<'py>(py: Python<'py>, pool: &PgconPool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        match core_migrate::try_advisory_lock(&pool).await.map_err(migrate_err)? {
            Some(conn) => Ok(Some(MigrationLock { inner: Arc::new(AsyncMutex::new(Some(conn))) })),
            None => Ok(None),
        }
    })
}

/// Upserts the process-wide schema snapshot (`_pylon."Schema"`) every
/// client fetches at startup instead of `.pylon/schema.json` — called by
/// `apply` after a migration lands and by `watch` after a dev-mode sync.
#[pyfunction]
fn migration_write_schema_snapshot<'py>(py: Python<'py>, pool: &PgconPool, snapshot_json: String) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        core_migrate::write_schema_snapshot(&pool, &snapshot_json).await.map_err(migrate_err)
    })
}

/// Reads the current schema snapshot back — `None` if neither `migration
/// apply` nor `watch` has ever run against this database. Pair with
/// `SchemaDescriptor.from_json` to install the *migrated* schema as the
/// query-compilation singleton (`pylon.query._set_schema`), rather than
/// trusting whatever `pylon.finalize()` built from the current `.py` files —
/// see `pylon.finalize()`'s own doc comment for why those can differ.
#[pyfunction]
fn migration_read_schema_snapshot<'py>(py: Python<'py>, pool: &PgconPool) -> PyResult<Bound<'py, PyAny>> {
    let pool = pool.inner.clone();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        core_migrate::read_schema_snapshot(&pool).await.map_err(migrate_err)
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(migration_ensure_tracking_tables, m)?)?;
    m.add_function(wrap_pyfunction!(migration_read_tracking, m)?)?;
    m.add_function(wrap_pyfunction!(migration_applied_tip, m)?)?;
    m.add_function(wrap_pyfunction!(migration_record_applied, m)?)?;
    m.add_function(wrap_pyfunction!(migration_apply_one, m)?)?;
    m.add_function(wrap_pyfunction!(migration_advisory_lock, m)?)?;
    m.add_function(wrap_pyfunction!(migration_try_advisory_lock, m)?)?;
    m.add_function(wrap_pyfunction!(migration_write_schema_snapshot, m)?)?;
    m.add_function(wrap_pyfunction!(migration_read_schema_snapshot, m)?)?;
    m.add_class::<MigrationLock>()?;
    Ok(())
}
