//! ChatGPT-backend Responses adapter tests: request shape (headers,
//! store:false, instructions, input items), output_item.done
//! accumulation, empty-output backfill, tool-call round trip,
//! 401 → refresh → retry, and failure mapping.

use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_protocol::events::*;
use rness_providers::auth::openai::{CodexCredentialSource, CodexOAuthClient, STORE_KEY};
use rness_providers::auth::{CredentialStore, Tokens};
use rness_providers::responses::ResponsesProvider;
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(events: &[(&str, serde_json::Value)]) -> ResponseTemplate {
    let body: String = events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect();
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// A store holding valid (non-expired) ChatGPT tokens.
fn store_with_tokens(dir: &TempDir, access: &str) -> CredentialStore {
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    let mut tokens = Tokens {
        access_token: access.into(),
        refresh_token: "rt-1".into(),
        expires_at: Some((jiff::Timestamp::now() + jiff::SignedDuration::from_secs(3600)).to_string()),
        ..Default::default()
    };
    tokens.extra.insert("accountId".into(), json!("acct-42"));
    store.save_tokens(STORE_KEY, &tokens).unwrap();
    store
}

fn provider_for(server: &MockServer, store: CredentialStore) -> ResponsesProvider {
    let source = CodexCredentialSource::new(store)
        .with_oauth_client(CodexOAuthClient::with_base_url(&server.uri()));
    ResponsesProvider::new(source, "gpt-5.5").with_base_url(server.uri())
}

fn user_context(text: &str) -> ModelContext {
    ModelContext {
        turns: vec![ModelTurn::User {
            content: vec![ContentPart::Text { text: text.into() }],
        }],
        ..Default::default()
    }
}

async fn step(provider: &ResponsesProvider, context: &ModelContext, system: &str) -> StepOutcome {
    provider
        .step(
            StepRequest { context, system, tools: &[], on_delta: None },
            &CancellationToken::new(),
        )
        .await
}

fn happy_events() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("response.output_text.delta", json!({"item_id": "msg_1", "delta": "hello"})),
        (
            "response.output_item.done",
            json!({"item": {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "hello"},
            ]}}),
        ),
        (
            "response.completed",
            json!({"response": {"status": "completed", "output": [],
                "usage": {"input_tokens": 12, "output_tokens": 5}}}),
        ),
    ]
}

#[tokio::test]
async fn sends_bearer_account_header_and_codex_body_shape() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(header("authorization", "Bearer at-1"))
        .and(header("chatgpt-account-id", "acct-42"))
        .and(header("originator", "rness"))
        .and(header("openai-beta", "responses=experimental"))
        .and(body_partial_json(json!({
            "model": "gpt-5.5",
            "store": false,
            "stream": true,
            "instructions": "be brief",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hi"},
                ]},
            ],
        })))
        .respond_with(sse(&happy_events()))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server, store_with_tokens(&dir, "at-1"));
    let outcome = step(&provider, &user_context("hi"), "be brief").await;

    let StepOutcome::Committed(msg) = outcome else {
        panic!("expected commit");
    };
    assert_eq!(msg.content, vec![ContentPart::Text { text: "hello".into() }]);
    assert_eq!(msg.stop, StopReason::EndTurn);
    assert_eq!(msg.usage.input_tokens, 12);
    assert_eq!(msg.usage.output_tokens, 5);
    assert!(matches!(&msg.chunks[0].delta, ChunkDelta::Text { t } if t == "hello"));
}

#[tokio::test]
async fn empty_completed_output_backfills_from_done_items() {
    // response.completed with output: [] must not wipe accumulated items —
    // that's the documented backend quirk.
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(sse(&[
            (
                "response.output_item.done",
                json!({"item": {"type": "function_call", "call_id": "c9",
                    "name": "Read", "arguments": "{\"path\":\"x\"}"}}),
            ),
            ("response.completed", json!({"response": {"status": "completed", "output": []}})),
        ]))
        .mount(&server)
        .await;

    let provider = provider_for(&server, store_with_tokens(&dir, "at-1"));
    let StepOutcome::Committed(msg) = step(&provider, &user_context("go"), "").await else {
        panic!("expected commit");
    };
    assert_eq!(msg.stop, StopReason::ToolUse);
    assert_eq!(
        msg.content,
        vec![ContentPart::ToolUse {
            call: "c9".into(),
            name: "Read".into(),
            args: json!({"path": "x"}),
        }]
    );
}

#[tokio::test]
async fn history_replays_as_function_call_items() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(body_partial_json(json!({
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "go"},
                ]},
                {"type": "message", "role": "assistant", "status": "completed", "content": [
                    {"type": "output_text", "text": "using tool"},
                ]},
                {"type": "function_call", "call_id": "c1", "name": "Echo", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "out"},
            ],
        })))
        .respond_with(sse(&happy_events()))
        .expect(1)
        .mount(&server)
        .await;

    let context = ModelContext {
        turns: vec![
            ModelTurn::User { content: vec![ContentPart::Text { text: "go".into() }] },
            ModelTurn::Assistant {
                content: vec![
                    ContentPart::Text { text: "using tool".into() },
                    ContentPart::ToolUse { call: "c1".into(), name: "Echo".into(), args: json!({}) },
                ],
            },
            ModelTurn::ToolResults {
                results: vec![ToolResult {
                    call: "c1".into(),
                    name: "Echo".into(),
                    output: "out".into(),
                    is_error: false,
                    duration_ms: 1,
                }],
            },
        ],
        ..Default::default()
    };

    let provider = provider_for(&server, store_with_tokens(&dir, "at-1"));
    assert!(matches!(step(&provider, &context, "").await, StepOutcome::Committed(_)));
}

#[tokio::test]
async fn unauthorized_refreshes_once_and_retries() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;

    // Expired-at-server access token → 401 once.
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(header("authorization", "Bearer at-stale"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    // Refresh endpoint issues a fresh token.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-fresh",
            "refresh_token": "rt-2",
            "expires_in": 3600,
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Retry with the fresh token succeeds.
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(header("authorization", "Bearer at-fresh"))
        .respond_with(sse(&happy_events()))
        .expect(1)
        .mount(&server)
        .await;

    let store = store_with_tokens(&dir, "at-stale");
    let provider = provider_for(&server, store);
    let outcome = step(&provider, &user_context("hi"), "").await;
    let StepOutcome::Committed(_) = outcome else {
        match outcome {
            StepOutcome::Failed { error, .. } => panic!("failed: {}", error.message),
            _ => panic!("expected commit"),
        }
    };

    // Refreshed tokens persisted, accountId preserved from old extra.
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    let tokens = store.tokens(STORE_KEY).unwrap().unwrap();
    assert_eq!(tokens.access_token, "at-fresh");
    assert_eq!(tokens.extra["accountId"], "acct-42");
}

#[tokio::test]
async fn no_credentials_is_fatal_with_login_hint() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    let provider = provider_for(&server, store);

    let StepOutcome::Failed { error, .. } = step(&provider, &user_context("hi"), "").await else {
        panic!("expected failure");
    };
    assert!(!error.retryable);
    assert!(error.message.contains("auth login"), "{}", error.message);
}

#[tokio::test]
async fn response_failed_event_maps_to_retryable_failure() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(sse(&[
            ("response.output_text.delta", json!({"item_id": "m", "delta": "part"})),
            (
                "response.failed",
                json!({"response": {"status": "failed",
                    "error": {"code": "rate_limit_exceeded", "message": "usage limit reached"}}}),
            ),
        ]))
        .mount(&server)
        .await;

    let provider = provider_for(&server, store_with_tokens(&dir, "at-1"));
    let StepOutcome::Failed { error, partial } = step(&provider, &user_context("hi"), "").await
    else {
        panic!("expected failure");
    };
    assert_eq!(error.message, "usage limit reached");
    assert_eq!(partial.len(), 1);
}

#[tokio::test]
async fn reasoning_summary_becomes_thinking() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(sse(&[
            ("response.reasoning_summary_text.delta", json!({"delta": "pondering"})),
            (
                "response.output_item.done",
                json!({"item": {"type": "reasoning", "summary": [
                    {"type": "summary_text", "text": "pondering"},
                ]}}),
            ),
            (
                "response.output_item.done",
                json!({"item": {"type": "message", "content": [
                    {"type": "output_text", "text": "answer"},
                ]}}),
            ),
            ("response.completed", json!({"response": {"status": "completed"}})),
        ]))
        .mount(&server)
        .await;

    let provider = provider_for(&server, store_with_tokens(&dir, "at-1"));
    let StepOutcome::Committed(msg) = step(&provider, &user_context("think"), "").await else {
        panic!("expected commit");
    };
    assert_eq!(
        msg.content,
        vec![
            ContentPart::Thinking { text: "pondering".into(), signature: None },
            ContentPart::Text { text: "answer".into() },
        ]
    );
    assert!(matches!(&msg.chunks[0].delta, ChunkDelta::Thinking { t } if t == "pondering"));
}
