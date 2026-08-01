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

//! Fetches the schema snapshot from `_pylon."Schema"` on a background
//! thread — the same source `pylon-client`/`pylon-server` read (see
//! `pylon_core::migrate::read_schema_snapshot`) instead of the retired
//! `.pylon/schema.json` file this used to poll for on every diagnostics
//! pass. There's no embedded Python interpreter in this binary to build a
//! `SchemaDescriptor` itself, so it depends on `pylon migration apply`
//! (or `pylon migration watch`'s dev-mode sync) having written one to the
//! configured `[database]` connection's database. If none is reachable yet
//! — no `pylon.toml`, no `[database]` block, connection refused, no
//! snapshot written yet — diagnostics gracefully degrade to parser-only
//! (matching the server's original behavior) rather than failing outright.
//!
//! A background poll (not a per-keystroke fetch) because a DB round trip
//! on every diagnostics pass would be far too slow for an LSP; a 5-second
//! interval is a reasonable balance between "picks up a fresh migration
//! promptly" and "doesn't hammer the database from an idle editor".

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pylon_core::schema::SchemaDescriptor;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct SchemaState {
    shared: Arc<Mutex<Option<SchemaDescriptor>>>,
}

impl SchemaState {
    /// Spawns the background poll thread (a no-op if `workspace_root` is
    /// `None` — nothing to resolve `pylon.toml` relative to). Diagnostics
    /// start in parser-only mode and pick up the DB schema whenever the
    /// first successful poll lands.
    pub fn new(workspace_root: Option<PathBuf>) -> Self {
        let shared: Arc<Mutex<Option<SchemaDescriptor>>> = Arc::new(Mutex::new(None));
        if let Some(root) = workspace_root {
            let shared = shared.clone();
            std::thread::spawn(move || poll_thread(root, shared));
        }
        Self { shared }
    }

    /// A clone of the currently-known schema, or `None` if no poll has
    /// succeeded yet (or ever will, e.g. no `[database]` configured).
    pub fn get(&self) -> Option<SchemaDescriptor> {
        self.shared.lock().unwrap().clone()
    }
}

fn poll_thread(workspace_root: PathBuf, shared: Arc<Mutex<Option<SchemaDescriptor>>>) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("pylon-lsp: failed to start the schema-poll runtime, staying parser-only: {err}");
            return;
        }
    };
    rt.block_on(poll_loop(workspace_root, shared));
}

async fn poll_loop(workspace_root: PathBuf, shared: Arc<Mutex<Option<SchemaDescriptor>>>) {
    let dsn = match resolve_dsn(&workspace_root) {
        Ok(dsn) => dsn,
        Err(err) => {
            eprintln!("pylon-lsp: {err}, staying parser-only");
            return;
        }
    };
    let pool = match pylon_pgcon::PgPool::connect(&dsn, 1).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("pylon-lsp: could not connect to the database ({err}), staying parser-only");
            return;
        }
    };

    let mut last_json: Option<String> = None;
    loop {
        match pylon_core::migrate::read_schema_snapshot(&pool).await {
            Ok(Some(json)) if last_json.as_ref() != Some(&json) => match serde_json::from_str::<SchemaDescriptor>(&json) {
                Ok(schema) => {
                    *shared.lock().unwrap() = Some(schema);
                    pylon_core::query::clear_query_cache();
                    eprintln!("pylon-lsp: loaded schema snapshot from the database");
                    last_json = Some(json);
                }
                Err(err) => eprintln!("pylon-lsp: failed to parse schema snapshot: {err}"),
            },
            Ok(_) => {} // unchanged, or no snapshot written yet
            Err(err) => eprintln!("pylon-lsp: schema poll failed: {err}"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn resolve_dsn(workspace_root: &std::path::Path) -> Result<String, String> {
    let config = pylon_config::config::load_config(Some(workspace_root)).map_err(|e| format!("failed to load pylon.toml: {e}"))?;
    let db = config.connections.get("default").ok_or_else(|| "no [database] configured in pylon.toml".to_string())?;
    Ok(db.dsn_string())
}
