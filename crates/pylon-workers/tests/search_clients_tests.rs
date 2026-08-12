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

use std::collections::HashMap;
use std::time::Duration;

use pylon_workers::{MeilisearchClient, OpenSearchClient};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn timeout() -> Duration {
    Duration::from_secs(5)
}

#[tokio::test]
async fn meilisearch_indexes_a_document_with_id_merged_into_the_body() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/indexes/products/documents"))
        .respond_with(|req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let docs = body.as_array().unwrap();
            assert_eq!(docs.len(), 1);
            assert_eq!(docs[0]["id"], "doc-1");
            assert_eq!(docs[0]["name"], "Lamp");
            ResponseTemplate::new(202).set_body_json(serde_json::json!({"taskUid": 1}))
        })
        .mount(&server)
        .await;

    let client = MeilisearchClient::new(&server.uri(), None, timeout()).unwrap();
    let mut fields = HashMap::new();
    fields.insert("name".to_string(), "Lamp".to_string());
    client.index_document("products", "doc-1", &fields).await.unwrap();
}

#[tokio::test]
async fn meilisearch_sends_bearer_auth_when_an_api_key_is_given() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/indexes/products/documents"))
        .and(header("authorization", "Bearer secret-key"))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let client = MeilisearchClient::new(&server.uri(), Some("secret-key"), timeout()).unwrap();
    client
        .index_document("products", "doc-1", &HashMap::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn meilisearch_delete_document_treats_404_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/indexes/products/documents/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = MeilisearchClient::new(&server.uri(), None, timeout()).unwrap();
    client.delete_document("products", "missing").await.unwrap();
}

#[tokio::test]
async fn meilisearch_delete_document_propagates_a_real_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/indexes/products/documents/doc-1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = MeilisearchClient::new(&server.uri(), None, timeout()).unwrap();
    assert!(client.delete_document("products", "doc-1").await.is_err());
}

#[tokio::test]
async fn meilisearch_create_index_treats_index_already_exists_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({"code": "index_already_exists"})))
        .mount(&server)
        .await;

    let client = MeilisearchClient::new(&server.uri(), None, timeout()).unwrap();
    client.create_index("products").await.unwrap();
}

#[tokio::test]
async fn meilisearch_create_index_propagates_a_different_error_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({"code": "invalid_index_uid"})))
        .mount(&server)
        .await;

    let client = MeilisearchClient::new(&server.uri(), None, timeout()).unwrap();
    assert!(client.create_index("products").await.is_err());
}

#[tokio::test]
async fn meilisearch_search_returns_id_score_pairs_ordered_by_relevance() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/indexes/products/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "hits": [
                {"id": "doc-1", "_rankingScore": 0.9},
                {"id": "doc-2", "_rankingScore": 0.4},
            ]
        })))
        .mount(&server)
        .await;

    let client = MeilisearchClient::new(&server.uri(), None, timeout()).unwrap();
    let hits = client.search("products", "lamp", 10).await.unwrap();
    assert_eq!(hits, vec![("doc-1".to_string(), 0.9), ("doc-2".to_string(), 0.4)]);
}

#[tokio::test]
async fn opensearch_indexes_a_document_body_verbatim_with_no_id_field() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/products/_doc/doc-1"))
        .respond_with(|req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["name"], "Lamp");
            assert!(body.get("id").is_none());
            ResponseTemplate::new(201).set_body_json(serde_json::json!({"_id": "doc-1"}))
        })
        .mount(&server)
        .await;

    let client = OpenSearchClient::new(&server.uri(), None, timeout()).unwrap();
    let mut fields = HashMap::new();
    fields.insert("name".to_string(), "Lamp".to_string());
    client.index_document("products", "doc-1", &fields).await.unwrap();
}

#[tokio::test]
async fn opensearch_sends_basic_auth_when_credentials_are_given() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/products/_doc/doc-1"))
        .and(header("authorization", "Basic dXNlcjpwYXNz"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let client = OpenSearchClient::new(&server.uri(), Some(("user", "pass")), timeout()).unwrap();
    client
        .index_document("products", "doc-1", &HashMap::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn opensearch_delete_document_treats_404_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/products/_doc/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = OpenSearchClient::new(&server.uri(), None, timeout()).unwrap();
    client.delete_document("products", "missing").await.unwrap();
}

#[tokio::test]
async fn opensearch_create_index_treats_resource_already_exists_as_success() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/products"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"type": "resource_already_exists_exception"}
        })))
        .mount(&server)
        .await;

    let client = OpenSearchClient::new(&server.uri(), None, timeout()).unwrap();
    client.create_index("products", None).await.unwrap();
}

#[tokio::test]
async fn opensearch_create_index_propagates_a_different_400_error() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/products"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"type": "mapper_parsing_exception"}
        })))
        .mount(&server)
        .await;

    let client = OpenSearchClient::new(&server.uri(), None, timeout()).unwrap();
    assert!(client.create_index("products", None).await.is_err());
}

#[tokio::test]
async fn opensearch_search_returns_id_score_pairs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/products/_search"))
        .respond_with(|req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["query"]["multi_match"]["query"], "lamp");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "hits": {"hits": [{"_id": "doc-1", "_score": 1.5}, {"_id": "doc-2", "_score": 0.5}]}
            }))
        })
        .mount(&server)
        .await;

    let client = OpenSearchClient::new(&server.uri(), None, timeout()).unwrap();
    let hits = client.search("products", "lamp", None, 10).await.unwrap();
    assert_eq!(hits, vec![("doc-1".to_string(), 1.5), ("doc-2".to_string(), 0.5)]);
}
