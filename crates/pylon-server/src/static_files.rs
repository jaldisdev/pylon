//! Serves the built React SPA at `/` when `[ui].enabled` — Rust port of
//! `asgi.py::_serve_static`. Hand-rolled (no `tower-http::ServeDir`
//! integration) to mirror the existing simple implementation directly:
//! path-traversal guard, falling back to `index.html` for client-side
//! routing, `mime_guess` for the content type.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use crate::json::not_found;

pub async fn serve(static_dir: &std::path::Path, path: &str) -> Response<Full<Bytes>> {
    let Ok(root) = static_dir.canonicalize() else {
        return not_found();
    };

    let candidate = root.join(path.trim_start_matches('/'));
    let candidate = match candidate.canonicalize() {
        Ok(c) if c.starts_with(&root) && c.is_file() => c,
        _ => root.join("index.html"),
    };

    let Ok(bytes) = tokio::fs::read(&candidate).await else {
        return not_found();
    };
    let content_type = mime_guess::from_path(&candidate).first_or_octet_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type.as_ref())
        .body(Full::new(Bytes::from(bytes)))
        .expect("static header name/value, status always valid")
}
