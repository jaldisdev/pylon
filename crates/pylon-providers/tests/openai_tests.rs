use pylon_providers::{Message, OpenAiProvider};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn embeds_a_batch_in_declared_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"index": 0, "embedding": [0.1, 0.2]},
                {"index": 1, "embedding": [0.3, 0.4]},
            ]
        })))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(&server.uri(), "text-embedding-3-small", Some("sk-test")).unwrap();
    let out = provider.embed_batch(&["a".to_string(), "b".to_string()]).await.unwrap();

    assert_eq!(out, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
}

#[tokio::test]
async fn re_sorts_an_out_of_order_response_by_index() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"index": 1, "embedding": [0.3, 0.4]},
                {"index": 0, "embedding": [0.1, 0.2]},
            ]
        })))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(&server.uri(), "m", None).unwrap();
    let out = provider.embed_batch(&["a".to_string(), "b".to_string()]).await.unwrap();

    assert_eq!(out, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
}

#[tokio::test]
async fn chunks_large_batches_at_max_batch_size() {
    let server = MockServer::start().await;
    // MAX_BATCH is 2048 — 2049 inputs must produce two separate POSTs.
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(|req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let n = body["input"].as_array().unwrap().len();
            let data: Vec<_> = (0..n).map(|i| serde_json::json!({"index": i, "embedding": [i as f64]})).collect();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": data}))
        })
        .expect(2)
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(&server.uri(), "m", None).unwrap();
    let texts: Vec<String> = (0..2049).map(|i| i.to_string()).collect();
    let out = provider.embed_batch(&texts).await.unwrap();

    assert_eq!(out.len(), 2049);
}

#[tokio::test]
async fn propagates_a_non_2xx_response_as_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(&server.uri(), "m", None).unwrap();
    let err = provider.embed_batch(&["a".to_string()]).await.unwrap_err();

    assert!(matches!(err, pylon_providers::Error::Http(_)));
}

#[tokio::test]
async fn chat_posts_model_and_messages_and_extracts_the_first_choice() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "hello there"}}]
        })))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(&server.uri(), "gpt", None).unwrap();
    let reply = provider
        .chat(&[Message { role: "user".into(), content: "hi".into() }])
        .await
        .unwrap();

    assert_eq!(reply, "hello there");
}

#[tokio::test]
async fn omits_the_authorization_header_when_no_api_key_is_given() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(|req: &wiremock::Request| {
            assert!(!req.headers.contains_key("authorization"));
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []}))
        })
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(&server.uri(), "m", None).unwrap();
    provider.embed_batch(&["a".to_string()]).await.unwrap();
}
