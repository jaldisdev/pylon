//! [`Client`] construction, connection, globals/config, and query methods —
//! see `crate::transaction` for the closure-based retrying transaction API.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use pylon_core::ir::SessionConfig;
use pylon_core::schema::SchemaDescriptor;
use pylon_value::CachedValue;

use crate::error::{Error, Result};
use crate::exec;
use crate::schema;
use crate::transaction::{Isolation, Transaction};
use crate::value::Value;

/// A transaction attempt body's return type — boxed since a plain generic
/// `Fut: Future` can't express "this future borrows the `&Transaction` it
/// was handed" without higher-ranked lifetimes on the future type itself;
/// boxing sidesteps that and matches how most async-closure-taking APIs in
/// the ecosystem handle exactly this shape.
pub type TxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Builds a [`Client`]. `dsn` is required up front (this crate never
/// parses `pylon.toml` — see the crate-level docs); everything else has a
/// sensible default.
pub struct Builder {
    dsn: String,
    max_pool_size: usize,
    schema_path: PathBuf,
    cache: Option<(PathBuf, usize)>,
}

impl Builder {
    pub fn new(dsn: impl Into<String>) -> Self {
        Self {
            dsn: dsn.into(),
            max_pool_size: 10,
            schema_path: schema::default_schema_path(),
            cache: None,
        }
    }

    pub fn max_pool_size(mut self, max_pool_size: usize) -> Self {
        self.max_pool_size = max_pool_size;
        self
    }

    /// Defaults to `.pylon/schema.json` relative to the current working
    /// directory — the same artifact `pylon.finalize()` writes and
    /// `pylon-lsp` already consumes.
    pub fn schema_path(mut self, schema_path: impl Into<PathBuf>) -> Self {
        self.schema_path = schema_path.into();
        self
    }

    /// Opts into read-through result caching at an LMDB-backed directory —
    /// mirrors `pylon.toml`'s `[cache]` section, minus per-type
    /// (`[cache.sets.<Name>]`) overrides: caching here is a single global
    /// on/off. Omit this entirely for no caching (today's default
    /// behavior). Only `Client`'s own query methods read/write the cache —
    /// `Transaction` never does, matching `pylon/client.py`'s
    /// `AsyncTransaction`.
    ///
    /// Nothing evicts entries automatically here — pair this with a
    /// process elsewhere that invalidates the same directory (e.g.
    /// `pylon worker start`) if the underlying data changes while cached.
    pub fn cache(mut self, path: impl Into<PathBuf>, max_size_mb: usize) -> Self {
        self.cache = Some((path.into(), max_size_mb));
        self
    }

    /// Connects (eagerly — a bad DSN/host/credentials fails right here,
    /// matching `PgPool::connect`'s own eager-connect behavior) and loads
    /// the schema.
    pub async fn build(self) -> Result<Client> {
        let pool = pylon_pgcon::PgPool::connect(&self.dsn, self.max_pool_size).await.map_err(Error::Db)?;
        let schema = schema::load(&self.schema_path)?;
        let cache = self
            .cache
            .map(|(path, max_size_mb)| {
                pylon_cache::Cache::open(&path, max_size_mb).map(Arc::new).map_err(|e| Error::Cache(e.to_string()))
            })
            .transpose()?;
        Ok(Client {
            pool: Arc::new(pool),
            schema: Arc::new(RwLock::new(schema)),
            schema_path: Arc::new(self.schema_path),
            globals: Arc::new(HashMap::new()),
            config: SessionConfig::default(),
            cache,
        })
    }
}

/// An async Pylon client — a connection pool plus a compiled schema,
/// shared cheaply across every [`Client::with_globals`]/[`Client::with_config`]
/// view of it (mirrors `pylon/client.py`'s own shared-pool-ref pattern).
#[derive(Clone)]
pub struct Client {
    pool: Arc<pylon_pgcon::PgPool>,
    /// Every query method clones this out from behind the lock before
    /// compiling/awaiting anything — a `std::sync::RwLockReadGuard` isn't
    /// `Send`, and holding one across an `.await` point would make the
    /// resulting future non-`Send` (fatal for `Client::transaction`'s boxed
    /// futures, and a footgun on a multi-threaded runtime generally).
    schema: Arc<RwLock<SchemaDescriptor>>,
    schema_path: Arc<PathBuf>,
    globals: Arc<HashMap<String, CachedValue>>,
    config: SessionConfig,
    /// `None` unless `Builder::cache` was called — read-through caching is
    /// opt-in. Shared across `with_globals`/`with_config` clones, same as
    /// `pool`/`schema`.
    cache: Option<Arc<pylon_cache::Cache>>,
}

impl Client {
    pub fn builder(dsn: impl Into<String>) -> Builder {
        Builder::new(dsn)
    }

    /// Re-reads `.pylon/schema.json` from disk. Visible to every clone
    /// sharing this client's pool (`with_globals`/`with_config` views
    /// included) — there's only one schema slot per underlying connection
    /// pool, matching `pylon/client.py`'s single process-level singleton.
    pub fn reload_schema(&self) -> Result<()> {
        let fresh = schema::load(&self.schema_path)?;
        *self.schema.write().unwrap() = fresh;
        pylon_core::query::clear_query_cache();
        Ok(())
    }

    /// Returns a client view that injects `globals` into every query,
    /// keyed by qualified name (`"module::name"`) — sharing the same
    /// connection pool. Mirrors `pylon/client.py:287-304`.
    pub fn with_globals(&self, globals: impl IntoIterator<Item = (String, CachedValue)>) -> Client {
        let mut merged = (*self.globals).clone();
        merged.extend(globals);
        Client { globals: Arc::new(merged), ..self.clone() }
    }

    /// Returns a client view that applies `config` to every query — sharing
    /// the same connection pool. Mirrors `pylon/client.py:306-327`.
    pub fn with_config(&self, config: SessionConfig) -> Client {
        Client { config, ..self.clone() }
    }

    /// Escape hatch for hand-written SQL outside PyQL — mirrors
    /// `pylon/client.py:567-579`.
    pub fn raw_connection(&self) -> &pylon_pgcon::PgPool {
        &self.pool
    }

    /// A clone of the currently-loaded schema — for callers that need to
    /// introspect it directly (e.g. a schema-browser endpoint), not just
    /// compile queries against it. Clones out from behind the lock rather
    /// than returning a guard, same reasoning as every query method here.
    pub fn schema(&self) -> SchemaDescriptor {
        self.schema.read().unwrap().clone()
    }

    pub async fn query(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<Vec<Value>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query(&*self.pool, pyql, params, &schema, &self.config, &self.globals, self.cache.as_deref()).await
    }

    pub async fn query_single(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<Option<Value>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_single(&*self.pool, pyql, params, &schema, &self.config, &self.globals, self.cache.as_deref())
            .await
    }

    pub async fn query_required_single(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<Value> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_required_single(
            &*self.pool, pyql, params, &schema, &self.config, &self.globals, self.cache.as_deref(),
        )
        .await
    }

    pub async fn execute(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<()> {
        let schema = self.schema.read().unwrap().clone();
        exec::execute(&*self.pool, pyql, params, &schema, &self.config, &self.globals).await
    }

    pub async fn query_json(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<String> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_json(&*self.pool, pyql, params, &schema, &self.config, &self.globals, self.cache.as_deref())
            .await
    }

    pub async fn query_single_json(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<Option<String>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_single_json(
            &*self.pool, pyql, params, &schema, &self.config, &self.globals, self.cache.as_deref(),
        )
        .await
    }

    pub async fn query_required_single_json(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<String> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_required_single_json(
            &*self.pool, pyql, params, &schema, &self.config, &self.globals, self.cache.as_deref(),
        )
        .await
    }

    /// Current cache size, or `None` if `Builder::cache` wasn't configured
    /// — mirrors `pylon.cache.stat()`.
    pub fn cache_stat(&self) -> Result<Option<pylon_cache::CacheStats>> {
        match &self.cache {
            None => Ok(None),
            Some(cache) => cache.stat().map(Some).map_err(|e| Error::Cache(e.to_string())),
        }
    }

    /// Evicts every cache entry — a no-op if `Builder::cache` wasn't
    /// configured. Mirrors `pylon.cache.clear()`.
    pub fn cache_clear(&self) -> Result<()> {
        match &self.cache {
            None => Ok(()),
            Some(cache) => cache.clear().map_err(|e| Error::Cache(e.to_string())),
        }
    }

    /// Runs `pyql` through Postgres's `EXPLAIN (ANALYZE, FORMAT JSON)` and
    /// returns a query plan grouped by the query's own shape instead of raw
    /// SQL relation names. `pyql` doesn't need the leading `analyze`
    /// keyword already written. Mirrors `pylon/client.py:461-480`.
    pub async fn analyze(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<String> {
        let schema = self.schema.read().unwrap().clone();
        exec::analyze(&*self.pool, pyql, params, &schema, &self.config, &self.globals).await
    }

    /// Runs a retrying transaction with the default isolation level
    /// (`Serializable`) and attempt budget (3) — see
    /// [`Client::transaction_with_attempts`] for full control.
    ///
    /// `body` is re-run once per attempt against a fresh [`Transaction`];
    /// it commits automatically when `body` returns `Ok`, and rolls back
    /// and retries (with a `0ms, 100ms, 200ms, …` back-off) when `body`
    /// returns a serialization-failure/deadlock error, up to the attempt
    /// budget. Any other error rolls back and propagates immediately.
    ///
    /// ```no_run
    /// # use pylon_client::CachedValue;
    /// # async fn go(client: pylon_client::Client) -> pylon_client::Result<()> {
    /// client.transaction(pylon_client::Isolation::Serializable, |tx| Box::pin(async move {
    ///     tx.execute("insert Person { name := <str>$name }", &[("name", CachedValue::Str("Bob".into()))]).await
    /// })).await?;
    /// # Ok(()) }
    /// ```
    pub async fn transaction<T, F>(&self, isolation: Isolation, body: F) -> Result<T>
    where
        F: for<'a> FnMut(&'a Transaction) -> TxFuture<'a, T>,
    {
        self.transaction_with_attempts(isolation, 3, body).await
    }

    pub async fn transaction_with_attempts<T, F>(
        &self,
        isolation: Isolation,
        max_attempts: u32,
        mut body: F,
    ) -> Result<T>
    where
        F: for<'a> FnMut(&'a Transaction) -> TxFuture<'a, T>,
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            if attempt > 1 {
                tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt - 1))).await;
            }
            let pg_tx = self.pool.begin(isolation.as_str()).await.map_err(Error::Db)?;
            let tx = Transaction {
                inner: pg_tx,
                schema: self.schema.clone(),
                config: self.config.clone(),
                globals: self.globals.clone(),
            };
            let result = body(&tx).await;
            match result {
                Ok(value) => {
                    tx.inner.commit().await.map_err(Error::Db)?;
                    return Ok(value);
                }
                Err(e) if e.is_retriable() && attempt < max_attempts => {
                    let _ = tx.inner.rollback().await;
                }
                Err(e) => {
                    let _ = tx.inner.rollback().await;
                    return Err(e);
                }
            }
        }
    }
}
