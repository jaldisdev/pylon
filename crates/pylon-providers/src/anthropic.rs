//! Anthropic Messages API provider — chat completions only. Functionally
//! identical to `pylon.vector.models.anthropic.AnthropicProvider`.
//!
//! Anthropic has no embeddings endpoint, so this only implements `chat`.
//!
//!   `POST {api_url}/messages`
//!   `x-api-key: {api_key}`
//!   `anthropic-version: 2023-06-01`
//!   `{"model": "...", "max_tokens": ..., "system": "...", "messages": [...]}`
//!
//! Anthropic takes the system prompt as a separate top-level field, not a
//! `"system"`-role message — split out here so callers can build a uniform
//! `[{"role": "system", ...}, {"role": "user", ...}, ...]` list regardless
//! of which provider ends up handling it.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::message::Message;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 1024;

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
}

#[derive(Deserialize)]
struct ChatResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

impl AnthropicProvider {
    /// Same 60s-timeout, no-retry contract as `OpenAiProvider::new`.
    pub fn new(api_url: &str, model: &str, api_key: Option<&str>) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        if let Some(key) = api_key {
            headers.insert(
                "x-api-key",
                reqwest::header::HeaderValue::from_str(key)
                    .map_err(|e| crate::error::Error::Shape(e.to_string()))?,
            );
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self { client, base_url: api_url.trim_end_matches('/').to_string(), model: model.to_string() })
    }

    pub async fn chat(&self, messages: &[Message]) -> Result<String> {
        let system = messages.iter().find(|m| m.role == "system").map(|m| m.content.as_str());
        let turns: Vec<AnthropicMessage<'_>> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| AnthropicMessage { role: &m.role, content: &m.content })
            .collect();
        let body = ChatRequest { model: &self.model, max_tokens: MAX_TOKENS, messages: turns, system };
        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let parsed: ChatResponse = response.json().await?;
        Ok(parsed
            .content
            .into_iter()
            .filter(|block| block.kind == "text")
            .map(|block| block.text)
            .collect::<Vec<_>>()
            .join(""))
    }
}
