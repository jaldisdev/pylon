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

//! Generic OpenAI-compatible provider — embeddings and chat completions.
//! Functionally identical to `pylon.vector.models.openai.OpenAIProvider`:
//!
//! Embeddings:
//!   `POST {api_url}/embeddings`
//!   `Authorization: Bearer {api_key}`
//!   `{"model": "...", "input": ["text", ...]}`
//!
//! Chat completions:
//!   `POST {api_url}/chat/completions`
//!   `Authorization: Bearer {api_key}`
//!   `{"model": "...", "messages": [{"role": ..., "content": ...}, ...]}`
//!
//! Compatible providers: OpenAI, Mistral, any OpenAI-compatible endpoint.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::message::Message;

/// Matches the Python provider's own chunking — a single `/embeddings`
/// call is capped at this many input texts.
const MAX_BATCH: usize = 2048;

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

impl OpenAiProvider {
    /// `api_url` is the provider's base URL (trailing slash tolerated, like
    /// the Python `str.rstrip("/")`). Fixed 60s timeout, no retry — matches
    /// the Python client exactly (no sophistication that doesn't exist
    /// there today).
    pub fn new(api_url: &str, model: &str, api_key: Option<&str>) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(key) = api_key {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|e| crate::error::Error::Shape(e.to_string()))?,
            );
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self { client, base_url: api_url.trim_end_matches('/').to_string(), model: model.to_string() })
    }

    /// Returns one embedding vector per input text, in the same order —
    /// chunks at `MAX_BATCH`, re-sorts each chunk's response by `index`
    /// before extracting, matching the Python provider exactly (a defense
    /// against providers returning results out of order).
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(MAX_BATCH) {
            let body = EmbeddingsRequest { model: &self.model, input: chunk };
            let response = self
                .client
                .post(format!("{}/embeddings", self.base_url))
                .json(&body)
                .send()
                .await?
                .error_for_status()?;
            let mut parsed: EmbeddingsResponse = response.json().await?;
            parsed.data.sort_by_key(|item| item.index);
            results.extend(parsed.data.into_iter().map(|item| item.embedding));
        }
        Ok(results)
    }

    pub async fn chat(&self, messages: &[Message]) -> Result<String> {
        let body = ChatRequest {
            model: &self.model,
            messages: messages.iter().map(|m| ChatMessage { role: &m.role, content: &m.content }).collect(),
        };
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let parsed: ChatResponse = response.json().await?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| crate::error::Error::Shape("no choices in chat completion response".into()))
    }
}
