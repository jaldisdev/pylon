//! A native Rust query client for Pylon — lets a Rust project run PyQL
//! queries against a Pylon-managed Postgres database directly, without
//! going through `pylon-py`/pyo3. Query results decode into a generic,
//! dynamically-typed [`Value`]/[`Object`] rather than per-schema-type
//! structs.
//!
//! Compilation reuses `pylon_core::query::compile` as-is; connection
//! execution reuses `pylon_pgcon::PgPool`/`PgTransaction` as-is. The one
//! genuinely new piece is `decode`, which walks a compiled query's
//! `ShapeNode` alongside its decoded `CachedValue` row — the Rust
//! counterpart of `pylon/query.py`'s `_decode` (which only ever ran
//! Python-side, operating on already-Pythonized values).

mod client;
mod decode;
mod error;
mod exec;
mod schema;
mod transaction;
mod value;

pub use client::{Builder, Client, TxFuture};
pub use error::{Error, Result};
pub use pylon_value::CachedValue;
pub use transaction::{Isolation, Transaction};
pub use value::{Group, Object, Range, Value};
