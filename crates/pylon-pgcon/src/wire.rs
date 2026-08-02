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

//! Recursive decoder from PostgreSQL's binary wire format into
//! `pylon_value::CachedValue` — the shared decode target `pylon-cache` also
//! stores, so a cache hit and a fresh row decode into the exact same shape.
//!
//! Every PyQL query result is emitted by `pylon-core` as a single
//! `SELECT (...) AS result` — an anonymous composite (`record`, OID 2249).
//! `tokio-postgres` has no generic "decode any composite into a value tree"
//! API (its `FromSql` machinery targets known Rust types), so this module
//! walks PostgreSQL's own documented binary wire format directly, the same
//! way the outgoing Python implementation's `_pg_decode_record`/
//! `_pg_decode_value` (`pylon/client.py`) did — except comprehensive,
//! rather than split across asyncpg's built-in composite decoder plus a
//! handful of hand-registered codec overrides for the cases asyncpg
//! couldn't handle natively (jsonb, `record[]`, `vector`).
//!
//! Composite field layout (used recursively for `record`/`record[]`):
//! `i32 nfields`, then per field: `u32 type_oid`, `i32 field_len`
//! (`-1` = NULL), `field_len` bytes of that field's own wire encoding.
//! Array layout (used for every `T[]` OID below): `i32 ndim`, `i32
//! has_null_flag`, `u32 element_oid`, then *one* `(i32 dim_size, i32
//! lower_bound)` pair — Pylon's `pylon.Array[T]` is always 1-dimensional,
//! so multi-dimensional arrays are out of scope, same as the Python
//! implementation this replaces — then per element: `i32 len` (`-1` =
//! NULL) + `len` bytes.

use pylon_value::CachedValue;
use rust_decimal::Decimal;

pub use crate::error::Error;
pub type Result<T> = crate::Result<T>;

// Fixed, well-known OIDs (see `pg_type.h` / `SELECT oid, typname FROM
// pg_type`) — stable across every Postgres install, unlike extension types
// (`vector`, PostGIS geometry/geography) whose OIDs are assigned at
// `CREATE EXTENSION` time and must be discovered per-database at connect
// time (see `ExtensionOids`, threaded through by the caller once known —
// not yet wired to a live discovery query in this phase).
const OID_BOOL: u32 = 16;
const OID_BYTEA: u32 = 17;
const OID_INT8: u32 = 20;
const OID_INT2: u32 = 21;
const OID_INT4: u32 = 23;
const OID_TEXT: u32 = 25;
const OID_JSONB: u32 = 3802;
const OID_FLOAT4: u32 = 700;
const OID_FLOAT8: u32 = 701;
const OID_BPCHAR: u32 = 1042;
const OID_VARCHAR: u32 = 1043;
const OID_NUMERIC: u32 = 1700;
const OID_DATE: u32 = 1082;
const OID_TIME: u32 = 1083;
const OID_TIMESTAMP: u32 = 1114;
const OID_TIMESTAMPTZ: u32 = 1184;
const OID_INTERVAL: u32 = 1186;
const OID_UUID: u32 = 2950;
const OID_RECORD: u32 = 2249;
const OID_RECORD_ARRAY: u32 = 2287;

const OID_BOOL_ARRAY: u32 = 1000;
const OID_BYTEA_ARRAY: u32 = 1001;
const OID_INT2_ARRAY: u32 = 1005;
const OID_INT4_ARRAY: u32 = 1007;
const OID_TEXT_ARRAY: u32 = 1009;
const OID_BPCHAR_ARRAY: u32 = 1014;
const OID_VARCHAR_ARRAY: u32 = 1015;
const OID_INT8_ARRAY: u32 = 1016;
const OID_FLOAT4_ARRAY: u32 = 1021;
const OID_FLOAT8_ARRAY: u32 = 1022;
const OID_NUMERIC_ARRAY: u32 = 1231;
const OID_UUID_ARRAY: u32 = 2951;
const OID_JSONB_ARRAY: u32 = 3807;

// Native PostgreSQL range/multirange type OIDs, paired with the element
// type OID their bound values decode with (int4range's bounds are int4,
// etc.) — mirrors `range_ctor_for_pg_type`/`multirange_ctor_for_range_ctor`
// in `pylon-core`'s `ir/compiler.rs`, the other side of this same "which 5
// PG range families does Pylon support" decision.
const OID_INT4RANGE: u32 = 3904;
const OID_INT8RANGE: u32 = 3926;
const OID_NUMRANGE: u32 = 3906;
const OID_TSRANGE: u32 = 3908;
const OID_TSTZRANGE: u32 = 3910;
const OID_DATERANGE: u32 = 3912;
const OID_INT4MULTIRANGE: u32 = 4451;
const OID_INT8MULTIRANGE: u32 = 4536;
const OID_NUMMULTIRANGE: u32 = 4532;
const OID_TSMULTIRANGE: u32 = 4533;
const OID_TSTZMULTIRANGE: u32 = 4534;
const OID_DATEMULTIRANGE: u32 = 4535;

/// The element OID a range/multirange type's bound values decode with —
/// `None` for anything that isn't one of the 6 native range/multirange
/// families this module knows about.
fn range_element_oid(oid: u32) -> Option<u32> {
    match oid {
        OID_INT4RANGE | OID_INT4MULTIRANGE => Some(OID_INT4),
        OID_INT8RANGE | OID_INT8MULTIRANGE => Some(OID_INT8),
        OID_NUMRANGE | OID_NUMMULTIRANGE => Some(OID_NUMERIC),
        OID_TSRANGE | OID_TSMULTIRANGE => Some(OID_TIMESTAMP),
        OID_TSTZRANGE | OID_TSTZMULTIRANGE => Some(OID_TIMESTAMPTZ),
        OID_DATERANGE | OID_DATEMULTIRANGE => Some(OID_DATE),
        _ => None,
    }
}

/// Extension type OIDs, assigned per-database at `CREATE EXTENSION` time —
/// discovered once at connect time (mirroring `_setup_codecs`'s runtime
/// `pg_type` lookup for `vector` today) and threaded through decode calls.
/// Not yet populated by a live discovery query in this phase; defaults to
/// "no extension types known," under which those OIDs fall through to the
/// generic text-fallback case exactly like any other unrecognized OID.
#[derive(Debug, Clone, Default)]
pub struct ExtensionOids {
    pub vector: Option<u32>,
}

/// Decodes one field's raw buffer (already length-stripped, matching what
/// `postgres_types::FromSql::from_sql` receives) into a `CachedValue`,
/// given its Postgres type OID. NULL is handled by the caller (a `-1`
/// field length never reaches this function) — see `decode_record`/
/// `decode_array` for where that's checked.
pub fn decode_value(oid: u32, data: &[u8], ext: &ExtensionOids) -> Result<CachedValue> {
    if let Some(vector_oid) = ext.vector {
        if oid == vector_oid {
            return Ok(CachedValue::Array(decode_vector(data)?));
        }
    }
    match oid {
        OID_BOOL => Ok(CachedValue::Bool(data.first().copied().unwrap_or(0) != 0)),
        OID_INT2 => Ok(CachedValue::I64(i16::from_be_bytes(data.try_into()?) as i64)),
        OID_INT4 => Ok(CachedValue::I64(i32::from_be_bytes(data.try_into()?) as i64)),
        OID_INT8 => Ok(CachedValue::I64(i64::from_be_bytes(data.try_into()?))),
        OID_FLOAT4 => Ok(CachedValue::F64(f32::from_be_bytes(data.try_into()?) as f64)),
        OID_FLOAT8 => Ok(CachedValue::F64(f64::from_be_bytes(data.try_into()?))),
        OID_TEXT | OID_VARCHAR | OID_BPCHAR => Ok(CachedValue::Str(std::str::from_utf8(data)?.to_string())),
        OID_UUID => {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(data);
            Ok(CachedValue::Uuid(bytes))
        }
        OID_BYTEA => Ok(CachedValue::Bytes(data.to_vec())),
        OID_NUMERIC => decode_numeric(data),
        OID_INTERVAL => decode_interval(data),
        OID_DATE => Ok(CachedValue::Date(i32::from_be_bytes(data.try_into()?))),
        OID_TIME => Ok(CachedValue::Time(i64::from_be_bytes(data.try_into()?))),
        OID_TIMESTAMP => Ok(CachedValue::Timestamp(i64::from_be_bytes(data.try_into()?))),
        OID_TIMESTAMPTZ => Ok(CachedValue::Timestamptz(i64::from_be_bytes(data.try_into()?))),
        OID_JSONB => decode_jsonb(data),
        OID_RECORD => decode_record(data, ext),
        OID_RECORD_ARRAY => decode_array(data, ext),
        OID_BOOL_ARRAY | OID_BYTEA_ARRAY | OID_INT2_ARRAY | OID_INT4_ARRAY | OID_INT8_ARRAY
        | OID_TEXT_ARRAY | OID_BPCHAR_ARRAY | OID_VARCHAR_ARRAY | OID_FLOAT4_ARRAY
        | OID_FLOAT8_ARRAY | OID_NUMERIC_ARRAY | OID_UUID_ARRAY | OID_JSONB_ARRAY => {
            decode_array(data, ext)
        }
        OID_INT4RANGE | OID_INT8RANGE | OID_NUMRANGE | OID_TSRANGE | OID_TSTZRANGE | OID_DATERANGE => {
            decode_range(data, range_element_oid(oid).expect("range OID"), ext)
        }
        OID_INT4MULTIRANGE | OID_INT8MULTIRANGE | OID_NUMMULTIRANGE | OID_TSMULTIRANGE
        | OID_TSTZMULTIRANGE | OID_DATEMULTIRANGE => {
            decode_multirange(data, range_element_oid(oid).expect("multirange OID"), ext)
        }
        // Enums, domains, and other extension/text-compatible custom types
        // (schema-qualified enums are always emitted `::text`-cast by
        // pylon-core — see `sql/mod.rs::emit_scalar` — so their runtime
        // OID, unknown to us statically, never needs a dedicated case).
        _ => Ok(CachedValue::Str(std::str::from_utf8(data)?.to_string())),
    }
}

fn decode_numeric(data: &[u8]) -> Result<CachedValue> {
    use postgres_types::{FromSql, Type};
    let decimal = Decimal::from_sql(&Type::NUMERIC, data)?;
    Ok(CachedValue::Decimal(decimal.to_string()))
}

/// PostgreSQL's binary `interval` wire format: `i64 microseconds, i32 days,
/// i32 months`, in that order — see `interval_send` in Postgres's own
/// `timestamp.c`. Backs both `std::duration` and `cal::relative_duration`
/// (see `CachedValue::Interval`'s own doc comment for why `months` isn't
/// folded into `days`).
fn decode_interval(data: &[u8]) -> Result<CachedValue> {
    if data.len() != 16 {
        return Err(Error::message(format!(
            "malformed interval: expected 16 bytes, got {}", data.len()
        )));
    }
    let microseconds = i64::from_be_bytes(data[0..8].try_into()?);
    let days = i32::from_be_bytes(data[8..12].try_into()?);
    let months = i32::from_be_bytes(data[12..16].try_into()?);
    Ok(CachedValue::Interval { months, days, microseconds })
}

// PostgreSQL's range binary-format flag bits (`rangetypes.h`).
const RANGE_EMPTY: u8 = 0x01;
const RANGE_LB_INC: u8 = 0x02;
const RANGE_UB_INC: u8 = 0x04;
const RANGE_LB_INF: u8 = 0x08;
const RANGE_UB_INF: u8 = 0x10;

/// Binary range: `u8 flags`, then — only when not empty — a length-prefixed
/// lower bound (skipped if `RANGE_LB_INF`) and a length-prefixed upper bound
/// (skipped if `RANGE_UB_INF`), each bound decoded with `element_oid`'s own
/// decoder (see `range_element_oid` for which element type backs which
/// range OID).
fn decode_range(data: &[u8], element_oid: u32, ext: &ExtensionOids) -> Result<CachedValue> {
    let flags = data[0];
    let mut offset = 1usize;
    if flags & RANGE_EMPTY != 0 {
        return Ok(CachedValue::Range { lower: None, upper: None, inc_lower: false, inc_upper: false, empty: true });
    }
    let lower = if flags & RANGE_LB_INF != 0 {
        None
    } else {
        let len = i32::from_be_bytes(data[offset..offset + 4].try_into()?) as usize;
        offset += 4;
        let value = decode_value(element_oid, &data[offset..offset + len], ext)?;
        offset += len;
        Some(Box::new(value))
    };
    let upper = if flags & RANGE_UB_INF != 0 {
        None
    } else {
        let len = i32::from_be_bytes(data[offset..offset + 4].try_into()?) as usize;
        offset += 4;
        Some(Box::new(decode_value(element_oid, &data[offset..offset + len], ext)?))
    };
    Ok(CachedValue::Range {
        lower,
        upper,
        inc_lower: flags & RANGE_LB_INC != 0,
        inc_upper: flags & RANGE_UB_INC != 0,
        empty: false,
    })
}

/// Binary multirange: `i32 range_count`, then per range an `i32 len` +
/// `len` bytes of that range's own binary encoding (the same format
/// `decode_range` reads). Decodes to a plain `Array` of `Range` values —
/// see `CachedValue::Range`'s own doc comment for why there's no separate
/// multirange variant.
fn decode_multirange(data: &[u8], element_oid: u32, ext: &ExtensionOids) -> Result<CachedValue> {
    let mut offset = 0usize;
    let count = i32::from_be_bytes(data[offset..offset + 4].try_into()?) as usize;
    offset += 4;
    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        let len = i32::from_be_bytes(data[offset..offset + 4].try_into()?) as usize;
        offset += 4;
        ranges.push(decode_range(&data[offset..offset + len], element_oid, ext)?);
        offset += len;
    }
    Ok(CachedValue::Array(ranges))
}

/// Binary jsonb: a 1-byte format-version prefix (always `1` today) followed
/// by the UTF-8 JSON text itself. Parsed into a `CachedValue` tree (not
/// left as an opaque string) so nested jsonb-backed named tuples decode
/// the same way a composite field would.
fn decode_jsonb(data: &[u8]) -> Result<CachedValue> {
    let text = std::str::from_utf8(&data[1..])?;
    let value: serde_json::Value = serde_json::from_str(text)?;
    Ok(json_to_cached(value))
}

fn json_to_cached(value: serde_json::Value) -> CachedValue {
    match value {
        serde_json::Value::Null => CachedValue::Null,
        serde_json::Value::Bool(b) => CachedValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CachedValue::I64(i)
            } else {
                CachedValue::F64(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => CachedValue::Str(s),
        serde_json::Value::Array(items) => CachedValue::Array(items.into_iter().map(json_to_cached).collect()),
        serde_json::Value::Object(map) => {
            CachedValue::Object(map.into_iter().map(|(k, v)| (k, json_to_cached(v))).collect())
        }
    }
}

/// `pgvector`'s binary format: `u16 ndim`, `u16 reserved` (always 0), then
/// `ndim` big-endian `f32`s. Matches `_decode_vector_binary` exactly.
fn decode_vector(data: &[u8]) -> Result<Vec<CachedValue>> {
    let ndim = u16::from_be_bytes(data[0..2].try_into()?) as usize;
    let mut values = Vec::with_capacity(ndim);
    for i in 0..ndim {
        let start = 4 + i * 4;
        let f = f32::from_be_bytes(data[start..start + 4].try_into()?);
        values.push(CachedValue::F64(f as f64));
    }
    Ok(values)
}

/// Encodes `items` (each expected to be `CachedValue::F64`/`I64`) as
/// pgvector's binary format — the inverse of `decode_vector`.
fn encode_vector(items: &[CachedValue], out: &mut bytes::BytesMut) -> Result<()> {
    let ndim: u16 = items.len().try_into().map_err(|_| Error::message("vector has too many dimensions to encode"))?;
    out.put_u16(ndim);
    out.put_u16(0); // reserved
    for item in items {
        let f = match item {
            CachedValue::F64(f) => *f as f32,
            CachedValue::I64(i) => *i as f32,
            other => return Err(Error::message(format!("cannot encode {other:?} as a vector element"))),
        };
        out.put_f32(f);
    }
    Ok(())
}

/// Decodes a `record`-typed field: `i32 nfields`, then per field `u32
/// type_oid` + `i32 field_len` (`-1` = NULL) + `field_len` bytes.
fn decode_record(data: &[u8], ext: &ExtensionOids) -> Result<CachedValue> {
    let mut offset = 0usize;
    let nfields = i32::from_be_bytes(data[offset..offset + 4].try_into()?) as usize;
    offset += 4;
    let mut fields = Vec::with_capacity(nfields);
    for _ in 0..nfields {
        let type_oid = u32::from_be_bytes(data[offset..offset + 4].try_into()?);
        offset += 4;
        let field_len = i32::from_be_bytes(data[offset..offset + 4].try_into()?);
        offset += 4;
        if field_len == -1 {
            fields.push(CachedValue::Null);
        } else {
            let len = field_len as usize;
            fields.push(decode_value(type_oid, &data[offset..offset + len], ext)?);
            offset += len;
        }
    }
    Ok(CachedValue::Composite(fields))
}

/// Decodes any 1-dimensional array: `i32 ndim`, `i32 has_null_flag`, `u32
/// element_oid`, one `(i32 dim_size, i32 lower_bound)` pair, then per
/// element `i32 len` (`-1` = NULL) + `len` bytes. An empty array
/// (`ndim == 0`) has no dimension pair to read.
fn decode_array(data: &[u8], ext: &ExtensionOids) -> Result<CachedValue> {
    let mut offset = 0usize;
    let ndim = i32::from_be_bytes(data[offset..offset + 4].try_into()?);
    offset += 4;
    offset += 4; // has-null flag — not needed, NULL is signaled per-element via len == -1
    let element_oid = u32::from_be_bytes(data[offset..offset + 4].try_into()?);
    offset += 4;
    if ndim == 0 {
        return Ok(CachedValue::Array(vec![]));
    }
    let dim_size = i32::from_be_bytes(data[offset..offset + 4].try_into()?) as usize;
    offset += 4;
    offset += 4; // lower bound — Pylon arrays are always 1-based, not needed

    let mut items = Vec::with_capacity(dim_size);
    for _ in 0..dim_size {
        let elem_len = i32::from_be_bytes(data[offset..offset + 4].try_into()?);
        offset += 4;
        if elem_len == -1 {
            items.push(CachedValue::Null);
        } else {
            let len = elem_len as usize;
            items.push(decode_value(element_oid, &data[offset..offset + len], ext)?);
            offset += len;
        }
    }
    Ok(CachedValue::Array(items))
}

// ── Parameter encoding (the inverse direction: CachedValue -> wire bytes) ──
//
// Bound query parameters don't need pylon-core to supply explicit
// per-parameter Postgres types up front: `Client::prepare` already asks
// Postgres itself to analyze the SQL and report each `$1, $2, ...`'s
// expected `Type` back (`Statement::params()`) — exactly what asyncpg's
// own extended-query-protocol binding already relies on today, just
// surfaced explicitly here instead of hidden inside asyncpg's codec
// registry. So encoding is *type-directed*: given a `CachedValue` and the
// `Type` Postgres reported for that position, write the matching binary
// representation. See `BoundParam` (in `lib.rs`) for the `ToSql` glue that
// makes this pluggable into `tokio_postgres::Client::query`.

use bytes::BufMut;
use postgres_types::{IsNull, Kind, ToSql, Type};

/// Encodes `value` as `ty`'s binary wire format into `out`. `ty` comes from
/// `Statement::params()[i]` — Postgres's own analysis of the prepared SQL,
/// not a guess — so this only needs to pick the right byte width/shape for
/// whatever `CachedValue` variant is actually being sent, not infer the
/// target type itself.
pub fn encode_value(value: &CachedValue, ty: &Type, out: &mut bytes::BytesMut) -> Result<IsNull> {
    let CachedValue::Null = value else {
        return encode_non_null(value, ty, out);
    };
    Ok(IsNull::Yes)
}

fn encode_non_null(value: &CachedValue, ty: &Type, out: &mut bytes::BytesMut) -> Result<IsNull> {
    // A `<std::decimal>$pN` cast makes Postgres report that parameter's
    // type as `numeric` regardless of which `CachedValue` variant the JSON
    // request body produced (`I64`/`F64` for a JSON number, `Str` for a
    // JSON string — `json_to_cached_value` in pylon-server has no visibility
    // into the target PG type at parse time). Without this, the arms below
    // write raw int8/float8 bytes or raw UTF-8 text straight into a
    // numeric-typed slot, which Postgres's binary numeric decoder then reads
    // as a corrupt header — "invalid sign in external representation" for
    // I64/F64 (garbage sign field), "insufficient data left in message" for
    // Str (too few bytes for the header). Route every numeric-ish variant
    // through the same `rust_decimal` encoding the `Decimal` arm below uses.
    if *ty == Type::NUMERIC {
        let decimal: Decimal = match value {
            CachedValue::Decimal(s) => s.parse()?,
            CachedValue::Str(s) => s.parse()?,
            CachedValue::I64(i) => Decimal::from(*i),
            CachedValue::F64(f) => Decimal::try_from(*f).map_err(|e| Error::message(format!("invalid decimal value: {e}")))?,
            _ => return Err(Error::message("cannot bind this value as a numeric parameter")),
        };
        decimal.to_sql(&Type::NUMERIC, out)?;
        return Ok(IsNull::No);
    }
    match value {
        CachedValue::Null => unreachable!("caller already handled NULL"),
        CachedValue::Bool(b) => out.put_u8(*b as u8),
        CachedValue::I64(i) => {
            if *ty == Type::INT2 {
                out.put_i16(*i as i16);
            } else if *ty == Type::INT4 {
                out.put_i32(*i as i32);
            } else {
                out.put_i64(*i);
            }
        }
        CachedValue::F64(f) => {
            if *ty == Type::FLOAT4 {
                out.put_f32(*f as f32);
            } else {
                out.put_f64(*f);
            }
        }
        CachedValue::Str(s) => {
            if *ty == Type::UUID {
                // A JSON API request body necessarily carries a UUID query
                // parameter as plain text (there's no JSON "uuid" type), so
                // it arrives here as a `CachedValue::Str`, not `::Uuid` —
                // asyncpg's own `uuid` codec accepted a plain string the
                // same way. Without this, the raw UTF-8 text bytes get sent
                // for a binary-format `uuid` parameter, which Postgres
                // rejects with "incorrect binary data format".
                out.put_slice(&parse_uuid_str(s)?);
            } else if *ty == Type::JSONB {
                // A caller that already has serialized JSON text (e.g.
                // `schema_to_db_state_json`'s output, bound as `$1::jsonb`
                // in `migration apply`'s db_state snapshot update) arrives
                // here as `CachedValue::Str`, not `::Object` — treat it as
                // already-valid JSON text and just add jsonb's binary
                // version-byte prefix (see `decode_jsonb`/the `Object` arm
                // below), rather than writing raw text bytes with no
                // framing, which Postgres would reject.
                out.put_u8(1);
                out.put_slice(s.as_bytes());
            } else {
                out.put_slice(s.as_bytes());
            }
        }
        CachedValue::Bytes(b) => out.put_slice(b),
        CachedValue::Uuid(bytes) => out.put_slice(bytes),
        CachedValue::Decimal(s) => {
            let decimal: Decimal = s.parse()?;
            decimal.to_sql(&Type::NUMERIC, out)?;
        }
        CachedValue::Array(items) if ty.name() == "vector" => {
            // `$n::vector` casts the parameter directly (unlike
            // `vector::search`'s `$n::float8[]::vector`, where the *inner*
            // cast is what Postgres's prepare step reports as the param's
            // type) — Postgres reports `$n` itself as `vector`, a scalar
            // extension type, not `Kind::Array`. Bypass the generic array
            // path entirely and write pgvector's own binary format.
            encode_vector(items, out)?;
        }
        CachedValue::Array(items) => {
            let element_ty = match ty.kind() {
                Kind::Array(inner) => inner.clone(),
                // Not actually an array type per Postgres's own analysis —
                // fall back to TEXT so encoding still proceeds deterministically
                // rather than panicking; a real mismatch surfaces as a
                // Postgres-side type error on execute, same as today.
                _ => Type::TEXT,
            };
            encode_array(items, &element_ty, out)?;
        }
        CachedValue::Composite(_) => {
            // Composites only ever arise from *decoding* a query result
            // (see `decode_record`) — PyQL never binds a raw composite as
            // a query parameter, and encoding one correctly would need a
            // per-field Postgres type that isn't available here (only the
            // original compiled query's shape carries that). Erroring is
            // safer than guessing wrong field types.
            return Err(Error::message("cannot bind a composite value as a query parameter"));
        }
        CachedValue::Object(fields) => {
            let json = cached_object_to_json(fields);
            out.put_u8(1); // jsonb binary format version prefix
            out.put_slice(json.to_string().as_bytes());
        }
        CachedValue::Interval { months, days, microseconds } => {
            // Same field order as `decode_interval`'s read.
            out.put_i64(*microseconds);
            out.put_i32(*days);
            out.put_i32(*months);
        }
        CachedValue::Date(days) => out.put_i32(*days),
        CachedValue::Time(us) => out.put_i64(*us),
        CachedValue::Timestamp(us) => out.put_i64(*us),
        CachedValue::Timestamptz(us) => out.put_i64(*us),
        CachedValue::Range { lower, upper, inc_lower, inc_upper, empty } => {
            if *empty {
                out.put_u8(RANGE_EMPTY);
                return Ok(IsNull::No);
            }
            let element_ty = match ty.kind() {
                Kind::Range(inner) => inner.clone(),
                // Not actually a range type per Postgres's own analysis —
                // fall back to TEXT so encoding proceeds deterministically;
                // a real mismatch surfaces as a Postgres-side error, same
                // as the analogous fallback in the `Array` arm above.
                _ => Type::TEXT,
            };
            let mut flags = 0u8;
            if *inc_lower { flags |= RANGE_LB_INC; }
            if *inc_upper { flags |= RANGE_UB_INC; }
            if lower.is_none() { flags |= RANGE_LB_INF; }
            if upper.is_none() { flags |= RANGE_UB_INF; }
            out.put_u8(flags);
            for bound in [lower, upper].into_iter().flatten() {
                let mut buf = bytes::BytesMut::new();
                encode_value(bound, &element_ty, &mut buf)?;
                out.put_i32(buf.len() as i32);
                out.put_slice(&buf);
            }
        }
    }
    Ok(IsNull::No)
}

fn cached_object_to_json(fields: &[(String, CachedValue)]) -> serde_json::Value {
    serde_json::Value::Object(fields.iter().map(|(k, v)| (k.clone(), cached_to_json(v))).collect())
}

fn cached_to_json(value: &CachedValue) -> serde_json::Value {
    match value {
        CachedValue::Null => serde_json::Value::Null,
        CachedValue::Bool(b) => serde_json::Value::Bool(*b),
        CachedValue::I64(i) => serde_json::Value::Number((*i).into()),
        CachedValue::F64(f) => serde_json::Number::from_f64(*f).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null),
        CachedValue::Str(s) => serde_json::Value::String(s.clone()),
        CachedValue::Bytes(b) => serde_json::Value::String(hex::encode(b)),
        CachedValue::Uuid(bytes) => serde_json::Value::String(format_uuid(bytes)),
        CachedValue::Decimal(s) => serde_json::Value::String(s.clone()),
        CachedValue::Array(items) | CachedValue::Composite(items) => {
            serde_json::Value::Array(items.iter().map(cached_to_json).collect())
        }
        CachedValue::Object(fields) => cached_object_to_json(fields),
        // No natural JSON scalar for an interval; only reachable if an
        // Interval value ends up nested inside an Object being sent as a
        // jsonb parameter — represented as its raw components so it's at
        // least round-trippable, not silently dropped.
        CachedValue::Interval { months, days, microseconds } => serde_json::json!({
            "months": months, "days": days, "microseconds": microseconds,
        }),
        // Same rationale as Interval above — raw PG wire units, not a
        // formatted calendar string (calendar math is deliberately left to
        // Python's own `datetime` module at the `pgvalue.rs` boundary, not
        // reimplemented here).
        CachedValue::Date(days) => serde_json::json!({ "days_since_2000_01_01": days }),
        CachedValue::Time(us) => serde_json::json!({ "microseconds_since_midnight": us }),
        CachedValue::Timestamp(us) => serde_json::json!({ "microseconds_since_2000_01_01": us }),
        CachedValue::Timestamptz(us) => serde_json::json!({ "microseconds_since_2000_01_01_utc": us }),
        CachedValue::Range { lower, upper, inc_lower, inc_upper, empty } => serde_json::json!({
            "lower": lower.as_deref().map(cached_to_json),
            "upper": upper.as_deref().map(cached_to_json),
            "inc_lower": inc_lower,
            "inc_upper": inc_upper,
            "empty": empty,
        }),
    }
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex = hex::encode(bytes);
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// Parses a hyphenated UUID string into its 16 raw bytes — the inverse of
/// `format_uuid`. Tolerates the hyphens being anywhere/absent (just strips
/// every `-` and hex-decodes what's left) rather than validating the exact
/// `8-4-4-4-12` grouping, since the only thing that matters here is
/// recovering the right 16 bytes, not rejecting non-canonical formatting.
fn parse_uuid_str(s: &str) -> Result<[u8; 16]> {
    let hex_only: String = s.chars().filter(|c| *c != '-').collect();
    let bytes = hex::decode(&hex_only).map_err(|_| Error::message(format!("invalid UUID string: {s:?}")))?;
    bytes.try_into().map_err(|_: Vec<u8>| Error::message(format!("invalid UUID string: {s:?}")))
}

/// 1-dimensional Postgres array binary format (see the module doc comment
/// for the layout) — the encode-side mirror of `decode_array`.
fn encode_array(items: &[CachedValue], element_ty: &Type, out: &mut bytes::BytesMut) -> Result<()> {
    if items.is_empty() {
        out.put_i32(0); // ndim
        out.put_i32(0); // has-null flag
        out.put_u32(element_ty.oid());
        return Ok(());
    }
    let has_null = items.iter().any(|v| matches!(v, CachedValue::Null));
    out.put_i32(1); // ndim — Pylon arrays are always 1-D
    out.put_i32(has_null as i32);
    out.put_u32(element_ty.oid());
    out.put_i32(items.len() as i32); // dim size
    out.put_i32(1); // lower bound

    for item in items {
        if matches!(item, CachedValue::Null) {
            out.put_i32(-1);
            continue;
        }
        let start = out.len();
        out.put_i32(0); // placeholder length, patched below
        let is_null = encode_value(item, element_ty, out)?;
        let len = (out.len() - start - 4) as i32;
        let len = if matches!(is_null, IsNull::Yes) { -1 } else { len };
        out[start..start + 4].copy_from_slice(&len.to_be_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_ext() -> ExtensionOids {
        ExtensionOids::default()
    }

    #[test]
    fn decodes_bool() {
        assert_eq!(decode_value(OID_BOOL, &[1], &no_ext()).unwrap(), CachedValue::Bool(true));
        assert_eq!(decode_value(OID_BOOL, &[0], &no_ext()).unwrap(), CachedValue::Bool(false));
    }

    #[test]
    fn decodes_integers() {
        assert_eq!(decode_value(OID_INT2, &7i16.to_be_bytes(), &no_ext()).unwrap(), CachedValue::I64(7));
        assert_eq!(decode_value(OID_INT4, &(-42i32).to_be_bytes(), &no_ext()).unwrap(), CachedValue::I64(-42));
        assert_eq!(
            decode_value(OID_INT8, &9_223_372_036_854_775_807i64.to_be_bytes(), &no_ext()).unwrap(),
            CachedValue::I64(9_223_372_036_854_775_807)
        );
    }

    #[test]
    fn decodes_floats() {
        assert_eq!(decode_value(OID_FLOAT4, &1.5f32.to_be_bytes(), &no_ext()).unwrap(), CachedValue::F64(1.5));
        assert_eq!(decode_value(OID_FLOAT8, &2.25f64.to_be_bytes(), &no_ext()).unwrap(), CachedValue::F64(2.25));
    }

    #[test]
    fn decodes_text_varchar_bpchar() {
        for oid in [OID_TEXT, OID_VARCHAR, OID_BPCHAR] {
            assert_eq!(
                decode_value(oid, "hello".as_bytes(), &no_ext()).unwrap(),
                CachedValue::Str("hello".to_string())
            );
        }
    }

    #[test]
    fn decodes_unicode_text() {
        assert_eq!(
            decode_value(OID_TEXT, "héllo wörld 🎉".as_bytes(), &no_ext()).unwrap(),
            CachedValue::Str("héllo wörld 🎉".to_string())
        );
    }

    #[test]
    fn decodes_uuid() {
        let bytes: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        assert_eq!(decode_value(OID_UUID, &bytes, &no_ext()).unwrap(), CachedValue::Uuid(bytes));
    }

    #[test]
    fn decodes_bytea() {
        assert_eq!(
            decode_value(OID_BYTEA, &[1, 2, 3, 255], &no_ext()).unwrap(),
            CachedValue::Bytes(vec![1, 2, 3, 255])
        );
    }

    #[test]
    fn decodes_interval() {
        // Regression: interval has no dedicated binary decoder — it used to
        // fall through to the UTF-8-text fallback, which panics/errors on
        // interval's actual binary payload (microseconds/days/months, not text).
        let mut data = Vec::new();
        data.extend_from_slice(&3_600_000_000i64.to_be_bytes()); // 1 hour, in microseconds
        data.extend_from_slice(&2i32.to_be_bytes()); // 2 days
        data.extend_from_slice(&1i32.to_be_bytes()); // 1 month
        assert_eq!(
            decode_value(OID_INTERVAL, &data, &no_ext()).unwrap(),
            CachedValue::Interval { months: 1, days: 2, microseconds: 3_600_000_000 }
        );
    }

    #[test]
    fn encodes_interval() {
        let value = CachedValue::Interval { months: 1, days: 2, microseconds: 3_600_000_000 };
        let mut out = bytes::BytesMut::new();
        encode_value(&value, &postgres_types::Type::INTERVAL, &mut out).unwrap();
        assert_eq!(decode_value(OID_INTERVAL, &out, &no_ext()).unwrap(), value);
    }

    #[test]
    fn decodes_date_time_timestamp_timestamptz() {
        // Regression: these had no binary decoder either — date silently
        // returned garbage bytes (never even errored), the others panicked
        // on the same UTF-8-text-fallback assumption interval did.
        assert_eq!(decode_value(OID_DATE, &9525i32.to_be_bytes(), &no_ext()).unwrap(), CachedValue::Date(9525));
        assert_eq!(decode_value(OID_TIME, &3_600_000_000i64.to_be_bytes(), &no_ext()).unwrap(), CachedValue::Time(3_600_000_000));
        assert_eq!(
            decode_value(OID_TIMESTAMP, &1_000_000_000i64.to_be_bytes(), &no_ext()).unwrap(),
            CachedValue::Timestamp(1_000_000_000)
        );
        assert_eq!(
            decode_value(OID_TIMESTAMPTZ, &1_000_000_000i64.to_be_bytes(), &no_ext()).unwrap(),
            CachedValue::Timestamptz(1_000_000_000)
        );
    }

    #[test]
    fn encodes_date_time_timestamp_timestamptz() {
        for (value, ty) in [
            (CachedValue::Date(9525), postgres_types::Type::DATE),
            (CachedValue::Time(3_600_000_000), postgres_types::Type::TIME),
            (CachedValue::Timestamp(1_000_000_000), postgres_types::Type::TIMESTAMP),
            (CachedValue::Timestamptz(1_000_000_000), postgres_types::Type::TIMESTAMPTZ),
        ] {
            let mut out = bytes::BytesMut::new();
            encode_value(&value, &ty, &mut out).unwrap();
            let oid = match &value {
                CachedValue::Date(_) => OID_DATE,
                CachedValue::Time(_) => OID_TIME,
                CachedValue::Timestamp(_) => OID_TIMESTAMP,
                CachedValue::Timestamptz(_) => OID_TIMESTAMPTZ,
                _ => unreachable!(),
            };
            assert_eq!(decode_value(oid, &out, &no_ext()).unwrap(), value);
        }
    }

    #[test]
    fn decodes_a_bounded_int8range() {
        // flags = LB_INC | UB_INC-off = 0x02 (inclusive lower, exclusive upper)
        let mut data = vec![RANGE_LB_INC];
        data.extend_from_slice(&8i32.to_be_bytes());
        data.extend_from_slice(&1i64.to_be_bytes());
        data.extend_from_slice(&8i32.to_be_bytes());
        data.extend_from_slice(&10i64.to_be_bytes());
        assert_eq!(
            decode_value(OID_INT8RANGE, &data, &no_ext()).unwrap(),
            CachedValue::Range {
                lower: Some(Box::new(CachedValue::I64(1))),
                upper: Some(Box::new(CachedValue::I64(10))),
                inc_lower: true,
                inc_upper: false,
                empty: false,
            }
        );
    }

    #[test]
    fn decodes_an_empty_range() {
        assert_eq!(
            decode_value(OID_INT8RANGE, &[RANGE_EMPTY], &no_ext()).unwrap(),
            CachedValue::Range { lower: None, upper: None, inc_lower: false, inc_upper: false, empty: true }
        );
    }

    #[test]
    fn decodes_an_unbounded_range() {
        // Both bounds infinite: flags = LB_INF | UB_INF, no bound payloads follow.
        let data = [RANGE_LB_INF | RANGE_UB_INF];
        assert_eq!(
            decode_value(OID_INT8RANGE, &data, &no_ext()).unwrap(),
            CachedValue::Range { lower: None, upper: None, inc_lower: false, inc_upper: false, empty: false }
        );
    }

    #[test]
    fn encodes_and_round_trips_an_int8range() {
        let value = CachedValue::Range {
            lower: Some(Box::new(CachedValue::I64(1))),
            upper: Some(Box::new(CachedValue::I64(10))),
            inc_lower: true,
            inc_upper: false,
            empty: false,
        };
        let mut out = bytes::BytesMut::new();
        encode_value(&value, &postgres_types::Type::INT8_RANGE, &mut out).unwrap();
        assert_eq!(decode_value(OID_INT8RANGE, &out, &no_ext()).unwrap(), value);
    }

    #[test]
    fn decodes_a_multirange_of_int8ranges() {
        let mut range1 = vec![RANGE_LB_INC];
        range1.extend_from_slice(&8i32.to_be_bytes());
        range1.extend_from_slice(&1i64.to_be_bytes());
        range1.extend_from_slice(&8i32.to_be_bytes());
        range1.extend_from_slice(&3i64.to_be_bytes());

        let mut range2 = vec![RANGE_LB_INC];
        range2.extend_from_slice(&8i32.to_be_bytes());
        range2.extend_from_slice(&5i64.to_be_bytes());
        range2.extend_from_slice(&8i32.to_be_bytes());
        range2.extend_from_slice(&7i64.to_be_bytes());

        let mut data = 2i32.to_be_bytes().to_vec();
        data.extend_from_slice(&(range1.len() as i32).to_be_bytes());
        data.extend_from_slice(&range1);
        data.extend_from_slice(&(range2.len() as i32).to_be_bytes());
        data.extend_from_slice(&range2);

        let decoded = decode_value(OID_INT8MULTIRANGE, &data, &no_ext()).unwrap();
        assert_eq!(
            decoded,
            CachedValue::Array(vec![
                CachedValue::Range {
                    lower: Some(Box::new(CachedValue::I64(1))),
                    upper: Some(Box::new(CachedValue::I64(3))),
                    inc_lower: true, inc_upper: false, empty: false,
                },
                CachedValue::Range {
                    lower: Some(Box::new(CachedValue::I64(5))),
                    upper: Some(Box::new(CachedValue::I64(7))),
                    inc_lower: true, inc_upper: false, empty: false,
                },
            ])
        );
    }

    #[test]
    fn decodes_unrecognized_oid_as_text_fallback() {
        // Enums/domains — always ::text-cast by pylon-core's SQL emission,
        // so their real (unknown-to-us) OID never actually reaches here in
        // practice, but the fallback must still behave like plain text.
        assert_eq!(
            decode_value(999_999, "Active".as_bytes(), &no_ext()).unwrap(),
            CachedValue::Str("Active".to_string())
        );
    }

    /// Builds the Postgres binary `numeric` wire format by hand: `u16
    /// ndigits`, `i16 weight`, `u16 sign`, `i16 dscale`, then `ndigits`
    /// base-10000 digit groups (each a `u16`, matching `NBASE = 10000`).
    fn encode_numeric(sign: u16, weight: i16, dscale: i16, digits: &[u16]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(digits.len() as u16).to_be_bytes());
        buf.extend_from_slice(&weight.to_be_bytes());
        buf.extend_from_slice(&sign.to_be_bytes());
        buf.extend_from_slice(&dscale.to_be_bytes());
        for d in digits {
            buf.extend_from_slice(&d.to_be_bytes());
        }
        buf
    }

    #[test]
    fn decodes_numeric_integer() {
        // 12345 = digit groups [1, 2345] at weight 1 (10000^1 * 1 + 10000^0 * 2345)
        let data = encode_numeric(0x0000, 1, 0, &[1, 2345]);
        assert_eq!(decode_value(OID_NUMERIC, &data, &no_ext()).unwrap(), CachedValue::Decimal("12345".to_string()));
    }

    #[test]
    fn decodes_numeric_with_fraction() {
        // 12.50, dscale=2: digit groups [12, 5000] at weight 0
        let data = encode_numeric(0x0000, 0, 2, &[12, 5000]);
        assert_eq!(decode_value(OID_NUMERIC, &data, &no_ext()).unwrap(), CachedValue::Decimal("12.50".to_string()));
    }

    #[test]
    fn decodes_negative_numeric() {
        let data = encode_numeric(0x4000, 0, 2, &[12, 5000]);
        assert_eq!(decode_value(OID_NUMERIC, &data, &no_ext()).unwrap(), CachedValue::Decimal("-12.50".to_string()));
    }

    #[test]
    fn decodes_jsonb_object() {
        let mut data = vec![1u8]; // version prefix
        data.extend_from_slice(br#"{"a":1,"b":"two","c":[1,2,3],"d":null}"#);
        let decoded = decode_value(OID_JSONB, &data, &no_ext()).unwrap();
        assert_eq!(
            decoded,
            CachedValue::Object(vec![
                ("a".into(), CachedValue::I64(1)),
                ("b".into(), CachedValue::Str("two".into())),
                ("c".into(), CachedValue::Array(vec![CachedValue::I64(1), CachedValue::I64(2), CachedValue::I64(3)])),
                ("d".into(), CachedValue::Null),
            ])
        );
    }

    #[test]
    fn decodes_jsonb_scalar_and_array() {
        let mut data = vec![1u8];
        data.extend_from_slice(b"42");
        assert_eq!(decode_value(OID_JSONB, &data, &no_ext()).unwrap(), CachedValue::I64(42));

        let mut data2 = vec![1u8];
        data2.extend_from_slice(b"[1.5, 2.5]");
        assert_eq!(
            decode_value(OID_JSONB, &data2, &no_ext()).unwrap(),
            CachedValue::Array(vec![CachedValue::F64(1.5), CachedValue::F64(2.5)])
        );
    }

    /// Builds a `record`-field's binary payload by hand: `i32 nfields`,
    /// then per field `u32 type_oid` + `i32 field_len` (`-1` = NULL) +
    /// bytes — mirroring exactly what `decode_record` reads.
    fn encode_record(fields: &[(u32, Option<&[u8]>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(fields.len() as i32).to_be_bytes());
        for (oid, data) in fields {
            buf.extend_from_slice(&oid.to_be_bytes());
            match data {
                None => buf.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    buf.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    buf.extend_from_slice(bytes);
                }
            }
        }
        buf
    }

    #[test]
    fn decodes_flat_record() {
        let data = encode_record(&[
            (OID_INT8, Some(&42i64.to_be_bytes())),
            (OID_TEXT, Some(b"alice")),
            (OID_BOOL, None),
        ]);
        let decoded = decode_value(OID_RECORD, &data, &no_ext()).unwrap();
        assert_eq!(
            decoded,
            CachedValue::Composite(vec![CachedValue::I64(42), CachedValue::Str("alice".into()), CachedValue::Null])
        );
    }

    #[test]
    fn decodes_nested_record() {
        let inner = encode_record(&[(OID_INT8, Some(&1i64.to_be_bytes()))]);
        let outer = encode_record(&[(OID_RECORD, Some(&inner)), (OID_TEXT, Some(b"outer"))]);
        let decoded = decode_value(OID_RECORD, &outer, &no_ext()).unwrap();
        assert_eq!(
            decoded,
            CachedValue::Composite(vec![
                CachedValue::Composite(vec![CachedValue::I64(1)]),
                CachedValue::Str("outer".into()),
            ])
        );
    }

    /// Builds a Postgres array's binary payload by hand: `i32 ndim`, `i32
    /// has_null`, `u32 element_oid`, `(i32 dim, i32 lbound)`, then per
    /// element `i32 len` (`-1` = NULL) + bytes.
    fn encode_array(element_oid: u32, elements: &[Option<&[u8]>]) -> Vec<u8> {
        if elements.is_empty() {
            let mut buf = Vec::new();
            buf.extend_from_slice(&0i32.to_be_bytes());
            buf.extend_from_slice(&0i32.to_be_bytes());
            buf.extend_from_slice(&element_oid.to_be_bytes());
            return buf;
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(&1i32.to_be_bytes());
        buf.extend_from_slice(&0i32.to_be_bytes());
        buf.extend_from_slice(&element_oid.to_be_bytes());
        buf.extend_from_slice(&(elements.len() as i32).to_be_bytes());
        buf.extend_from_slice(&1i32.to_be_bytes());
        for data in elements {
            match data {
                None => buf.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    buf.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    buf.extend_from_slice(bytes);
                }
            }
        }
        buf
    }

    #[test]
    fn decodes_array_of_scalars() {
        let data = encode_array(OID_TEXT, &[Some(b"a"), Some(b"b"), None]);
        let decoded = decode_value(OID_TEXT_ARRAY, &data, &no_ext()).unwrap();
        assert_eq!(
            decoded,
            CachedValue::Array(vec![CachedValue::Str("a".into()), CachedValue::Str("b".into()), CachedValue::Null])
        );
    }

    #[test]
    fn decodes_empty_array() {
        let data = encode_array(OID_TEXT, &[]);
        assert_eq!(decode_value(OID_TEXT_ARRAY, &data, &no_ext()).unwrap(), CachedValue::Array(vec![]));
    }

    #[test]
    fn decodes_array_of_records() {
        let rec1 = encode_record(&[(OID_INT8, Some(&1i64.to_be_bytes()))]);
        let rec2 = encode_record(&[(OID_INT8, Some(&2i64.to_be_bytes()))]);
        let data = encode_array(OID_RECORD, &[Some(&rec1), Some(&rec2)]);
        let decoded = decode_value(OID_RECORD_ARRAY, &data, &no_ext()).unwrap();
        assert_eq!(
            decoded,
            CachedValue::Array(vec![
                CachedValue::Composite(vec![CachedValue::I64(1)]),
                CachedValue::Composite(vec![CachedValue::I64(2)]),
            ])
        );
    }

    #[test]
    fn decodes_vector_when_extension_oid_known() {
        let mut data = 2u16.to_be_bytes().to_vec(); // ndim = 2
        data.extend_from_slice(&0u16.to_be_bytes()); // reserved
        data.extend_from_slice(&1.5f32.to_be_bytes());
        data.extend_from_slice(&2.5f32.to_be_bytes());

        let ext = ExtensionOids { vector: Some(50_000) };
        let decoded = decode_value(50_000, &data, &ext).unwrap();
        assert_eq!(decoded, CachedValue::Array(vec![CachedValue::F64(1.5), CachedValue::F64(2.5)]));
    }

    #[test]
    fn unknown_oid_without_vector_extension_falls_back_to_text() {
        // Same OID as the vector test above, but with no extension OID
        // configured — must not be misinterpreted as vector binary data.
        assert_eq!(
            decode_value(50_000, "some-domain-value".as_bytes(), &no_ext()).unwrap(),
            CachedValue::Str("some-domain-value".to_string())
        );
    }

    #[test]
    fn encodes_a_str_value_as_uuid_binary_when_the_target_type_is_uuid() {
        // Regression test: a JSON API request body carries a UUID query
        // parameter as plain text (there's no JSON "uuid" type), so it
        // arrives as `CachedValue::Str` — binding it directly against a
        // `uuid`-typed parameter must produce the 16-byte binary form, not
        // the raw 36-character text bytes (which Postgres rejects with
        // "incorrect binary data format").
        let value = CachedValue::Str("11111111-2222-3333-4444-555555555555".to_string());
        let mut out = bytes::BytesMut::new();
        encode_value(&value, &postgres_types::Type::UUID, &mut out).unwrap();
        assert_eq!(
            out.as_ref(),
            &[0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x33, 0x33, 0x44, 0x44, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55]
        );
    }

    #[test]
    fn a_str_value_still_encodes_as_plain_text_for_a_text_target() {
        let value = CachedValue::Str("11111111-2222-3333-4444-555555555555".to_string());
        let mut out = bytes::BytesMut::new();
        encode_value(&value, &postgres_types::Type::TEXT, &mut out).unwrap();
        assert_eq!(out.as_ref(), "11111111-2222-3333-4444-555555555555".as_bytes());
    }

    #[test]
    fn rejects_a_malformed_uuid_string_instead_of_sending_garbage_bytes() {
        let value = CachedValue::Str("not-a-uuid".to_string());
        let mut out = bytes::BytesMut::new();
        assert!(encode_value(&value, &postgres_types::Type::UUID, &mut out).is_err());
    }

    fn vector_type() -> Type {
        // `vector` is a pgvector extension type, not a `postgres_types`
        // builtin — construct it the way `Statement::params()` would
        // report it back (any OID works here; encoding only inspects the
        // name via `ty.name()`, matching `$n::vector`'s cast-reported type).
        Type::new("vector".to_string(), 50_000, postgres_types::Kind::Simple, "public".to_string())
    }

    #[test]
    fn encodes_an_array_value_as_pgvector_binary_when_the_target_type_is_vector() {
        // Regression test: `$n::vector` reports the parameter's type as
        // the scalar `vector` type itself (unlike `$n::float8[]::vector`,
        // where the *inner* cast makes Postgres report `float8[]`) — an
        // `Array` value bound against it must produce pgvector's own
        // binary format (`u16 ndim`, `u16 reserved`, then big-endian
        // `f32`s), not the generic Postgres array wire format.
        let value = CachedValue::Array(vec![CachedValue::F64(1.5), CachedValue::F64(-2.25), CachedValue::F64(0.0)]);
        let mut out = bytes::BytesMut::new();
        encode_value(&value, &vector_type(), &mut out).unwrap();
        let mut expected = vec![0u8, 3, 0, 0];
        expected.extend_from_slice(&1.5f32.to_be_bytes());
        expected.extend_from_slice(&(-2.25f32).to_be_bytes());
        expected.extend_from_slice(&0.0f32.to_be_bytes());
        assert_eq!(out.as_ref(), expected.as_slice());
    }

    #[test]
    fn a_vector_encoded_value_round_trips_through_decode_vector() {
        let value = CachedValue::Array(vec![CachedValue::F64(1.0), CachedValue::F64(2.0), CachedValue::F64(3.0)]);
        let mut out = bytes::BytesMut::new();
        encode_value(&value, &vector_type(), &mut out).unwrap();
        let decoded = decode_vector(out.as_ref()).unwrap();
        assert_eq!(decoded, vec![CachedValue::F64(1.0), CachedValue::F64(2.0), CachedValue::F64(3.0)]);
    }

    #[test]
    fn an_array_value_still_encodes_as_a_plain_postgres_array_for_a_non_vector_target() {
        let value = CachedValue::Array(vec![CachedValue::F64(1.0), CachedValue::F64(2.0)]);
        let mut out = bytes::BytesMut::new();
        encode_value(&value, &postgres_types::Type::FLOAT8_ARRAY, &mut out).unwrap();
        // Generic array format starts with ndim=1 (i32), not pgvector's
        // ndim=2 (u16) — first four bytes distinguish the two encodings.
        assert_eq!(&out.as_ref()[0..4], &1i32.to_be_bytes());
    }

    #[test]
    fn encodes_a_str_value_as_jsonb_binary_when_the_target_type_is_jsonb() {
        // Regression test for the same class of bug as the UUID case above:
        // a caller with already-serialized JSON text (e.g.
        // `schema_to_db_state_json`'s output) arrives as `CachedValue::Str`,
        // not `::Object` — binding it against a `jsonb` parameter must add
        // the binary version-byte prefix, not send raw unframed text.
        let value = CachedValue::Str(r#"{"a":1}"#.to_string());
        let mut out = bytes::BytesMut::new();
        encode_value(&value, &postgres_types::Type::JSONB, &mut out).unwrap();
        assert_eq!(out.as_ref(), [&[1u8][..], br#"{"a":1}"#].concat());
        // And decodes back correctly through the normal jsonb decode path.
        assert_eq!(decode_value(OID_JSONB, &out, &no_ext()).unwrap(), CachedValue::Object(vec![("a".into(), CachedValue::I64(1))]));
    }
}
