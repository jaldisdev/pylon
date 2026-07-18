//! Thin async HTTP clients for Meilisearch and OpenSearch — functionally
//! identical to `pylon.search.meilisearch.MeilisearchClient`/
//! `pylon.search.opensearch.OpenSearchClient` (same endpoints, request
//! bodies, and 404/already-exists tolerance). Kept separate from
//! `pylon-providers` (LLM embedding/chat clients) since these are a
//! different kind of external service, only ever used by the search-index
//! workers below.

use std::collections::HashMap;
use std::time::Duration;

use crate::error::{Error, Result};

fn client(timeout: Duration, headers: reqwest::header::HeaderMap) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(timeout).default_headers(headers).build()?)
}

pub struct MeilisearchClient {
    http: reqwest::Client,
    base_url: String,
}

impl MeilisearchClient {
    pub fn new(base_url: &str, api_key: Option<&str>, timeout: Duration) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(key) = api_key {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).map_err(|e| Error::Decode(e.to_string()))?,
            );
        }
        Ok(Self { http: client(timeout, headers)?, base_url: base_url.trim_end_matches('/').to_string() })
    }

    /// Full-text query; returns `[(id, score)]` ordered by relevance.
    pub async fn search(&self, index: &str, query_text: &str, size: usize) -> Result<Vec<(String, f64)>> {
        let url = format!("{}/indexes/{index}/search", self.base_url);
        let body = serde_json::json!({"q": query_text, "limit": size, "showRankingScore": true});
        let resp = self.http.post(&url).json(&body).send().await?.error_for_status()?;
        let data: serde_json::Value = resp.json().await?;
        let hits = data["hits"].as_array().cloned().unwrap_or_default();
        Ok(hits
            .into_iter()
            .map(|h| {
                let id = h["id"].as_str().map(str::to_string).unwrap_or_default();
                let score = h.get("_rankingScore").and_then(|v| v.as_f64()).unwrap_or(1.0);
                (id, score)
            })
            .collect())
    }

    /// Upserts a document into the Meilisearch index.
    pub async fn index_document(&self, index: &str, doc_id: &str, fields: &HashMap<String, String>) -> Result<()> {
        let url = format!("{}/indexes/{index}/documents", self.base_url);
        let mut doc: HashMap<&str, &str> = HashMap::with_capacity(fields.len() + 1);
        doc.insert("id", doc_id);
        for (k, v) in fields {
            doc.insert(k, v);
        }
        self.http.put(&url).json(&[doc]).send().await?.error_for_status()?;
        Ok(())
    }

    /// Removes a document from the Meilisearch index (no-op if not found).
    pub async fn delete_document(&self, index: &str, doc_id: &str) -> Result<()> {
        let url = format!("{}/indexes/{index}/documents/{doc_id}", self.base_url);
        let resp = self.http.delete(&url).send().await?;
        if resp.status() != reqwest::StatusCode::NOT_FOUND {
            resp.error_for_status()?;
        }
        Ok(())
    }

    /// Creates the Meilisearch index if it doesn't already exist.
    pub async fn create_index(&self, index: &str) -> Result<()> {
        let url = format!("{}/indexes", self.base_url);
        let body = serde_json::json!({"uid": index, "primaryKey": "id"});
        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        if !matches!(status.as_u16(), 200..=202) {
            let data: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            if data.get("code").and_then(|v| v.as_str()) != Some("index_already_exists") {
                return Err(Error::Decode(format!("create_index failed: HTTP {status}: {data}")));
            }
        }
        Ok(())
    }
}

pub struct OpenSearchClient {
    http: reqwest::Client,
    base_url: String,
}

impl OpenSearchClient {
    pub fn new(base_url: &str, auth: Option<(&str, &str)>, timeout: Duration) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some((user, password)) = auth {
            let credentials = format!("{user}:{password}");
            let encoded = base64_encode(credentials.as_bytes());
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Basic {encoded}")).map_err(|e| Error::Decode(e.to_string()))?,
            );
        }
        Ok(Self { http: client(timeout, headers)?, base_url: base_url.trim_end_matches('/').to_string() })
    }

    /// Full-text query; returns `[(id, score)]` ordered by relevance.
    pub async fn search(&self, index: &str, query_text: &str, fields: Option<&[String]>, size: usize) -> Result<Vec<(String, f64)>> {
        let url = format!("{}/{index}/_search", self.base_url);
        let fields = fields.map(|f| f.to_vec()).unwrap_or_else(|| vec!["*".to_string()]);
        let body = serde_json::json!({
            "query": {"multi_match": {"query": query_text, "fields": fields, "type": "best_fields"}},
            "_source": false,
            "size": size,
        });
        let resp = self.http.post(&url).json(&body).send().await?.error_for_status()?;
        let data: serde_json::Value = resp.json().await?;
        let hits = data["hits"]["hits"].as_array().cloned().unwrap_or_default();
        Ok(hits
            .into_iter()
            .map(|h| {
                let id = h["_id"].as_str().map(str::to_string).unwrap_or_default();
                let score = h["_score"].as_f64().unwrap_or(0.0);
                (id, score)
            })
            .collect())
    }

    /// Upserts a document into the OpenSearch index.
    pub async fn index_document(&self, index: &str, doc_id: &str, fields: &HashMap<String, String>) -> Result<()> {
        let url = format!("{}/{index}/_doc/{doc_id}", self.base_url);
        self.http.put(&url).json(fields).send().await?.error_for_status()?;
        Ok(())
    }

    /// Removes a document from the OpenSearch index (no-op if not found).
    pub async fn delete_document(&self, index: &str, doc_id: &str) -> Result<()> {
        let url = format!("{}/{index}/_doc/{doc_id}", self.base_url);
        let resp = self.http.delete(&url).send().await?;
        if resp.status() != reqwest::StatusCode::NOT_FOUND {
            resp.error_for_status()?;
        }
        Ok(())
    }

    /// Creates the OpenSearch index if it doesn't already exist.
    pub async fn create_index(&self, index: &str, mappings: Option<serde_json::Value>) -> Result<()> {
        let url = format!("{}/{index}", self.base_url);
        let mut body = serde_json::Map::new();
        if let Some(m) = mappings {
            body.insert("mappings".to_string(), m);
        }
        let resp = self.http.put(&url).json(&body).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::BAD_REQUEST {
            let data: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            let error_type = data["error"]["type"].as_str().unwrap_or("");
            if error_type.contains("resource_already_exists_exception") {
                return Ok(());
            }
            return Err(Error::Decode(format!("create_index failed: HTTP {status}: {data}")));
        }
        resp.error_for_status()?;
        Ok(())
    }
}

/// Minimal base64 encoder for HTTP Basic auth credentials — avoids pulling
/// in the `base64` crate for one narrow use.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(if let Some(b1) = b1 { ALPHABET[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char } else { '=' });
        out.push(if let Some(b2) = b2 { ALPHABET[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
    }
}
