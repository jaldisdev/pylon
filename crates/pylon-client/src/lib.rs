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
//! going through `pylon-py`/pyo3. Query results decode into a generic,
//! dynamically-typed [`Value`]/[`Object`] rather than per-schema-type
//! structs.
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
mod listen;
mod schema;
mod transaction;
mod value;

pub use client::{Builder, Client, TxFuture};
pub use error::{Error, Result};
pub use listen::ChannelListener;
pub use pylon_cache::CacheStats;
pub use pylon_value::DecodedValue;
pub use transaction::{Isolation, Transaction};
pub use value::{Group, Object, Range, Value};
