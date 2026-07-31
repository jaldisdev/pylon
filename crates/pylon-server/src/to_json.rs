//! Converts `pylon_client::Value` into `serde_json::Value` for the JSON
//! API responses — the Rust counterpart of `pylon/server/asgi.py`'s
//! `_to_jsonable`. Considerably simpler than the Python original: `Value`
//! is already a generic, fully-decoded tree (no dataclass hydration, no
//! `__pylon_saved__`-style internal bookkeeping keys to filter out), so
//! this is a straight structural map.
//!
//! A few leaf conversions are deliberate new choices rather than a
//! byte-for-byte port, noted inline where that's the case.

use chrono::{Duration as ChronoDuration, NaiveDate, NaiveTime};
use pylon_client::{Group, Object, Range, Value};
use serde_json::{Map, Value as Json};

const PG_EPOCH_YEAR: i32 = 2000;

fn pg_epoch_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(PG_EPOCH_YEAR, 1, 1).expect("2000-01-01 is always valid")
}

fn date_to_iso(days: i32) -> String {
    (pg_epoch_date() + ChronoDuration::days(i64::from(days))).format("%Y-%m-%d").to_string()
}

fn time_to_iso(microseconds: i64) -> String {
    let secs = microseconds.div_euclid(1_000_000);
    let micros = microseconds.rem_euclid(1_000_000);
    let time = NaiveTime::from_num_seconds_from_midnight_opt(secs as u32, (micros * 1000) as u32)
        .unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    time.format("%H:%M:%S%.6f").to_string()
}

fn timestamp_to_iso(microseconds: i64) -> String {
    let naive = pg_epoch_date().and_hms_opt(0, 0, 0).unwrap() + ChronoDuration::microseconds(microseconds);
    naive.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
}

fn timestamptz_to_iso(microseconds: i64) -> String {
    format!("{}+00:00", timestamp_to_iso(microseconds))
}

/// `Duration` (Postgres `interval`) has no dedicated case in the Python
/// original either (it isn't one of the `isinstance` checks in
/// `_to_jsonable`) — this ISO-8601 duration string is a deliberate new
/// choice, not a port of existing behavior.
fn duration_to_iso(months: i32, days: i32, microseconds: i64) -> String {
    let secs = microseconds.div_euclid(1_000_000);
    let micros = microseconds.rem_euclid(1_000_000);
    if micros == 0 {
        format!("P{months}M{days}DT{secs}S")
    } else {
        format!("P{months}M{days}DT{secs}.{micros:06}S")
    }
}

fn object_to_json(obj: &Object) -> Json {
    let mut map = Map::new();
    // `__pylon_type__` passes through as its own key when present — mirrors
    // `_to_jsonable`'s explicit exception for that one dunder-prefixed
    // attribute (every other `__pylon_*` key it strips is Python-only
    // bookkeeping `Object` never carries in the first place).
    if let Some(type_name) = obj.type_name() {
        map.insert("__pylon_type__".to_string(), Json::from(type_name));
    }
    for (name, value) in obj.fields() {
        map.insert(name.to_string(), value_to_json(value));
    }
    Json::Object(map)
}

fn range_to_json(range: &Range) -> Json {
    serde_json::json!({
        "lower": range.lower.as_ref().map(value_to_json),
        "upper": range.upper.as_ref().map(value_to_json),
        "inc_lower": range.inc_lower,
        "inc_upper": range.inc_upper,
        "empty": range.empty,
    })
}

fn group_to_json(group: &Group) -> Json {
    serde_json::json!({
        "key": object_to_json(&group.key),
        "grouping": group.grouping,
        "elements": group.elements.iter().map(value_to_json).collect::<Vec<_>>(),
    })
}

pub fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int64(i) => Json::from(*i),
        Value::Float64(f) => Json::from(*f),
        Value::Str(s) => Json::from(s.clone()),
        // No dedicated `bytes` case in `_to_jsonable` either (it would hit
        // the catch-all `return value`, which isn't actually JSON-safe for
        // raw Python `bytes`) — hex-encoding is a deliberate, safe new
        // choice rather than replicating that latent gap.
        Value::Bytes(b) => Json::from(hex::encode(b)),
        Value::Uuid(u) => Json::from(u.to_string()),
        // Matches `_to_jsonable`'s own `float(value)` — lossy, but this
        // keeps wire compatibility with the existing frontend, which
        // expects a JSON number at a `{"kind": "decimal"}`-tagged shape
        // position, not a string.
        Value::Decimal(s) => s.parse::<f64>().map(Json::from).unwrap_or(Json::Null),
        Value::Duration { months, days, microseconds } => Json::from(duration_to_iso(*months, *days, *microseconds)),
        Value::Date(days) => Json::from(date_to_iso(*days)),
        Value::Time(us) => Json::from(time_to_iso(*us)),
        Value::Timestamp(us) => Json::from(timestamp_to_iso(*us)),
        Value::Timestamptz(us) => Json::from(timestamptz_to_iso(*us)),
        Value::Range(r) => range_to_json(r),
        Value::Array(items) => Json::Array(items.iter().map(value_to_json).collect()),
        Value::Tuple(items) => Json::Array(items.iter().map(value_to_json).collect()),
        Value::Object(obj) => object_to_json(obj),
        // The enum's own string label — matches what the frontend actually
        // needs to display/compare, not an internally-tagged representation.
        Value::Enum { value, .. } => Json::from(value.clone()),
        Value::Group(g) => group_to_json(g),
        Value::VectorSearch { object, distance } => {
            serde_json::json!({"object": value_to_json(object), "distance": distance})
        }
        Value::FtsSearch { object, score } => {
            serde_json::json!({"object": value_to_json(object), "score": score})
        }
    }
}

/// Structured error payload for a PyQL compile failure — the Rust
/// counterpart of `_pylon_error_payload`. `hint`/`details` are omitted
/// entirely: confirmed this session that `pylon-py`'s own
/// `construct_pylon_error` never actually populates them (only
/// `query`/`position_start`/`position_end`/`line`/`col` are passed to
/// `PylonError._from_transpiler`), so those fields are dead code in the
/// current system, not something this port needs to replicate.
pub fn compile_error_payload(err: &pylon_core::error::PyQLError) -> Json {
    let (class_name, message, position) = err.class_name_message_position();
    let mut payload = serde_json::json!({"error": message, "errorType": class_name});
    // `Position { line: 0, col: 0 }` is this codebase's own established
    // "no meaningful position" sentinel (used throughout `ir/compiler.rs`
    // for errors with nothing to point at).
    if position.line != 0 || position.col != 0 {
        payload["position"] = serde_json::json!({"line": position.line, "col": position.col});
    }
    payload
}

/// Error payload for anything else (`pylon_client::Error`) — no compile
/// position to report, just a message and a coarse error-kind label.
pub fn client_error_payload(err: &pylon_client::Error) -> Json {
    if let pylon_client::Error::Compile(e) = err {
        return compile_error_payload(e);
    }
    serde_json::json!({"error": err.to_string(), "errorType": "PylonExecutionError"})
}
