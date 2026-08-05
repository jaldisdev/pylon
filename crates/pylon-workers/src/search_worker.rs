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

//! Native port of `pylon.search.meilisearch.MeilisearchWorker`/
//! `pylon.search.worker.OpenSearchWorker` — both are near-identical in the
//! Python source (same grouping, fetch, and document-body logic, differing
//! only in which HTTP client they call), so this is one generic
//! `SearchIndexWorker<C>` instead of two near-duplicate types.
//!
//! Note: `ClaimedRow` (see `index_worker.rs`) has no `operation` field —
//! `CLAIM_BATCH_SQL`'s `RETURNING` clause never surfaces the
//! `_pylon."IndexOutbox".operation` column. The Python workers already
//! have this gap (`row.get("operation", "index")` always defaults to
//! `"index"` since the key is never present), so their `"delete"` branch
//! is unreachable dead code today — this port carries the same limitation
//! forward rather than fixing something out of scope for a straight port.
//! `SearchSink::delete_document` is still implemented and reachable once
//! `operation` support is added to the claim path.

use std::collections::HashMap;

use pylon_core as core;
use pylon_pgcon::{ExtensionOids, PgListener};
use pylon_value::DecodedValue;
use tokio::sync::Mutex as AsyncMutex;

use crate::error::{Error, Result};
use crate::index_worker::{BatchProcessor, ClaimedRow};
use crate::search_clients::{MeilisearchClient, OpenSearchClient};

/// Adapts `MeilisearchClient`/`OpenSearchClient`'s slightly different
/// constructors into one shape `SearchIndexWorker` can drive generically.
pub trait SearchSink: Send + Sync {
    fn index_document(&self, index: &str, doc_id: &str, fields: &HashMap<String, String>) -> impl std::future::Future<Output = Result<()>> + Send;
    #[allow(dead_code)] // see module doc — unreachable until `operation` is threaded through claim_batch
    fn delete_document(&self, index: &str, doc_id: &str) -> impl std::future::Future<Output = Result<()>> + Send;
}

impl SearchSink for MeilisearchClient {
    async fn index_document(&self, index: &str, doc_id: &str, fields: &HashMap<String, String>) -> Result<()> {
        MeilisearchClient::index_document(self, index, doc_id, fields).await
    }
    async fn delete_document(&self, index: &str, doc_id: &str) -> Result<()> {
        MeilisearchClient::delete_document(self, index, doc_id).await
    }
}

impl SearchSink for OpenSearchClient {
    async fn index_document(&self, index: &str, doc_id: &str, fields: &HashMap<String, String>) -> Result<()> {
        OpenSearchClient::index_document(self, index, doc_id, fields).await
    }
    async fn delete_document(&self, index: &str, doc_id: &str) -> Result<()> {
        OpenSearchClient::delete_document(self, index, doc_id).await
    }
}

/// Mirrors `MeilisearchWorker`/`OpenSearchWorker._deferred_index_name`.
fn deferred_index_name(type_name: &str, index_name: Option<&str>) -> String {
    let (module, tname) = type_name.rsplit_once("::").unwrap_or(("default", type_name));
    let base = format!("{module}__{tname}").to_lowercase();
    match index_name {
        Some(name) => format!("{base}__{}", name.to_lowercase()),
        None => base,
    }
}

/// Mirrors `..._search_index_pointers`.
fn search_index_pointers(schema: &core::schema::SchemaDescriptor, type_name: &str, index_name: Option<&str>) -> Vec<String> {
    let Some(td) = schema.types.iter().find(|t| format!("{}::{}", t.module, t.name) == type_name) else {
        return Vec::new();
    };
    let Some(si) = td.search_indexes.iter().find(|s| s.index_name.as_deref() == index_name) else {
        return Vec::new();
    };
    si.pointers.iter().map(|p| p.name.clone()).collect()
}

/// Mirrors the `pointer_names`/`\n`-split document-body logic shared by
/// both Python workers.
fn split_source_text(pointer_names: &[String], source_text: &str) -> HashMap<String, String> {
    if !pointer_names.is_empty() && source_text.contains('\n') {
        source_text
            .splitn(pointer_names.len(), '\n')
            .zip(pointer_names.iter())
            .map(|(part, name)| (name.clone(), part.to_string()))
            .collect()
    } else {
        HashMap::from([("text".to_string(), source_text.to_string())])
    }
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex = hex::encode(bytes);
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

struct FetchedDoc {
    id: [u8; 16],
    source_text: String,
}

fn decode_fetched_doc(value: &DecodedValue) -> Result<FetchedDoc> {
    let DecodedValue::Object(fields) = value else {
        return Err(Error::Decode("compile_search_index_fetch: expected a named-column row".into()));
    };
    let field = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);
    let id = match field("id") {
        Some(DecodedValue::Uuid(b)) => *b,
        _ => return Err(Error::Decode("compile_search_index_fetch: missing/invalid 'id'".into())),
    };
    let source_text = match field("source_text") {
        Some(DecodedValue::Str(s)) => s.clone(),
        Some(DecodedValue::Null) | None => String::new(),
        _ => return Err(Error::Decode("compile_search_index_fetch: invalid 'source_text'".into())),
    };
    Ok(FetchedDoc { id, source_text })
}

pub struct SearchIndexWorker<C: SearchSink> {
    schema: core::schema::SchemaDescriptor,
    client: C,
    index_kind: &'static str,
    fetch_sql: AsyncMutex<HashMap<(String, Option<String>), String>>,
}

impl<C: SearchSink> SearchIndexWorker<C> {
    pub fn new(schema: core::schema::SchemaDescriptor, client: C, index_kind: &'static str) -> Self {
        Self { schema, client, index_kind, fetch_sql: AsyncMutex::new(HashMap::new()) }
    }

    /// Mirrors `_ensure_fetch_sql` — lazily compiled and cached per
    /// `(type_name, index_name)`; a compile failure is logged and *not*
    /// cached, so it's retried on the next batch (matches the Python
    /// version's own behavior: it only writes the dict entry inside the
    /// `try`, so an exception leaves the key absent).
    async fn ensure_fetch_sql(&self, type_name: &str, index_name: Option<&str>) -> Option<String> {
        let key = (type_name.to_string(), index_name.map(str::to_string));
        if let Some(sql) = self.fetch_sql.lock().await.get(&key) {
            return Some(sql.clone());
        }
        match core::export::compile_search_index_fetch(type_name, index_name, &self.schema) {
            Ok(sql) => {
                self.fetch_sql.lock().await.insert(key, sql.clone());
                Some(sql)
            }
            Err(_) => {
                eprintln!("{}IndexWorker: cannot compile fetch SQL for ({type_name}, {index_name:?})", self.index_kind);
                None
            }
        }
    }
}

impl<C: SearchSink + 'static> BatchProcessor for SearchIndexWorker<C> {
    fn index_kind(&self) -> &'static str {
        self.index_kind
    }

    async fn process_batch(&self, listener: &PgListener, rows: &[ClaimedRow]) -> Result<()> {
        let mut groups: HashMap<(String, Option<String>), Vec<&ClaimedRow>> = HashMap::new();
        for r in rows {
            groups.entry((r.type_name.clone(), r.index_name.clone())).or_default().push(r);
        }

        for ((type_name, index_name), group_rows) in groups {
            let search_index = deferred_index_name(&type_name, index_name.as_deref());

            let Some(fetch_sql) = self.ensure_fetch_sql(&type_name, index_name.as_deref()).await else {
                continue;
            };

            let ids: Vec<DecodedValue> = group_rows.iter().map(|r| DecodedValue::Uuid(r.object_id)).collect();
            let raw_records = listener
                .query_typed_named(&fetch_sql, &[DecodedValue::Array(ids)], &ExtensionOids::default())
                .await?;
            if raw_records.is_empty() {
                continue;
            }
            let records = raw_records.iter().map(decode_fetched_doc).collect::<Result<Vec<_>>>()?;

            let pointer_names = search_index_pointers(&self.schema, &type_name, index_name.as_deref());
            for record in &records {
                let doc_id = format_uuid(&record.id);
                let doc_body = split_source_text(&pointer_names, &record.source_text);
                self.client.index_document(&search_index, &doc_id, &doc_body).await?;
            }
        }
        Ok(())
    }
}
