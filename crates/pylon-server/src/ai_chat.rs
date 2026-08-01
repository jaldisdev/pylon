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

//! `POST /api/<connection>/ai/chat` — Rust port of `pylon/server/asgi.py`'s
//! `_handle_ai_chat`/`_make_chat_provider`/`_resolve_vector_index_pointers`.
//!
//! The AI tab's RAG loop: runs `vector::search` for context, templates a
//! (still hardcoded-default — no prompt-template registry exists in Pylon
//! yet) system/user prompt around it, and calls the selected chat-purpose
//! model via `pylon_providers`. Unlike the Python original — which calls a
//! separate, parallel implementation in `pylon/vector/models/*.py` for this
//! path — this uses the already-existing Rust `OpenAiProvider`/
//! `AnthropicProvider` directly.
//!
//! **Real gap found while porting, fixed here rather than in `pylon-client`**:
//! `vector::search(Type, query := $text)`'s "text overload" never sends
//! `$text` itself to Postgres — `ir/compiler.rs::try_compile_vector_search`
//! swaps that parameter node for a `__deferred_vec__` one at compile time
//! (cast `float8[] -> vector`), which the *caller* is expected to fill in
//! with a real embedding vector before executing (mirrors `client.py`'s
//! `_compile_and_resolve`, which does the same embed-then-inject step for
//! every generic query, not just this endpoint). `pylon-client`'s generic
//! `compile_and_bind` has no notion of `CompiledQuery.inference_plan` at
//! all yet — porting that generically is real, separate work (it would
//! need its own model-registry concept, and an `fts::search`-backend
//! HTTP client, neither of which exist in `pylon-client` today). Since this
//! handler already knows the exact PyQL it built and already has
//! `pylon.toml`'s model registry in `state.config.models`, it just computes
//! the embedding itself and binds it under the literal name
//! `"__deferred_vec__"` — no `pylon-client` changes needed for this one
//! call site.

use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};
use pylon_core::schema::{SchemaDescriptor, VectorIndexDescriptor};
use pylon_providers::{AnthropicProvider, Message, OpenAiProvider};
use pylon_value::CachedValue;

use crate::config::{ApiStyle, ModelPurpose};
use crate::json::json_response;
use crate::state::AppState;
use crate::to_json::{client_error_payload, value_to_json};

const DEFAULT_PROMPT_SYSTEM: &str = "You are an expert Q&A system.
Always answer questions based on the provided context information. Never use prior knowledge.
Follow these additional rules:
1. Never directly reference the given context in your answer.
2. Never include phrases like 'Based on the context, ...' or any similar phrases in your responses.
3. When the context does not provide information about the question, answer with 'No information available.'.
Context information is below:
{context}
Given the context information above and not prior knowledge, answer the user query.";

const DEFAULT_PROMPT_USER: &str = "Query: {query}\nAnswer:";

/// `"module::Name"` -> `("module", "Name")`; mirrors Python's own
/// `pylon_type.partition("::")` (an unqualified string with no `"::"` at
/// all falls back to a name that can never match a real type, same as the
/// Python original — this only ever happens with a malformed request).
fn split_module_name(pylon_type: &str) -> (&str, &str) {
    match pylon_type.split_once("::") {
        Some((module, name)) => (module, name),
        None => (pylon_type, ""),
    }
}

/// Looks up the `VectorIndex` matching `index_name` on `pylon_type` — its
/// pointers tell the RAG context builder what the similarity search
/// actually matched on (not an arbitrary object dump), and its `model`
/// (a `[models.*]` registry key, not a raw provider model string) says
/// which embedding model to run the query text through.
fn resolve_vector_index(schema: &SchemaDescriptor, pylon_type: &str, index_name: Option<&str>) -> Option<VectorIndexDescriptor> {
    let (module, name) = split_module_name(pylon_type);
    schema
        .types
        .iter()
        .find(|t| t.module == module && t.name == name)
        .and_then(|t| t.vector_indexes.iter().find(|vi| vi.index_name.as_deref() == index_name))
        .cloned()
}

/// Renders a decoded JSON leaf the way the context builder needs it as
/// text — covers every JSON type a vector-indexed pointer could plausibly
/// be (text, numeric, uuid-as-string). Not a byte-for-byte match of
/// Python's `str()` for every type (e.g. booleans render lowercase here,
/// `"true"`/`"false"`, not `"True"`/`"False"`) since indexed pointers are
/// virtually always text in practice.
fn json_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Python's `index_name!r}` — `None` -> `None`, `Some("foo")` -> `'foo'`.
fn repr_option_str(v: Option<&str>) -> String {
    match v {
        None => "None".to_string(),
        Some(s) => format!("'{s}'"),
    }
}

pub async fn handle_ai_chat(state: Arc<AppState>, connection: &str, body: serde_json::Value) -> Response<Full<Bytes>> {
    let client = match state.resolve_client(connection).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return json_response(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": format!("No connection named {connection:?}")}),
            )
        }
        Err(e) => return json_response(StatusCode::INTERNAL_SERVER_ERROR, &client_error_payload(&e)),
    };

    let model_name = body.get("modelName").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let pylon_type = body.get("pylonType").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let index_name = body.get("indexName").and_then(|v| v.as_str()).map(str::to_string);
    let context_query = body.get("contextQuery").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
    let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let history: Vec<&serde_json::Value> = body.get("history").and_then(|v| v.as_array()).map(|a| a.iter().collect()).unwrap_or_default();

    let Some(model_cfg) = state.config.models.get(&model_name).filter(|m| m.purpose == ModelPurpose::Chat) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": format!("'{model_name}' is not a configured chat model")}),
        );
    };

    let Some(vector_index) = resolve_vector_index(&client.schema(), &pylon_type, index_name.as_deref()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({
                "error": format!(
                    "no VectorIndex found on '{pylon_type}' matching index_name={}",
                    repr_option_str(index_name.as_deref())
                )
            }),
        );
    };
    let index_pointers = &vector_index.pointers;

    let Some(embedding_model_cfg) = state.config.models.get(&vector_index.model) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": format!("vector::search: no model config found for '{}' in pylon.toml", vector_index.model)}),
        );
    };
    let embedding: Vec<f32> = match embedding_model_cfg.api_style {
        ApiStyle::Anthropic => {
            return json_response(StatusCode::BAD_GATEWAY, &serde_json::json!({"error": "AnthropicProvider does not support embeddings"}))
        }
        ApiStyle::OpenAi => {
            let provider = match OpenAiProvider::new(&embedding_model_cfg.api_url, &embedding_model_cfg.model, embedding_model_cfg.secret.as_deref()) {
                Ok(p) => p,
                Err(e) => {
                    return json_response(StatusCode::BAD_GATEWAY, &serde_json::json!({"error": format!("embedding request failed: {e}")}))
                }
            };
            match provider.embed_batch(std::slice::from_ref(&message)).await {
                Ok(mut batch) => batch.pop().unwrap_or_default(),
                Err(e) => {
                    return json_response(StatusCode::BAD_GATEWAY, &serde_json::json!({"error": format!("embedding request failed: {e}")}))
                }
            }
        }
    };

    let shape = index_pointers.join(", ");
    // `index_name := <str>$indexName` is inert today even when `index_name`
    // is `Some` — `ir/compiler.rs::try_compile_vector_search` only ever
    // recognizes a *literal* string there, never a parameter, so this
    // never actually narrows anything beyond the default index. Matches
    // `asgi.py::_handle_ai_chat`'s existing behavior (not a regression
    // introduced by this port) — fixing it is compiler-level work, out of
    // scope here.
    let index_clause = if index_name.is_some() { ", index_name := <str>$indexName" } else { "" };
    let search_target = match &context_query {
        Some(q) => format!("({q})"),
        None => pylon_type.clone(),
    };
    let pyql = format!(
        "select vector::search({search_target}, query := <str>$queryText{index_clause}) {{ object {{ {shape} }}, distance }} order by .distance limit 5"
    );
    let mut params: Vec<(&str, CachedValue)> = vec![
        ("__deferred_vec__", CachedValue::Array(embedding.iter().map(|f| CachedValue::F64(*f as f64)).collect())),
    ];
    if let Some(idx) = &index_name {
        params.push(("indexName", CachedValue::Str(idx.clone())));
    }

    let objects = match client.query(&pyql, &params).await {
        Ok(o) => o,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &client_error_payload(&e)),
    };
    let results: Vec<serde_json::Value> = objects.iter().map(value_to_json).collect();

    // One line per result, just the indexed pointers concatenated — the
    // same text that was embedded, not a "key: value" dump of the object.
    let context = results
        .iter()
        .map(|r| {
            let object = &r["object"];
            let line = index_pointers.iter().map(|p| json_display(&object[p.as_str()])).collect::<Vec<_>>().join(". ");
            format!("- {line}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut messages = vec![Message { role: "system".to_string(), content: DEFAULT_PROMPT_SYSTEM.replace("{context}", &context) }];
    for h in &history {
        let role = h.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let content = h.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        messages.push(Message { role, content });
    }
    messages.push(Message { role: "user".to_string(), content: DEFAULT_PROMPT_USER.replace("{query}", &message) });

    let chat_result = match model_cfg.api_style {
        ApiStyle::OpenAi => match OpenAiProvider::new(&model_cfg.api_url, &model_cfg.model, model_cfg.secret.as_deref()) {
            Ok(p) => p.chat(&messages).await,
            Err(e) => Err(e),
        },
        ApiStyle::Anthropic => match AnthropicProvider::new(&model_cfg.api_url, &model_cfg.model, model_cfg.secret.as_deref()) {
            Ok(p) => p.chat(&messages).await,
            Err(e) => Err(e),
        },
    };
    let reply = match chat_result {
        Ok(r) => r,
        Err(e) => {
            return json_response(StatusCode::BAD_GATEWAY, &serde_json::json!({"error": format!("chat model request failed: {e}")}))
        }
    };

    json_response(StatusCode::OK, &serde_json::json!({"reply": reply, "results": results}))
}
