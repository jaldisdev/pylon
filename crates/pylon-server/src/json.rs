//! JSON request/response helpers — the hyper counterpart of
//! `pylon/server/asgi.py`'s `_read_json_body`/`_send_json`. hyper's own
//! `Incoming` body already gives the whole request body via
//! `http_body_util::BodyExt::collect()` — no manual ASGI-style
//! `more_body` loop needed.

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Reads and parses the request body as JSON — an empty body decodes as
/// `{}`, matching `_read_json_body`'s own `json.loads(body) if body else {}`.
pub async fn read_json_body(req: Request<Incoming>) -> Result<serde_json::Value, BoxError> {
    let bytes = req.into_body().collect().await?.to_bytes();
    if bytes.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn json_response(status: StatusCode, payload: &serde_json::Value) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("static header name/value, status always valid")
}

pub fn not_found() -> Response<Full<Bytes>> {
    json_response(StatusCode::NOT_FOUND, &serde_json::json!({"error": "not found"}))
}

pub fn text_response(status: StatusCode, content_type: &str, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Full::new(Bytes::from(body)))
        .expect("static header name/value, status always valid")
}
