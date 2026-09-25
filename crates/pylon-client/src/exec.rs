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

//! Shared query-execution logic for [`crate::Client`] and
//! [`crate::Transaction`] — both expose the same set of query methods
//! (mirrors how `pylon/client.py`'s `Client` and `AsyncTransaction` share
//! `_compile_and_bind`), differing only in what actually runs the SQL
//! (a pooled connection vs. an already-open transaction).

use std::collections::HashMap;

use pylon_core::ir::SessionConfig;
use pylon_core::query::{CompiledQuery, compile_with_config};
use pylon_core::schema::SchemaDescriptor;
use pylon_value::DecodedValue;
use std::sync::Arc;

use crate::decode::decode;
use crate::error::{Error, Result};
use crate::value::Value;

/// Whatever can run compiled SQL — a pooled connection (`PgPool`) or an
/// open transaction (`PgTransaction`). Kept minimal: just the three
/// primitives every query method above it is built from.
pub(crate) trait Executor {
    /// `globals`, when set, is what a database trigger reads its session
    /// globals from — see `trigger_globals`.
    async fn run_query(
        &self,
        sql: &str,
        params: &[DecodedValue],
        globals: Option<&str>,
    ) -> pylon_pgcon::Result<Vec<DecodedValue>>;
    async fn run_execute(&self, sql: &str, params: &[DecodedValue], globals: Option<&str>) -> pylon_pgcon::Result<u64>;
    async fn run_explain(&self, sql: &str, params: &[DecodedValue]) -> pylon_pgcon::Result<String>;
}

impl Executor for pylon_pgcon::PgPool {
    async fn run_query(
        &self,
        sql: &str,
        params: &[DecodedValue],
        globals: Option<&str>,
    ) -> pylon_pgcon::Result<Vec<DecodedValue>> {
        match globals {
            Some(globals) => self.query_typed_with_globals(sql, params, self.types(), globals).await,
            None => self.query_typed(sql, params, self.types()).await,
        }
    }
    async fn run_execute(&self, sql: &str, params: &[DecodedValue], globals: Option<&str>) -> pylon_pgcon::Result<u64> {
        match globals {
            Some(globals) => self.execute_typed_with_globals(sql, params, globals).await,
            None => self.execute_typed(sql, params).await,
        }
    }
    async fn run_explain(&self, sql: &str, params: &[DecodedValue]) -> pylon_pgcon::Result<String> {
        self.query_explain(sql, params).await
    }
}

impl Executor for pylon_pgcon::PgTransaction {
    async fn run_query(
        &self,
        sql: &str,
        params: &[DecodedValue],
        globals: Option<&str>,
    ) -> pylon_pgcon::Result<Vec<DecodedValue>> {
        match globals {
            Some(globals) => self.query_typed_with_globals(sql, params, self.types(), globals).await,
            None => self.query_typed(sql, params, self.types()).await,
        }
    }
    async fn run_execute(&self, sql: &str, params: &[DecodedValue], globals: Option<&str>) -> pylon_pgcon::Result<u64> {
        match globals {
            Some(globals) => self.execute_typed_with_globals(sql, params, globals).await,
            None => self.execute_typed(sql, params).await,
        }
    }
    async fn run_explain(&self, sql: &str, params: &[DecodedValue]) -> pylon_pgcon::Result<String> {
        // `PgTransaction` has no dedicated EXPLAIN helper (`analyze` inside
        // an explicit transaction isn't a scenario the Python client
        // supports either — `Client.analyze` only ever runs on the pool).
        let _ = (sql, params);
        let boxed: Box<dyn std::error::Error + Send + Sync> =
            "analyze is not supported inside an explicit transaction".into();
        Err(pylon_pgcon::Error::from(boxed))
    }
}

/// Compiles `pyql` and resolves its `param_names` into positional
/// `DecodedValue`s — `__global__`-prefixed names are filled from `globals`
/// (missing = `NULL`, matching `pylon/client.py`'s own `.get(qname)`
/// default), everything else from `params` (missing = a hard error, unlike
/// globals — mirrors `client.py:670-678`).
pub(crate) fn compile_and_bind(
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
) -> Result<(Arc<CompiledQuery>, Vec<DecodedValue>)> {
    let compiled = compile_with_config(pyql, schema, config).map_err(Error::Compile)?;
    let bound = compiled
        .param_names
        .iter()
        .map(|name| {
            if let Some(qname) = name.strip_prefix("__global__") {
                Ok(globals.get(qname).cloned().unwrap_or(DecodedValue::Null))
            } else {
                let value = params
                    .iter()
                    .find(|(k, _)| *k == name.as_str())
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| Error::MissingParam(name.clone()))?;
                check_array_elements(name, &value)?;
                Ok(value)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((compiled, bound))
}

/// Refuse a NULL inside an array argument.
///
/// PyQL has no `array<optional T>`, so a NULL element is never a value the
/// query can mean — but bound as SQL NULL it compares equal to nothing and the
/// statement quietly returns no rows rather than failing. Rejected
/// client-side before execution, with the wording
/// `pylon/client.py`'s `_check_array_elements` raises on the Python side.
fn check_array_elements(name: &str, value: &DecodedValue) -> Result<()> {
    let DecodedValue::Array(items) = value else {
        return Ok(());
    };
    let Some(index) = items.iter().position(|item| matches!(item, DecodedValue::Null)) else {
        return Ok(());
    };
    Err(Error::InvalidArgument(format!(
        "invalid input for query argument ${name}: {} \
         (invalid array element at index {index}: None is not allowed)",
        render_array(items)
    )))
}

/// The array as the message renders it — Python's `repr` of a list, which is
/// the form the Python client's own wording was written against.
fn render_array(items: &[DecodedValue]) -> String {
    let rendered: Vec<String> = items
        .iter()
        .map(|item| match item {
            DecodedValue::Null => "None".to_string(),
            DecodedValue::Str(s) => format!("{s:?}"),
            other => format!("{other:?}"),
        })
        .collect();
    format!("[{}]", rendered.join(", "))
}

/// The session globals as the JSON a database trigger reads them from
/// (`pylon.globals`), for a statement that writes — only a write fires one.
fn trigger_globals(compiled: &CompiledQuery, globals: &HashMap<String, DecodedValue>) -> Option<String> {
    compiled.mutates.then(|| {
        let fields = globals
            .iter()
            .map(|(name, value)| {
                format!(
                    "{}: {}",
                    crate::json::to_json(&crate::value::Value::Str(name.clone())),
                    crate::json::to_json(&crate::decode::cached_to_value(value))
                )
            })
            .collect::<Vec<_>>();
        format!("{{{}}}", fields.join(", "))
    })
}

pub(crate) async fn query<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
    access: crate::cache::CacheAccess<'_>,
) -> Result<Vec<Value>> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    if let Some(rows) = crate::cache::get_rows(access, &compiled, &bound)? {
        return Ok(rows.iter().map(|row| decode(&compiled.shape.root, row)).collect());
    }
    let rows = executor
        .run_query(&compiled.sql, &bound, trigger_globals(&compiled, globals).as_deref())
        .await
        .map_err(Error::Db)?;
    crate::cache::invalidate_for(access, &compiled)?;
    crate::cache::put_rows(access, &compiled, &bound, &rows)?;
    Ok(rows.iter().map(|row| decode(&compiled.shape.root, row)).collect())
}

pub(crate) async fn query_single<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
    access: crate::cache::CacheAccess<'_>,
) -> Result<Option<Value>> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    if let Some(rows) = crate::cache::get_rows(access, &compiled, &bound)? {
        if rows.len() > 1 {
            return Err(Error::ResultCardinality { got: rows.len() });
        }
        return Ok(rows.first().map(|row| decode(&compiled.shape.root, row)));
    }
    let rows = executor
        .run_query(&compiled.sql, &bound, trigger_globals(&compiled, globals).as_deref())
        .await
        .map_err(Error::Db)?;
    if rows.len() > 1 {
        return Err(Error::ResultCardinality { got: rows.len() });
    }
    crate::cache::invalidate_for(access, &compiled)?;
    crate::cache::put_rows(access, &compiled, &bound, &rows)?;
    Ok(rows.first().map(|row| decode(&compiled.shape.root, row)))
}

pub(crate) async fn query_required_single<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
    access: crate::cache::CacheAccess<'_>,
) -> Result<Value> {
    query_single(executor, pyql, params, schema, config, globals, access)
        .await?
        .ok_or(Error::NoData)
}

pub(crate) async fn execute<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
    access: crate::cache::CacheAccess<'_>,
) -> Result<()> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    executor
        .run_execute(&compiled.sql, &bound, trigger_globals(&compiled, globals).as_deref())
        .await
        .map_err(Error::Db)?;
    crate::cache::invalidate_for(access, &compiled)?;
    Ok(())
}

/// The query's rows rendered as a JSON array, each against the compiled
/// shape (see `crate::json`) — so it runs once, and objects keep the names
/// of the pointers they selected.
pub(crate) async fn query_json<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
    access: crate::cache::CacheAccess<'_>,
) -> Result<String> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    if let Some(value) = crate::cache::get_json(access, "json_all", &compiled, &bound)? {
        return Ok(value.unwrap_or_else(|| "[]".to_string()));
    }
    let rows = executor
        .run_query(&compiled.sql, &bound, trigger_globals(&compiled, globals).as_deref())
        .await
        .map_err(Error::Db)?;
    let documents: Vec<String> = rows
        .iter()
        .map(|row| crate::json::row_to_json(&compiled.shape.root, row))
        .collect();
    let value = format!("[{}]", documents.join(", "));
    crate::cache::invalidate_for(access, &compiled)?;
    crate::cache::put_json(access, "json_all", &compiled, &bound, Some(&value))?;
    Ok(value)
}

pub(crate) async fn query_single_json<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
    access: crate::cache::CacheAccess<'_>,
) -> Result<Option<String>> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    if let Some(value) = crate::cache::get_json(access, "json_single", &compiled, &bound)? {
        return Ok(value);
    }
    let rows = executor
        .run_query(&compiled.sql, &bound, trigger_globals(&compiled, globals).as_deref())
        .await
        .map_err(Error::Db)?;
    if rows.len() > 1 {
        return Err(Error::ResultCardinality { got: rows.len() });
    }
    if rows.is_empty() {
        crate::cache::invalidate_for(access, &compiled)?;
        crate::cache::put_json(access, "json_single", &compiled, &bound, None)?;
        return Ok(None);
    }
    let value = rows
        .first()
        .map(|row| crate::json::row_to_json(&compiled.shape.root, row));
    crate::cache::invalidate_for(access, &compiled)?;
    crate::cache::put_json(access, "json_single", &compiled, &bound, value.as_deref())?;
    Ok(value)
}

pub(crate) async fn query_required_single_json<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
    access: crate::cache::CacheAccess<'_>,
) -> Result<String> {
    query_single_json(executor, pyql, params, schema, config, globals, access)
        .await?
        .ok_or(Error::NoData)
}

pub(crate) async fn analyze<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, DecodedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, DecodedValue>,
) -> Result<String> {
    // `analyze` is a soft keyword — legal only as a leading statement
    // token — so accept the query with or without it already written,
    // mirroring `pylon/client.py`'s `_ANALYZE_PREFIX_RE`.
    let normalized = if pyql.trim_start().to_ascii_lowercase().starts_with("analyze") {
        pyql.to_string()
    } else {
        format!("analyze {pyql}")
    };
    let (compiled, bound) = compile_and_bind(&normalized, params, schema, config, globals)?;
    let path_aliases = compiled.analyze_paths.clone().unwrap_or_default();
    let raw_json = executor.run_explain(&compiled.sql, &bound).await.map_err(Error::Db)?;
    let tree = pylon_core::analyze::build_coarse_grained(&raw_json, &path_aliases).map_err(Error::Analyze)?;
    serde_json::to_string(&tree).map_err(Error::SchemaJson)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_inside_an_array_argument_is_refused_with_the_fixed_wording() {
        let value = DecodedValue::Array(vec![DecodedValue::Null]);
        let error = check_array_elements("ids", &value).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid input for query argument $ids: [None] \
             (invalid array element at index 0: None is not allowed)"
        );
    }

    #[test]
    fn the_index_named_is_the_offending_ones() {
        let value = DecodedValue::Array(vec![DecodedValue::Str("a".into()), DecodedValue::Null]);
        let error = check_array_elements("ids", &value).unwrap_err();
        assert!(
            error.to_string().contains("invalid array element at index 1"),
            "{error}"
        );
    }

    #[test]
    fn an_array_without_nulls_and_a_null_argument_both_pass() {
        // A whole argument that is NULL is an absent `<optional …>`, which is
        // a value the query can mean; only a NULL *element* is not.
        check_array_elements("ids", &DecodedValue::Array(vec![DecodedValue::Str("a".into())])).unwrap();
        check_array_elements("ids", &DecodedValue::Array(vec![])).unwrap();
        check_array_elements("ids", &DecodedValue::Null).unwrap();
    }
}
