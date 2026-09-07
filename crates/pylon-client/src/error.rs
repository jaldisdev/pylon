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

//! The crate's single error type. Follows `pylon_pgcon::Error`'s own
//! convention (a real enum, not a boxed `dyn Error`) for the same reason:
//! the transaction retry loop (`transaction.rs`) needs the Postgres SQLSTATE
//! to tell a retriable serialization failure/deadlock apart from anything
//! else, and erasing to `dyn Error` at every `?` site would throw that away.

use tokio_postgres::error::SqlState;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A connection/pool/decode/server-response failure from the driver.
    #[error(transparent)]
    Db(#[from] pylon_pgcon::Error),
    /// PyQL failed to compile — a syntax/type/resolution/cardinality error.
    #[error(transparent)]
    Compile(#[from] pylon_core::error::PyQLError),
    /// A required query parameter (named or `__global__`-prefixed) had no
    /// matching entry in the params/globals passed by the caller.
    #[error("missing query parameter: {0}")]
    MissingParam(String),
    /// `query_single`/`query_required_single` (and their `_json` siblings)
    /// got more than one row back.
    #[error("expected at most one result, got {got}")]
    ResultCardinality { got: usize },
    /// `query_required_single` (and its `_json` sibling) got zero rows.
    #[error("expected exactly one result, got none")]
    NoData,
    /// Neither `pylon migration apply` nor `pylon migration watch` has ever
    /// run against this database, so there's no schema snapshot to load —
    /// a bare schema-file edit has no effect until one of those does.
    #[error(
        "no schema snapshot found in the database — run `pylon migration create` and `pylon migration apply` first"
    )]
    NoSchemaSnapshot,
    #[error("failed to parse schema JSON: {0}")]
    SchemaJson(#[from] serde_json::Error),
    /// `EXPLAIN`'s raw JSON output failed to correlate against the query's
    /// own `analyze_paths` (`pylon_core::analyze::build_coarse_grained`).
    #[error("failed to build analyze tree: {0}")]
    Analyze(String),
    /// A `pylon_cache::Cache` open/get/put failure — that crate's own
    /// errors are a boxed `dyn Error + Send + Sync`, not a good match for
    /// thiserror's `#[from]`/`transparent`, so this just carries the
    /// rendered message.
    #[error("cache error: {0}")]
    Cache(String),
    /// `Client::listen(name)` — no `Channel` declared in the schema matches
    /// *name* (bare or `module::name`).
    #[error("'{0}' is not a known Channel")]
    UnknownChannel(String),
    /// A `Client::listen()` NOTIFY payload didn't match its Channel's own
    /// declared shape (bad JSON, a value that doesn't parse as its
    /// declared scalar type, a missing Object field, ...).
    #[error("payload doesn't match its declared Channel shape: {0}")]
    MalformedPayload(String),
    /// Not a failure: a transaction body asking to be rolled back instead
    /// of committed. The Rust counterpart of `pylon.Rollback` — a body that
    /// wants to write, read its own writes and then leave nothing behind
    /// (a test, a dry run) returns this. It rides in `Error` because the
    /// body's return type is `Result<T>` and there is no `T` to hand back
    /// on a path that deliberately produced nothing; [`Client::transaction_opt`]
    /// turns it into `Ok(None)` for callers who would rather not see an
    /// error at all.
    #[error("transaction rolled back at the request of its body")]
    Rollback,
}

impl Error {
    /// The Postgres SQLSTATE code, when this wraps a real server response —
    /// `None` for a compile error or a connection/pool/decode failure.
    pub fn sqlstate(&self) -> Option<&SqlState> {
        match self {
            Error::Db(e) => e.sqlstate(),
            _ => None,
        }
    }

    /// `40001` — a serializable/repeatable-read transaction lost a write
    /// skew race. Retriable by re-running the whole transaction body.
    pub fn is_serialization_error(&self) -> bool {
        self.sqlstate() == Some(&SqlState::T_R_SERIALIZATION_FAILURE)
    }

    /// `40P01` — Postgres broke a deadlock by aborting this transaction.
    /// Retriable the same way a serialization failure is.
    pub fn is_deadlock(&self) -> bool {
        self.sqlstate() == Some(&SqlState::T_R_DEADLOCK_DETECTED)
    }

    /// Either of the two conditions the retrying transaction loop
    /// (`transaction.rs`) automatically retries on.
    pub fn is_retriable(&self) -> bool {
        self.is_serialization_error() || self.is_deadlock()
    }

    /// A deliberate abort ([`Error::Rollback`]) rather than a failure —
    /// worth distinguishing when a caller drives
    /// [`Client::transaction_with_attempts`](crate::Client::transaction_with_attempts)
    /// directly instead of going through
    /// [`Client::transaction_opt`](crate::Client::transaction_opt).
    pub fn is_rollback(&self) -> bool {
        matches!(self, Error::Rollback)
    }
}
