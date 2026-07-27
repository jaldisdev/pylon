//! Shared query-execution logic for [`crate::Client`] and
//! [`crate::Transaction`] — both expose the same set of query methods
//! (mirrors how `pylon/client.py`'s `Client` and `AsyncTransaction` share
//! `_compile_and_bind`), differing only in what actually runs the SQL
//! (a pooled connection vs. an already-open transaction).

use std::collections::HashMap;

use pylon_core::ir::SessionConfig;
use pylon_core::query::{compile_with_config, CompiledQuery};
use pylon_core::schema::SchemaDescriptor;
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;

use crate::decode::decode;
use crate::error::{Error, Result};
use crate::value::Value;

/// Whatever can run compiled SQL — a pooled connection (`PgPool`) or an
/// open transaction (`PgTransaction`). Kept minimal: just the three
/// primitives every query method above it is built from.
pub(crate) trait Executor {
    async fn run_query(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<Vec<CachedValue>>;
    async fn run_execute(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<u64>;
    async fn run_explain(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<String>;
}

impl Executor for pylon_pgcon::PgPool {
    async fn run_query(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<Vec<CachedValue>> {
        self.query_typed(sql, params, &ExtensionOids::default()).await
    }
    async fn run_execute(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<u64> {
        self.execute_typed(sql, params).await
    }
    async fn run_explain(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<String> {
        self.query_explain(sql, params).await
    }
}

impl Executor for pylon_pgcon::PgTransaction {
    async fn run_query(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<Vec<CachedValue>> {
        self.query_typed(sql, params, &ExtensionOids::default()).await
    }
    async fn run_execute(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<u64> {
        self.execute_typed(sql, params).await
    }
    async fn run_explain(&self, sql: &str, params: &[CachedValue]) -> pylon_pgcon::Result<String> {
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
/// `CachedValue`s — `__global__`-prefixed names are filled from `globals`
/// (missing = `NULL`, matching `pylon/client.py`'s own `.get(qname)`
/// default), everything else from `params` (missing = a hard error, unlike
/// globals — mirrors `client.py:670-678`).
pub(crate) fn compile_and_bind(
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
) -> Result<(CompiledQuery, Vec<CachedValue>)> {
    let compiled = compile_with_config(pyql, schema, config).map_err(Error::Compile)?;
    let bound = compiled
        .param_names
        .iter()
        .map(|name| {
            if let Some(qname) = name.strip_prefix("__global__") {
                Ok(globals.get(qname).cloned().unwrap_or(CachedValue::Null))
            } else {
                params
                    .iter()
                    .find(|(k, _)| *k == name.as_str())
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| Error::MissingParam(name.clone()))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((compiled, bound))
}

pub(crate) async fn query<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
    cache: Option<&pylon_cache::Cache>,
) -> Result<Vec<Value>> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    if let Some(cache) = cache {
        if let Some(rows) = crate::cache::get_rows(cache, &compiled, &bound)? {
            return Ok(rows.iter().map(|row| decode(&compiled.shape.root, row)).collect());
        }
    }
    let rows = executor.run_query(&compiled.sql, &bound).await.map_err(Error::Db)?;
    if let Some(cache) = cache {
        crate::cache::put_rows(cache, &compiled, &bound, &rows)?;
    }
    Ok(rows.iter().map(|row| decode(&compiled.shape.root, row)).collect())
}

pub(crate) async fn query_single<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
    cache: Option<&pylon_cache::Cache>,
) -> Result<Option<Value>> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    if let Some(cache) = cache {
        if let Some(rows) = crate::cache::get_rows(cache, &compiled, &bound)? {
            if rows.len() > 1 {
                return Err(Error::ResultCardinality { got: rows.len() });
            }
            return Ok(rows.first().map(|row| decode(&compiled.shape.root, row)));
        }
    }
    let rows = executor.run_query(&compiled.sql, &bound).await.map_err(Error::Db)?;
    if rows.len() > 1 {
        return Err(Error::ResultCardinality { got: rows.len() });
    }
    if let Some(cache) = cache {
        crate::cache::put_rows(cache, &compiled, &bound, &rows)?;
    }
    Ok(rows.first().map(|row| decode(&compiled.shape.root, row)))
}

pub(crate) async fn query_required_single<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
    cache: Option<&pylon_cache::Cache>,
) -> Result<Value> {
    query_single(executor, pyql, params, schema, config, globals, cache)
        .await?
        .ok_or(Error::NoData)
}

pub(crate) async fn execute<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
) -> Result<()> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    executor.run_execute(&compiled.sql, &bound).await.map_err(Error::Db)?;
    Ok(())
}

/// Runs `sql` wrapped so Postgres itself materializes the JSON, matching
/// `pylon-py`'s `query_compiled_json_agg`/`query_compiled_row_to_json`
/// (`crates/pylon-py/src/pgcon.rs:300-325`) — the same string-wrapping
/// convention, just applied here instead of at the pyo3 boundary, since
/// this crate has direct, non-opaque access to `compiled.sql`.
pub(crate) async fn query_json<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
    cache: Option<&pylon_cache::Cache>,
) -> Result<String> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    if let Some(cache) = cache {
        if let Some(value) = crate::cache::get_json(cache, "json_all", &compiled, &bound)? {
            return Ok(value.unwrap_or_else(|| "[]".to_string()));
        }
    }
    let sql = format!("SELECT COALESCE(json_agg(q), '[]') FROM ({}) q", compiled.sql);
    let rows = executor.run_query(&sql, &bound).await.map_err(Error::Db)?;
    let value = match rows.into_iter().next() {
        Some(CachedValue::Str(s)) => s,
        _ => "[]".to_string(),
    };
    if let Some(cache) = cache {
        crate::cache::put_json(cache, "json_all", &compiled, &bound, Some(&value))?;
    }
    Ok(value)
}

pub(crate) async fn query_single_json<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
    cache: Option<&pylon_cache::Cache>,
) -> Result<Option<String>> {
    let (compiled, bound) = compile_and_bind(pyql, params, schema, config, globals)?;
    if let Some(cache) = cache {
        if let Some(value) = crate::cache::get_json(cache, "json_single", &compiled, &bound)? {
            return Ok(value);
        }
    }
    let rows = executor.run_query(&compiled.sql, &bound).await.map_err(Error::Db)?;
    if rows.len() > 1 {
        return Err(Error::ResultCardinality { got: rows.len() });
    }
    if rows.is_empty() {
        if let Some(cache) = cache {
            crate::cache::put_json(cache, "json_single", &compiled, &bound, None)?;
        }
        return Ok(None);
    }
    let sql = format!("SELECT row_to_json(q) FROM ({} LIMIT 1) q", compiled.sql);
    let json_rows = executor.run_query(&sql, &bound).await.map_err(Error::Db)?;
    let value = match json_rows.into_iter().next() {
        Some(CachedValue::Str(s)) => Some(s),
        _ => None,
    };
    if let Some(cache) = cache {
        crate::cache::put_json(cache, "json_single", &compiled, &bound, value.as_deref())?;
    }
    Ok(value)
}

pub(crate) async fn query_required_single_json<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
    cache: Option<&pylon_cache::Cache>,
) -> Result<String> {
    query_single_json(executor, pyql, params, schema, config, globals, cache)
        .await?
        .ok_or(Error::NoData)
}

pub(crate) async fn analyze<E: Executor>(
    executor: &E,
    pyql: &str,
    params: &[(&str, CachedValue)],
    schema: &SchemaDescriptor,
    config: &SessionConfig,
    globals: &HashMap<String, CachedValue>,
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
