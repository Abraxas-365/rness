//! Anthropic adapter tests against a wiremock SSE server: request shape,
//! stream accumulation (text + tool_use), chunk traceability, error
//! mapping, and cancellation.

use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::tools::ToolSpec;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_protocol::events::*;
use rness_providers::anthropic::AnthropicProvider;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(events: &[(&str, serde_json::Value)]) -> String {
    events
        .iter()
        .map(|(e, d)| format!("event: {e}\ndata: {d}\n\n"))
        .collect()
}

fn sse_response(events: &[(&str, serde_json::Value)]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse(events))
}

fn user_context(text: &str) -> ModelContext {
    ModelContext {
        turns: vec![ModelTurn::User {
            content: vec![ContentPart::Text { text: text.into() }],
        }],
        ..Default::default()
    }
}

fn stream_happy(text: &str) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("message_start", json!({"message": {"usage": {"input_tokens": 10}}})),
        ("content_block_start", json!({"content_block": {"type": "text"}})),
        ("content_block_delta", json!({"delta": {"type": "text_delta", "text": text}})),
        ("content_block_stop", json!({})),
        ("message_delta", json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}})),
        ("message_stop", json!({})),
    ]
}

async fn step(
    provider: &AnthropicProvider,
    context: &ModelContext,
    system: &str,
    tools: &[ToolSpec],
) -> StepOutcome {
    provider
        .step(
            StepRequest { context, system, tools, on_delta: None },
            &CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn clean_eof_without_terminal_event_is_failure_not_commit() {
    let server = MockServer::start().await;
    let mut events = stream_happy("partial");
    events.pop();
    Mock::given(method("POST")).respond_with(sse_response(&events)).mount(&server).await;
    let provider = AnthropicProvider::new("fake", "test").with_base_url(server.uri());
    let StepOutcome::Failed { error, partial } = step(&provider, &user_context("hi"), "", &[]).await else { panic!("truncated stream committed") };
    assert!(error.message.contains("message_stop"));
    assert!(error.retryable);
    assert!(!partial.is_empty());
}

#[tokio::test]
async fn happy_path_text_stream_commits_with_chunks_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_partial_json(json!({
            "model": "claude-test-1",
            "stream": true,
            "system": [{"type": "text", "text": "be brief"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        })))
        .respond_with(sse_response(&stream_happy("hello there")))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        AnthropicProvider::new("test-key", "claude-test-1").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("hi"), "be brief", &[]).await;

    let StepOutcome::Committed(msg) = outcome else {
        panic!("expected commit");
    };
    assert_eq!(msg.model, "claude-test-1");
    assert_eq!(msg.content, vec![ContentPart::Text { text: "hello there".into() }]);
    assert_eq!(msg.stop, StopReason::EndTurn);
    assert_eq!(msg.usage.input_tokens, 10);
    assert_eq!(msg.usage.output_tokens, 7);
    // The exact stream is embedded (traceability).
    assert_eq!(msg.chunks.len(), 1);
    assert!(matches!(&msg.chunks[0].delta, ChunkDelta::Text { t } if t == "hello there"));
}

#[tokio::test]
async fn tool_use_stream_assembles_args_and_advertises_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "tools": [{"name": "Read", "description": "read a file"}],
        })))
        .respond_with(sse_response(&[
            ("message_start", json!({"message": {"usage": {"input_tokens": 1}}})),
            ("content_block_start", json!({"content_block": {"type": "text"}})),
            ("content_block_delta", json!({"delta": {"type": "text_delta", "text": "reading"}})),
            ("content_block_stop", json!({})),
            (
                "content_block_start",
                json!({"content_block": {"type": "tool_use", "id": "call_1", "name": "Read"}}),
            ),
            (
                "content_block_delta",
                json!({"delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
            ),
            (
                "content_block_delta",
                json!({"delta": {"type": "input_json_delta", "partial_json": "\"a.txt\"}"}}),
            ),
            ("content_block_stop", json!({})),
            ("message_delta", json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 3}})),
            ("message_stop", json!({})),
        ]))
        .expect(1)
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("k", "claude-test-1").with_base_url(server.uri());
    let tools = vec![ToolSpec {
        name: "Read".into(),
        description: "read a file".into(),
        input_schema: json!({"type": "object"}),
    }];
    let outcome = step(&provider, &user_context("read a.txt"), "", &tools).await;

    let StepOutcome::Committed(msg) = outcome else {
        panic!("expected commit");
    };
    assert_eq!(msg.stop, StopReason::ToolUse);
    assert_eq!(msg.content.len(), 2);
    assert_eq!(
        msg.content[1],
        ContentPart::ToolUse {
            call: "call_1".into(),
            name: "Read".into(),
            args: json!({"path": "a.txt"}),
        }
    );
    // Tool-arg deltas recorded chunk by chunk.
    let arg_chunks = msg
        .chunks
        .iter()
        .filter(|c| matches!(c.delta, ChunkDelta::ToolArgs { .. }))
        .count();
    assert_eq!(arg_chunks, 2);
}

#[tokio::test]
async fn no_arg_tool_call_commits_empty_object_args() {
    // Tools like job_list take no args: the API streams NO
    // input_json_delta at all. Committed args must be {} — replaying
    // null input is rejected by the API ("Input should be an object").
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(&[
            ("message_start", json!({"message": {"usage": {"input_tokens": 1}}})),
            (
                "content_block_start",
                json!({"content_block": {"type": "tool_use", "id": "call_1", "name": "job_list"}}),
            ),
            ("content_block_stop", json!({})),
            ("message_delta", json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}})),
            ("message_stop", json!({})),
        ]))
        .expect(1)
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("k", "claude-test-1").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("list jobs"), "", &[]).await;

    let StepOutcome::Committed(msg) = outcome else {
        panic!("expected commit");
    };
    assert_eq!(
        msg.content[0],
        ContentPart::ToolUse { call: "call_1".into(), name: "job_list".into(), args: json!({}) }
    );
}

#[tokio::test]
async fn tool_results_map_to_tool_result_messages() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "go"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "using tool"},
                    {"type": "tool_use", "id": "c1", "name": "Echo", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "c1", "content": "out", "is_error": false},
                ]},
            ],
        })))
        .respond_with(sse_response(&stream_happy("done")))
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

    let provider = AnthropicProvider::new("k", "claude-test-1").with_base_url(server.uri());
    let outcome = step(&provider, &context, "", &[]).await;
    assert!(matches!(outcome, StepOutcome::Committed(_)));
}

#[tokio::test]
async fn http_429_is_retryable_with_api_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"type": "rate_limit_error", "message": "slow down"},
        })))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("hi"), "", &[]).await;
    let StepOutcome::Failed { error, partial } = outcome else {
        panic!("expected failure");
    };
    assert!(error.retryable);
    assert_eq!(error.message, "slow down");
    assert!(partial.is_empty());
}

#[tokio::test]
async fn http_400_is_fatal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"type": "invalid_request_error", "message": "bad request"},
        })))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("hi"), "", &[]).await;
    let StepOutcome::Failed { error, .. } = outcome else {
        panic!("expected failure");
    };
    assert!(!error.retryable);
    assert_eq!(error.message, "bad request");
}

#[tokio::test]
async fn stream_error_event_preserves_partial_chunks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(&[
            ("message_start", json!({"message": {"usage": {"input_tokens": 1}}})),
            ("content_block_start", json!({"content_block": {"type": "text"}})),
            ("content_block_delta", json!({"delta": {"type": "text_delta", "text": "partial "}})),
            ("error", json!({"error": {"type": "overloaded_error", "message": "overloaded"}})),
        ]))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("hi"), "", &[]).await;
    let StepOutcome::Failed { error, partial } = outcome else {
        panic!("expected failure");
    };
    assert_eq!(error.message, "overloaded");
    assert_eq!(partial.len(), 1);
    assert!(matches!(&partial[0].delta, ChunkDelta::Text { t } if t == "partial "));
}

#[tokio::test]
async fn pre_cancelled_token_cancels_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(&stream_happy("never")))
        .expect(0)
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("k", "m").with_base_url(server.uri());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = user_context("hi");
    let outcome = provider
        .step(StepRequest { context: &ctx, system: "", tools: &[], on_delta: None }, &cancel)
        .await;
    assert!(matches!(outcome, StepOutcome::Cancelled { .. }));
}

#[tokio::test]
async fn thinking_blocks_accumulate_and_replay_in_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(&[
            ("message_start", json!({"message": {"usage": {"input_tokens": 1}}})),
            ("content_block_start", json!({"content_block": {"type": "thinking"}})),
            ("content_block_delta", json!({"delta": {"type": "thinking_delta", "thinking": "hmm"}})),
            ("content_block_delta", json!({"delta": {"type": "signature_delta", "signature": "sig_abc123"}})),
            ("content_block_stop", json!({})),
            ("content_block_start", json!({"content_block": {"type": "text"}})),
            ("content_block_delta", json!({"delta": {"type": "text_delta", "text": "answer"}})),
            ("message_delta", json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}})),
            ("message_stop", json!({})),
        ]))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("think"), "", &[]).await;
    let StepOutcome::Committed(msg) = outcome else {
        panic!("expected commit");
    };
    assert_eq!(
        msg.content,
        vec![
            // Signature captured: replaying thinking without it is
            // rejected by the API ("thinking.signature: Field required").
            ContentPart::Thinking { text: "hmm".into(), signature: Some("sig_abc123".into()) },
            ContentPart::Text { text: "answer".into() },
        ]
    );
}

#[tokio::test]
async fn reasoning_budget_uses_explicit_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "thinking": {"type": "enabled", "budget_tokens": 8192},
        })))
        .respond_with(sse_response(&stream_happy("ok")))
        .expect(1)
        .mount(&server)
        .await;

    let mut context = user_context("hi");
    context.config = CallConfig { reasoning: Some(rness_protocol::events::Reasoning::BudgetTokens { tokens: 8192 }), max_output_tokens: Some(16384), ..Default::default() };
    let provider = AnthropicProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &context, "", &[]).await;
    assert!(matches!(outcome, StepOutcome::Committed(_)));
}

#[tokio::test]
async fn named_reasoning_sent_as_adaptive_thinking_with_effort() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
        })))
        .respond_with(sse_response(&stream_happy("ok")))
        .expect(1)
        .mount(&server)
        .await;

    let mut context = user_context("hi");
    context.config = CallConfig { reasoning: Some(rness_protocol::events::Reasoning::Effort { effort: "high".into() }), ..Default::default() };
    let provider = AnthropicProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &context, "", &[]).await;
    assert!(matches!(outcome, StepOutcome::Committed(_)));
}
