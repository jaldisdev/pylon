//! Shared per-request state — the Rust counterpart of `asgi.py`'s
//! `clients: dict[str, Client]` (populated lazily, one connected
//! `pylon_client::Client` per named connection).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::Config;

/// "main" is the frontend's fixed name for the base `[database]` block,
/// which `config.connections` stores under `"default"` — matches
/// `asgi.py`'s own `_MAIN_CONNECTION_ALIAS`.
const MAIN_CONNECTION_ALIAS: &str = "main";

pub struct AppState {
    pub config: Config,
    /// On-disk frontend build to serve instead of the assets embedded into
    /// the binary at compile time (`static_files::STATIC_DIR`) — set via
    /// `--static-dir`, for iterating on the frontend without a Rust
    /// rebuild each time. `None` (the default) serves the embedded build.
    static_dir_override: Option<PathBuf>,
    clients: Mutex<HashMap<String, Arc<pylon_client::Client>>>,
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
            let cache = pylon_cache::Cache::open(&config.cache.path, config.cache.max_size_mb as usize)
                .map_err(|e| crate::error::Error::Invalid(format!("failed to open cache at {}: {e}", config.cache.path.display())))?;
            Some(Arc::new(cache))
        } else {
            None
        };
        Ok(Self { config, static_dir_override, clients: Mutex::new(HashMap::new()), cache })
    }

    pub fn static_dir_override(&self) -> Option<&std::path::Path> {
        self.static_dir_override.as_deref()
    }

    /// Looks up (or lazily connects) the `Client` for `connection_name` —
    /// mirrors `asgi.py::_resolve_client`. `Ok(None)` means the name isn't
    /// a configured connection at all (the caller turns that into a 404).
    pub async fn resolve_client(&self, connection_name: &str) -> pylon_client::Result<Option<Arc<pylon_client::Client>>> {
        let key = if connection_name == MAIN_CONNECTION_ALIAS { "default" } else { connection_name };
        let Some(db) = self.config.connections.get(key) else {
            return Ok(None);
        };
        let mut clients = self.clients.lock().await;
        if let Some(existing) = clients.get(key) {
            return Ok(Some(existing.clone()));
        }
        let mut builder = pylon_client::Client::builder(db.dsn_string())
            .max_pool_size(db.pool_max_size as usize);
        if let Some(cache) = &self.cache {
            builder = builder.cache_handle(cache.clone());
        }
        let client = builder.build().await?;
        let client = Arc::new(client);
        clients.insert(key.to_string(), client.clone());
        Ok(Some(client))
    }
}
