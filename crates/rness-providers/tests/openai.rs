//! OpenAI-compatible adapter tests against a wiremock SSE server: request
//! shape (messages/tools/tool results mapping), stream accumulation
//! (content + streamed tool_calls + reasoning), finish_reason mapping,
//! error mapping, and cancellation.

use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::tools::ToolSpec;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_protocol::events::*;
use rness_providers::openai::OpenAiProvider;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(chunks: &[serde_json::Value]) -> String {
    let mut out: String = chunks.iter().map(|d| format!("data: {d}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

fn sse_response(chunks: &[serde_json::Value]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse(chunks))
}

fn user_context(text: &str) -> ModelContext {
    ModelContext {
        turns: vec![ModelTurn::User {
            content: vec![ContentPart::Text { text: text.into() }],
        }],
        ..Default::default()
    }
}

fn stream_happy(text: &str) -> Vec<serde_json::Value> {
    vec![
        json!({"choices": [{"delta": {"role": "assistant", "content": text}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 7}}),
    ]
}

async fn step(
    provider: &OpenAiProvider,
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
async fn happy_path_text_stream_commits_with_chunks_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": "gpt-test-1",
            "stream": true,
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
            ],
        })))
        .respond_with(sse_response(&stream_happy("hello there")))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("test-key", "gpt-test-1").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("hi"), "be brief", &[]).await;

    let StepOutcome::Committed(msg) = outcome else {
        panic!("expected commit");
    };
    assert_eq!(msg.model, "gpt-test-1");
    assert_eq!(msg.content, vec![ContentPart::Text { text: "hello there".into() }]);
    assert_eq!(msg.stop, StopReason::EndTurn);
    assert_eq!(msg.usage.input_tokens, 10);
    assert_eq!(msg.usage.output_tokens, 7);
    assert_eq!(msg.chunks.len(), 1);
    assert!(matches!(&msg.chunks[0].delta, ChunkDelta::Text { t } if t == "hello there"));
}

#[tokio::test]
async fn tool_call_stream_assembles_args_and_advertises_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({
            "tools": [{
                "type": "function",
                "function": {"name": "Read", "description": "read a file"},
            }],
        })))
        .respond_with(sse_response(&[
            json!({"choices": [{"delta": {"role": "assistant", "content": "reading"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "Read", "arguments": "{\"path\":"}},
            ]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "\"a.txt\"}"}},
            ]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 3}}),
        ]))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("k", "gpt-test-1").with_base_url(server.uri());
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
    let arg_chunks = msg
        .chunks
        .iter()
        .filter(|c| matches!(c.delta, ChunkDelta::ToolArgs { .. }))
        .count();
    assert_eq!(arg_chunks, 2);
}

#[tokio::test]
async fn context_maps_to_tool_messages_and_assistant_tool_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": "using tool", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "Echo", "arguments": "{}"}},
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "out"},
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

    let provider = OpenAiProvider::new("k", "gpt-test-1").with_base_url(server.uri());
    let outcome = step(&provider, &context, "", &[]).await;
    assert!(matches!(outcome, StepOutcome::Committed(_)));
}

#[tokio::test]
async fn reasoning_content_maps_to_thinking() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&[
            json!({"choices": [{"delta": {"reasoning_content": "hmm"}}]}),
            json!({"choices": [{"delta": {"content": "answer"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ]))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("think"), "", &[]).await;
    let StepOutcome::Committed(msg) = outcome else {
        panic!("expected commit");
    };
    assert_eq!(
        msg.content,
        vec![
            ContentPart::Thinking { text: "hmm".into(), signature: None },
            ContentPart::Text { text: "answer".into() },
        ]
    );
}

#[tokio::test]
async fn extra_body_fields_merge_into_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"temperature": 0.0})))
        .respond_with(sse_response(&stream_happy("ok")))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("k", "m")
        .with_base_url(server.uri())
        .with_extra_body(json!({"temperature": 0.0}));
    let outcome = step(&provider, &user_context("hi"), "", &[]).await;
    assert!(matches!(outcome, StepOutcome::Committed(_)));
}

#[tokio::test]
async fn http_429_is_retryable_with_api_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"type": "rate_limit_exceeded", "message": "slow down"},
        })))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("k", "m").with_base_url(server.uri());
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
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"type": "invalid_request_error", "message": "bad request"},
        })))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("hi"), "", &[]).await;
    let StepOutcome::Failed { error, .. } = outcome else {
        panic!("expected failure");
    };
    assert!(!error.retryable);
    assert_eq!(error.message, "bad request");
}

#[tokio::test]
async fn stream_error_object_preserves_partial_chunks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&[
            json!({"choices": [{"delta": {"content": "partial "}}]}),
            json!({"error": {"type": "server_error", "message": "overloaded"}}),
        ]))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("hi"), "", &[]).await;
    let StepOutcome::Failed { error, partial } = outcome else {
        panic!("expected failure");
    };
    assert_eq!(error.message, "overloaded");
    assert_eq!(partial.len(), 1);
    assert!(matches!(&partial[0].delta, ChunkDelta::Text { t } if t == "partial "));
}

#[tokio::test]
async fn length_finish_reason_maps_to_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&[
            json!({"choices": [{"delta": {"content": "truncat"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "length"}]}),
        ]))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &user_context("hi"), "", &[]).await;
    let StepOutcome::Committed(msg) = outcome else {
        panic!("expected commit");
    };
    assert_eq!(msg.stop, StopReason::MaxTokens);
}

#[tokio::test]
async fn pre_cancelled_token_cancels_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&stream_happy("never")))
        .expect(0)
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("k", "m").with_base_url(server.uri());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = user_context("hi");
    let outcome = provider
        .step(StepRequest { context: &ctx, system: "", tools: &[], on_delta: None }, &cancel)
        .await;
    assert!(matches!(outcome, StepOutcome::Cancelled { .. }));
}

#[tokio::test]
async fn reasoning_config_sent_as_reasoning_effort() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"reasoning_effort": "high"})))
        .respond_with(sse_response(&stream_happy("ok")))
        .expect(1)
        .mount(&server)
        .await;

    let mut context = user_context("hi");
    context.config = CallConfig { reasoning: Some(rness_protocol::events::Reasoning::Effort { effort: "high".into() }), ..Default::default() };
    let provider = OpenAiProvider::new("k", "m").with_base_url(server.uri());
    let outcome = step(&provider, &context, "", &[]).await;
    assert!(matches!(outcome, StepOutcome::Committed(_)));
}
