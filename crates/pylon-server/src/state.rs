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
    /// `.pylon/schema.json`, resolved relative to `pylon.toml`'s own
    /// directory (mirrors how `[cache].path` and `[project].schema-dir`
    /// are both resolved relative to it too).
    schema_path: PathBuf,
    /// Directory a production build of the frontend is copied into —
    /// mirrors `asgi.py::STATIC_DIR`. Placeholder location (no build
    /// output is shipped yet on the Python side either, so this simply
    /// 404s in practice today, same as the system it's replacing) pending
    /// a real packaging decision in a later phase.
    static_dir: PathBuf,
    clients: Mutex<HashMap<String, Arc<pylon_client::Client>>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let toml_dir = config.toml_path.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();
        let schema_path = toml_dir.join(".pylon/schema.json");
        let static_dir = toml_dir.join(".pylon/static");
        Self { config, schema_path, static_dir, clients: Mutex::new(HashMap::new()) }
    }

    pub fn static_dir(&self) -> &std::path::Path {
        &self.static_dir
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
        let client = pylon_client::Client::builder(db.dsn_string())
            .max_pool_size(db.pool_max_size as usize)
            .schema_path(self.schema_path.clone())
            .build()
            .await?;
        let client = Arc::new(client);
        clients.insert(key.to_string(), client.clone());
        Ok(Some(client))
    }
}
