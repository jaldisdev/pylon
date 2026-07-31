//! The hyper server itself — `pub fn run` is the sole entry point a future
//! pyo3 binding (Phase 8) will call from `pylon serve`, blocking inside
//! `py.allow_threads` until shutdown. Builds its own Tokio runtime; no
//! Python event loop involved anywhere in this crate.
//!
//! HTTP/2 support is via `hyper-util`'s auto H1/H2 connection builder —
//! cleartext only (h2c), matching this project's existing no-TLS-anywhere
//! posture (`pylon-pgcon` has no TLS support either, by the same
//! reasoning: nothing in this codebase terminates TLS itself today).

use std::convert::Infallible;
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

/// Shared state every request handler sees — just the parsed config for
/// now; Phase 4 adds a per-connection `pylon_client::Client` map here
/// (mirrors `asgi.py`'s own lazily-built `clients: dict[str, Client]`).
pub struct AppState {
    pub config: Config,
}

/// Builds a fresh multi-threaded Tokio runtime and blocks on `serve` until
/// Ctrl+C. Intended to be called from inside `py.allow_threads` once the
/// CLI cutover (Phase 8) lands — nothing here depends on pyo3 or a Python
/// event loop.
pub fn run(config: Config) -> Result<()> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| Error::Invalid(format!("failed to start Tokio runtime: {e}")))?;
    rt.block_on(serve(config))
}

async fn serve(config: Config) -> Result<()> {
    let addr = format!("{}:{}", config.webserver.host, config.webserver.port);
    let state = Arc::new(AppState { config });

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
                return Ok(());
            }
        }
    }
}

/// Top-level route dispatch — mirrors `asgi.py`'s own `app()` if/elif
/// chain. Only `/metrics` is wired up in this phase (fully mechanical —
/// `pylon_workers::metrics::render()` needs no per-connection state at
/// all); `/api/...` routes land in Phase 4 once the per-connection
/// `pylon_client::Client` map exists on `AppState`.
async fn route(req: Request<Incoming>, state: Arc<AppState>) -> Response<Full<Bytes>> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") if state.config.metrics.enabled => {
            let body = pylon_workers::metrics::render();
            crate::json::text_response(StatusCode::OK, "text/plain; version=0.0.4; charset=utf-8", body)
        }
        _ => not_found(),
    }
}
