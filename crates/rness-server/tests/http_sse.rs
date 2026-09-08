//! The HTTP/SSE surface against a real engine with a scripted provider:
//! create, send (turn runs), history reconciliation, fork, and frames
//! over SSE — the same lifecycle a remote frontend would drive.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::service::{FrameEv, SessionService, SessionsPlugin};
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::Kernel;
use rness_protocol::events::*;
use rness_server::{router, ServerState};
use tokio_util::sync::CancellationToken;

struct OneAnswer;

#[async_trait]
impl Provider for OneAnswer {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _r: StepRequest<'_>, _c: &CancellationToken) -> StepOutcome {
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content: vec![ContentPart::Text { text: "respuesta del servidor".into() }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            chunks: vec![],
        })
    }
}

/// First step asks for the sensitive tool, second step ends the turn —
/// the minimal shape that exercises the approval pause.
struct ToolThenDone {
    asked: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Provider for ToolThenDone {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _r: StepRequest<'_>, _c: &CancellationToken) -> StepOutcome {
        let first = !self.asked.swap(true, std::sync::atomic::Ordering::SeqCst);
        let (content, stop) = if first {
            (
                vec![ContentPart::ToolUse {
                    call: "call-1".into(),
                    name: "Danger".into(),
                    args: serde_json::json!({"cmd": "rm -rf /"}),
                }],
                StopReason::ToolUse,
            )
        } else {
            (vec![ContentPart::Text { text: "hecho".into() }], StopReason::EndTurn)
        };
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content,
            stop,
            usage: Usage::default(),
            chunks: vec![],
        })
    }
}

/// A sensitive tool that records whether it actually ran.
struct Danger {
    ran: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl rness_engine::tools::Tool for Danger {
    fn name(&self) -> &str {
        "Danger"
    }
    fn sensitive(&self) -> bool {
        true
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<String, String> {
        self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok("boom contained".into())
    }
}

/// Engine + server on an ephemeral port. Returns the base URL.
async fn serve(dir: &std::path::Path) -> (String, Arc<SessionService>, Kernel) {
    serve_with(dir, Arc::new(OneAnswer), Arc::new(ToolRegistry::default())).await
}

/// Same, but with a caller-chosen provider/tools — approval tests mount
/// a sensitive tool and flip the policy to `ask`.
async fn serve_with(
    dir: &std::path::Path,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
) -> (String, Arc<SessionService>, Kernel) {
    let mut kernel = Kernel::new();
    kernel
        .mount(SessionsPlugin {
            root: dir.to_path_buf(),
            provider,
            resolver: None,
            creation_seed: Default::default(),
            agents: [("planner".into(), rness_engine::config::AgentDefinition {
                subagent: false, description: "Planner".into(), instructions: "PLAN_MARKER".into(), profile: None, tools: Some(vec![]),
            })].into(),
            models: Default::default(),
            tools: Arc::clone(&tools),
            config: TurnConfig::default(),
        })
        .unwrap();
    let sessions = kernel.services().get::<SessionService>("sessions").unwrap();

    let state = ServerState::new(Arc::clone(&sessions));
    let _sub = Box::leak(Box::new(kernel.bus().on::<FrameEv>(state.frame_sink())));
    // Mirror the CLI composition root: the server's remote answerer is
    // the approver whenever the seam is set to `ask`.
    tools.approvals().set_answerer(Arc::clone(&state.approvals) as _);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    (format!("http://{addr}"), sessions, kernel)
}

async fn get_json(url: &str) -> serde_json::Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
}

async fn post_json(url: &str, body: serde_json::Value) -> serde_json::Value {
    let resp = reqwest::Client::new().post(url).json(&body).send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    serde_json::from_str(&text)
        .unwrap_or_else(|_| panic!("POST {url} -> {status}: {text}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn slash_agent_uses_service_without_starting_model() {
    let dir = tempfile::tempdir().unwrap();
    let (base, sessions, _kernel) = serve(dir.path()).await;
    let created = post_json(&format!("{base}/api/sessions"), serde_json::json!({})).await;
    let id = created["session"].as_str().unwrap();
    let response = post_json(&format!("{base}/api/request"), serde_json::json!({
        "type":"send", "session":id, "intent":"followup", "content":[{"kind":"text","text":"/agent planner"}]
    })).await;
    assert_ne!(response["status"], "started");
    let history = sessions.store().history(&id.to_string()).unwrap();
    assert!(history.iter().any(|e| matches!(&e.event, SessionEvent::RequestConfig(c) if c.agent.as_ref().is_some_and(|a| a.name == "planner"))));
    assert!(!history.iter().any(|e| matches!(e.event, SessionEvent::UserMessage(_) | SessionEvent::AssistantMessage(_))));
    for text in ["/agent", "/agent missing", "/agent planner extra"] {
        let response = reqwest::Client::new().post(format!("{base}/api/request")).json(&serde_json::json!({
            "type":"send", "session":id, "intent":"followup", "content":[{"kind":"text","text":text}]
        })).send().await.unwrap();
        assert!(!response.status().is_success());
    }
    assert_eq!(sessions.store().history(&id.to_string()).unwrap().len(), history.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_send_uses_shared_resolver_and_rejects_before_logging() {
    let dir = tempfile::tempdir().unwrap();
    let (base, sessions, _kernel) = serve(dir.path()).await;
    sessions.set_input_resolver(Arc::new(|mut content| {
        if matches!(content.first(), Some(ContentPart::Text { text }) if text == "/skill missing") {
            return Err("unknown skill".into());
        }
        content.push(ContentPart::Text { text: "resolved instructions".into() });
        Ok(content)
    }));
    let created = post_json(&format!("{base}/api/sessions"), serde_json::json!({})).await;
    let id = created["session"].as_str().unwrap();
    let before = sessions.store().history(&id.to_string()).unwrap().len();
    let response = reqwest::Client::new().post(format!("{base}/api/request")).json(&serde_json::json!({
        "type":"send", "session":id, "intent":"followup", "content":[{"kind":"text","text":"/skill missing"}]
    })).send().await.unwrap();
    assert!(!response.status().is_success());
    assert_eq!(sessions.store().history(&id.to_string()).unwrap().len(), before);
    let sent = post_json(&format!("{base}/api/request"), serde_json::json!({
        "type":"send", "session":id, "intent":"followup", "content":[{"kind":"text","text":"/review input"}]
    })).await;
    assert_eq!(sent["status"], "started");
    let history = get_json(&format!("{base}/api/sessions/{id}")).await;
    assert!(history.to_string().contains("resolved instructions"));
    assert!(history.to_string().contains("/review input"));
}

#[tokio::test(flavor = "multi_thread")]
async fn full_remote_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _sessions, _kernel) = serve(dir.path()).await;

    // Create.
    let created = post_json(&format!("{base}/api/sessions"), serde_json::json!({})).await;
    let id = created["session"].as_str().unwrap().to_string();

    // Listed.
    let list = get_json(&format!("{base}/api/sessions")).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Send a ClientRequest — the SAME shape the TUI uses.
    let sent = post_json(
        &format!("{base}/api/request"),
        serde_json::json!({
            "type": "send",
            "session": id,
            "intent": "followup",
            "content": [{ "kind": "text", "text": "hola servidor" }],
        }),
    )
    .await;
    assert_eq!(sent["status"], "started");

    // Poll phase to idle.
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let phase = get_json(&format!("{base}/api/sessions/{id}/phase")).await;
        if phase["phase"] == "idle" {
            break;
        }
    }

    // History has header + user + assistant + turn markers.
    let history = get_json(&format!("{base}/api/sessions/{id}")).await;
    let envelopes = history["envelopes"].as_array().unwrap();
    assert!(envelopes.iter().any(|e| e["type"] == "assistant/message" || e["type"] == "message/assistant"));

    // Fork through the API.
    let forked = post_json(&format!("{base}/api/sessions/{id}/fork"), serde_json::json!({})).await;
    assert_ne!(forked["session"], id);
    let list = get_json(&format!("{base}/api/sessions")).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_stream_over_sse() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _sessions, _kernel) = serve(dir.path()).await;

    let created = post_json(&format!("{base}/api/sessions"), serde_json::json!({})).await;
    let id = created["session"].as_str().unwrap().to_string();

    // Attach SSE FIRST, then trigger the turn.
    let response = reqwest::get(format!("{base}/api/events/{id}")).await.unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");

    post_json(
        &format!("{base}/api/request"),
        serde_json::json!({
            "type": "send",
            "session": id,
            "intent": "followup",
            "content": [{ "kind": "text", "text": "stream me" }],
        }),
    )
    .await;

    // Read the raw SSE body until TurnIdle (bounded).
    let mut body = String::new();
    let mut stream = response;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let chunk = tokio::time::timeout_at(deadline, stream.chunk()).await;
        let Ok(Ok(Some(chunk))) = chunk else { break };
        body.push_str(&String::from_utf8_lossy(&chunk));
        if body.contains("turn_idle") {
            break;
        }
    }
    assert!(body.contains("step_started"), "{body}");
    assert!(body.contains("step_committed"), "{body}");
    assert!(body.contains("turn_idle"), "{body}");

    // Unknown session: 404, not 500.
    let resp = reqwest::get(format!("{base}/api/sessions/01UNKNOWN")).await.unwrap();
    assert_eq!(resp.status(), 404);
}

// -- remote approvals -------------------------------------------------------

async fn ask_server(
    dir: &std::path::Path,
    ran: Arc<std::sync::atomic::AtomicBool>,
) -> (String, Arc<SessionService>, Kernel) {
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(Danger { ran }));
    tools.approvals().set_policy(rness_engine::approval::Policy::Ask);
    let provider = Arc::new(ToolThenDone { asked: std::sync::atomic::AtomicBool::new(false) });
    serve_with(dir, provider, tools).await
}

async fn start_turn(base: &str, id: &str) {
    post_json(
        &format!("{base}/api/request"),
        serde_json::json!({
            "type": "send",
            "session": id,
            "intent": "followup",
            "content": [{ "kind": "text", "text": "do the thing" }],
        }),
    )
    .await;
}

async fn wait_idle(base: &str, id: &str) {
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if get_json(&format!("{base}/api/sessions/{id}/phase")).await["phase"] == "idle" {
            return;
        }
    }
    panic!("session never went idle");
}

/// Poll the reconcile endpoint until the question shows up.
async fn wait_pending(base: &str) -> serde_json::Value {
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let pending = get_json(&format!("{base}/api/approvals")).await;
        if !pending.as_array().unwrap().is_empty() {
            return pending[0].clone();
        }
    }
    panic!("no approval ever became pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_approval_allows_a_sensitive_call() {
    let dir = tempfile::tempdir().unwrap();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (base, sessions, _kernel) = ask_server(dir.path(), Arc::clone(&ran)).await;
    let id = post_json(&format!("{base}/api/sessions"), serde_json::json!({})).await["session"]
        .as_str()
        .unwrap()
        .to_string();

    // Attach SSE first: the question must be announced as a frame.
    let sse = reqwest::get(format!("{base}/api/events/{id}")).await.unwrap();

    start_turn(&base, &id).await;

    // The reconcile endpoint carries the full request, session included.
    let question = wait_pending(&base).await;
    assert_eq!(question["session"], id);
    assert_eq!(question["tool"], "Danger");
    assert_eq!(question["args"]["cmd"], "rm -rf /");
    let call = question["call"].as_str().unwrap().to_string();

    // Answer over plain HTTP — any client, no channel plumbing.
    let resolved = post_json(
        &format!("{base}/api/approvals/{call}"),
        serde_json::json!({ "decision": "allowed" }),
    )
    .await;
    assert_eq!(resolved["status"], "resolved");

    wait_idle(&base, &id).await;
    assert!(ran.load(std::sync::atomic::Ordering::SeqCst), "approved tool must run");

    // The durable log has the real tool output — approval was one-shot glue.
    let history = sessions.store().history(&id).unwrap();
    let logged = serde_json::to_string(&history).unwrap();
    assert!(logged.contains("boom contained"), "{logged}");

    // Both announcement frames went over SSE.
    let mut body = String::new();
    let mut stream = sse;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !body.contains("turn_idle") {
        let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, stream.chunk()).await else {
            break;
        };
        body.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(body.contains("approval_requested"), "{body}");
    assert!(body.contains("approval_resolved"), "{body}");

    // Nothing pending afterwards.
    let pending = get_json(&format!("{base}/api/approvals")).await;
    assert!(pending.as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_rejection_blocks_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (base, sessions, _kernel) = ask_server(dir.path(), Arc::clone(&ran)).await;
    let id = post_json(&format!("{base}/api/sessions"), serde_json::json!({})).await["session"]
        .as_str()
        .unwrap()
        .to_string();

    start_turn(&base, &id).await;
    let call = wait_pending(&base).await["call"].as_str().unwrap().to_string();

    post_json(
        &format!("{base}/api/approvals/{call}"),
        serde_json::json!({ "decision": "rejected" }),
    )
    .await;

    wait_idle(&base, &id).await;
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst), "rejected tool must not run");
    let logged = serde_json::to_string(&sessions.store().history(&id).unwrap()).unwrap();
    assert!(logged.contains("rejected this tool call"), "{logged}");

    // Answering the same call twice is a 404, not a silent no-op.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/approvals/{call}"))
        .json(&serde_json::json!({ "decision": "allowed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_the_turn_withdraws_the_question() {
    let dir = tempfile::tempdir().unwrap();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (base, _sessions, _kernel) = ask_server(dir.path(), Arc::clone(&ran)).await;
    let id = post_json(&format!("{base}/api/sessions"), serde_json::json!({})).await["session"]
        .as_str()
        .unwrap()
        .to_string();

    start_turn(&base, &id).await;
    wait_pending(&base).await;

    // Cancel instead of answering: the pending set must drain itself.
    post_json(
        &format!("{base}/api/request"),
        serde_json::json!({ "type": "cancel", "session": id }),
    )
    .await;
    wait_idle(&base, &id).await;

    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if get_json(&format!("{base}/api/approvals")).await.as_array().unwrap().is_empty() {
            break;
        }
    }
    let pending = get_json(&format!("{base}/api/approvals")).await;
    assert!(pending.as_array().unwrap().is_empty(), "withdrawn question leaked: {pending}");
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
}
