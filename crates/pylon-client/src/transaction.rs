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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use pylon_core::ir::SessionConfig;
use pylon_core::schema::SchemaDescriptor;
use pylon_value::CachedValue;

use crate::error::Result;
use crate::exec;
use crate::value::Value;

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
    pub(crate) globals: Arc<HashMap<String, CachedValue>>,
}

impl Transaction {
    pub async fn query(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<Vec<Value>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query(&self.inner, pyql, params, &schema, &self.config, &self.globals).await
    }

    pub async fn query_single(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<Option<Value>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_single(&self.inner, pyql, params, &schema, &self.config, &self.globals).await
    }

    pub async fn query_required_single(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<Value> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_required_single(&self.inner, pyql, params, &schema, &self.config, &self.globals).await
    }

    pub async fn execute(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<()> {
        let schema = self.schema.read().unwrap().clone();
        exec::execute(&self.inner, pyql, params, &schema, &self.config, &self.globals).await
    }

    pub async fn query_json(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<String> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_json(&self.inner, pyql, params, &schema, &self.config, &self.globals).await
    }

    pub async fn query_single_json(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<Option<String>> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_single_json(&self.inner, pyql, params, &schema, &self.config, &self.globals).await
    }

    pub async fn query_required_single_json(&self, pyql: &str, params: &[(&str, CachedValue)]) -> Result<String> {
        let schema = self.schema.read().unwrap().clone();
        exec::query_required_single_json(&self.inner, pyql, params, &schema, &self.config, &self.globals).await
    }
}
