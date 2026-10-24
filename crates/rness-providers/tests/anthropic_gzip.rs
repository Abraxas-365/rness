//! Anthropic request-body compression: large bodies go out gzip-compressed
//! (and decode to the same JSON), small bodies and opted-out routes stay
//! plain, and an endpoint that rejects gzip gets the request resent plain
//! once, after which the provider stops compressing.

use std::io::Read;

use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_protocol::events::ContentPart;
use rness_providers::anthropic::AnthropicProvider;
use rness_providers::routes::RequestCompression;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn ok_stream() -> ResponseTemplate {
    let events = [
        (
            "message_start",
            json!({"message":{"usage":{"input_tokens":1}}}),
        ),
        (
            "content_block_start",
            json!({"content_block":{"type":"text"}}),
        ),
        (
            "content_block_delta",
            json!({"delta":{"type":"text_delta","text":"OK"}}),
        ),
        ("content_block_stop", json!({})),
        (
            "message_delta",
            json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        ),
        ("message_stop", json!({})),
    ];
    let body: String = events
        .iter()
        .map(|(e, d)| format!("event: {e}\ndata: {d}\n\n"))
        .collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn context(text: &str) -> ModelContext {
    ModelContext {
        turns: vec![ModelTurn::User {
            content: vec![ContentPart::Text { text: text.into() }],
        }],
        ..Default::default()
    }
}

/// Repetitive, transcript-like text well above the compression threshold.
fn large_text() -> String {
    (0..2000)
        .map(|i| format!("line {i}: cargo test -p rness-providers passed\n"))
        .collect()
}

async fn step(provider: &AnthropicProvider, context: &ModelContext) -> StepOutcome {
    provider
        .step(
            StepRequest {
                context,
                system: "sys",
                tools: &[],
                on_delta: None,
            },
            &CancellationToken::new(),
        )
        .await
}

fn encoding(request: &Request) -> Option<String> {
    request
        .headers
        .get("content-encoding")
        .map(|v| v.to_str().unwrap().to_owned())
}

/// The request body as JSON, decoding gzip when the request declares it.
fn decoded(request: &Request) -> Value {
    let bytes = if encoding(request).as_deref() == Some("gzip") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&request.body[..])
            .read_to_end(&mut out)
            .expect("body declared gzip must be valid gzip");
        out
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).expect("request body must be JSON")
}

fn user_text(body: &Value) -> &str {
    body["messages"][0]["content"][0]["text"].as_str().unwrap()
}

#[tokio::test]
async fn large_bodies_are_gzipped_and_decode_to_the_same_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ok_stream())
        .expect(2)
        .mount(&server)
        .await;
    let text = large_text();
    let plain = AnthropicProvider::new("key", "m")
        .with_base_url(server.uri())
        .with_request_compression(RequestCompression::Off);
    let gzip = AnthropicProvider::new("key", "m")
        .with_base_url(server.uri())
        .with_request_compression(RequestCompression::Gzip);
    assert!(matches!(
        step(&plain, &context(&text)).await,
        StepOutcome::Committed(_)
    ));
    assert!(matches!(
        step(&gzip, &context(&text)).await,
        StepOutcome::Committed(_)
    ));

    let requests = server.received_requests().await.unwrap();
    let (plain_req, gzip_req) = (&requests[0], &requests[1]);
    assert_eq!(encoding(plain_req), None);
    assert_eq!(encoding(gzip_req).as_deref(), Some("gzip"));
    assert_eq!(
        gzip_req.headers["content-type"].to_str().unwrap(),
        "application/json"
    );
    assert!(
        gzip_req.body.len() * 4 < plain_req.body.len(),
        "gzip {} B vs plain {} B",
        gzip_req.body.len(),
        plain_req.body.len()
    );
    // The server sees exactly the request it would have seen uncompressed.
    assert_eq!(decoded(gzip_req), decoded(plain_req));
    assert_eq!(user_text(&decoded(gzip_req)), text);
}

#[tokio::test]
async fn small_bodies_stay_plain() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ok_stream())
        .expect(1)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new("key", "m")
        .with_base_url(server.uri())
        .with_request_compression(RequestCompression::Gzip);
    assert!(matches!(
        step(&provider, &context("hi")).await,
        StepOutcome::Committed(_)
    ));
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(encoding(request), None);
    assert_eq!(user_text(&decoded(request)), "hi");
}

#[tokio::test]
async fn auto_does_not_compress_toward_other_endpoints() {
    // Auto only targets https://api.anthropic.com; a gateway or local mock
    // may not decode request bodies.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ok_stream())
        .expect(1)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new("key", "m").with_base_url(server.uri());
    assert!(matches!(
        step(&provider, &context(&large_text())).await,
        StepOutcome::Committed(_)
    ));
    assert_eq!(
        encoding(&server.received_requests().await.unwrap()[0]),
        None
    );
}

#[tokio::test]
async fn rejected_gzip_is_resent_plain_and_never_compressed_again() {
    let server = MockServer::start().await;
    // What api.anthropic.com answers when gzip bytes are read as JSON.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("content-encoding", "gzip"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type":"error",
            "error":{"type":"invalid_request_error",
                     "message":"The request body is not valid JSON: EOF while parsing a value at line 1 column 0"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ok_stream())
        .expect(2)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new("key", "m")
        .with_base_url(server.uri())
        .with_request_compression(RequestCompression::Gzip);
    let text = large_text();
    assert!(matches!(
        step(&provider, &context(&text)).await,
        StepOutcome::Committed(_)
    ));
    assert!(matches!(
        step(&provider, &context(&text)).await,
        StepOutcome::Committed(_)
    ));

    let requests = server.received_requests().await.unwrap();
    let encodings: Vec<_> = requests.iter().map(encoding).collect();
    assert_eq!(encodings, [Some("gzip".to_owned()), None, None]);
    for request in &requests {
        assert_eq!(user_text(&decoded(request)), text);
    }
}

#[tokio::test]
async fn unsupported_media_type_also_falls_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("content-encoding", "gzip"))
        .respond_with(ResponseTemplate::new(415))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ok_stream())
        .expect(1)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new("key", "m")
        .with_base_url(server.uri())
        .with_request_compression(RequestCompression::Gzip);
    assert!(matches!(
        step(&provider, &context(&large_text())).await,
        StepOutcome::Committed(_)
    ));
}

#[tokio::test]
async fn unrelated_400_is_not_retried_uncompressed() {
    // A genuine request error must surface once, not trigger a resend.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "type":"error",
            "error":{"type":"invalid_request_error","message":"max_tokens: must be at most 64000"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new("key", "m")
        .with_base_url(server.uri())
        .with_request_compression(RequestCompression::Gzip);
    let StepOutcome::Failed { error, .. } = step(&provider, &context(&large_text())).await else {
        panic!("expected failure");
    };
    assert!(!error.retryable);
    assert!(error.message.contains("max_tokens"), "{}", error.message);
}
