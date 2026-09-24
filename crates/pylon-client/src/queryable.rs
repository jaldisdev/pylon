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

//! [`Queryable`] — decoding a generic [`Value`] into a caller's own type,
//! and the helpers `#[derive(Queryable)]` generates calls to.
//!
//! Mirrors `the upstream Rust client`'s trait of the same name in role, not in
//! mechanism: the upstream engine decodes straight off its binary wire format against a
//! type descriptor, while Pylon has already walked the compiled shape into
//! a [`Value`] by the time this runs, so this is a plain
//! `&Value -> Result<Self>` conversion.
//!
//! **Fields match by name, not by shape position.** the upstream engine's derive compares
//! the struct's field order against the shape's pointer order and rejects a
//! mismatch; a Pylon [`Object`] is name-keyed, so order is irrelevant here.
//! The bug the upstream engine's positional check catches is still caught: a struct field
//! the query never selected is a [`DecodeErrorKind::MissingField`], not a
//! silently-wrong value.

use std::fmt;

use crate::value::Value;

/// A type a query result can be decoded into — implemented for the scalars
/// Pylon returns, for `Option`/`Vec` of those, and derivable for a row
/// struct or a schema enum with `#[derive(Queryable)]`.
pub trait Queryable: Sized {
    fn decode(value: &Value) -> Result<Self, DecodeError>;
}

/// What went wrong, and where in the result it was.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeError {
    /// The type `query::<R, _>` was asked for. Set by every derived
    /// `decode` as the error passes back out of it, so the *outermost* one
    /// wins — the caller's own row type, not whichever nested struct
    /// happened to fail.
    type_root: Option<String>,
    /// Field path from `type_root` down to the offending value, built up as
    /// the error propagates out — `transformer.latest_version.runner`.
    path: Vec<String>,
    kind: DecodeErrorKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DecodeErrorKind {
    /// The query's shape never selected this pointer. Usually a shape that
    /// forgot a field the struct declares, not a bad value.
    MissingField(String),
    WrongType {
        expected: &'static str,
        actual: &'static str,
    },
    /// An enum label the Rust enum has no variant for.
    UnknownVariant(String),
    /// Pylon returns every integer as an `i64`; this is the narrowing to a
    /// smaller Rust integer failing.
    OutOfRange { value: i64, target: &'static str },
    /// A `#[pylon(json)]` field or container failed to deserialize.
    Json(String),
    /// A value that parses in principle but not into this type — a
    /// malformed decimal string, a timestamp outside `chrono`'s range.
    Invalid(String),
}

impl DecodeError {
    fn new(kind: DecodeErrorKind) -> Self {
        Self {
            type_root: None,
            path: Vec::new(),
            kind,
        }
    }

    /// Records that this failure happened inside the field (or set element)
    /// named `segment`.
    fn under(mut self, segment: impl Into<String>) -> Self {
        self.path.insert(0, segment.into());
        self
    }

    /// Names the type being decoded. Overwrites any name a nested decode
    /// already set, because the field path is absolute and the intermediate
    /// type names in the middle of it are noise.
    fn rooted_at(mut self, type_name: &str) -> Self {
        self.type_root = Some(type_name.to_string());
        self
    }

    pub fn kind(&self) -> &DecodeErrorKind {
        &self.kind
    }

    fn wrong_type(expected: &'static str, actual: &Value) -> Self {
        Self::new(DecodeErrorKind::WrongType {
            expected,
            actual: type_name(actual),
        })
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut location: Vec<&str> = Vec::with_capacity(self.path.len() + 1);
        location.extend(self.type_root.as_deref());
        location.extend(self.path.iter().map(String::as_str));
        if location.is_empty() {
            write!(f, "cannot decode query result: {}", self.kind)
        } else {
            write!(f, "cannot decode {}: {}", location.join("."), self.kind)
        }
    }
}

impl std::error::Error for DecodeError {}

impl fmt::Display for DecodeErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField(name) => write!(f, "the query's shape has no '{name}'"),
            Self::WrongType { expected, actual } => write!(f, "expected {expected}, got {actual}"),
            Self::UnknownVariant(label) => write!(f, "'{label}' is not a known variant"),
            Self::OutOfRange { value, target } => write!(f, "{value} is out of range for {target}"),
            Self::Json(message) => write!(f, "{message}"),
            Self::Invalid(message) => write!(f, "{message}"),
        }
    }
}

/// The variant name as it appears in an error message.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "an empty set",
        Value::Bool(_) => "a bool",
        Value::Int64(_) => "an integer",
        Value::Float64(_) => "a float",
        Value::Str(_) => "a string",
        Value::Bytes(_) => "bytes",
        Value::Uuid(_) => "a uuid",
        Value::Decimal(_) => "a decimal",
        Value::Duration { .. } => "a duration",
        Value::Date(_) => "a date",
        Value::Time(_) => "a time",
        Value::Timestamp(_) => "a local datetime",
        Value::Timestamptz(_) => "a datetime",
        Value::Range(_) => "a range",
        Value::Array(_) => "a set",
        Value::Tuple(_) => "a tuple",
        Value::Object(_) => "an object",
        Value::Enum { .. } => "an enum value",
        Value::Group(_) => "a group",
        Value::VectorSearch { .. } => "a vector search result",
        Value::FtsSearch { .. } => "an FTS search result",
    }
}

/// Decoding `Value` into itself — lets `query::<Value, _>(...)` (the
/// untyped path every caller used before `Queryable` existed) go through
/// the same generic method as a derived row struct.
impl Queryable for Value {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        Ok(value.clone())
    }
}

/// An empty set decodes as `None`; anything else has to decode as `T`.
impl<T: Queryable> Queryable for Option<T> {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Null => Ok(None),
            other => T::decode(other).map(Some),
        }
    }
}

/// A multi pointer or any other set-valued result. A single (non-`Array`)
/// value decodes as a one-element `Vec` — a shape's cardinality is the
/// query's business, and rejecting it here would make `Vec<T>` unusable for
/// a pointer Pylon happens to return unwrapped.
impl<T: Queryable> Queryable for Vec<T> {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(index, item)| T::decode(item).map_err(|error| error.under(index.to_string())))
                .collect(),
            Value::Null => Ok(Vec::new()),
            single => T::decode(single).map(|decoded| vec![decoded]),
        }
    }
}

impl Queryable for bool {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Bool(b) => Ok(*b),
            other => Err(DecodeError::wrong_type("a bool", other)),
        }
    }
}

impl Queryable for String {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Str(s) => Ok(s.clone()),
            // An enum read into a `String` is what `<str>.status` produces
            // once the cast is applied, and a decimal's canonical form is a
            // string by definition — both are the value the caller asked
            // for, not a type confusion.
            Value::Enum { value, .. } => Ok(value.clone()),
            Value::Decimal(s) => Ok(s.clone()),
            other => Err(DecodeError::wrong_type("a string", other)),
        }
    }
}

impl Queryable for Vec<u8> {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Bytes(b) => Ok(b.clone()),
            other => Err(DecodeError::wrong_type("bytes", other)),
        }
    }
}

impl Queryable for uuid::Uuid {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Uuid(u) => Ok(*u),
            other => Err(DecodeError::wrong_type("a uuid", other)),
        }
    }
}

impl Queryable for i64 {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Int64(n) => Ok(*n),
            other => Err(DecodeError::wrong_type("an integer", other)),
        }
    }
}

/// Pylon decodes every width of integer into `Value::Int64`, so the
/// narrower Rust integers range-check rather than matching a variant of
/// their own.
macro_rules! queryable_narrow_int {
    ($($target:ty),* $(,)?) => {
        $(
            impl Queryable for $target {
                fn decode(value: &Value) -> Result<Self, DecodeError> {
                    match value {
                        Value::Int64(n) => <$target>::try_from(*n).map_err(|_| {
                            DecodeError::new(DecodeErrorKind::OutOfRange {
                                value: *n,
                                target: stringify!($target),
                            })
                        }),
                        other => Err(DecodeError::wrong_type("an integer", other)),
                    }
                }
            }
        )*
    };
}

queryable_narrow_int!(i16, i32, u16, u32, u64);

impl Queryable for f64 {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Float64(f) => Ok(*f),
            // An `int64` widens to a float without loss of the value the
            // caller cares about, and `select 1` in a float slot is a
            // routine shape.
            Value::Int64(n) => Ok(*n as f64),
            other => Err(DecodeError::wrong_type("a float", other)),
        }
    }
}

impl Queryable for f32 {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        f64::decode(value).map(|f| f as f32)
    }
}

impl Queryable for chrono::DateTime<chrono::Utc> {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Timestamptz(micros) => from_pg_micros(*micros).map(|naive| naive.and_utc()),
            other => Err(DecodeError::wrong_type("a datetime", other)),
        }
    }
}

impl Queryable for chrono::NaiveDateTime {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Timestamp(micros) => from_pg_micros(*micros),
            other => Err(DecodeError::wrong_type("a local datetime", other)),
        }
    }
}

impl Queryable for chrono::NaiveDate {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Date(days) => pg_epoch()
                .checked_add_signed(chrono::Duration::days(i64::from(*days)))
                .ok_or_else(|| {
                    DecodeError::new(DecodeErrorKind::Invalid(format!(
                        "date {days} days from 2000-01-01 is outside the supported range"
                    )))
                }),
            other => Err(DecodeError::wrong_type("a date", other)),
        }
    }
}

impl Queryable for chrono::NaiveTime {
    fn decode(value: &Value) -> Result<Self, DecodeError> {
        match value {
            Value::Time(micros) => chrono::NaiveTime::from_hms_opt(0, 0, 0)
                .and_then(|midnight| {
                    midnight
                        .overflowing_add_signed(chrono::Duration::microseconds(*micros))
                        .0
                        .into()
                })
                .ok_or_else(|| {
                    DecodeError::new(DecodeErrorKind::Invalid(format!(
                        "time {micros}\u{b5}s after midnight is not a valid time of day"
                    )))
                }),
            other => Err(DecodeError::wrong_type("a time", other)),
        }
    }
}

fn pg_epoch() -> chrono::NaiveDate {
    // 2000-01-01 is a valid date, so this cannot fail.
    chrono::NaiveDate::from_ymd_opt(2000, 1, 1).expect("2000-01-01 is a valid date")
}

fn from_pg_micros(micros: i64) -> Result<chrono::NaiveDateTime, DecodeError> {
    pg_epoch()
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| midnight.checked_add_signed(chrono::Duration::microseconds(micros)))
        .ok_or_else(|| {
            DecodeError::new(DecodeErrorKind::Invalid(format!(
                "timestamp {micros}\u{b5}s from 2000-01-01 is outside the supported range"
            )))
        })
}

/// Helpers `#[derive(Queryable)]` expands to. Public because the generated
/// code lives in the caller's crate, but not part of the API to program
/// against — the signatures here follow the macro's needs.
#[doc(hidden)]
pub mod derive {
    use super::{DecodeError, DecodeErrorKind, Queryable};
    use crate::value::{Object, Value};

    pub fn object<'v>(value: &'v Value, container: &'static str) -> Result<&'v Object, DecodeError> {
        match value {
            Value::Object(object) => Ok(object),
            other => Err(DecodeError::wrong_type("an object", other).rooted_at(container)),
        }
    }

    /// A field the shape omitted reports only the container — the kind
    /// already names the field, so repeating it in the path would read
    /// `Row.last_error: the query's shape has no 'last_error'`.
    fn missing(container: &'static str, name: &str) -> DecodeError {
        DecodeError::new(DecodeErrorKind::MissingField(name.to_string())).rooted_at(container)
    }

    pub fn field<T: Queryable>(object: &Object, container: &'static str, name: &str) -> Result<T, DecodeError> {
        let value = object.get(name).ok_or_else(|| missing(container, name))?;
        T::decode(value).map_err(|error| error.under(name).rooted_at(container))
    }

    pub fn json_field<T: serde::de::DeserializeOwned>(
        object: &Object,
        container: &'static str,
        name: &str,
    ) -> Result<T, DecodeError> {
        let value = object.get(name).ok_or_else(|| missing(container, name))?;
        from_json_value(value).map_err(|error| error.under(name).rooted_at(container))
    }

    pub fn from_json<T: serde::de::DeserializeOwned>(value: &Value, container: &'static str) -> Result<T, DecodeError> {
        from_json_value(value).map_err(|error| error.rooted_at(container))
    }

    /// Pylon decodes a `json` column natively (an object becomes
    /// [`Value::Object`], not a string of JSON text), so a
    /// `#[pylon(json)]` target is reached by rendering that back to JSON
    /// and letting serde read it. Rendering first rather than mapping
    /// `Value` onto `serde_json::Value` directly keeps one definition of
    /// how each variant looks as JSON — `crate::json`'s, which is also
    /// what `query_json` returns.
    fn from_json_value<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, DecodeError> {
        serde_json::from_str(&crate::json::to_json(value))
            .map_err(|error| DecodeError::new(DecodeErrorKind::Json(error.to_string())))
    }

    /// An enum label, whether the shape returned it as a real enum or cast
    /// it to `str` first (`<str>.status`).
    pub fn enum_label<'v>(value: &'v Value, container: &'static str) -> Result<&'v str, DecodeError> {
        match value {
            Value::Enum { value, .. } => Ok(value.as_str()),
            Value::Str(label) => Ok(label.as_str()),
            other => Err(DecodeError::wrong_type("an enum value", other).rooted_at(container)),
        }
    }

    pub fn unknown_variant(container: &'static str, label: &str) -> DecodeError {
        DecodeError::new(DecodeErrorKind::UnknownVariant(label.to_string())).rooted_at(container)
    }
}

/// Decodes every row of an untyped result into `R` — the shared tail of
/// `Client`/`Transaction`'s `query` methods, which run the query through
/// `exec` generically and only then know what they are decoding into.
pub(crate) fn decode_rows<R: Queryable>(values: Vec<Value>) -> crate::Result<Vec<R>> {
    values
        .iter()
        .map(|value| R::decode(value).map_err(crate::Error::from))
        .collect()
}

pub(crate) fn decode_optional_row<R: Queryable>(value: Option<Value>) -> crate::Result<Option<R>> {
    value.as_ref().map(R::decode).transpose().map_err(crate::Error::from)
}

pub(crate) fn decode_row<R: Queryable>(value: Value) -> crate::Result<R> {
    R::decode(&value).map_err(crate::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Object;
    use crate::{QueryArgs, named_args};

    // `crate_path = crate` is what lets the derive be exercised from inside
    // the crate that defines the trait — the generated code otherwise names
    // `::pylon_client`, which doesn't resolve here.
    #[derive(Debug, PartialEq, crate::Queryable)]
    #[pylon(crate_path = crate)]
    struct Row {
        id: uuid::Uuid,
        attempt: i32,
        last_error: Option<String>,
        created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Debug, PartialEq, crate::Queryable)]
    #[pylon(crate_path = crate)]
    enum WebhookEvent {
        #[pylon(rename = "contact.created")]
        ContactCreated,
        #[pylon(rename = "contact.updated")]
        ContactUpdated,
        Other,
    }

    #[derive(Debug, PartialEq, crate::Queryable)]
    #[pylon(crate_path = crate)]
    struct Nested {
        latest_version: Option<Inner>,
        #[pylon(rename = "type")]
        kind: WebhookEvent,
    }

    #[derive(Debug, PartialEq, crate::Queryable)]
    #[pylon(crate_path = crate)]
    struct Inner {
        runner: String,
    }

    fn object(fields: Vec<(&str, Value)>) -> Value {
        Value::Object(Object {
            type_name: Some("test::Row".to_string()),
            fields: fields.into_iter().map(|(n, v)| (n.to_string(), v)).collect(),
        })
    }

    #[test]
    fn decodes_a_row_struct() {
        let id = uuid::Uuid::from_u128(7);
        let row = Row::decode(&object(vec![
            ("id", Value::Uuid(id)),
            ("attempt", Value::Int64(3)),
            ("last_error", Value::Null),
            ("created_at", Value::Timestamptz(0)),
        ]))
        .unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.attempt, 3);
        assert_eq!(row.last_error, None);
        assert_eq!(row.created_at.to_rfc3339(), "2000-01-01T00:00:00+00:00");
    }

    /// Pylon objects are name-keyed, so the shape listing its pointers in a
    /// different order than the struct declares its fields is not an error —
    /// unlike the upstream engine's positional check.
    #[test]
    fn field_order_does_not_matter() {
        let id = uuid::Uuid::from_u128(1);
        let row = Row::decode(&object(vec![
            ("created_at", Value::Timestamptz(0)),
            ("last_error", Value::Str("boom".into())),
            ("attempt", Value::Int64(1)),
            ("id", Value::Uuid(id)),
        ]))
        .unwrap();
        assert_eq!(row.last_error.as_deref(), Some("boom"));
        assert_eq!(row.id, id);
    }

    /// The bug the upstream engine's positional check exists to catch: a struct field the
    /// query never selected has to be an error, not a default.
    #[test]
    fn a_field_the_shape_omitted_is_an_error() {
        let error = Row::decode(&object(vec![
            ("id", Value::Uuid(uuid::Uuid::nil())),
            ("attempt", Value::Int64(1)),
            ("created_at", Value::Timestamptz(0)),
        ]))
        .unwrap_err();
        assert_eq!(error.kind(), &DecodeErrorKind::MissingField("last_error".to_string()));
        assert_eq!(
            error.to_string(),
            "cannot decode Row: the query's shape has no 'last_error'"
        );
    }

    #[test]
    fn nested_failures_name_their_whole_path() {
        let error = Nested::decode(&object(vec![
            ("latest_version", object(vec![("runner", Value::Int64(4))])),
            ("type", Value::Str("contact.created".into())),
        ]))
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "cannot decode Nested.latest_version.runner: expected a string, got an integer"
        );
    }

    #[test]
    fn decodes_a_renamed_enum_from_either_a_real_enum_or_a_str_cast() {
        let from_enum = WebhookEvent::decode(&Value::Enum {
            type_name: "integration::WebhookEvent".to_string(),
            value: "contact.updated".to_string(),
        })
        .unwrap();
        assert_eq!(from_enum, WebhookEvent::ContactUpdated);
        // `<str>.event` — the cast is applied server-side, so the label
        // arrives as a plain string.
        let from_cast = WebhookEvent::decode(&Value::Str("contact.created".into())).unwrap();
        assert_eq!(from_cast, WebhookEvent::ContactCreated);
        // An un-renamed variant keeps its Rust name as the label.
        assert_eq!(
            WebhookEvent::decode(&Value::Str("Other".into())).unwrap(),
            WebhookEvent::Other
        );
    }

    #[test]
    fn an_unknown_enum_label_names_the_label_it_saw() {
        let error = WebhookEvent::decode(&Value::Str("contact.merged".into())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "cannot decode WebhookEvent: 'contact.merged' is not a known variant"
        );
    }

    /// An optional link that is itself empty, inside an optional outer link —
    /// `event-bridge`'s `TransformerRefRow` shape.
    #[test]
    fn decodes_doubly_optional_nesting() {
        let row = Nested::decode(&object(vec![
            ("latest_version", Value::Null),
            ("type", Value::Str("Other".into())),
        ]))
        .unwrap();
        assert_eq!(row.latest_version, None);
    }

    #[test]
    fn narrow_integers_range_check_instead_of_wrapping() {
        assert_eq!(i32::decode(&Value::Int64(-5)).unwrap(), -5);
        let error = i32::decode(&Value::Int64(i64::from(i32::MAX) + 1)).unwrap_err();
        assert_eq!(
            error.kind(),
            &DecodeErrorKind::OutOfRange {
                value: 2_147_483_648,
                target: "i32"
            }
        );
    }

    #[test]
    fn a_multi_pointer_decodes_into_a_vec() {
        let rows: Vec<Inner> = Vec::decode(&Value::Array(vec![
            object(vec![("runner", Value::Str("lambda".into()))]),
            object(vec![("runner", Value::Str("firecracker".into()))]),
        ]))
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].runner, "firecracker");
        // An empty multi pointer comes back as an empty set, not as an error.
        assert_eq!(Vec::<Inner>::decode(&Value::Null).unwrap(), vec![]);
    }

    #[test]
    fn a_json_field_deserializes_from_a_natively_decoded_document() {
        #[derive(Debug, PartialEq, crate::Queryable)]
        #[pylon(crate_path = crate)]
        struct WithJson {
            #[pylon(json)]
            payload: Option<serde_json::Value>,
        }
        // Pylon decodes `jsonb` natively, so the column arrives as an
        // Object/Array, never as a string of JSON text.
        let row = WithJson::decode(&object(vec![(
            "payload",
            Value::Object(Object {
                type_name: None,
                fields: vec![("email".to_string(), Value::Str("a@b.test".into()))],
            }),
        )]))
        .unwrap();
        assert_eq!(row.payload.unwrap()["email"], "a@b.test");
    }

    #[test]
    fn positional_arguments_bind_by_index() {
        let id = uuid::Uuid::from_u128(9);
        // `to_params` borrows the names out of the argument collection, so
        // the collection has to outlive the params — exactly as it does at a
        // real call site, where it is a temporary in the `query(...)` call.
        let args = (id, "urgent", 4i32);
        let params = args.to_params();
        assert_eq!(params[0].0, "0");
        assert_eq!(params[1], ("1", pylon_value::DecodedValue::Str("urgent".into())));
        assert_eq!(params[2], ("2", pylon_value::DecodedValue::I64(4)));
        assert!(().to_params().is_empty());
    }

    #[test]
    fn named_arguments_accept_mixed_types_including_absent_optionals() {
        let args = named_args! {
            "id" => uuid::Uuid::nil(),
            "width" => Option::<i32>::None,
            "labels" => vec!["a".to_string()],
        };
        let params: std::collections::HashMap<_, _> = args.to_params().into_iter().collect();
        assert_eq!(params["width"], pylon_value::DecodedValue::Null);
        assert_eq!(
            params["labels"],
            pylon_value::DecodedValue::Array(vec![pylon_value::DecodedValue::Str("a".into())])
        );
        assert_eq!(params["id"], pylon_value::DecodedValue::Uuid([0; 16]));
    }

    /// A datetime argument and a datetime result have to agree about the
    /// epoch, or every timestamp written back is 30 years out.
    #[test]
    fn datetimes_round_trip_through_the_pg_epoch() {
        let when = chrono::DateTime::parse_from_rfc3339("2026-09-24T12:34:56Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let bound = crate::QueryArg::to_decoded(&when);
        let pylon_value::DecodedValue::Timestamptz(micros) = bound else {
            panic!("a datetime must bind as a timestamptz, got {bound:?}");
        };
        assert_eq!(
            chrono::DateTime::<chrono::Utc>::decode(&Value::Timestamptz(micros)).unwrap(),
            when
        );
    }
}
