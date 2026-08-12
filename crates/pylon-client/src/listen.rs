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

//! `Client::listen()` — subscribes to a schema-declared `Channel` over a
//! dedicated (non-pooled) LISTEN/NOTIFY connection and decodes each payload
//! into the crate's generic `Value`. Mirrors `pylon/client.py`'s own
//! `Client.listen()` (there, a typed async generator; here, a `recv()`-based
//! handle — this crate has no `Stream`/async-generator precedent to build
//! on, and `recv()` matches `tokio::sync::mpsc::Receiver`'s own idiom
//! closely enough not to need one).

use pylon_core::schema::{ChannelDescriptor, ChannelPayload, SchemaDescriptor};
use pylon_pgcon::PgListener;
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::value::{Object, Value};

/// A live subscription to one `Channel`, returned by [`crate::Client::listen`].
///
/// Holds a dedicated (non-pooled) connection for as long as it's alive —
/// `LISTEN` is per-session, so sharing a pooled connection would leak the
/// subscription onto whatever unrelated query later borrows that same
/// connection back out of the pool. Dropping this closes the connection
/// and ends the server-side subscription with it.
pub struct ChannelListener {
    rx: mpsc::UnboundedReceiver<Result<Value>>,
    _conn: PgListener,
}

impl ChannelListener {
    /// Waits for the next decoded payload. Returns `None` once the
    /// dedicated connection closes — mirrors
    /// `tokio::sync::mpsc::Receiver::recv`'s own end-of-stream signal, not
    /// an error.
    ///
    /// `Err(Error::MalformedPayload(_))` if a payload arrives that doesn't
    /// match the Channel's own declared shape — that's returned rather than
    /// silently skipped, but doesn't end the subscription itself; the next
    /// `recv()` call keeps listening for the next notification.
    pub async fn recv(&mut self) -> Option<Result<Value>> {
        self.rx.recv().await
    }
}

pub(crate) async fn listen(dsn: &str, schema: &SchemaDescriptor, channel: &str) -> Result<ChannelListener> {
    let ch = schema
        .find_channel(channel)
        .cloned()
        .ok_or_else(|| Error::UnknownChannel(channel.to_string()))?;
    let wire_name = ch.wire_name.clone();

    let (tx, rx) = mpsc::unbounded_channel();
    let conn = PgListener::connect(dsn, move |n| {
        // The receiving end only ever drops once `ChannelListener` itself
        // does (which also drops `_conn`, ending this callback's own
        // background task) — a send failure here means that already
        // happened moments ago; nothing left to report it to.
        let _ = tx.send(decode_payload(&ch, n.payload()));
    })
    .await
    .map_err(Error::Db)?;
    conn.listen(&wire_name).await.map_err(Error::Db)?;

    Ok(ChannelListener { rx, _conn: conn })
}

fn decode_payload(ch: &ChannelDescriptor, raw: &str) -> Result<Value> {
    match &ch.payload {
        ChannelPayload::Type(_) => decode_uuid(raw),
        ChannelPayload::Scalar(pg_type) => decode_scalar_text(raw, pg_type),
        ChannelPayload::Object(fields) => decode_object_payload(fields, raw),
    }
}

fn decode_uuid(text: &str) -> Result<Value> {
    text.parse::<uuid::Uuid>()
        .map(Value::Uuid)
        .map_err(|e| Error::MalformedPayload(format!("invalid uuid {text:?}: {e}")))
}

/// Decodes NOTIFY's raw text payload as PostgreSQL's own `<pg_type>::text`
/// cast would have rendered it (see `notify()`'s SQL emission — the
/// payload is always literally cast to `text` before being sent).
///
/// Covers `uuid`/`int2`/`int4`/`int8`/`float4`/`float8`/`numeric`/`boolean`
/// — every base type this reasonably expects as a Channel payload.
/// `date`/`time`/`timestamp`/`timestamptz`/`interval`/`bytea` fall back to
/// the raw string: this crate has no date/time dependency to convert them
/// into `Value::Date`/`Time`/`Timestamp`'s PG-epoch-relative integer
/// representations correctly, and getting that wrong silently would be
/// worse than returning the text as-is (matches
/// `pylon.schema._channels._decode_scalar_text`'s own `interval`/`bytea`
/// carve-out on the Python side, just a wider one here).
fn decode_scalar_text(text: &str, pg_type: &str) -> Result<Value> {
    match pg_type {
        "uuid" => decode_uuid(text),
        "int2" | "int4" | "int8" => text
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|e| Error::MalformedPayload(format!("invalid integer {text:?}: {e}"))),
        "float4" | "float8" => text
            .parse::<f64>()
            .map(Value::Float64)
            .map_err(|e| Error::MalformedPayload(format!("invalid float {text:?}: {e}"))),
        // Kept as its canonical string form, same as `Value::Decimal`'s own
        // documented convention elsewhere — no parsing needed.
        "numeric" => Ok(Value::Decimal(text.to_string())),
        "boolean" => Ok(Value::Bool(text == "true" || text == "t")),
        _ => Ok(Value::Str(text.to_string())),
    }
}

fn decode_object_payload(declared_fields: &[(String, String)], raw_payload: &str) -> Result<Value> {
    let parsed: serde_json::Value =
        serde_json::from_str(raw_payload).map_err(|e| Error::MalformedPayload(format!("invalid JSON payload: {e}")))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| Error::MalformedPayload("expected a JSON object payload".to_string()))?;

    let mut fields = Vec::with_capacity(declared_fields.len());
    for (name, pg_type) in declared_fields {
        let json_value = obj
            .get(name)
            .ok_or_else(|| Error::MalformedPayload(format!("payload is missing declared field {name:?}")))?;
        fields.push((name.clone(), decode_json_value(json_value, pg_type)?));
    }
    Ok(Value::Object(Object {
        type_name: None,
        fields,
    }))
}

/// Like `decode_scalar_text`, but for a value already parsed out of an
/// Object channel's JSON payload — `serde_json` already turned a JSON
/// number/bool/null into the right shape, so only the types `to_jsonb()`
/// renders as a JSON *string* (uuid, numeric, and the same date/time/etc.
/// carve-out `decode_scalar_text` documents) need any further decoding here.
fn decode_json_value(value: &serde_json::Value, pg_type: &str) -> Result<Value> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if pg_type == "numeric" {
                Ok(Value::Decimal(n.to_string()))
            } else if let Some(i) = n.as_i64() {
                Ok(Value::Int64(i))
            } else {
                Ok(Value::Float64(n.as_f64().unwrap_or_default()))
            }
        }
        serde_json::Value::String(s) => decode_scalar_text(s, pg_type),
        other => Err(Error::MalformedPayload(format!(
            "unexpected JSON shape {other:?} for a field of type '{pg_type}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(payload: ChannelPayload) -> ChannelDescriptor {
        ChannelDescriptor {
            name: "X".into(),
            module: "m".into(),
            wire_name: "m__x".into(),
            payload,
            description: None,
        }
    }

    #[test]
    fn decode_scalar_uuid() {
        let u = uuid::Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap();
        assert_eq!(decode_scalar_text(&u.to_string(), "uuid").unwrap(), Value::Uuid(u));
    }

    #[test]
    fn decode_scalar_integers() {
        assert_eq!(decode_scalar_text("42", "int2").unwrap(), Value::Int64(42));
        assert_eq!(decode_scalar_text("42", "int4").unwrap(), Value::Int64(42));
        assert_eq!(decode_scalar_text("42", "int8").unwrap(), Value::Int64(42));
    }

    #[test]
    fn decode_scalar_floats() {
        assert_eq!(decode_scalar_text("0.5", "float4").unwrap(), Value::Float64(0.5));
        assert_eq!(decode_scalar_text("0.5", "float8").unwrap(), Value::Float64(0.5));
    }

    #[test]
    fn decode_scalar_numeric_kept_as_string() {
        assert_eq!(
            decode_scalar_text("123.456", "numeric").unwrap(),
            Value::Decimal("123.456".to_string())
        );
    }

    #[test]
    fn decode_scalar_boolean() {
        assert_eq!(decode_scalar_text("true", "boolean").unwrap(), Value::Bool(true));
        assert_eq!(decode_scalar_text("false", "boolean").unwrap(), Value::Bool(false));
    }

    #[test]
    fn decode_scalar_text_passthrough() {
        assert_eq!(
            decode_scalar_text("hello", "text").unwrap(),
            Value::Str("hello".to_string())
        );
    }

    #[test]
    fn decode_scalar_unsupported_type_falls_back_to_raw_string() {
        assert_eq!(
            decode_scalar_text("1 day 02:00:00", "interval").unwrap(),
            Value::Str("1 day 02:00:00".to_string())
        );
    }

    #[test]
    fn decode_scalar_rejects_malformed_uuid() {
        let err = decode_scalar_text("not-a-uuid", "uuid").unwrap_err();
        assert!(matches!(err, Error::MalformedPayload(_)), "got: {err:?}");
    }

    #[test]
    fn decode_scalar_rejects_malformed_integer() {
        let err = decode_scalar_text("not-an-int", "int8").unwrap_err();
        assert!(matches!(err, Error::MalformedPayload(_)), "got: {err:?}");
    }

    #[test]
    fn decode_payload_type_kind_is_a_uuid() {
        let u = uuid::Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap();
        let ch = channel(ChannelPayload::Type("m::Widget".into()));
        assert_eq!(decode_payload(&ch, &u.to_string()).unwrap(), Value::Uuid(u));
    }

    #[test]
    fn decode_payload_scalar_kind() {
        let ch = channel(ChannelPayload::Scalar("text".into()));
        assert_eq!(decode_payload(&ch, "hello").unwrap(), Value::Str("hello".to_string()));
    }

    #[test]
    fn decode_payload_object_kind() {
        let u = uuid::Uuid::parse_str("3fa85f64-5717-4562-b3fc-2c963f66afa6").unwrap();
        let ch = channel(ChannelPayload::Object(vec![
            ("doc_id".into(), "uuid".into()),
            ("score".into(), "float8".into()),
        ]));
        let payload = format!(r#"{{"doc_id": "{u}", "score": 0.5}}"#);
        let Value::Object(obj) = decode_payload(&ch, &payload).unwrap() else {
            panic!("expected Object")
        };
        assert_eq!(obj.get("doc_id"), Some(&Value::Uuid(u)));
        assert_eq!(obj.get("score"), Some(&Value::Float64(0.5)));
    }

    #[test]
    fn decode_payload_object_kind_rejects_malformed_json() {
        let ch = channel(ChannelPayload::Object(vec![("doc_id".into(), "uuid".into())]));
        let err = decode_payload(&ch, "not json at all").unwrap_err();
        assert!(matches!(err, Error::MalformedPayload(_)), "got: {err:?}");
    }

    #[test]
    fn decode_payload_object_kind_rejects_missing_field() {
        let ch = channel(ChannelPayload::Object(vec![("doc_id".into(), "uuid".into())]));
        let err = decode_payload(&ch, "{}").unwrap_err();
        assert!(matches!(err, Error::MalformedPayload(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn listen_rejects_an_unknown_channel() {
        let schema = SchemaDescriptor::default();
        let err = listen("postgresql://ignored", &schema, "NoSuchChannel")
            .await
            .err()
            .expect("expected an error");
        assert!(matches!(err, Error::UnknownChannel(_)), "got: {err:?}");
    }
}
