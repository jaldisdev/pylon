use pylon_providers::{AnthropicProvider, Message};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn sends_x_api_key_and_anthropic_version_headers_not_authorization() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(|req: &wiremock::Request| {
            assert!(!req.headers.contains_key("authorization"));
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"content": [{"type": "text", "text": "hi"}]}))
        })
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(&server.uri(), "claude", Some("sk-ant-test")).unwrap();
    let reply = provider
        .chat(&[Message { role: "user".into(), content: "hello".into() }])
        .await
        .unwrap();

    assert_eq!(reply, "hi");
}

#[tokio::test]
async fn splits_the_system_role_message_into_a_top_level_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(|req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["system"], "be nice");
            assert_eq!(body["max_tokens"], 1024);
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["role"], "user");
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"content": [{"type": "text", "text": "ok"}]}))
        })
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(&server.uri(), "claude", None).unwrap();
    provider
        .chat(&[
            Message { role: "system".into(), content: "be nice".into() },
            Message { role: "user".into(), content: "hello".into() },
        ])
        .await
        .unwrap();
}

#[tokio::test]
async fn joins_multiple_text_content_blocks_and_skips_non_text_blocks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "tool_use", "text": ""},
                {"type": "text", "text": "world"},
            ]
        })))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(&server.uri(), "claude", None).unwrap();
    let reply = provider
        .chat(&[Message { role: "user".into(), content: "hi".into() }])
        .await
        .unwrap();

    assert_eq!(reply, "hello world");
}

#[tokio::test]
async fn propagates_a_non_2xx_response_as_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(&server.uri(), "claude", None).unwrap();
    let err = provider
        .chat(&[Message { role: "user".into(), content: "hi".into() }])
        .await
        .unwrap_err();

    assert!(matches!(err, pylon_providers::Error::Http(_)));
}
