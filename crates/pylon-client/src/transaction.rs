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

//! Closure-based retrying transactions — see [`crate::Client::transaction`].
//!
//! Unlike `pylon/client.py`'s `RetryingTransaction` (an async iterator the
//! caller drives with `async for tx in client.transaction(): async with
//! tx: ...`), the body here is a closure re-run once per attempt; the
//! transaction commits automatically when it returns `Ok`, rolls back and
//! retries on a retriable error (serialization failure/deadlock), and
//! rolls back and propagates on anything else. `Transaction` itself has no
//! public `commit`/`rollback` — that decision is made for the caller by
//! the closure's own return value.
//!
//! That includes a deliberate abort: returning [`crate::Error::Rollback`]
//! rolls back without retrying, and
//! [`Client::transaction_opt`](crate::Client::transaction_opt) reports it
//! as `Ok(None)` instead of an error. It is the Rust counterpart of
//! `pylon.Rollback` in `pylon/client.py`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use pylon_core::ir::SessionConfig;
use pylon_core::schema::SchemaDescriptor;
use pylon_value::DecodedValue;

use crate::error::Result;
use crate::exec;
use crate::query_arg::QueryArgs;
use crate::queryable::{Queryable, decode_optional_row, decode_row, decode_rows};

/// PostgreSQL transaction isolation level. Defaults to `Serializable`,
/// matching `pylon/client.py`'s own default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    #[default]
    Serializable,
}

impl Isolation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Isolation::ReadUncommitted => "read_uncommitted",
            Isolation::ReadCommitted => "read_committed",
            Isolation::RepeatableRead => "repeatable_read",
            Isolation::Serializable => "serializable",
        }
    }
}

/// A single transaction attempt, handed to the closure passed to
/// [`crate::Client::transaction`]. Exposes the same query methods as
/// [`crate::Client`] itself (minus `analyze`, which the Python client also
/// never runs inside an explicit transaction).
pub struct Transaction {
    pub(crate) inner: pylon_pgcon::PgTransaction,
    pub(crate) schema: Arc<RwLock<SchemaDescriptor>>,
    pub(crate) config: SessionConfig,
    pub(crate) globals: Arc<HashMap<String, DecodedValue>>,
    /// Held only to *evict* on a write — see `execute`. Never used to read
    /// or populate: rows read inside a transaction aren't committed yet.
    pub(crate) cache: Option<Arc<pylon_cache::Cache>>,
}

impl Transaction {
    // Every method passes `CacheAccess::evict_only`: uncommitted rows must
    // never populate the read-through cache (matching `pylon/client.py`'s
    // `AsyncTransaction`), but a write still has to evict — and a write can
    // arrive through any of these, not just `execute`. `query("insert ...")`
    // is a normal way to insert and read the row back, and `Client.save`'s
    // Python counterpart uses `query_single` for exactly that.

    pub async fn query<R: Queryable, A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<Vec<R>> {
        let params = args.to_params();
        let schema = self.schema.read().unwrap().clone();
        let values = exec::query(
            &self.inner,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::evict_only(self.cache.as_deref()),
        )
        .await?;
        decode_rows(values)
    }

    pub async fn query_single<R: Queryable, A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<Option<R>> {
        let params = args.to_params();
        let schema = self.schema.read().unwrap().clone();
        let values = exec::query_single(
            &self.inner,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::evict_only(self.cache.as_deref()),
        )
        .await?;
        decode_optional_row(values)
    }

    pub async fn query_required_single<R: Queryable, A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<R> {
        let params = args.to_params();
        let schema = self.schema.read().unwrap().clone();
        let values = exec::query_required_single(
            &self.inner,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::evict_only(self.cache.as_deref()),
        )
        .await?;
        decode_row(values)
    }

    /// A write inside a transaction still evicts immediately: the cache is
    /// never populated from inside a transaction, so an aborted attempt can
    /// only over-evict — which costs a re-read and never serves stale data.
    pub async fn execute<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<()> {
        let params = args.to_params();
        let schema = self.schema.read().unwrap().clone();
        exec::execute(
            &self.inner,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::evict_only(self.cache.as_deref()),
        )
        .await
    }

    pub async fn query_json<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<String> {
        let params = args.to_params();
        let schema = self.schema.read().unwrap().clone();
        exec::query_json(
            &self.inner,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::evict_only(self.cache.as_deref()),
        )
        .await
    }

    pub async fn query_single_json<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<Option<String>> {
        let params = args.to_params();
        let schema = self.schema.read().unwrap().clone();
        exec::query_single_json(
            &self.inner,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::evict_only(self.cache.as_deref()),
        )
        .await
    }

    pub async fn query_required_single_json<A: QueryArgs + ?Sized>(&self, pyql: &str, args: &A) -> Result<String> {
        let params = args.to_params();
        let schema = self.schema.read().unwrap().clone();
        exec::query_required_single_json(
            &self.inner,
            pyql,
            &params,
            &schema,
            &self.config,
            &self.globals,
            crate::cache::CacheAccess::evict_only(self.cache.as_deref()),
        )
        .await
    }
}
