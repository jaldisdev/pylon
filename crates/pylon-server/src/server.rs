//! The hyper server itself — `pub fn run` is the sole entry point, meant to
//! be called from a standalone `pylon-server` binary rather than through
//! Python. Builds its own Tokio runtime; no Python event loop involved
//! anywhere in this crate.
//!
//! HTTP/2 support is via `hyper-util`'s auto H1/H2 connection builder —
//! cleartext only (h2c), matching this project's existing no-TLS-anywhere
//! posture (`pylon-pgcon` has no TLS support either, by the same
//! reasoning: nothing in this codebase terminates TLS itself today).

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use tokio::net::TcpListener;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::json::not_found;
use crate::state::AppState;

/// Builds a fresh multi-threaded Tokio runtime and blocks on `serve` until
/// Ctrl+C; nothing here depends on pyo3 or a Python event loop.
/// `static_dir` is an optional on-disk override for the frontend build,
/// otherwise served from the assets embedded into the binary at compile
/// time (see `static_files::STATIC_DIR`) — `None` is the common case.
pub fn run(config: Config, static_dir: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Invalid(format!("failed to start Tokio runtime: {e}")))?;
    rt.block_on(serve(config, static_dir))
}

async fn serve(config: Config, static_dir: Option<PathBuf>) -> Result<()> {
    let addr = format!("{}:{}", config.webserver.host, config.webserver.port);
    let state = Arc::new(AppState::new(config, static_dir)?);

    // Eagerly connect the base ("default"/"main") connection at startup —
    // mirrors `asgi.py::_handle_lifespan`'s own lifespan-startup behavior
    // (a bad DSN/host/credentials fails the whole server at startup there,
    // not just the first request that happens to touch it); every other
    // named connection still connects lazily on first use, same as before.
    // The resulting `SchemaDescriptor` is also what decides which
    // background workers to spawn (`workers::spawn`), same as
    // `build_worker_tasks(schema, config, ...)` being called from that
    // same lifespan-startup handler.
    let main_client = state
        .resolve_client("main")
        .await
        .map_err(|e| Error::Invalid(format!("failed to connect base [database] connection: {e}")))?
        .ok_or_else(|| Error::Invalid("no [database] connection configured".to_string()))?;
    let worker_handles = match state.config.connections.get("default") {
        Some(db) => crate::workers::spawn(&main_client.schema(), &state.config, &db.dsn_string(), state.cache.clone()),
        None => Vec::new(),
    };

    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| Error::Invalid(format!("failed to bind {addr}: {e}")))?;
    eprintln!("pylon-server: listening on {addr}");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("pylon-server: accept error: {e}");
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let state = state.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(route(req, state).await) }
                    });
                    if let Err(err) = AutoBuilder::new(TokioExecutor::new()).serve_connection(io, service).await {
                        eprintln!("pylon-server: connection error: {err}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("pylon-server: shutting down");
                // Mirrors `_handle_lifespan`'s own shutdown: cancel every
                // background worker task before returning.
                for handle in &worker_handles {
                    handle.abort();
                }
                return Ok(());
            }
        }
    }
}

/// Splits `"/api/<connection>/<rest>"` into `(connection, "/<rest>")` —
/// mirrors `asgi.py::_split_connection_path`. `None` for anything that
/// isn't at least `/api/<segment>/<segment>`, including the bare
/// process-level routes (`/api/schema`, `/api/connections`, ...), which
/// have no connection segment to split off at all.
fn split_connection_path(path: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 || parts[0] != "" || parts[1] != "api" || parts[2].is_empty() {
        return None;
    }
    Some((parts[2].to_string(), format!("/{}", parts[3..].join("/"))))
}

/// Top-level route dispatch — mirrors `asgi.py`'s own `app()` if/elif
/// chain.
async fn route(req: Request<Incoming>, state: Arc<AppState>) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET && path == "/metrics" && state.config.metrics.enabled {
        return crate::json::text_response(
            StatusCode::OK,
            "text/plain; version=0.0.4; charset=utf-8",
            pylon_workers::metrics::render(),
        );
    }
    if method == Method::GET && path == "/api/schema" {
        return crate::routes::handle_schema(state.clone()).await;
    }
    if method == Method::GET && path == "/api/globals" {
        return crate::routes::handle_globals(state.clone()).await;
    }
    if method == Method::GET && path == "/api/connections" {
        return crate::routes::handle_connections(&state);
    }
    if method == Method::GET && path == "/api/models" {
        return crate::routes::handle_models(&state);
    }
    if method == Method::GET && path == "/api/config-options" {
        return crate::routes::handle_config_options();
    }

    if let Some((connection, rest)) = split_connection_path(&path) {
        if method == Method::GET && rest == "/stats" {
            return crate::routes::handle_stats(state.clone(), &connection).await;
        }
        if method == Method::POST && rest == "/query" {
            return match crate::json::read_json_body(req).await {
                Ok(body) => crate::routes::handle_query(state.clone(), &connection, body).await,
                Err(e) => crate::json::json_response(StatusCode::BAD_REQUEST, &serde_json::json!({"error": e.to_string()})),
            };
        }
        if method == Method::POST && rest == "/analyze" {
            return match crate::json::read_json_body(req).await {
                Ok(body) => crate::routes::handle_analyze(state.clone(), &connection, body).await,
                Err(e) => crate::json::json_response(StatusCode::BAD_REQUEST, &serde_json::json!({"error": e.to_string()})),
            };
        }
        if method == Method::POST && rest == "/ai/chat" {
            return match crate::json::read_json_body(req).await {
                Ok(body) => crate::ai_chat::handle_ai_chat(state.clone(), &connection, body).await,
                Err(e) => crate::json::json_response(StatusCode::BAD_REQUEST, &serde_json::json!({"error": e.to_string()})),
            };
        }
    }

    if state.config.ui.enabled {
        return crate::static_files::serve(&path, state.static_dir_override()).await;
    }
    not_found()
}
