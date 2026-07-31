//! Background index/cache workers — spawns `pylon_workers`' native
//! LISTEN/NOTIFY loops as Tokio tasks on server startup, mirroring
//! `asgi.py::_handle_lifespan`'s `build_worker_tasks(schema, config,
//! shared_cache=True)` call (`asyncio.ensure_future`'d there,
//! `tokio::spawn`'d here — cancelled on shutdown either way).
//!
//! The signals dispatcher is deliberately excluded here (unlike every
//! other worker `build_worker_tasks` can return) — it needs a live
//! reference to each `@pylon.signal`-decorated Python callable, which only
//! exists in a Python process. A user relying on signals runs `pylon
//! worker start` alongside `pylon serve`, the same additive story that's
//! already true today (see the plan's own Context section).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use pylon_core::schema::{SchemaDescriptor, SearchBackend};
use pylon_workers::{CacheInvalidationWorker, MeilisearchClient, OpenSearchClient, ProviderConfig, SearchIndexWorker, VectorIndexWorker};

use crate::config::{ApiStyle, Config};

/// Matches `build_worker_tasks`'s own defaults (`pylon/cli/commands/
/// worker.py`) — `pylon serve` never overrides these, only the `worker
/// start` CLI's `--batch-size`/`--poll-interval` flags do.
const BATCH_SIZE: i64 = 50;
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Matches `run_meilisearch_worker`/`run_opensearch_worker`'s own default
/// `timeout_secs` (`pylon-py/src/workers.rs`).
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawns whatever background workers `schema`/`config` imply as detached
/// Tokio tasks, returning their `JoinHandle`s so the caller can abort them
/// on shutdown. A worker that fails to start (missing config, bad HTTP
/// client construction) logs and is simply skipped — mirrors
/// `build_worker_tasks`'s own `log.warning(...); continue` pattern, not a
/// startup-fatal condition (unlike the base DB connection itself).
pub fn spawn(
    schema: &SchemaDescriptor,
    config: &Config,
    dsn: &str,
    cache: Option<Arc<pylon_cache::Cache>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    let providers = build_providers(schema, config);
    if !providers.is_empty() {
        match VectorIndexWorker::new(schema.clone(), providers) {
            Ok(worker) => {
                eprintln!("pylon-server: VectorIndexWorker started");
                handles.push(spawn_index_worker(dsn.to_string(), worker, "VectorIndexWorker"));
            }
            Err(e) => eprintln!("pylon-server: failed to start VectorIndexWorker: {e}"),
        }
    }

    let want_opensearch =
        schema.types.iter().any(|t| t.search_indexes.iter().any(|si| si.backend == SearchBackend::OpenSearch));
    let want_meilisearch =
        schema.types.iter().any(|t| t.search_indexes.iter().any(|si| si.backend == SearchBackend::Meilisearch));

    if want_opensearch {
        match config.search.get("default") {
            None => eprintln!(
                "pylon-server: SearchIndex(backend=OpenSearch) declared but no [search] config found; skipping"
            ),
            Some(search_cfg) => {
                let base_url = format!("http://{}:{}", search_cfg.host, search_cfg.port);
                let auth = search_cfg.user.as_deref().zip(search_cfg.password.as_deref());
                match OpenSearchClient::new(&base_url, auth, SEARCH_TIMEOUT) {
                    Ok(client) => {
                        eprintln!("pylon-server: OpenSearchWorker started  base_url={base_url}");
                        let worker = SearchIndexWorker::new(schema.clone(), client, "OpenSearch");
                        handles.push(spawn_index_worker(dsn.to_string(), worker, "OpenSearchWorker"));
                    }
                    Err(e) => eprintln!("pylon-server: failed to start OpenSearchWorker: {e}"),
                }
            }
        }
    }

    if want_meilisearch {
        match config.search.get("default") {
            None => eprintln!(
                "pylon-server: SearchIndex(backend=Meilisearch) declared but no [search] config found; skipping"
            ),
            Some(search_cfg) => {
                let base_url = format!("http://{}:{}", search_cfg.host, search_cfg.port);
                match MeilisearchClient::new(&base_url, search_cfg.api_key.as_deref(), SEARCH_TIMEOUT) {
                    Ok(client) => {
                        eprintln!("pylon-server: MeilisearchWorker started  base_url={base_url}");
                        let worker = SearchIndexWorker::new(schema.clone(), client, "Meilisearch");
                        handles.push(spawn_index_worker(dsn.to_string(), worker, "MeilisearchWorker"));
                    }
                    Err(e) => eprintln!("pylon-server: failed to start MeilisearchWorker: {e}"),
                }
            }
        }
    }

    if let Some(cache) = cache {
        let dsn = dsn.to_string();
        eprintln!("pylon-server: CacheInvalidationWorker started (shared cache)  channel={}", pylon_workers::CACHE_NOTIFY_CHANNEL);
        handles.push(tokio::spawn(async move {
            match CacheInvalidationWorker::connect_with_cache(&dsn, cache).await {
                Ok(worker) => worker.run().await,
                Err(e) => eprintln!("pylon-server: failed to start CacheInvalidationWorker: {e}"),
            }
        }));
    }

    handles
}

fn spawn_index_worker<P>(dsn: String, worker: P, label: &'static str) -> tokio::task::JoinHandle<()>
where
    P: pylon_workers::index_worker::BatchProcessor + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = pylon_workers::index_worker::run(&dsn, BATCH_SIZE, POLL_INTERVAL, worker).await {
            eprintln!("pylon-server: {label} exited: {e}");
        }
    })
}

/// Mirrors `worker.py::_build_providers` — resolves each schema
/// `VectorIndex`'s `[models.*]` entry (falling back to `[models.default]`,
/// like the Python original), logging and skipping any index with no
/// matching config rather than failing the whole worker set.
fn build_providers(schema: &SchemaDescriptor, config: &Config) -> HashMap<(String, Option<String>), ProviderConfig> {
    let mut providers = HashMap::new();
    for t in &schema.types {
        for vi in &t.vector_indexes {
            let type_name = format!("{}::{}", t.module, t.name);
            let Some(model_cfg) = config.models.get(&vi.model).or_else(|| config.models.get("default")) else {
                eprintln!(
                    "pylon-server: no [models.{}] entry in pylon.toml for {type_name} (index={}); skipping",
                    vi.model,
                    vi.index_name.as_deref().unwrap_or("<default>")
                );
                continue;
            };
            providers.insert(
                (type_name, vi.index_name.clone()),
                ProviderConfig {
                    api_style: match model_cfg.api_style {
                        ApiStyle::OpenAi => "openai".to_string(),
                        ApiStyle::Anthropic => "anthropic".to_string(),
                    },
                    api_url: model_cfg.api_url.clone(),
                    model: model_cfg.model.clone(),
                    api_key: model_cfg.secret.clone(),
                },
            );
        }
    }
    providers
}
