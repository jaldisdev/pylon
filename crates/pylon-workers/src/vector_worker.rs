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

//! Native port of `pylon.vector.sync.VectorIndexWorker` — embeds
//! `_pylon."IndexOutbox"` rows with `index_kind = 'Vector'` and writes the
//! resulting vector back onto the source row. `pylon.toml` `[models.*]`
//! resolution stays in Python (per the plan's design decision); this only
//! takes already-resolved provider configs.

use std::collections::HashMap;

use pylon_core as core;
use pylon_pgcon::{ExtensionOids, PgListener};
use pylon_value::CachedValue;

use crate::error::{Error, Result};
use crate::index_worker::{BatchProcessor, ClaimedRow};

/// An already-resolved `[models.<name>]` entry — `pylon.toml` parsing and
/// `VectorIndex(model=...)` → config lookup both stay in Python; this is
/// just the plain data needed to make the HTTP call.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub api_style: String,
    pub api_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

pub struct VectorIndexWorker {
    schema: core::schema::SchemaDescriptor,
    providers: HashMap<(String, Option<String>), ProviderConfig>,
    fetch_sql: HashMap<(String, Option<String>), String>,
}

impl VectorIndexWorker {
    /// Pre-compiles fetch SQL for every `(type_name, index_name)` key in
    /// `providers`, once at startup — matches `VectorIndexWorker.__init__`.
    pub fn new(schema: core::schema::SchemaDescriptor, providers: HashMap<(String, Option<String>), ProviderConfig>) -> Result<Self> {
        let mut fetch_sql = HashMap::with_capacity(providers.len());
        for (type_name, index_name) in providers.keys() {
            let sql = core::export::compile_index_fetch(type_name, index_name.as_deref(), &schema)
                .map_err(|e| Error::Schema(e.to_string()))?;
            fetch_sql.insert((type_name.clone(), index_name.clone()), sql);
        }
        Ok(Self { schema, providers, fetch_sql })
    }

    /// Mirrors `VectorIndexWorker._table_and_col`.
    fn table_and_col(&self, type_name: &str, index_name: Option<&str>) -> Result<(String, String)> {
        let td = self
            .schema
            .types
            .iter()
            .find(|t| format!("{}::{}", t.module, t.name) == type_name)
            .ok_or_else(|| Error::Schema(format!("VectorIndexWorker: unknown type '{type_name}'")))?;
        let vi = td
            .vector_indexes
            .iter()
            .find(|v| v.index_name.as_deref() == index_name)
            .ok_or_else(|| {
                Error::Schema(format!(
                    "VectorIndexWorker: no VectorIndex '{}' on type '{type_name}'",
                    index_name.unwrap_or("<default>")
                ))
            })?;
        let pg_schema = if td.module == "default" { "public" } else { &td.module };
        let table = format!("\"{pg_schema}\".\"{}\"", td.table);
        let col = format!("\"{}\"", vi.column_name());
        Ok((table, col))
    }
}

struct FetchedRow {
    id: [u8; 16],
    source_text: String,
}

fn decode_fetched_row(value: &CachedValue) -> Result<FetchedRow> {
    let CachedValue::Object(fields) = value else {
        return Err(Error::Decode("compile_index_fetch: expected a named-column row".into()));
    };
    let field = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);
    let id = match field("id") {
        Some(CachedValue::Uuid(b)) => *b,
        _ => return Err(Error::Decode("compile_index_fetch: missing/invalid 'id'".into())),
    };
    let source_text = match field("source_text") {
        Some(CachedValue::Str(s)) => s.clone(),
        Some(CachedValue::Null) | None => String::new(),
        _ => return Err(Error::Decode("compile_index_fetch: invalid 'source_text'".into())),
    };
    Ok(FetchedRow { id, source_text })
}

impl BatchProcessor for VectorIndexWorker {
    fn index_kind(&self) -> &'static str {
        "Vector"
    }

    async fn process_batch(&self, listener: &PgListener, rows: &[ClaimedRow]) -> Result<()> {
        let mut groups: HashMap<(String, Option<String>), Vec<&ClaimedRow>> = HashMap::new();
        for r in rows {
            groups.entry((r.type_name.clone(), r.index_name.clone())).or_default().push(r);
        }

        for ((type_name, index_name), group_rows) in groups {
            let key = (type_name.clone(), index_name.clone());
            let Some(provider_cfg) = self.providers.get(&key) else {
                eprintln!("VectorIndexWorker: no provider for ({type_name}, {index_name:?}); skipping");
                continue;
            };
            let Some(fetch_sql) = self.fetch_sql.get(&key) else {
                eprintln!("VectorIndexWorker: no fetch SQL for ({type_name}, {index_name:?}); skipping");
                continue;
            };

            let ids: Vec<CachedValue> = group_rows.iter().map(|r| CachedValue::Uuid(r.object_id)).collect();
            let raw_records = listener
                .query_typed_named(fetch_sql, &[CachedValue::Array(ids)], &ExtensionOids::default())
                .await?;
            if raw_records.is_empty() {
                continue;
            }
            let records = raw_records.iter().map(decode_fetched_row).collect::<Result<Vec<_>>>()?;

            let texts: Vec<String> = records.iter().map(|r| r.source_text.clone()).collect();
            // Mirrors `_make_provider`'s dispatch — `AnthropicProvider` has
            // no embeddings endpoint in the Python version either.
            let vectors = if provider_cfg.api_style == "anthropic" {
                return Err(Error::Unsupported("AnthropicProvider does not support embeddings".into()));
            } else {
                let provider =
                    pylon_providers::OpenAiProvider::new(&provider_cfg.api_url, &provider_cfg.model, provider_cfg.api_key.as_deref())?;
                provider.embed_batch(&texts).await?
            };

            let (table, col) = self.table_and_col(&type_name, index_name.as_deref())?;
            let write_sql = format!("UPDATE {table} SET {col} = $2::vector WHERE \"id\" = $1");
            for (record, vec) in records.iter().zip(vectors.iter()) {
                // Bound directly as an array of floats — `wire.rs`'s
                // `encode_non_null` recognizes a `$n::vector`-cast target
                // and writes pgvector's own binary format. The old Python
                // worker had to go through a `"[0.1,0.2,...]"` text literal
                // instead, since that was the only thing asyncpg's Python
                // API could bind for a type it had no codec for; Rust
                // doesn't have that constraint.
                let cached_vec = CachedValue::Array(vec.iter().map(|f| CachedValue::F64(*f as f64)).collect());
                listener.execute_typed(&write_sql, &[CachedValue::Uuid(record.id), cached_vec]).await?;
            }
        }
        Ok(())
    }
}
