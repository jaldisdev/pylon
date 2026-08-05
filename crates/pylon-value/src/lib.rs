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

//! A purpose-built, self-describing value tree for decoded Postgres rows.
//!
//! Not `serde_json::Value` (would blur int64 vs float64 vs decimal vs uuid —
//! real distinctions PyQL's own type system preserves) and not raw
//! serialization-of-arbitrary-PyObject (Rust has no way to serialize an
//! arbitrary *registered* Python dataclass — those only exist Python-side).
//! Instead, this sits at the same layer `pylon.query.deserialize()`'s
//! `_decode()` consumes today: already decoded from Postgres wire format
//! (never raw bytes), but still generic/positional, not yet hydrated into a
//! user class.
//!
//! This is the shared decode target for both `pylon-cache` (stores it,
//! serialized via `rkyv`) and `pylon-pgcon` (the Postgres driver — decodes
//! wire bytes straight into this same representation) — the whole point
//! being one decode path regardless of whether a result came fresh from
//! Postgres or from the LMDB cache. Deliberately has no dependency on
//! `pylon-core`, pyo3, or any I/O crate: it's just the value shape.
//!
//! Serialized with `rkyv` (zero-copy) rather than `bincode` — `bincode` is
//! effectively unmaintained upstream (its 3.0.0 release is a deliberate
//! `compile_error!` protest, forcing anyone pinned loosely onto the
//! archived 2.x line).

use rkyv::{Archive, Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Archive, Serialize, Deserialize)]
#[rkyv(
    compare(PartialEq),
    derive(Debug),
    serialize_bounds(__S: rkyv::ser::Writer + rkyv::ser::Allocator),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
)]
pub enum DecodedValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    /// Raw 16-byte UUID, matching `QueryParam::Uuid`'s own convention in
    /// `pylon-core`. Stored as raw bytes rather than a `uuid::Uuid` field
    /// (every consuming crate already re-wraps this in its own richer type
    /// on the way out — e.g. `pylon-client`'s `Value::Uuid(uuid::Uuid)` —
    /// so there's no benefit to carrying that type this deep); `uuid` is
    /// still a dependency of this crate, purely so `From<uuid::Uuid>` below
    /// can convert into this variant without every caller writing
    /// `.into_bytes()` by hand.
    Uuid([u8; 16]),
    /// Arbitrary-precision decimal, stored as its canonical string form
    /// (matches how `_pg_decode_numeric` round-trips today) rather than a
    /// lossy f64 or a bespoke bignum encoding.
    Decimal(String),
    /// A PostgreSQL `interval` — backs both Pylon's `std::duration` (months
    /// always 0 by convention) and `cal::relative_duration` (months may be
    /// nonzero). Kept as the three raw wire components rather than folded
    /// into a single duration, since `months` (a calendar-relative unit —
    /// "1 month" isn't a fixed number of days) can't be losslessly combined
    /// with `days`/`microseconds` without a reference date.
    Interval { months: i32, days: i32, microseconds: i64 },
    /// PostgreSQL `date` — whole days since the PG epoch (2000-01-01),
    /// exactly as the wire encodes it. Backs `cal::local_date`.
    Date(i32),
    /// PostgreSQL `time` (no timezone) — microseconds since midnight,
    /// exactly as the wire encodes it. Backs `cal::local_time`.
    Time(i64),
    /// PostgreSQL `timestamp` (no timezone) — microseconds since the PG
    /// epoch (2000-01-01T00:00:00), exactly as the wire encodes it. Backs
    /// `cal::local_datetime`; decodes to a naive `datetime.datetime`.
    Timestamp(i64),
    /// PostgreSQL `timestamptz` — microseconds since the PG epoch
    /// (2000-01-01T00:00:00 UTC; PostgreSQL always normalizes `timestamptz`
    /// to UTC on the wire, regardless of session timezone), exactly as the
    /// wire encodes it. Backs `std::datetime`; decodes to a UTC-aware
    /// `datetime.datetime`. A distinct variant from `Timestamp` (not a
    /// shared representation with a tag) so decode/encode can't mix up
    /// naive vs. aware at the type level.
    Timestamptz(i64),
    // `omit_bounds` is required on self-referential fields: rkyv's derive
    // otherwise adds a naive `FieldType: Archive` bound per field, which
    // for a directly-recursive type like this overflows trait resolution
    // (`DecodedValue: Archive` requires `Vec<DecodedValue>: Archive` requires
    // `DecodedValue: Archive`, forever) — see rkyv's own docs on recursive
    // types.
    /// A genuine Postgres array (`text[]`, `int8[]`, ...) — reconstructed
    /// Python-side as a `list`, matching what asyncpg has always decoded a
    /// Postgres array into. Do not use this for a composite/record's
    /// positional fields; see `Composite`.
    Array(#[rkyv(omit_bounds)] Vec<DecodedValue>),
    /// A positional composite (`record` — a schema object's own field
    /// tuple, or a nested `ROW(...)`), reconstructed Python-side as a
    /// `tuple`, matching `asyncpg.Record`'s own behavior — critically,
    /// `isinstance(a_tuple, (dict, list))` is `False`, the same as a real
    /// `asyncpg.Record`, which `pylon.query._decode()`'s `"named_tuple"`
    /// case relies on to tell "this position holds a raw jsonb value"
    /// apart from "this position holds a composite that needs `value[pos]`
    /// indexing first." Using `Array` (→ `list`) here instead silently
    /// breaks that check — a real bug caught by comparing decoded output
    /// against the live asyncpg path on real queries.
    Composite(#[rkyv(omit_bounds)] Vec<DecodedValue>),
    /// Field name + value pairs, in shape order (not a map — field order is
    /// part of what `ShapeNode` positions describe, and duplicate names
    /// can't happen for a single object's own pointers).
    Object(#[rkyv(omit_bounds)] Vec<(String, DecodedValue)>),
    /// A PostgreSQL range value (`int8range`, `numrange`, `tsrange`,
    /// `tstzrange`, `daterange`, ...). `lower`/`upper` are `None` for an
    /// unbounded side; `empty == true` means the whole range is empty
    /// (`lower`/`upper` are meaningless in that case, not "both unbounded" —
    /// PostgreSQL's own binary encoding distinguishes the two). A
    /// `multirange<T>` decodes to a plain `Array` of these, not a separate
    /// variant — it's just an ordered collection of ranges.
    Range {
        #[rkyv(omit_bounds)]
        lower: Option<Box<DecodedValue>>,
        #[rkyv(omit_bounds)]
        upper: Option<Box<DecodedValue>>,
        inc_lower: bool,
        inc_upper: bool,
        empty: bool,
    },
}

// ── Native-type conversions ──────────────────────────────────────────────────
//
// Lets a caller write `"id".into()`/`some_uuid.into()` instead of spelling
// out `DecodedValue::Uuid(...)` at every query-parameter call site — mirrors
// `gel_protocol::value::Value`'s own `From<T>` impls (verified against
// `gel-protocol` 0.9.2's `value.rs`), which exist for exactly this reason:
// the wrapper enum itself isn't going away (a query parameter/result still
// has to carry its own runtime type tag), but constructing one shouldn't
// require spelling out the variant name by hand for the common cases.
//
// `i16`/`i32` and `f32` widen into this crate's single `I64`/`F64` variants
// rather than getting their own — `DecodedValue` has never distinguished
// integer/float width the way Postgres or Gel's own wire protocol does
// (every integer column decodes to `I64`, every float column to `F64`
// already, regardless of the underlying `int2`/`int4`/`int8` or
// `float4`/`float8` column type), so these conversions just meet that
// existing convention rather than introduce a new one.

impl From<String> for DecodedValue {
    fn from(value: String) -> Self {
        DecodedValue::Str(value)
    }
}

impl From<&str> for DecodedValue {
    fn from(value: &str) -> Self {
        DecodedValue::Str(value.to_string())
    }
}

impl From<bool> for DecodedValue {
    fn from(value: bool) -> Self {
        DecodedValue::Bool(value)
    }
}

impl From<i16> for DecodedValue {
    fn from(value: i16) -> Self {
        DecodedValue::I64(value.into())
    }
}

impl From<i32> for DecodedValue {
    fn from(value: i32) -> Self {
        DecodedValue::I64(value.into())
    }
}

impl From<i64> for DecodedValue {
    fn from(value: i64) -> Self {
        DecodedValue::I64(value)
    }
}

impl From<f32> for DecodedValue {
    fn from(value: f32) -> Self {
        DecodedValue::F64(value.into())
    }
}

impl From<f64> for DecodedValue {
    fn from(value: f64) -> Self {
        DecodedValue::F64(value)
    }
}

impl From<Vec<u8>> for DecodedValue {
    fn from(value: Vec<u8>) -> Self {
        DecodedValue::Bytes(value)
    }
}

impl From<uuid::Uuid> for DecodedValue {
    fn from(value: uuid::Uuid) -> Self {
        DecodedValue::Uuid(value.into_bytes())
    }
}

/// One cache entry: the cached rows plus the tags a write to any of which
/// must invalidate it — stored together so invalidation never needs a
/// second lookup to find out what a key was tagged with.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct CachedEntry {
    pub rows: Vec<DecodedValue>,
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rkyv::rancor::Error;

    #[test]
    fn round_trips_every_variant() {
        let value = DecodedValue::Object(vec![
            ("id".into(), DecodedValue::Uuid([1; 16])),
            ("name".into(), DecodedValue::Str("Alice".into())),
            ("age".into(), DecodedValue::I64(30)),
            ("score".into(), DecodedValue::F64(1.5)),
            ("active".into(), DecodedValue::Bool(true)),
            ("balance".into(), DecodedValue::Decimal("12.50".into())),
            ("tags".into(), DecodedValue::Array(vec![DecodedValue::Str("a".into()), DecodedValue::Null])),
            ("avatar".into(), DecodedValue::Bytes(vec![1, 2, 3])),
            ("point".into(), DecodedValue::Composite(vec![DecodedValue::F64(1.0), DecodedValue::F64(2.0)])),
            ("span".into(), DecodedValue::Interval { months: 1, days: 2, microseconds: 3_600_000_000 }),
            ("day".into(), DecodedValue::Date(9525)),
            ("clock".into(), DecodedValue::Time(3_600_000_000)),
            ("naive_ts".into(), DecodedValue::Timestamp(1_000_000_000)),
            ("aware_ts".into(), DecodedValue::Timestamptz(1_000_000_000)),
            ("span_range".into(), DecodedValue::Range {
                lower: Some(Box::new(DecodedValue::I64(1))),
                upper: Some(Box::new(DecodedValue::I64(10))),
                inc_lower: true,
                inc_upper: false,
                empty: false,
            }),
        ]);
        let bytes = rkyv::to_bytes::<Error>(&value).unwrap();
        // SAFETY: bytes were produced moments ago by `to_bytes` on this same
        // type, in this same process — not untrusted external input, so the
        // `bytecheck`-validated safe `access` API (which this crate opts out
        // of entirely; see the `default-features = false` note in Cargo.toml)
        // isn't needed here.
        let archived = unsafe { rkyv::access_unchecked::<ArchivedDecodedValue>(&bytes) };
        let decoded: DecodedValue = rkyv::deserialize::<DecodedValue, Error>(archived).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn round_trips_cached_entry() {
        let entry = CachedEntry {
            rows: vec![DecodedValue::I64(1), DecodedValue::I64(2)],
            tags: vec!["public.person".into()],
        };
        let bytes = rkyv::to_bytes::<Error>(&entry).unwrap();
        // SAFETY: see the comment in `round_trips_every_variant` above.
        let archived = unsafe { rkyv::access_unchecked::<ArchivedCachedEntry>(&bytes) };
        let decoded: CachedEntry = rkyv::deserialize::<CachedEntry, Error>(archived).unwrap();
        assert_eq!(decoded.rows, entry.rows);
        assert_eq!(decoded.tags, entry.tags);
    }

    #[test]
    fn from_native_string_types() {
        assert_eq!(DecodedValue::from("hello".to_string()), DecodedValue::Str("hello".into()));
        assert_eq!(DecodedValue::from("hello"), DecodedValue::Str("hello".into()));
    }

    #[test]
    fn from_native_bool() {
        assert_eq!(DecodedValue::from(true), DecodedValue::Bool(true));
    }

    #[test]
    fn from_native_integers_widen_into_i64() {
        assert_eq!(DecodedValue::from(1i16), DecodedValue::I64(1));
        assert_eq!(DecodedValue::from(2i32), DecodedValue::I64(2));
        assert_eq!(DecodedValue::from(3i64), DecodedValue::I64(3));
    }

    #[test]
    fn from_native_floats_widen_into_f64() {
        assert_eq!(DecodedValue::from(1.5f32), DecodedValue::F64(1.5));
        assert_eq!(DecodedValue::from(2.5f64), DecodedValue::F64(2.5));
    }

    #[test]
    fn from_native_bytes() {
        assert_eq!(DecodedValue::from(vec![1u8, 2, 3]), DecodedValue::Bytes(vec![1, 2, 3]));
    }

    #[test]
    fn from_native_uuid() {
        let u = uuid::Uuid::from_bytes([7; 16]);
        assert_eq!(DecodedValue::from(u), DecodedValue::Uuid([7; 16]));
    }

    #[test]
    fn into_conversion_works_at_a_query_param_style_call_site() {
        // The motivating case: `&[(&str, DecodedValue)]`-shaped params
        // accepting `.into()` instead of the explicit variant.
        let params: Vec<(&str, DecodedValue)> =
            vec![("name", "Ada".into()), ("age", 30i64.into()), ("active", true.into())];
        assert_eq!(params[0].1, DecodedValue::Str("Ada".into()));
        assert_eq!(params[1].1, DecodedValue::I64(30));
        assert_eq!(params[2].1, DecodedValue::Bool(true));
    }
}
