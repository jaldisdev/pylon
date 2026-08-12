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

//! Serves the built React SPA at `/` when `[ui].enabled` — Rust port of
//! `asgi.py::_serve_static`. The frontend build (`crates/pylon-server/static/`,
//! gitignored — a deploy/dev step copies the `pylon-ui` build output there
//! before `cargo build`) is embedded into the binary at compile time via
//! `include_dir!`, so a built `pylon-server` is fully self-contained by
//! default: no separate assets directory to ship. `--static-dir` still lets
//! you point at an on-disk build instead (handy for iterating on the
//! frontend without a Rust rebuild each time). Falls back to `index.html`
//! for client-side routing, `mime_guess` for the content type.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};
use include_dir::{Dir, include_dir};

use crate::json::not_found;

static STATIC_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static");

pub async fn serve(path: &str, override_dir: Option<&std::path::Path>) -> Response<Full<Bytes>> {
    let rel = path.trim_start_matches('/');
    match override_dir {
        Some(dir) => serve_from_disk(dir, rel).await,
        None => serve_from_embedded(rel),
    }
}

fn serve_from_embedded(rel: &str) -> Response<Full<Bytes>> {
    let Some(file) = STATIC_DIR.get_file(rel).or_else(|| STATIC_DIR.get_file("index.html")) else {
        return not_found();
    };
    respond(file.path(), file.contents().to_vec())
}

async fn serve_from_disk(dir: &std::path::Path, rel: &str) -> Response<Full<Bytes>> {
    let Ok(root) = dir.canonicalize() else {
        return not_found();
    };
    let candidate = root.join(rel);
    let candidate = match candidate.canonicalize() {
        Ok(c) if c.starts_with(&root) && c.is_file() => c,
        _ => root.join("index.html"),
    };
    let Ok(bytes) = tokio::fs::read(&candidate).await else {
        return not_found();
    };
    respond(&candidate, bytes)
}

fn respond(path: &std::path::Path, bytes: Vec<u8>) -> Response<Full<Bytes>> {
    let content_type = mime_guess::from_path(path).first_or_octet_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type.as_ref())
        .body(Full::new(Bytes::from(bytes)))
        .expect("static header name/value, status always valid")
}
