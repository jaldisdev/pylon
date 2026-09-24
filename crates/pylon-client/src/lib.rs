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

//! A native Rust query client for Pylon — lets a Rust project run PyQL
//! queries against a Pylon-managed Postgres database directly, without
//! going through `pylon-py`/pyo3. Query results decode either into a
//! generic, dynamically-typed [`Value`]/[`Object`] or into a caller's own
//! row struct via [`Queryable`] and `#[derive(Queryable)]` — there is no
//! generated per-schema-type code either way.
//!
//! Compilation reuses `pylon_core::query::compile` as-is; connection
//! execution reuses `pylon_pgcon::PgPool`/`PgTransaction` as-is. The one
//! genuinely new piece is `decode`, which walks a compiled query's
//! `ShapeNode` alongside its decoded `DecodedValue` row — the Rust
//! counterpart of `pylon/query.py`'s `_decode` (which only ever ran
//! Python-side, operating on already-Pythonized values).

mod cache;
mod client;
mod decode;
mod error;
mod exec;
pub mod json;
mod listen;
pub mod query_arg;
pub mod queryable;
mod schema;
mod transaction;
mod value;

pub use client::{Builder, Client, TxFuture};
pub use error::{Error, Result};
pub use listen::ChannelListener;
pub use pylon_cache::CacheStats;
pub use pylon_value::DecodedValue;
pub use query_arg::{QueryArg, QueryArgs, ValueOpt};
pub use queryable::{DecodeError, DecodeErrorKind, Queryable};
pub use transaction::{Isolation, Transaction};
pub use value::{Group, Object, Range, Value};

/// `#[derive(Queryable)]`. Shares the trait's name the way `serde`'s derives
/// share theirs — a macro and a trait live in separate namespaces, so one
/// `use pylon_client::Queryable;` brings in both.
pub use pylon_derive::Queryable;
