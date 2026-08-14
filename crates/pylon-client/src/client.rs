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
use pylon_value::DecodedValue;

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

/// Either a `(path, max_size_mb)` this `Builder` should open itself, or an
/// already-open handle a caller wants shared in as-is — see
/// `Builder::cache`/`Builder::cache_handle`.
enum CacheSource {
    Open { path: PathBuf, max_size_mb: usize },
    Shared(Arc<pylon_cache::Cache>),
}

/// Builds a [`Client`]. `dsn` is required up front (this crate never
/// parses `pylon.toml` — see the crate-level docs); everything else has a
/// sensible default.
pub struct Builder {
    dsn: String,
    max_pool_size: usize,
    cache: Option<CacheSource>,
}

impl Builder {
    pub fn new(dsn: impl Into<String>) -> Self {
        Self {
            dsn: dsn.into(),
            max_pool_size: 10,
            cache: None,
        }
    }

    pub fn max_pool_size(mut self, max_pool_size: usize) -> Self {
        self.max_pool_size = max_pool_size;
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
    ///
    /// Opens its own LMDB handle for `path` — `heed` (the LMDB binding this
    /// crate uses) refuses a second `Env::open` on the same canonicalized
    /// path while an earlier handle onto it is still alive within the same
    /// process, so a caller building more than one `Client` that should
    /// share one cache directory (e.g. one per named `pylon.toml`
    /// connection) must use `Builder::cache_handle` with one already-open
    /// `Arc<pylon_cache::Cache>` instead of calling this per client.
    pub fn cache(mut self, path: impl Into<PathBuf>, max_size_mb: usize) -> Self {
        self.cache = Some(CacheSource::Open {
            path: path.into(),
            max_size_mb,
        });
        self
    }

    /// Like `Builder::cache`, but attaches to an already-open handle instead
    /// of opening a new one — see that method's doc comment for why this
    /// exists.
    pub fn cache_handle(mut self, cache: Arc<pylon_cache::Cache>) -> Self {
        self.cache = Some(CacheSource::Shared(cache));
        self
    }

    /// Connects (eagerly — a bad DSN/host/credentials fails right here,
    /// matching `PgPool::connect`'s own eager-connect behavior) and fetches
    /// the schema snapshot from `_pylon."Schema"` — fails with
    /// `Error::NoSchemaSnapshot` if neither `pylon migration apply` nor
    /// `pylon migration watch` has ever run against this database.
    pub async fn build(self) -> Result<Client> {
        let pool = pylon_pgcon::PgPool::connect(&self.dsn, self.max_pool_size)
            .await
            .map_err(Error::Db)?;
        let schema = schema::fetch(&pool).await?;
        let cache = match self.cache {
            None => None,
            Some(CacheSource::Open { path, max_size_mb }) => Some(Arc::new(
                pylon_cache::Cache::open(&path, max_size_mb).map_err(|e| Error::Cache(e.to_string()))?,
            )),
            Some(CacheSource::Shared(cache)) => Some(cache),
        };
        Ok(Client {
            dsn: Arc::new(self.dsn),
            pool: Arc::new(pool),
            schema: Arc::new(RwLock::new(schema)),
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
    /// Kept around solely for `Client::listen()` — every other method
    /// operates through `pool`; a `LISTEN` subscription needs its own
    /// dedicated (non-pooled) connection instead, opened fresh from this
    /// DSN each time `listen()` is called.
    dsn: Arc<String>,
    pool: Arc<pylon_pgcon::PgPool>,
    /// Every query method clones this out from behind the lock before
    /// compiling/awaiting anything — a `std::sync::RwLockReadGuard` isn't
    /// `Send`, and holding one across an `.await` point would make the
    /// resulting future non-`Send` (fatal for `Client::transaction`'s boxed
    /// futures, and a footgun on a multi-threaded runtime generally).
    schema: Arc<RwLock<SchemaDescriptor>>,
    globals: Arc<HashMap<String, DecodedValue>>,
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

    /// Re-fetches the schema snapshot from `_pylon."Schema"`. Visible to
    /// every clone sharing this client's pool (`with_globals`/`with_config`
    /// views included) — there's only one schema slot per underlying
    /// connection pool, matching `pylon/client.py`'s single process-level
    /// singleton.
    pub async fn reload_schema(&self) -> Result<()> {
        let fresh = schema::fetch(&self.pool).await?;
        *self.schema.write().unwrap() = fresh;
        pylon_core::query::clear_query_cache();
        Ok(())
    }

    /// Returns a client view that injects `globals` into every query,
    /// keyed by qualified name (`"module::name"`) — sharing the same
    /// connection pool. Mirrors `pylon/client.py:287-304`.
    pub fn with_globals(&self, globals: impl IntoIterator<Item = (String, DecodedValue)>) -> Client {
        let mut merged = (*self.globals).clone();
        merged.extend(globals);
        Client {
            globals: Arc::new(merged),
            ..self.clone()
        }
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

    /// The `Arc<pylon_cache::Cache>` this client reads/writes through, if
    /// `Builder::cache` was configured — for a caller (`pylon-server`'s
    /// worker-wiring startup) that needs to attach a `CacheInvalidationWorker`
    /// to the exact same LMDB handle this client's own read-through caching
    /// uses, rather than opening a second one (LMDB refuses a second
    /// `Env::open` on the same path within one process).
    pub fn cache_handle(&self) -> Option<Arc<pylon_cache::Cache>> {
        self.cache.clone()
    }

    pub async fn query(&self, pyql: &str, params: &[(&str, DecodedValue)]) -> Result<Vec<Value>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query(
            &*self.pool,
            pyql,
            params,
            &schema,
            &self.config,
            &self.globals,
            self.cache.as_deref(),
        )
        .await
    }

    pub async fn query_single(&self, pyql: &str, params: &[(&str, DecodedValue)]) -> Result<Option<Value>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_single(
            &*self.pool,
            pyql,
            params,
            &schema,
            &self.config,
            &self.globals,
            self.cache.as_deref(),
        )
        .await
    }

    pub async fn query_required_single(&self, pyql: &str, params: &[(&str, DecodedValue)]) -> Result<Value> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_required_single(
            &*self.pool,
            pyql,
            params,
            &schema,
            &self.config,
            &self.globals,
            self.cache.as_deref(),
        )
        .await
    }

    pub async fn execute(&self, pyql: &str, params: &[(&str, DecodedValue)]) -> Result<()> {
        let schema = self.schema.read().unwrap().clone();
        exec::execute(
            &*self.pool,
            pyql,
            params,
            &schema,
            &self.config,
            &self.globals,
            self.cache.as_deref(),
        )
        .await
    }

    /// Subscribes to a schema-declared [`Channel`](pylon_core::schema::ChannelDescriptor)
    /// (bare or `module::name` reference — the same string a schema author
    /// already writes inside a PyQL `notify(...)` call) and returns a
    /// [`ChannelListener`] whose `recv()` yields decoded payloads matching
    /// that Channel's own declared shape: `Value::Uuid` for a Type-shaped
    /// channel (the changed row's `id`, not a fetched object — see
    /// `docs/schema/channels.md`), the matching `Value` variant for a
    /// Scalar-shaped channel, or `Value::Object` for an Object-shaped
    /// channel. A payload that doesn't match the declared shape comes back
    /// as `Err(Error::MalformedPayload(_))` from that `recv()` call rather
    /// than being silently dropped.
    ///
    /// Opens its own dedicated (non-pooled) connection, held for the
    /// returned `ChannelListener`'s lifetime — `LISTEN` is per-session, so
    /// running it on a pooled connection would leak the subscription onto
    /// whatever unrelated query later borrows that connection back out of
    /// the pool. The connection (and the server-side subscription with it)
    /// closes once the `ChannelListener` is dropped.
    ///
    /// Mirrors `pylon/client.py`'s own `Client.listen()` — there, a typed
    /// async generator; here, a `recv()`-based handle instead, since this
    /// crate has no `Stream`/async-generator precedent to build on.
    pub async fn listen(&self, channel: &str) -> Result<crate::ChannelListener> {
        let schema = self.schema.read().unwrap().clone();
        crate::listen::listen(&self.dsn, &schema, channel).await
    }

    pub async fn query_json(&self, pyql: &str, params: &[(&str, DecodedValue)]) -> Result<String> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_json(
            &*self.pool,
            pyql,
            params,
            &schema,
            &self.config,
            &self.globals,
            self.cache.as_deref(),
        )
        .await
    }

    pub async fn query_single_json(&self, pyql: &str, params: &[(&str, DecodedValue)]) -> Result<Option<String>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_single_json(
            &*self.pool,
            pyql,
            params,
            &schema,
            &self.config,
            &self.globals,
            self.cache.as_deref(),
        )
        .await
    }

    pub async fn query_required_single_json(&self, pyql: &str, params: &[(&str, DecodedValue)]) -> Result<String> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_required_single_json(
            &*self.pool,
            pyql,
            params,
            &schema,
            &self.config,
            &self.globals,
            self.cache.as_deref(),
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
    pub async fn analyze(&self, pyql: &str, params: &[(&str, DecodedValue)]) -> Result<String> {
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
    /// # use pylon_client::DecodedValue;
    /// # async fn go(client: pylon_client::Client) -> pylon_client::Result<()> {
    /// client.transaction(pylon_client::Isolation::Serializable, |tx| Box::pin(async move {
    ///     tx.execute("insert Person { name := <str>$name }", &[("name", DecodedValue::Str("Bob".into()))]).await
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
                cache: self.cache.clone(),
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
