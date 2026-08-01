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

//! The crate's single error type — deliberately *not* a boxed
//! `dyn std::error::Error`, unlike the rest of this crate's early phases.
//! Error mapping to Pylon's Python exception hierarchy (`pylon.exceptions`)
//! needs the Postgres SQLSTATE code, exactly like `asyncpg`'s own typed
//! exception classes (`asyncpg.SerializationError.sqlstate == "40001"`,
//! `asyncpg.DeadlockDetectedError.sqlstate == "40P01"`) already give the
//! current Python-side `_fmt_pg_error` today. Erasing into a boxed
//! `dyn Error` at every `?` site — this crate's original design — throws
//! that code away before it can ever reach the pyo3 boundary.

use tokio_postgres::error::SqlState;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
    #[error(transparent)]
    Pool(#[from] deadpool_postgres::PoolError),
    #[error(transparent)]
    Build(#[from] deadpool_postgres::BuildError),
    /// A wire-format buffer was the wrong length for the type being
    /// decoded (malformed or truncated data).
    #[error(transparent)]
    WireLength(#[from] std::array::TryFromSliceError),
    /// A `text`/`varchar`/`bpchar` field's bytes weren't valid UTF-8.
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    /// A jsonb field's bytes weren't valid JSON, or a `CachedValue::Object`
    /// being bound as a jsonb parameter failed to serialize.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A `CachedValue::Decimal`'s string form wasn't a valid decimal.
    #[error(transparent)]
    Decimal(#[from] rust_decimal::Error),
    /// Everything else: DSN parse failures, the "cannot bind a composite
    /// as a query parameter" case — none of these originate from a real
    /// Postgres response, so none of them carry a SQLSTATE.
    #[error("{0}")]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

// `postgres_types::{FromSql, ToSql}` (used directly for `rust_decimal`
// numeric decode/encode) are trait-mandated to return this exact boxed
// type — a direct `From` lets those sites keep using plain `?`.
impl From<Box<dyn std::error::Error + Send + Sync>> for Error {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Error::Other(e)
    }
}

impl Error {
    /// The Postgres SQLSTATE code, when this error is a real response from
    /// the server (as opposed to a connection/pool/decode failure, none of
    /// which have one) — the single source of truth error mapping at the
    /// pyo3 boundary classifies on.
    pub fn sqlstate(&self) -> Option<&SqlState> {
        match self {
            Error::Postgres(e) => e.code(),
            Error::Pool(deadpool_postgres::PoolError::Backend(e)) => e.code(),
            _ => None,
        }
    }

    pub(crate) fn message(msg: impl Into<String>) -> Self {
        Error::Other(msg.into().into())
    }

    /// The structured `DbError` a real server response carries — `None`
    /// for a connection/pool/decode failure, none of which have one.
    fn as_db_error(&self) -> Option<&tokio_postgres::error::DbError> {
        match self {
            Error::Postgres(e) => e.as_db_error(),
            Error::Pool(deadpool_postgres::PoolError::Backend(e)) => e.as_db_error(),
            _ => None,
        }
    }

    /// The message a caller should actually show. `tokio_postgres::Error`'s
    /// own `Display` only renders a generic category string for a
    /// server-side error (`"db error"` for every `DbError`-backed failure,
    /// regardless of what the server actually said) — the real detail
    /// (e.g. `duplicate key value violates unique constraint "..."`) lives
    /// one level deeper, in the `DbError` its `source()` wraps, so this
    /// prefers that when present and falls back to `Display` otherwise.
    pub fn pg_message(&self) -> String {
        match self.as_db_error() {
            Some(db) => db.message().to_string(),
            None => self.to_string(),
        }
    }

    /// `(schema, type name)` for the PostgreSQL scalar/domain a
    /// `CHECK_VIOLATION` failed against (Postgres's `SchemaName`/
    /// `DataTypeName` error fields — both populated for a domain check,
    /// confirmed live) — `Some(("public", "Email"))` for a registered
    /// custom scalar's own DOMAIN check; `None` for an ordinary
    /// table-level CHECK (use `violated_table` instead).
    pub fn violated_scalar(&self) -> Option<(&str, &str)> {
        let db = self.as_db_error()?;
        Some((db.schema()?, db.datatype()?))
    }

    /// `(schema, table)` a `CHECK_VIOLATION`'s table-level constraint
    /// belongs to, when Postgres reports one (a domain-level check
    /// reports `violated_datatype` instead, not this).
    pub fn violated_table(&self) -> Option<(&str, &str)> {
        let db = self.as_db_error()?;
        Some((db.schema()?, db.table()?))
    }
}
