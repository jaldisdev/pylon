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
    /// Set once a payload has failed to decode. The subscription is over at
    /// that point; further `recv()` calls report end-of-stream rather than
    /// resuming, so a caller can't accidentally keep consuming a channel it
    /// has already seen foreign data on.
    ended: bool,
}

impl ChannelListener {
    /// Waits for the next decoded payload. Returns `None` once the dedicated
    /// connection closes, or once a payload has failed to decode.
    ///
    /// A malformed payload yields `Err(Error::MalformedPayload(_))` **and
    /// ends the subscription** — every later call returns `None`. A channel
    /// is a database-wide name, so anything can publish to it; failing hard
    /// keeps foreign data on a typed channel visible instead of silently
    /// dropped, and not resuming keeps that failure from being papered over
    /// by the next good message.
    ///
    /// A consumer that needs to stay subscribed past a bad message
    /// re-subscribes itself:
    ///
    /// ```ignore
    /// loop {
    ///     let mut sub = client.listen("UserUpdates").await?;
    ///     while let Some(payload) = sub.recv().await {
    ///         match payload {
    ///             Ok(v) => handle(v),
    ///             Err(e) => { log(e); break; }  // re-listen on the next pass
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// This matches `Client.listen()` on the Python side, where the same
    /// failure raises out of the `async for` and ends the generator.
    pub async fn recv(&mut self) -> Option<Result<Value>> {
        if self.ended {
            return None;
        }
        let item = self.rx.recv().await;
        if matches!(item, Some(Err(_))) {
            self.ended = true;
        }
        item
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

    Ok(ChannelListener {
        rx,
        _conn: conn,
        ended: false,
    })
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
/// Covers every base type `PG_TYPE_MAP` maps a built-in scalar to, except
/// `interval` and `bytea`, whose text encodings are non-trivial to parse
/// correctly and uncommon as a pub/sub payload — both pass through as the
/// raw string rather than risk a wrong decode.
///
/// Deliberately identical in coverage to
/// `pylon.schema._channels._decode_scalar_text` on the Python side: the same
/// `Channel` declaration has to yield the same type through either client,
/// and this used to fall back to a raw string for every temporal type while
/// Python decoded them, so a `Channel(datetime)` produced a `datetime` in
/// Python and a bare string in Rust.
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
        "date" => parse_date(text),
        "time" => parse_time(text),
        "timestamp" => parse_timestamp(text).map(Value::Timestamp),
        "timestamptz" => parse_timestamptz(text).map(Value::Timestamptz),
        _ => Ok(Value::Str(text.to_string())),
    }
}

/// Days from the Unix epoch to PostgreSQL's (2000-01-01), the offset between
/// what `chrono` counts from and what `Value::Date`/`Timestamp` store.
const PG_EPOCH_DAYS_FROM_UNIX: i64 = 10_957;
const PG_EPOCH_MICROS_FROM_UNIX: i64 = PG_EPOCH_DAYS_FROM_UNIX * 86_400 * 1_000_000;

fn malformed(kind: &str, text: &str, e: impl std::fmt::Display) -> Error {
    Error::MalformedPayload(format!("invalid {kind} {text:?}: {e}"))
}

fn parse_date(text: &str) -> Result<Value> {
    let d = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|e| malformed("date", text, e))?;
    let days = d
        .signed_duration_since(chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch date"))
        .num_days();
    Ok(Value::Date((days - PG_EPOCH_DAYS_FROM_UNIX) as i32))
}

fn parse_time(text: &str) -> Result<Value> {
    // Postgres omits the fractional part when it's zero.
    let t = chrono::NaiveTime::parse_from_str(text, "%H:%M:%S%.f").map_err(|e| malformed("time", text, e))?;
    let micros = t
        .signed_duration_since(chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("valid midnight"))
        .num_microseconds()
        .ok_or_else(|| Error::MalformedPayload(format!("time out of range: {text:?}")))?;
    Ok(Value::Time(micros))
}

fn parse_timestamp(text: &str) -> Result<i64> {
    let ts = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .map_err(|e| malformed("timestamp", text, e))?;
    Ok(ts.and_utc().timestamp_micros() - PG_EPOCH_MICROS_FROM_UNIX)
}

/// `timestamptz` renders with a numeric UTC offset (`+00`, `-04:30`), which
/// `%#z` accepts in all the widths Postgres emits.
fn parse_timestamptz(text: &str) -> Result<i64> {
    let ts = chrono::DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f%#z")
        .map_err(|e| malformed("timestamptz", text, e))?;
    Ok(ts.timestamp_micros() - PG_EPOCH_MICROS_FROM_UNIX)
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
        implicit_id: false,
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

    // ── Temporal parity with `_channels._decode_scalar_text` ──────────
    //
    // These four used to fall through to `Value::Str` here while the Python
    // client decoded them, so one `Channel` declaration produced two
    // different types depending on which client read it.

    #[test]
    fn decodes_a_date_to_pg_epoch_days() {
        // 2000-01-01 is the PG epoch itself, so day 0.
        assert_eq!(decode_scalar_text("2000-01-01", "date").unwrap(), Value::Date(0));
        assert_eq!(decode_scalar_text("2000-01-02", "date").unwrap(), Value::Date(1));
        assert_eq!(decode_scalar_text("1999-12-31", "date").unwrap(), Value::Date(-1));
    }

    #[test]
    fn decodes_a_time_to_microseconds_since_midnight() {
        assert_eq!(decode_scalar_text("00:00:00", "time").unwrap(), Value::Time(0));
        assert_eq!(
            decode_scalar_text("01:00:00", "time").unwrap(),
            Value::Time(3_600_000_000)
        );
        // Postgres only prints the fractional part when it is non-zero.
        assert_eq!(decode_scalar_text("00:00:00.5", "time").unwrap(), Value::Time(500_000));
    }

    #[test]
    fn decodes_a_timestamp_to_pg_epoch_microseconds() {
        assert_eq!(
            decode_scalar_text("2000-01-01 00:00:00", "timestamp").unwrap(),
            Value::Timestamp(0)
        );
        assert_eq!(
            decode_scalar_text("2000-01-01 00:00:01.5", "timestamp").unwrap(),
            Value::Timestamp(1_500_000)
        );
    }

    #[test]
    fn decodes_a_timestamptz_and_normalises_the_offset() {
        assert_eq!(
            decode_scalar_text("2000-01-01 00:00:00+00", "timestamptz").unwrap(),
            Value::Timestamptz(0)
        );
        // Same instant, written in a different zone.
        assert_eq!(
            decode_scalar_text("2000-01-01 01:00:00+01", "timestamptz").unwrap(),
            Value::Timestamptz(0)
        );
        assert_eq!(
            decode_scalar_text("1999-12-31 23:30:00-00:30", "timestamptz").unwrap(),
            Value::Timestamptz(0)
        );
    }

    #[test]
    fn a_malformed_temporal_payload_is_an_error_not_a_string() {
        for (text, pg_type) in [
            ("not-a-date", "date"),
            ("25:99:99", "time"),
            ("nope", "timestamp"),
            ("2000-01-01 00:00:00", "timestamptz"), // missing the offset
        ] {
            let err = decode_scalar_text(text, pg_type).unwrap_err();
            assert!(
                matches!(err, Error::MalformedPayload(_)),
                "{pg_type} {text:?} should be MalformedPayload, got {err:?}"
            );
        }
    }

    #[test]
    fn interval_and_bytea_still_pass_through_as_text() {
        // Matches the Python side's own carve-out — deliberately not parsed.
        assert_eq!(
            decode_scalar_text("1 day", "interval").unwrap(),
            Value::Str("1 day".into())
        );
        assert_eq!(
            decode_scalar_text("\\xdeadbeef", "bytea").unwrap(),
            Value::Str("\\xdeadbeef".into())
        );
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
