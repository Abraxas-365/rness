//! GitHub Copilot: device-flow login against a mock GitHub, and the
//! composite provider's routing (`/models` → Messages / Responses / Chat)
//! with Copilot's headers on each wire shape.

use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_protocol::events::*;
use rness_providers::auth::github_copilot::{self, DeviceFlowClient, DevicePrompt, STORE_KEY};
use rness_providers::auth::{CredentialStore, Tokens};
use rness_providers::routes::{self, Kind, Route};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn sse(events: &[(&str, serde_json::Value)]) -> ResponseTemplate {
    let body: String = events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn chat_sse(chunks: &[serde_json::Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|d| format!("data: {d}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn store_with_token(dir: &TempDir) -> CredentialStore {
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    store
        .save_tokens(
            STORE_KEY,
            &Tokens {
                access_token: "gho_test".into(),
                ..Default::default()
            },
        )
        .unwrap();
    store
}

fn route(base: &str) -> Route {
    Route {
        kind: Kind::GitHubCopilot,
        base_url: Some(base.into()),
        credential: None,
        headers: Default::default(),
        stream_idle_timeout: Some(std::time::Duration::from_secs(5)),
        prompt_caching: false,
        cache_ttl: routes::CacheTtl::Auto,
        request_compression: Default::default(),
    }
}

fn user(text: &str) -> ModelTurn {
    ModelTurn::User {
        content: vec![ContentPart::Text { text: text.into() }],
    }
}

async fn step(provider: &dyn Provider, context: &ModelContext) -> StepOutcome {
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

async fn mount_models(server: &MockServer, models: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer gho_test"))
        .and(header_exists("x-github-api-version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": models })))
        .mount(server)
        .await;
}

fn model(id: &str, endpoints: &[&str]) -> serde_json::Value {
    json!({
        "id": id,
        "supported_endpoints": endpoints,
        "policy": {"state": "enabled"},
        "capabilities": {
            "limits": {"max_output_tokens": 64000, "max_prompt_tokens": 128000},
            "supports": {"tool_calls": true},
        },
    })
}

#[tokio::test]
async fn device_flow_stores_token_endpoint_and_plan() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .and(body_partial_json(json!({"scope": "read:user"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "verification_uri": "https://github.com/login/device",
            "user_code": "ABCD-1234",
            "device_code": "dev-1",
            "interval": 1,
            "expires_in": 60,
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_partial_json(json!({
            "device_code": "dev-1",
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "gho_new"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/copilot_internal/user"))
        .and(header("authorization", "Bearer gho_new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chat_enabled": true,
            "copilot_plan": "business",
            "endpoints": {"api": "https://api.business.githubcopilot.com/"},
        })))
        .expect(1)
        .mount(&server)
        .await;

    let shown = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = shown.clone();
    let prompt = DevicePrompt {
        on_code: Box::new(move |url, code| {
            *seen.lock().unwrap() = Some((url.to_string(), code.to_string()));
        }),
    };
    let dir = TempDir::new().unwrap();
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    let tokens = github_copilot::login_with(
        &DeviceFlowClient::with_base_url(&server.uri()),
        &store,
        &prompt,
        STORE_KEY,
    )
    .await
    .unwrap();
    assert_eq!(
        shown.lock().unwrap().clone().unwrap(),
        ("https://github.com/login/device".into(), "ABCD-1234".into())
    );
    assert_eq!(tokens.subscription_type, "business");
    let saved = store.tokens(STORE_KEY).unwrap().unwrap();
    assert_eq!(saved.access_token, "gho_new");
    assert_eq!(
        github_copilot::api_base(&saved),
        "https://api.business.githubcopilot.com"
    );
}

#[tokio::test]
async fn device_flow_refuses_accounts_without_copilot() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "verification_uri": "u", "user_code": "c", "device_code": "d", "interval": 1,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "gho_x"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/copilot_internal/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"chat_enabled": false})))
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    let prompt = DevicePrompt {
        on_code: Box::new(|_, _| {}),
    };
    let error = github_copilot::login_with(
        &DeviceFlowClient::with_base_url(&server.uri()),
        &store,
        &prompt,
        STORE_KEY,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("seat"), "{error}");
    assert!(store.tokens(STORE_KEY).unwrap().is_none());
}

#[tokio::test]
async fn claude_models_use_messages_with_copilot_headers() {
    let server = MockServer::start().await;
    mount_models(
        &server,
        json!([model(
            "claude-sonnet-4.5",
            &["/chat/completions", "/v1/messages"]
        )]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer gho_test"))
        .and(header("openai-intent", "conversation-edits"))
        .and(header("x-initiator", "user"))
        .and(header("anthropic-beta", "interleaved-thinking-2025-05-14"))
        .and(body_partial_json(
            json!({"model": "claude-sonnet-4.5", "max_tokens": 64000}),
        ))
        .respond_with(sse(&[
            (
                "message_start",
                json!({"message": {"usage": {"input_tokens": 3}}}),
            ),
            (
                "content_block_start",
                json!({"content_block": {"type": "text"}}),
            ),
            (
                "content_block_delta",
                json!({"delta": {"type": "text_delta", "text": "hola"}}),
            ),
            ("content_block_stop", json!({})),
            (
                "message_delta",
                json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
            ),
            ("message_stop", json!({})),
        ]))
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let provider = routes::build_with_oauth(
        &route(&server.uri()),
        "claude-sonnet-4.5",
        store_with_token(&dir),
        STORE_KEY.into(),
    )
    .unwrap();
    let context = ModelContext {
        turns: vec![user("hi")],
        ..Default::default()
    };
    let StepOutcome::Committed(msg) = step(provider.as_ref(), &context).await else {
        panic!("expected commit");
    };
    assert_eq!(
        msg.content,
        vec![ContentPart::Text {
            text: "hola".into()
        }]
    );
    // No API key or Claude Code attribution leaks into Copilot requests.
    let requests = server.received_requests().await.unwrap();
    let inference = requests
        .iter()
        .find(|r| r.url.path() == "/v1/messages")
        .unwrap();
    assert!(inference.headers.get("x-api-key").is_none());
    assert!(inference.headers.get("x-app").is_none());
}

#[tokio::test]
async fn gpt5_models_use_stateless_responses_and_tool_loops_are_agent() {
    let server = MockServer::start().await;
    mount_models(
        &server,
        json!([{
            "id": "gpt-5.2",
            "supported_endpoints": ["/responses"],
            "capabilities": {
                "limits": {"max_output_tokens": 128000, "max_prompt_tokens": 272000},
                "supports": {"tool_calls": true, "reasoning_effort": ["low", "medium", "high"]},
            },
        }]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer gho_test"))
        .and(header("x-initiator", "agent"))
        .and(body_partial_json(json!({
            "model": "gpt-5.2",
            "store": false,
            "include": ["reasoning.encrypted_content"],
        })))
        .respond_with(sse(&[
            // Copilot rotates item ids per event: deltas stay keyed by output_index.
            ("response.output_item.added", json!({"output_index": 0, "item": {"type": "function_call", "id": "a1", "call_id": "call_1", "name": "read"}})),
            ("response.function_call_arguments.delta", json!({"output_index": 0, "item_id": "a2", "delta": "{\"p\":"})),
            ("response.function_call_arguments.delta", json!({"output_index": 0, "item_id": "a3", "delta": "1}"})),
            ("response.output_item.done", json!({"output_index": 0, "item": {"type": "function_call", "id": "a4", "call_id": "call_1", "name": "read", "arguments": "{\"p\":1}"}})),
            ("response.completed", json!({"response": {"status": "completed", "output": [], "usage": {"input_tokens": 1, "output_tokens": 1}}})),
        ]))
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let provider = routes::build_with_oauth(
        &route(&server.uri()),
        "gpt-5.2",
        store_with_token(&dir),
        STORE_KEY.into(),
    )
    .unwrap();
    let context = ModelContext {
        turns: vec![
            user("look"),
            ModelTurn::Assistant {
                content: vec![ContentPart::ToolUse {
                    call: "call_0".into(),
                    name: "read".into(),
                    args: json!({}),
                }],
            },
            ModelTurn::ToolResults {
                results: vec![ToolResult {
                    call: "call_0".into(),
                    name: "read".into(),
                    content: vec![ToolResultContentPart::Text { text: "ok".into() }],
                    output: "ok".into(),
                    is_error: false,
                    duration_ms: 1,
                    tasks: None,
                    plan_review: None,
                    presentation: None,
                }],
            },
        ],
        ..Default::default()
    };
    let StepOutcome::Committed(msg) = step(provider.as_ref(), &context).await else {
        panic!("expected commit");
    };
    let args: Vec<_> = msg
        .chunks
        .iter()
        .filter_map(|c| match &c.delta {
            ChunkDelta::ToolArgs { call, .. } => Some(call.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        !args.is_empty() && args.iter().all(|c| c == "call_1"),
        "{args:?}"
    );
}

#[tokio::test]
async fn chat_models_replay_reasoning_opaque() {
    let server = MockServer::start().await;
    // /models unavailable: gemini falls back to Chat Completions by name.
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer gho_test"))
        .and(header("x-initiator", "user"))
        .respond_with(move |request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let assistant = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "assistant")
                .cloned();
            let replayed = assistant
                .is_some_and(|m| m["reasoning_opaque"] == "op-2" && m["reasoning_text"] == "think");
            chat_sse(&[
                json!({"choices": [{"delta": {"reasoning_text": "think"}}]}),
                json!({"choices": [{"delta": {"reasoning_opaque": "op-1"}}]}),
                json!({"choices": [{"delta": {"reasoning_opaque": "op-2"}}]}),
                json!({"choices": [{"delta": {"content": if replayed { "replayed" } else { "first" }}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
            ])
        })
        .expect(2)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let provider = routes::build_with_oauth(
        &route(&server.uri()),
        "gemini-2.5-pro",
        store_with_token(&dir),
        STORE_KEY.into(),
    )
    .unwrap();
    let mut context = ModelContext {
        turns: vec![user("one")],
        ..Default::default()
    };
    let StepOutcome::Committed(first) = step(provider.as_ref(), &context).await else {
        panic!("expected commit");
    };
    assert!(first.content.contains(&ContentPart::Thinking {
        text: "think".into(),
        signature: Some("copilot-opaque:op-2".into()),
    }));
    context.turns.push(ModelTurn::Assistant {
        content: first.content,
    });
    context.turns.push(user("two"));
    let StepOutcome::Committed(second) = step(provider.as_ref(), &context).await else {
        panic!("expected commit");
    };
    assert!(second.content.contains(&ContentPart::Text {
        text: "replayed".into()
    }));
}

#[tokio::test]
async fn disabled_models_fail_with_the_reason() {
    let server = MockServer::start().await;
    let mut disabled = model("claude-opus-4.5", &["/v1/messages"]);
    disabled["policy"]["state"] = json!("disabled");
    mount_models(&server, json!([disabled])).await;
    let dir = TempDir::new().unwrap();
    let provider = routes::build_with_oauth(
        &route(&server.uri()),
        "claude-opus-4.5",
        store_with_token(&dir),
        STORE_KEY.into(),
    )
    .unwrap();
    let context = ModelContext {
        turns: vec![user("hi")],
        ..Default::default()
    };
    let StepOutcome::Failed { error, .. } = step(provider.as_ref(), &context).await else {
        panic!("expected failure");
    };
    assert!(error.message.contains("disabled"), "{}", error.message);
    assert!(!error.retryable);
}

#[tokio::test]
async fn rejected_token_points_to_login_and_relogin_is_picked_up() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer gho_test"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    // The second mount only matches the re-logged-in token.
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer gho_new"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [model("gpt-4.1", &["/chat/completions"])]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer gho_new"))
        .respond_with(chat_sse(&[
            json!({"choices": [{"delta": {"content": "ok"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ]))
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let store = store_with_token(&dir);
    let provider = routes::build_with_oauth(
        &route(&server.uri()),
        "gpt-4.1",
        store.clone(),
        STORE_KEY.into(),
    )
    .unwrap();
    let context = ModelContext {
        turns: vec![user("hi")],
        ..Default::default()
    };
    let StepOutcome::Failed { error, .. } = step(provider.as_ref(), &context).await else {
        panic!("expected failure");
    };
    assert!(
        error
            .message
            .contains("auth login --provider github-copilot"),
        "{}",
        error.message
    );
    store
        .save_tokens(
            STORE_KEY,
            &Tokens {
                access_token: "gho_new".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(matches!(
        step(provider.as_ref(), &context).await,
        StepOutcome::Committed(_)
    ));
}

#[tokio::test]
async fn missing_login_points_to_the_login_command() {
    let dir = TempDir::new().unwrap();
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    let provider = routes::build_with_oauth(
        &route("http://127.0.0.1:9"),
        "gpt-4.1",
        store,
        STORE_KEY.into(),
    )
    .unwrap();
    let context = ModelContext {
        turns: vec![user("hi")],
        ..Default::default()
    };
    let StepOutcome::Failed { error, .. } = step(provider.as_ref(), &context).await else {
        panic!("expected failure");
    };
    assert!(
        error
            .message
            .contains("rness auth login --provider github-copilot"),
        "{}",
        error.message
    );
}
