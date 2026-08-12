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

//! pyo3 async bindings over `pylon-pgcon` — the real query/execute
//! surface. Parameters convert Python -> `DecodedValue` synchronously,
//! before entering the async block (the GIL is already held there, at the
//! top of a `#[pyfunction]` call); results convert the other way only
//! *after* the future resolves, via `PyDecodedValue`'s `IntoPyObject` impl,
//! which `pyo3_async_runtimes::tokio::future_into_py` calls once it has
//! re-acquired the GIL — so no Python object ever needs to cross the
//! `.await` inside these futures.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use pyo3::prelude::*;
use tokio::sync::Mutex as AsyncMutex;

use pylon_pgcon::{PgListener, PgPool, PgTransaction};
use pylon_value::DecodedValue;

use crate::pgvalue::{cached_to_py, py_to_cached};
use crate::{CompiledQuery, PylonPgconError};

/// Maps a `pylon-pgcon` error to the real `pylon.exceptions.*` class the
/// `client.py` has always raised for the same situation —
/// so once `Client` is wired onto this driver (a later phase), no further
/// exception translation is needed in Python, and user code catching
/// `except pylon.exceptions.TransactionSerializationError` (etc.) keeps
/// working unchanged. Classifies by SQLSTATE (`"40001"` serialization
/// failure, `"40P01"` deadlock detected); anything else —
/// including every other constraint violation — becomes the same generic
/// `QueryError` `_fmt_pg_error` already produces for those today (the
/// hierarchy is preserved as-is in this pass, not redesigned).
pub(crate) fn pgcon_err(err: pylon_pgcon::Error) -> PyErr {
    use tokio_postgres::error::SqlState;

    let sqlstate = err.sqlstate().map(|code| code.code().to_string());
    let class_name = match err.sqlstate() {
        Some(code) if *code == SqlState::T_R_SERIALIZATION_FAILURE => "TransactionSerializationError",
        Some(code) if *code == SqlState::T_R_DEADLOCK_DETECTED => "TransactionDeadlockError",
        _ => "QueryError",
    };
    // The old `_fmt_pg_error` (`client.py`) only ever rewrote the message
    // on the generic `QueryError` path — `SerializationError`/
    // `DeadlockDetectedError` were always raised with the raw message —
    // so this matches that exactly rather than applying it uniformly.
    let message = if class_name == "QueryError" {
        check_violation_message(&err).unwrap_or_else(|| pylonize_pg_message(&err.pg_message()))
    } else {
        err.pg_message()
    };
    Python::attach(|py| {
        let cls = py
            .import("pylon.exceptions")
            .and_then(|m| m.getattr(class_name))
            .expect("pylon.exceptions must define the core exception hierarchy");
        match cls.call1((message,)) {
            Ok(instance) => {
                // The `.sqlstate` attribute (present on
                // every `PostgresError`) — lets callers branch on a precise
                // error code (e.g. `42501` insufficient privilege) without
                // a dedicated exception class per SQLSTATE.
                if let Some(code) = &sqlstate {
                    let _ = instance.setattr("sqlstate", code);
                }
                PyErr::from_value(instance)
            }
            Err(construct_err) => construct_err,
        }
    })
}

/// A friendlier message for a `CHECK_VIOLATION` (SQLSTATE 23514), or
/// `None` for anything else (falls back to `pylonize_pg_message`).
/// Postgres's own message names the *auto-generated* constraint identifier
/// (`value for domain "Email" violates check constraint "Email_check"`) —
/// an internal detail the user never wrote and shouldn't be shown —
/// so this substitutes the scalar type or object type name instead, using
/// the structured `DataTypeName`/`TableName` error fields rather than
/// parsing the message text.
fn check_violation_message(err: &pylon_pgcon::Error) -> Option<String> {
    use tokio_postgres::error::SqlState;
    if err.sqlstate() != Some(&SqlState::CHECK_VIOLATION) {
        return None;
    }
    if let Some((schema, datatype)) = err.violated_scalar() {
        let module = if schema == "public" { "default" } else { schema };
        return Some(format!(
            "value violates a check constraint for scalar type '{module}::{datatype}'"
        ));
    }
    if let Some((schema, table)) = err.violated_table() {
        let module = if schema == "public" { "default" } else { schema };
        return Some(format!(
            "value violates a check constraint for object type '{module}::{table}'"
        ));
    }
    None
}

/// Rewrites Postgres's `"schema"."table"` quoted-identifier notation to
/// Pylon's own `'schema::table'` convention — mirrors the old
/// `_fmt_pg_error`'s regex substitution in `client.py` (`r'"([^"]+)"\."([^"]+)"'`
/// -> `"'{a}::{b}'"`), reimplemented by hand here rather than pulling in
/// the `regex` crate for one narrow, fixed substitution.
fn pylonize_pg_message(msg: &str) -> String {
    let chars: Vec<char> = msg.chars().collect();
    let mut out = String::with_capacity(msg.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some((a, b, next_i)) = match_quoted_pair(&chars, i) {
            out.push('\'');
            out.push_str(&a);
            out.push_str("::");
            out.push_str(&b);
            out.push('\'');
            i = next_i;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// If `chars[start..]` begins with `"<a>"."<b>"` (both `<a>`/`<b>`
/// non-empty and quote-free), returns `(a, b, index just past the match)`.
fn match_quoted_pair(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let mut i = start;
    if *chars.get(i)? != '"' {
        return None;
    }
    i += 1;
    let a_start = i;
    while *chars.get(i)? != '"' {
        i += 1;
    }
    let a: String = chars[a_start..i].iter().collect();
    if a.is_empty() {
        return None;
    }
    i += 1; // past closing quote of a
    if *chars.get(i)? != '.' {
        return None;
    }
    i += 1;
    if *chars.get(i)? != '"' {
        return None;
    }
    i += 1;
    let b_start = i;
    while *chars.get(i)? != '"' {
        i += 1;
    }
    let b: String = chars[b_start..i].iter().collect();
    if b.is_empty() {
        return None;
    }
    i += 1; // past closing quote of b
    Some((a, b, i))
}

#[cfg(test)]
mod message_format_tests {
    use super::pylonize_pg_message;

    #[test]
    fn rewrites_a_single_quoted_pair() {
        assert_eq!(
            pylonize_pg_message(r#"duplicate key value violates unique constraint "person_pkey" on "public"."person""#),
            "duplicate key value violates unique constraint \"person_pkey\" on 'public::person'",
        );
    }

    #[test]
    fn rewrites_multiple_quoted_pairs() {
        assert_eq!(pylonize_pg_message(r#""a"."b" and "c"."d""#), "'a::b' and 'c::d'",);
    }

    #[test]
    fn leaves_a_lone_quoted_identifier_untouched() {
        assert_eq!(
            pylonize_pg_message(r#"column "name" does not exist"#),
            r#"column "name" does not exist"#
        );
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(pylonize_pg_message("no quotes here at all"), "no quotes here at all");
    }
}

/// Maps a `pylon-pgcon` error from `pgcon_connect` specifically — distinct
/// from `pgcon_err` above, which is for errors during query/execute/
/// transaction use on an *already-established* pool. `Client.ensure_connected()`
/// depends on connect failures raising `ConnectionFailedError` (or
/// `ConnectionTimeoutError` for the timeout case), not `QueryError` —
/// splitting "database does not exist" from "cannot reach the server" from
/// "timed out" the same way the triage in `client.py` does, which never maps
/// any connect-time failure to `QueryError`.
fn pgcon_connect_err(err: pylon_pgcon::Error) -> PyErr {
    let class_name = if matches!(err, pylon_pgcon::Error::Pool(deadpool_postgres::PoolError::Timeout(_))) {
        "ConnectionTimeoutError"
    } else {
        "ConnectionFailedError"
    };
    let message = err.pg_message();
    Python::attach(|py| {
        let cls = py
            .import("pylon.exceptions")
            .and_then(|m| m.getattr(class_name))
            .expect("pylon.exceptions must define the core exception hierarchy");
        match cls.call1((message,)) {
            Ok(instance) => PyErr::from_value(instance),
            Err(construct_err) => construct_err,
        }
    })
}

/// Wraps a `DecodedValue` so it can be returned from an async future body
/// and converted to a Python object by `future_into_py` after the GIL is
/// reacquired — the same job `cached_to_py` does, just reachable through
/// `IntoPyObject` instead of called directly (orphan rules mean we can't
/// impl `IntoPyObject` for `pylon_value::DecodedValue` itself here).
pub(crate) struct PyDecodedValue(pub(crate) DecodedValue);

impl<'py> IntoPyObject<'py> for PyDecodedValue {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        cached_to_py(py, &self.0)
    }
}

/// A connected pool.
/// One per `Client` instance, not a process-wide global: `pylon serve`
/// genuinely holds several independently-configured `Client`s at once (one
/// per named multi-tenant connection in `pylon.toml`'s `[connections]`),
/// each against a potentially different database, so a single global pool
/// slot (this module's earlier design, before any Python code depended on
/// it) can't represent that.
#[pyclass(module = "pylon._core", frozen)]
pub struct PgconPool {
    pub(crate) inner: PgPool,
}

#[pymethods]
impl PgconPool {
    /// Runs `sql` with positional `params` (`$1, $2, ...`, matching
    /// `CompiledQuery.param_names`'s own convention) and returns the
    /// decoded `result` column of every row, each ready to feed into
    /// `pylon.query.deserialize()` unmodified — structurally identical to
    /// what a cache hit already reconstructs today.
    fn query<'py>(&self, py: Python<'py>, sql: String, params: Vec<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let rows = pool
                .query_typed(&sql, &cached_params, pool.types())
                .await
                .map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    /// Like `query`, but decodes every column of every row by name (a
    /// Python dict per row) instead of assuming a single `(...) AS result`
    /// column — for hand-written admin SQL that reads named columns
    /// directly, mirroring `PgconListener::query_named`.
    fn query_named<'py>(
        &self,
        py: Python<'py>,
        sql: String,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let rows = pool
                .query_typed_named(&sql, &cached_params, pool.types())
                .await
                .map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    /// Like `query`, but reads the SQL text directly out of an already-
    /// compiled `CompiledQuery` instead of taking it as a Python `str`.
    /// Closes the round trip `Client.query` used to do: compile in Rust →
    /// hand `.sql` to Python as a string → hand that string right back
    /// into `query()`. Here the SQL never leaves Rust-owned memory as a
    /// Python object; only `compiled` (an opaque handle) and the
    /// already-resolved `params` cross the boundary.
    fn query_compiled<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        let sql = compiled.inner.sql.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let result = pool.query_typed(&sql, &cached_params, pool.types()).await;
            pylon_workers::metrics::record_query_result(&result);
            let rows = result.map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    /// Like `query_compiled`, but wraps the compiled SQL in `SELECT
    /// COALESCE(json_agg(q), '[]') FROM (...) q` before executing — the
    /// Rust-side equivalent of `Client.query_json`'s old Python
    /// string-wrapping (`f"SELECT COALESCE(json_agg(q), '[]') FROM
    /// ({sql}) q"`). Still just plain string formatting, but it happens
    /// here instead of in Python, so no Python code ever touches `.sql`.
    fn query_compiled_json_agg<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        let sql = format!("SELECT COALESCE(json_agg(q), '[]') FROM ({}) q", compiled.inner.sql);
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let result = pool.query_typed(&sql, &cached_params, pool.types()).await;
            pylon_workers::metrics::record_query_result(&result);
            let rows = result.map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    /// Like `query_compiled`, but wraps the compiled SQL in `SELECT
    /// row_to_json(q) FROM (... LIMIT 1) q` — the Rust-side equivalent of
    /// `Client.query_single_json`'s second (JSON-materializing) query.
    fn query_compiled_row_to_json<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        let sql = format!("SELECT row_to_json(q) FROM ({} LIMIT 1) q", compiled.inner.sql);
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let result = pool.query_typed(&sql, &cached_params, pool.types()).await;
            pylon_workers::metrics::record_query_result(&result);
            let rows = result.map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    /// Runs `compiled`'s SQL through `EXPLAIN (ANALYZE, FORMAT JSON,
    /// VERBOSE)` and correlates the result against `compiled`'s own
    /// `analyze_paths` (populated only for `analyze <query>` — see
    /// `pylon_core::query::CompiledQuery::analyze_paths`) into the
    /// coarse-grained tree (`pylon_core::analyze::CoarseGrainedNode`),
    /// returned as a JSON string — same convention as
    /// `SchemaDescriptor::to_json` (`lib.rs`): parse with `json.loads()`
    /// Python-side rather than adding a second Rust-to-Python tree-walking
    /// bridge just for this one payload.
    fn analyze_compiled<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        let sql = compiled.inner.sql.clone();
        let path_aliases = compiled.inner.analyze_paths.clone().unwrap_or_default();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let result = pool.query_explain(&sql, &cached_params).await;
            pylon_workers::metrics::record_query_result(&result);
            let raw_json = result.map_err(pgcon_err)?;
            let tree = pylon_core::analyze::build_coarse_grained(&raw_json, &path_aliases)
                .map_err(pyo3::exceptions::PyValueError::new_err)?;
            serde_json::to_string(&tree).map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
        })
    }

    /// Runs `sql` with positional `params` and discards the result,
    /// returning the number of rows affected — for `INSERT`/`UPDATE`/
    /// `DELETE` with no `RETURNING` clause to decode.
    fn execute<'py>(
        &self,
        py: Python<'py>,
        sql: String,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            pool.execute_typed(&sql, &cached_params).await.map_err(pgcon_err)
        })
    }

    /// Like `execute`, but reads the SQL text directly out of an
    /// already-compiled `CompiledQuery` — see `query_compiled`.
    fn execute_compiled<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        let sql = compiled.inner.sql.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let result = pool.execute_typed(&sql, &cached_params).await;
            pylon_workers::metrics::record_query_result(&result);
            result.map_err(pgcon_err)
        })
    }

    /// Starts an explicit transaction on a fresh pooled connection.
    /// `isolation` is one of `"read_uncommitted"`, `"read_committed"`,
    /// `"repeatable_read"`, `"serializable"` — the same values
    /// `AsyncTransaction`/`client.transaction()` already accept today.
    fn transaction<'py>(&self, py: Python<'py>, isolation: String) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let tx = pool.begin(&isolation).await.map_err(pgcon_err)?;
            Ok(PgconTransaction {
                inner: Arc::new(AsyncMutex::new(Some(tx))),
            })
        })
    }

    /// Runs `sql` via the simple query protocol, with no bind parameters —
    /// unlike `query`/`execute` (which prepare via the extended protocol
    /// and so accept exactly one statement), this can run several
    /// `;`-separated statements — including `DO $$ ... $$` blocks — in one
    /// call, matching `Connection.execute(sql)` called with no
    /// arguments. For admin DDL blobs like `export_stdlib()`'s output
    /// (`database initialize`) or a `watch`-computed diff op list, not
    /// something meant to be split and bound.
    fn batch_execute<'py>(&self, py: Python<'py>, sql: String) -> PyResult<Bound<'py, PyAny>> {
        let pool = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move { pool.batch_execute(&sql).await.map_err(pgcon_err) })
    }
}

/// Opens a new pool against `dsn`, returning a `PgconPool` handle.
#[pyfunction]
fn pgcon_connect(py: Python<'_>, dsn: String, max_size: usize) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let pool = PgPool::connect(&dsn, max_size).await.map_err(pgcon_connect_err)?;
        Ok(PgconPool { inner: pool })
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
            let rows = tx
                .query_typed(&sql, &cached_params, tx.types())
                .await
                .map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    fn execute<'py>(
        &self,
        py: Python<'py>,
        sql: String,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let guard = inner.lock().await;
            let tx = guard.as_ref().ok_or_else(closed_tx_err)?;
            tx.execute_typed(&sql, &cached_params).await.map_err(pgcon_err)
        })
    }

    /// See `PgconPool::query_compiled` — same "read SQL straight out of an
    /// already-compiled `CompiledQuery`" fusion, on a transaction handle.
    fn query_compiled<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let sql = compiled.inner.sql.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let guard = inner.lock().await;
            let tx = guard.as_ref().ok_or_else(closed_tx_err)?;
            let result = tx.query_typed(&sql, &cached_params, tx.types()).await;
            pylon_workers::metrics::record_query_result(&result);
            let rows = result.map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    /// See `PgconPool::execute_compiled`.
    fn execute_compiled<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let sql = compiled.inner.sql.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let guard = inner.lock().await;
            let tx = guard.as_ref().ok_or_else(closed_tx_err)?;
            let result = tx.execute_typed(&sql, &cached_params).await;
            pylon_workers::metrics::record_query_result(&result);
            result.map_err(pgcon_err)
        })
    }

    /// See `PgconPool::query_compiled_json_agg`.
    fn query_compiled_json_agg<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let sql = format!("SELECT COALESCE(json_agg(q), '[]') FROM ({}) q", compiled.inner.sql);
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let guard = inner.lock().await;
            let tx = guard.as_ref().ok_or_else(closed_tx_err)?;
            let result = tx.query_typed(&sql, &cached_params, tx.types()).await;
            pylon_workers::metrics::record_query_result(&result);
            let rows = result.map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    /// See `PgconPool::query_compiled_row_to_json`.
    fn query_compiled_row_to_json<'py>(
        &self,
        py: Python<'py>,
        compiled: &CompiledQuery,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let sql = format!("SELECT row_to_json(q) FROM ({} LIMIT 1) q", compiled.inner.sql);
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let guard = inner.lock().await;
            let tx = guard.as_ref().ok_or_else(closed_tx_err)?;
            let result = tx.query_typed(&sql, &cached_params, tx.types()).await;
            pylon_workers::metrics::record_query_result(&result);
            let rows = result.map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }

    /// See `PgconPool::batch_execute` — same simple-protocol,
    /// multi-statement capability, run inside this transaction instead of
    /// on a fresh connection.
    fn batch_execute<'py>(&self, py: Python<'py>, sql: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let guard = inner.lock().await;
            let tx = guard.as_ref().ok_or_else(closed_tx_err)?;
            tx.batch_execute(&sql).await.map_err(pgcon_err)
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

/// One callback per channel — every real call site in `pylon.worker`/
/// `pylon.cache` registers exactly one listener per channel on its own
/// dedicated connection, so this doesn't need to support a more
/// general multi-callback-per-channel case.
type CallbackRegistry = Arc<StdMutex<HashMap<String, Py<PyAny>>>>;

/// A dedicated LISTEN/NOTIFY connection, exposed close enough to
/// the `add_listener`/`remove_listener`/`execute`/`fetch`
/// that `pylon.worker.IndexWorker` and `pylon.cache.CacheInvalidationWorker`
/// need minimal changes to run on it (a later phase's job — this phase
/// only builds and verifies the primitive).
#[pyclass(module = "pylon._core")]
struct PgconListener {
    inner: Arc<PgListener>,
    callbacks: CallbackRegistry,
}

#[pymethods]
impl PgconListener {
    /// `add_listener(channel, callback)`:
    /// `callback` is invoked as `callback(None, pid, channel, payload)` —
    /// `None` stands in for the leading `connection` argument, which
    /// every existing callback in this codebase already ignores (both are
    /// named `_conn`).
    fn add_listener<'py>(&self, py: Python<'py>, channel: String, callback: Py<PyAny>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let callbacks = self.callbacks.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            inner.listen(&channel).await.map_err(pgcon_err)?;
            callbacks.lock().unwrap().insert(channel, callback);
            Ok(())
        })
    }

    fn execute<'py>(
        &self,
        py: Python<'py>,
        sql: String,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            inner.execute_typed(&sql, &cached_params).await.map_err(pgcon_err)
        })
    }

    /// Like `query`, but decodes every column of every row by name into a
    /// dict — supporting `row["col"]` access, unlike
    /// `query` (which only ever decodes column 0, the convention PyQL-
    /// compiled SQL uses). For hand-written admin/worker queries with
    /// several named columns.
    fn query_named<'py>(
        &self,
        py: Python<'py>,
        sql: String,
        params: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let cached_params = params.iter().map(py_to_cached).collect::<PyResult<Vec<_>>>()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let rows = inner
                .query_typed_named(&sql, &cached_params, inner.types())
                .await
                .map_err(pgcon_err)?;
            Ok(rows.into_iter().map(PyDecodedValue).collect::<Vec<_>>())
        })
    }
}

/// Opens a new, non-pooled connection dedicated to LISTEN/NOTIFY (plus
/// ordinary queries on the same connection, mirroring how
/// `IndexWorker`/`CacheInvalidationWorker` use their one connection for
/// both today).
#[pyfunction]
fn pgcon_listen(py: Python<'_>, dsn: String) -> PyResult<Bound<'_, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let callbacks: CallbackRegistry = Arc::new(StdMutex::new(HashMap::new()));
        let dispatch_callbacks = callbacks.clone();
        let listener = PgListener::connect(&dsn, move |n| {
            Python::attach(|py| {
                let callback = dispatch_callbacks
                    .lock()
                    .unwrap()
                    .get(n.channel())
                    .map(|cb| cb.clone_ref(py));
                let Some(callback) = callback else { return };
                let args = (
                    py.None(),
                    n.process_id(),
                    n.channel().to_string(),
                    n.payload().to_string(),
                );
                if let Err(e) = callback.call1(py, args) {
                    e.print(py);
                }
            });
        })
        .await
        .map_err(pgcon_err)?;
        Ok(PgconListener {
            inner: Arc::new(listener),
            callbacks,
        })
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pgcon_connect, m)?)?;
    m.add_class::<PgconPool>()?;
    m.add_class::<PgconTransaction>()?;
    m.add_function(wrap_pyfunction!(pgcon_listen, m)?)?;
    m.add_class::<PgconListener>()?;
    Ok(())
}
