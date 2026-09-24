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

//! Shared per-request state: one lazily-connected `pylon_client::Client`
//! per named connection.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;

/// "main" is the frontend's fixed name for the base `[database]` block,
/// which `config.connections` stores under `"default"`.
const MAIN_CONNECTION_ALIAS: &str = "main";

pub struct AppState {
    pub config: Config,
    /// On-disk frontend build to serve instead of the assets embedded into
    /// the binary at compile time (`static_files::STATIC_DIR`) — set via
    /// `--static-dir`, for iterating on the frontend without a Rust
    /// rebuild each time. `None` (the default) serves the embedded build.
    static_dir_override: Option<PathBuf>,
    /// One `Client` per configured connection, all built during `new` and
    /// never mutated afterwards — so the request path is a plain map lookup
    /// with no lock to contend on. Building a client opens no connection
    /// (see `pylon_client::Builder::build`); each one connects on the first
    /// request that actually resolves it, and a configured connection
    /// nothing ever queries stays closed for the process's whole life.
    clients: HashMap<String, Arc<pylon_client::Client>>,
    /// Opened once here (not per-connection) and handed to every `Client`
    /// via `Builder::cache_handle` — `heed` (the LMDB binding
    /// `pylon-cache` uses) refuses a second `Env::open` on the same
    /// canonicalized path while an earlier handle is still alive in the
    /// same process, which a naive per-connection `Builder::cache(path,
    /// ...)` call would hit the moment a second named connection got
    /// resolved (confirmed live: the second `Cache::open` call for the
    /// same path returns `Err("environment already open in this
    /// program...")` rather than silently deduping). `None` when
    /// `[cache].enabled = false`.
    pub cache: Option<Arc<pylon_cache::Cache>>,
}

impl AppState {
    pub fn new(config: Config, static_dir_override: Option<PathBuf>) -> crate::error::Result<Self> {
        let cache = if config.cache.enabled {
            let cache =
                pylon_cache::Cache::open(&config.cache.path, config.cache.max_size_mb as usize).map_err(|e| {
                    crate::error::Error::Invalid(format!(
                        "failed to open cache at {}: {e}",
                        config.cache.path.display()
                    ))
                })?;
            Some(Arc::new(cache))
        } else {
            None
        };
        let mut clients = HashMap::new();
        for (name, db) in &config.connections {
            let mut builder = pylon_client::Client::builder(db.dsn_string()).max_pool_size(db.pool_max_size as usize);
            if let Some(cache) = &cache {
                builder = builder.cache_handle(cache.clone());
            }
            let client = builder.build().map_err(|e| {
                crate::error::Error::Invalid(format!("failed to build the {name:?} connection's client: {e}"))
            })?;
            clients.insert(name.clone(), Arc::new(client));
        }
        Ok(Self {
            config,
            static_dir_override,
            clients,
            cache,
        })
    }

    pub fn static_dir_override(&self) -> Option<&std::path::Path> {
        self.static_dir_override.as_deref()
    }

    /// Looks up the `Client` for `connection_name`, connecting it if this is
    /// the first request to reach it. `Ok(None)` means the name isn't a
    /// configured connection at all (the caller turns that into a 404);
    /// `Err` means it is configured but unreachable (a 500).
    ///
    /// Connecting here rather than leaving it to the client's own first
    /// query keeps an unreachable database reported as a server-side
    /// failure, instead of it surfacing as whatever status the query that
    /// happened to trip over it maps to.
    pub async fn resolve_client(
        &self,
        connection_name: &str,
    ) -> pylon_client::Result<Option<Arc<pylon_client::Client>>> {
        let key = if connection_name == MAIN_CONNECTION_ALIAS {
            "default"
        } else {
            connection_name
        };
        let Some(client) = self.clients.get(key) else {
            return Ok(None);
        };
        client.ensure_connected().await?;
        Ok(Some(client.clone()))
    }

    /// Samples every currently-connected client's pool accounting
    /// (size/available/waiting/max_size) into the `pylon_pgcon_pool_*`
    /// Prometheus gauges, labeled by connection name — called right before
    /// rendering `/metrics`. Only already-connected clients are sampled; a
    /// named connection nothing has touched yet has no pool to sample.
    pub fn record_pool_metrics(&self) {
        for (name, client) in &self.clients {
            // `pool_if_connected` rather than a connecting accessor: a
            // metrics scrape must never be the thing that opens a pool, or
            // every configured connection would report as live purely
            // because something asked about it.
            if let Some(pool) = client.pool_if_connected() {
                pylon_workers::metrics::record_pool_status(name, &pool.status());
            }
        }
    }

    /// Samples `_pylon."IndexOutbox"` into the queue-depth gauges, using the
    /// main connection. Sampled here rather than from inside a worker on
    /// purpose: the case worth seeing is an index kind with rows piling up
    /// and *no* worker running to report on them (a `[search]` or
    /// `[models.*]` section that was never configured), which a
    /// worker-driven metric can't observe.
    ///
    /// Best-effort — a failure here must not fail the `/metrics` response.
    pub async fn record_outbox_metrics(&self) {
        // Keyed by the config name, not the frontend's "main" alias —
        // `resolve_client` maps the latter onto the former before looking up.
        // Only sampled once something has actually connected it, for the
        // same reason `record_pool_metrics` checks.
        let Some(pool) = self.clients.get("default").and_then(|c| c.pool_if_connected()) else {
            return;
        };
        let rows = match pool
            .query_typed_named(pylon_workers::metrics::OUTBOX_DEPTH_SQL, &[], pool.types())
            .await
        {
            Ok(rows) => rows,
            // The outbox table only exists once a schema with an index has
            // been migrated; before that there is simply nothing to report.
            Err(_) => return,
        };
        for row in &rows {
            let pylon_value::DecodedValue::Object(fields) = row else {
                continue;
            };
            let get = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);
            let (
                Some(pylon_value::DecodedValue::Str(kind)),
                Some(pylon_value::DecodedValue::Str(status)),
                Some(pylon_value::DecodedValue::I64(depth)),
                Some(pylon_value::DecodedValue::I64(age)),
            ) = (get("index_kind"), get("status"), get("depth"), get("oldest_age"))
            else {
                continue;
            };
            pylon_workers::metrics::record_outbox_depth(kind, status, *depth, *age);
        }
    }
}
