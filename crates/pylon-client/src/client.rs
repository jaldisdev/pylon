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
use tokio::sync::OnceCell;

use crate::error::{Error, Result};
use crate::exec;
use crate::query_arg::QueryArgs;
use crate::queryable::{Queryable, decode_optional_row, decode_row, decode_rows};
use crate::schema;
use crate::transaction::{Isolation, Transaction};

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
    /// `Transaction` never populates it, since rows read inside a
    /// transaction aren't committed, matching `pylon/client.py`'s
    /// `AsyncTransaction`.
    ///
    /// This client's *own* writes evict the tags they touch, from inside a
    /// transaction too, so a program that writes and then re-reads sees its
    /// own change. Writes from *other* processes still need a listener on
    /// the same directory (e.g. `pylon worker start`) to invalidate.
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

    /// Builds the client without touching the database: the connection pool
    /// and the schema snapshot are opened together on first use, or on an
    /// explicit [`Client::ensure_connected`]. A `Client` can therefore be
    /// constructed outside an async context, and a configured-but-never-queried
    /// connection costs nothing.
    ///
    /// The cache, when [`Builder::cache`] configured one, *is* opened here —
    /// it's a local LMDB directory rather than a network resource, and a bad
    /// path is worth failing on at construction rather than on whichever
    /// query happens to run first.
    ///
    /// A process that wants a bad DSN/host/credentials — or a database where
    /// neither `pylon migration apply` nor `pylon migration watch` has ever
    /// run, so there is no schema snapshot to fetch — to fail at startup
    /// instead of on its first query should call [`Client::ensure_connected`]
    /// right after this.
    pub fn build(self) -> Result<Client> {
        let cache = match self.cache {
            None => None,
            Some(CacheSource::Open { path, max_size_mb }) => Some(Arc::new(
                pylon_cache::Cache::open(&path, max_size_mb).map_err(|e| Error::Cache(e.to_string()))?,
            )),
            Some(CacheSource::Shared(cache)) => Some(cache),
        };
        Ok(Client {
            dsn: Arc::new(self.dsn),
            max_pool_size: self.max_pool_size,
            connected: Arc::new(OnceCell::new()),
            globals: Arc::new(HashMap::new()),
            config: SessionConfig::default(),
            cache,
        })
    }
}

/// The half of a [`Client`] that only exists once it has actually reached
/// the database — held behind a `OnceCell` so construction stays cheap and
/// synchronous. The pool and the schema snapshot are initialised together
/// because a client with one and not the other can't serve a query anyway.
struct Connected {
    pool: pylon_pgcon::PgPool,
    /// An `Arc` rather than a plain `RwLock` so `Client::transaction` can
    /// hand a `Transaction` its own handle on the same schema slot without
    /// borrowing from the `OnceCell` for the transaction's whole lifetime.
    schema: Arc<RwLock<SchemaDescriptor>>,
}

/// An async Pylon client — a connection pool plus a compiled schema,
/// shared cheaply across every [`Client::with_globals`]/[`Client::with_config`]
/// view of it (mirrors `pylon/client.py`'s own shared-pool-ref pattern).
///
/// Connecting is lazy: [`Builder::build`] reaches nothing over the network,
/// and the first query (or an explicit [`Client::ensure_connected`]) opens
/// the pool and fetches the schema. Clones — including every
/// `with_globals`/`with_config` view — share one connection, so connecting
/// through any of them connects all of them, exactly as
/// `pylon/client.py`'s `_PoolRef` is shared across its own views.
#[derive(Clone)]
pub struct Client {
    /// Used to open the pool on first use, and on every `Client::listen()`
    /// call — a `LISTEN` subscription needs its own dedicated (non-pooled)
    /// connection, opened fresh from this DSN each time.
    dsn: Arc<String>,
    max_pool_size: usize,
    /// Shared across clones so that all of them see one pool and one schema
    /// slot. `tokio::sync::OnceCell` (not `std`'s) because initialising it
    /// has to await; it serialises concurrent initialisers, so N requests
    /// racing to be the first make one connection attempt between them, and
    /// it stays empty when an attempt fails, so a database that is merely
    /// not up yet is retried by the next query rather than poisoning the
    /// client for good.
    connected: Arc<OnceCell<Connected>>,
    globals: Arc<HashMap<String, DecodedValue>>,
    config: SessionConfig,
    /// `None` unless `Builder::cache` was called — read-through caching is
    /// opt-in. Shared across `with_globals`/`with_config` clones, same as
    /// `connected`. Opened eagerly by `Builder::build`, since it's local.
    cache: Option<Arc<pylon_cache::Cache>>,
}

impl Client {
    pub fn builder(dsn: impl Into<String>) -> Builder {
        Builder::new(dsn)
    }

    /// The pool and schema, opening them on first use.
    ///
    /// A client is reached from request handlers, background workers and CLI
    /// commands alike, which share no startup between them to connect from,
    /// so every query method goes through here rather than making callers
    /// remember an explicit connect step — the same reasoning as
    /// `pylon/client.py`'s `_connected_pool`.
    ///
    /// Note that every caller clones the schema out from behind the lock
    /// rather than holding the guard: a `std::sync::RwLockReadGuard` isn't
    /// `Send`, and holding one across an `.await` point would make the
    /// resulting future non-`Send` (fatal for `Client::transaction`'s boxed
    /// futures, and a footgun on a multi-threaded runtime generally).
    async fn connected(&self) -> Result<&Connected> {
        self.connected
            .get_or_try_init(|| async {
                let pool = pylon_pgcon::PgPool::connect(&self.dsn, self.max_pool_size)
                    .await
                    .map_err(Error::Db)?;
                let schema = schema::fetch(&pool).await?;
                Ok(Connected {
                    pool,
                    schema: Arc::new(RwLock::new(schema)),
                })
            })
            .await
    }

    /// Opens the connection pool and fetches the schema snapshot if that
    /// hasn't happened yet. Safe to call repeatedly; after the first success
    /// it costs one atomic load.
    ///
    /// Queries connect on their own, so this is never required — it exists
    /// for a process that would rather learn about an unreachable database,
    /// bad credentials or a database with no schema snapshot
    /// ([`Error::NoSchemaSnapshot`]) at startup than on whichever request
    /// arrives first. Mirrors `pylon/client.py`'s `Client.ensure_connected`.
    pub async fn ensure_connected(&self) -> Result<()> {
        self.connected().await.map(|_| ())
    }

    /// Re-fetches the schema snapshot from `_pylon."Schema"`. Visible to
    /// every clone sharing this client's pool (`with_globals`/`with_config`
    /// views included) — there's only one schema slot per underlying
    /// connection pool, matching `pylon/client.py`'s single process-level
    /// singleton.
    pub async fn reload_schema(&self) -> Result<()> {
        let conn = self.connected().await?;
        let fresh = schema::fetch(&conn.pool).await?;
        *conn.schema.write().unwrap() = fresh;
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
    /// `pylon/client.py:567-579`. Connects if this client hasn't yet.
    pub async fn raw_connection(&self) -> Result<&pylon_pgcon::PgPool> {
        Ok(&self.connected().await?.pool)
    }

    /// The pool, but only if this client is already connected — `None`
    /// rather than connecting. For an observer that wants to report on
    /// whatever connections a process happens to be holding (pool-status
    /// metrics, say) without a metrics scrape being the thing that opens
    /// them.
    pub fn pool_if_connected(&self) -> Option<&pylon_pgcon::PgPool> {
        self.connected.get().map(|conn| &conn.pool)
    }

    /// A clone of the currently-loaded schema — for callers that need to
    /// introspect it directly (e.g. a schema-browser endpoint), not just
    /// compile queries against it. Connects (and so fetches the snapshot) if
    /// this client hasn't yet. Clones out from behind the lock rather than
    /// returning a guard, same reasoning as every query method here.
    pub async fn schema(&self) -> Result<SchemaDescriptor> {
        Ok(self.connected().await?.schema.read().unwrap().clone())
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

    pub async fn query<R: Queryable, A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<Vec<R>> {
        let params = args.to_params();
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        let values = exec::query(
            &conn.pool,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::read_write(self.cache.as_deref()),
        )
        .await?;
        decode_rows(values)
    }

    pub async fn query_single<R: Queryable, A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<Option<R>> {
        let params = args.to_params();
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        let values = exec::query_single(
            &conn.pool,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::read_write(self.cache.as_deref()),
        )
        .await?;
        decode_optional_row(values)
    }

    pub async fn query_required_single<R: Queryable, A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<R> {
        let params = args.to_params();
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        let values = exec::query_required_single(
            &conn.pool,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::read_write(self.cache.as_deref()),
        )
        .await?;
        decode_row(values)
    }

    pub async fn execute<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<()> {
        let params = args.to_params();
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        exec::execute(
            &conn.pool,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::read_write(self.cache.as_deref()),
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
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        crate::listen::listen(&self.dsn, &schema, channel).await
    }

    pub async fn query_json<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<String> {
        let params = args.to_params();
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        exec::query_json(
            &conn.pool,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::read_write(self.cache.as_deref()),
        )
        .await
    }

    pub async fn query_single_json<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<Option<String>> {
        let params = args.to_params();
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        exec::query_single_json(
            &conn.pool,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::read_write(self.cache.as_deref()),
        )
        .await
    }

    pub async fn query_required_single_json<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<String> {
        let params = args.to_params();
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        exec::query_required_single_json(
            &conn.pool,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::read_write(self.cache.as_deref()),
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
    pub async fn analyze<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<String> {
        let params = args.to_params();
        let conn = self.connected().await?;
        let schema = conn.schema.read().unwrap().clone();
        exec::analyze(&conn.pool, pyql, &params, &schema, &self.config, &self.globals).await
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
    /// A body that returns [`Error::Rollback`] rolls back and is never
    /// retried — a decision, not a failure. It still propagates here (there
    /// is no `T` to return); [`Client::transaction_opt`] is the same call
    /// with that sentinel folded into `Ok(None)`.
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

    /// [`Client::transaction`], but a body that deliberately rolls back is
    /// an outcome rather than an error: `Ok(Some(value))` when it committed,
    /// `Ok(None)` when it returned [`Error::Rollback`]. Real failures still
    /// propagate as `Err`.
    ///
    /// This is the closest Rust gets to `pylon.Rollback` in the Python
    /// client, where the exception is simply swallowed and the loop ends.
    /// Everything the body wrote is visible to the body's own queries and
    /// to nothing else — the point being a test or dry run that needs real
    /// writes without leaving rows behind.
    ///
    /// ```no_run
    /// # use pylon_client::{DecodedValue, Error};
    /// # async fn go(client: pylon_client::Client) -> pylon_client::Result<()> {
    /// let committed: Option<()> = client
    ///     .transaction_opt(pylon_client::Isolation::Serializable, |tx| Box::pin(async move {
    ///         tx.execute("insert Person { name := <str>$name }", &[("name", DecodedValue::Str("Bob".into()))]).await?;
    ///         // ... assert on what the transaction can see, then discard it.
    ///         Err(Error::Rollback)
    ///     }))
    ///     .await?;
    /// assert!(committed.is_none());
    /// # Ok(()) }
    /// ```
    pub async fn transaction_opt<T, F>(&self, isolation: Isolation, body: F) -> Result<Option<T>>
    where
        F: for<'a> FnMut(&'a Transaction) -> TxFuture<'a, T>,
    {
        self.transaction_opt_with_attempts(isolation, 3, body).await
    }

    /// [`Client::transaction_opt`] with an explicit attempt budget — the
    /// `Ok(None)`-on-rollback counterpart to
    /// [`Client::transaction_with_attempts`].
    pub async fn transaction_opt_with_attempts<T, F>(
        &self,
        isolation: Isolation,
        max_attempts: u32,
        body: F,
    ) -> Result<Option<T>>
    where
        F: for<'a> FnMut(&'a Transaction) -> TxFuture<'a, T>,
    {
        match self.transaction_with_attempts(isolation, max_attempts, body).await {
            Ok(value) => Ok(Some(value)),
            Err(Error::Rollback) => Ok(None),
            Err(e) => Err(e),
        }
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
        let conn = self.connected().await?;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            if attempt > 1 {
                tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt - 1))).await;
            }
            let pg_tx = conn.pool.begin(isolation.as_str()).await.map_err(Error::Db)?;
            let tx = Transaction {
                inner: pg_tx,
                schema: conn.schema.clone(),
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
                // `Error::Rollback` is not retriable, so a body that asks to
                // be discarded lands here and is rolled back once rather
                // than re-run — re-running a body that already said "don't
                // keep this" would just repeat its writes to throw them away
                // again.
                Err(e) => {
                    let _ = tx.inner.rollback().await;
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DSN nothing is listening on, so a connection attempt fails fast
    /// without needing a live Postgres — port 1 is reserved and never bound.
    const UNREACHABLE_DSN: &str = "postgresql://nobody@127.0.0.1:1/nothing";

    #[test]
    fn build_does_not_connect() {
        let client = Client::builder(UNREACHABLE_DSN).build().unwrap();
        assert!(client.pool_if_connected().is_none());
    }

    /// A failed attempt must leave the cell empty rather than storing the
    /// failure, or a client built before its database finished starting
    /// would stay broken for the life of the process.
    #[tokio::test]
    async fn a_failed_connect_is_retried_rather_than_remembered() {
        let client = Client::builder(UNREACHABLE_DSN).build().unwrap();

        assert!(client.ensure_connected().await.is_err());
        assert!(client.pool_if_connected().is_none());
        assert!(client.ensure_connected().await.is_err());
    }

    /// Views share the one connection slot, the way `with_globals` siblings
    /// share `pylon/client.py`'s `_PoolRef`.
    #[test]
    fn views_share_the_connection_slot() {
        let client = Client::builder(UNREACHABLE_DSN).build().unwrap();
        let view = client.with_globals([]);

        assert!(Arc::ptr_eq(&client.connected, &view.connected));
    }
}
