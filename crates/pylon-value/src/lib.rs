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
pub enum CachedValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    /// Raw 16-byte UUID, matching `QueryParam::Uuid`'s own convention in
    /// `pylon-core` — avoids pulling in the `uuid` crate for just this.
    Uuid([u8; 16]),
    /// Arbitrary-precision decimal, stored as its canonical string form
    /// (matches how `_pg_decode_numeric` round-trips today) rather than a
    /// lossy f64 or a bespoke bignum encoding.
    Decimal(String),
    // `omit_bounds` is required on self-referential fields: rkyv's derive
    // otherwise adds a naive `FieldType: Archive` bound per field, which
    // for a directly-recursive type like this overflows trait resolution
    // (`CachedValue: Archive` requires `Vec<CachedValue>: Archive` requires
    // `CachedValue: Archive`, forever) — see rkyv's own docs on recursive
    // types.
    Array(#[rkyv(omit_bounds)] Vec<CachedValue>),
    /// Field name + value pairs, in shape order (not a map — field order is
    /// part of what `ShapeNode` positions describe, and duplicate names
    /// can't happen for a single object's own pointers).
    Object(#[rkyv(omit_bounds)] Vec<(String, CachedValue)>),
}

/// One cache entry: the cached rows plus the tags a write to any of which
/// must invalidate it — stored together so invalidation never needs a
/// second lookup to find out what a key was tagged with.
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[rkyv(derive(Debug))]
pub struct CachedEntry {
    pub rows: Vec<CachedValue>,
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rkyv::rancor::Error;

    #[test]
    fn round_trips_every_variant() {
        let value = CachedValue::Object(vec![
            ("id".into(), CachedValue::Uuid([1; 16])),
            ("name".into(), CachedValue::Str("Alice".into())),
            ("age".into(), CachedValue::I64(30)),
            ("score".into(), CachedValue::F64(1.5)),
            ("active".into(), CachedValue::Bool(true)),
            ("balance".into(), CachedValue::Decimal("12.50".into())),
            ("tags".into(), CachedValue::Array(vec![CachedValue::Str("a".into()), CachedValue::Null])),
            ("avatar".into(), CachedValue::Bytes(vec![1, 2, 3])),
        ]);
        let bytes = rkyv::to_bytes::<Error>(&value).unwrap();
        // SAFETY: bytes were produced moments ago by `to_bytes` on this same
        // type, in this same process — not untrusted external input, so the
        // `bytecheck`-validated safe `access` API (which this crate opts out
        // of entirely; see the `default-features = false` note in Cargo.toml)
        // isn't needed here.
        let archived = unsafe { rkyv::access_unchecked::<ArchivedCachedValue>(&bytes) };
        let decoded: CachedValue = rkyv::deserialize::<CachedValue, Error>(archived).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn round_trips_cached_entry() {
        let entry = CachedEntry {
            rows: vec![CachedValue::I64(1), CachedValue::I64(2)],
            tags: vec!["public.person".into()],
        };
        let bytes = rkyv::to_bytes::<Error>(&entry).unwrap();
        // SAFETY: see the comment in `round_trips_every_variant` above.
        let archived = unsafe { rkyv::access_unchecked::<ArchivedCachedEntry>(&bytes) };
        let decoded: CachedEntry = rkyv::deserialize::<CachedEntry, Error>(archived).unwrap();
        assert_eq!(decoded.rows, entry.rows);
        assert_eq!(decoded.tags, entry.tags);
    }
}
