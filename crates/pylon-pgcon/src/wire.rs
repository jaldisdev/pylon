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
        OID_JSONB => decode_jsonb(data),
        OID_RECORD => decode_record(data, ext),
        OID_RECORD_ARRAY => decode_array(data, ext),
        OID_BOOL_ARRAY | OID_BYTEA_ARRAY | OID_INT2_ARRAY | OID_INT4_ARRAY | OID_INT8_ARRAY
        | OID_TEXT_ARRAY | OID_BPCHAR_ARRAY | OID_VARCHAR_ARRAY | OID_FLOAT4_ARRAY
        | OID_FLOAT8_ARRAY | OID_NUMERIC_ARRAY | OID_UUID_ARRAY | OID_JSONB_ARRAY => {
            decode_array(data, ext)
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
}
