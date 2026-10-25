//! ACP over an in-memory duplex stream against a real engine with scripted
//! providers: handshake, session setup, a prompt with committed updates,
//! permission round-trips, cancellation, load/list and parameter errors.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rness_acp::{AcpAgent, AcpConfig};
use rness_engine::config::{
    ModelCapabilities, ModelDeclaration, ModelRegistry, ReasoningCapabilities,
};
use rness_engine::service::{ProviderResolver, SessionService, SessionsPlugin};
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::Kernel;
use rness_protocol::events::*;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, WriteHalf};
use tokio_util::sync::CancellationToken;

// -- scripted providers ------------------------------------------------------

fn message(content: Vec<ContentPart>, stop: StopReason) -> StepOutcome {
    StepOutcome::Committed(AssistantMessage {
        model: "fake-1".into(),
        content,
        stop,
        usage: Usage::default(),
        estimated_input: 0,
        chunks: vec![],
    })
}

struct Answer;

#[async_trait]
impl Provider for Answer {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _r: StepRequest<'_>, _c: &CancellationToken) -> StepOutcome {
        message(
            vec![
                ContentPart::Thinking {
                    text: "pensando".into(),
                    signature: None,
                },
                ContentPart::Text {
                    text: "hola desde rness".into(),
                },
            ],
            StopReason::EndTurn,
        )
    }
}

/// First step calls the sensitive `Danger` tool, then ends the turn.
struct ToolThenDone {
    steps: AtomicUsize,
}

#[async_trait]
impl Provider for ToolThenDone {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _r: StepRequest<'_>, _c: &CancellationToken) -> StepOutcome {
        if self.steps.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
            message(
                vec![ContentPart::ToolUse {
                    call: "call-1".into(),
                    name: "Danger".into(),
                    args: json!({"command": "rm -rf /tmp/x"}),
                }],
                StopReason::ToolUse,
            )
        } else {
            message(
                vec![ContentPart::Text {
                    text: "hecho".into(),
                }],
                StopReason::EndTurn,
            )
        }
    }
}

/// Never finishes on its own: waits for cancellation.
struct Hang;

#[async_trait]
impl Provider for Hang {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _r: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        cancel.cancelled().await;
        StepOutcome::Cancelled { partial: vec![] }
    }
}

struct Danger {
    ran: Arc<AtomicBool>,
}

#[async_trait]
impl rness_engine::tools::Tool for Danger {
    fn name(&self) -> &str {
        "Danger"
    }
    fn sensitive(&self) -> bool {
        true
    }
    async fn execute(&self, _args: Value) -> Result<String, String> {
        self.ran.store(true, Ordering::SeqCst);
        Ok("boom contained".into())
    }
}

// -- harness -----------------------------------------------------------------

struct Client {
    lines: Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    next: u64,
    _kernel: Kernel,
    sessions: Arc<SessionService>,
    _dir: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Client {
    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        id
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("timed out waiting for the agent")
            .unwrap()
            .expect("agent closed its output");
        serde_json::from_str(&line).unwrap()
    }

    /// Read messages until the response to `id`; returns (notifications, response).
    async fn until_response(&mut self, id: u64) -> (Vec<Value>, Value) {
        let mut seen = Vec::new();
        loop {
            let message = self.recv().await;
            if message["id"] == json!(id) && message.get("method").is_none() {
                return (seen, message);
            }
            seen.push(message);
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params).await;
        self.until_response(id).await.1
    }

    fn cwd(&self) -> String {
        self.workspace
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    }

    async fn new_session(&mut self) -> String {
        let cwd = self.cwd();
        let response = self
            .call("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new failed: {response}"))
            .to_string()
    }
}

fn start(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>, ask: bool) -> Client {
    start_with(provider, tools, ask, None)
}

/// Two providers: `alpha` (images, efforts low/high) and `beta` (text only).
fn catalog() -> ModelRegistry {
    let mut models = ModelRegistry::default();
    for (provider, model, image, efforts) in [
        (
            "alpha",
            "a1",
            true,
            Some(vec!["low".to_string(), "high".to_string()]),
        ),
        ("alpha", "a2", true, None),
        ("beta", "b1", false, None),
    ] {
        models
            .declare_model(ModelDeclaration {
                provider: provider.into(),
                model: model.into(),
                capabilities: ModelCapabilities {
                    image_input: Some(image),
                    context_window: Some(1000),
                    reasoning: efforts.map(|efforts| ReasoningCapabilities {
                        efforts: Some(efforts),
                        budget_tokens: None,
                    }),
                    ..Default::default()
                },
            })
            .unwrap();
    }
    models
}

fn start_with(
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    ask: bool,
    models: Option<(ModelRegistry, ModelSelection)>,
) -> Client {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut kernel = Kernel::new();
    let images = models.is_some();
    let (models, creation_seed, resolver) = match models {
        Some((models, selection)) => {
            let resolved = Arc::clone(&provider);
            let resolver: Arc<ProviderResolver> = Arc::new(move |_| Ok(Arc::clone(&resolved)));
            let seed = CallConfig {
                selection: Some(selection),
                ..Default::default()
            };
            (models, seed, Some(resolver))
        }
        None => (Default::default(), Default::default(), None),
    };
    kernel
        .mount(SessionsPlugin {
            root: dir.path().into(),
            provider,
            resolver,
            creation_seed,
            sandbox: Default::default(),
            agents: Default::default(),
            models,
            tools: Arc::clone(&tools),
            config: TurnConfig::default(),
        })
        .unwrap();
    let sessions = kernel.services().get::<SessionService>("sessions").unwrap();
    if images {
        sessions
            .set_images(Arc::new(
                rness_engine::images::ImageStore::new(
                    dir.path().join("images"),
                    Default::default(),
                )
                .unwrap(),
            ))
            .unwrap();
    }
    let agent = AcpAgent::new(
        Arc::clone(&sessions),
        AcpConfig {
            images,
            ..Default::default()
        },
    );
    if ask {
        tools
            .approvals()
            .set_rules([("Danger".into(), rness_engine::approval::ToolPolicy::Ask)].into());
        tools.approvals().set_answerer(Arc::clone(&agent) as _);
    }
    let (client_side, agent_side) = tokio::io::duplex(1 << 20);
    let (agent_read, agent_write) = tokio::io::split(agent_side);
    tokio::spawn(async move {
        agent.serve(agent_read, agent_write).await.unwrap();
    });
    let (client_read, client_write) = tokio::io::split(client_side);
    Client {
        lines: BufReader::new(client_read).lines(),
        writer: client_write,
        next: 0,
        _kernel: kernel,
        sessions,
        _dir: dir,
        workspace,
    }
}

fn updates(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|m| m["method"] == "session/update")
        .map(|m| &m["params"]["update"])
        .collect()
}

// -- tests -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn handshake_new_session_and_prompt_stream_committed_updates() {
    let mut client = start(Arc::new(Answer), Arc::new(ToolRegistry::default()), false);
    let init = client
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(init["result"]["agentInfo"]["name"], "rness");
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);
    assert_eq!(init["result"]["authMethods"], json!([]));

    let session = client.new_session().await;
    let id = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "hola"}]}),
        )
        .await;
    let (seen, response) = client.until_response(id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let updates = updates(&seen);
    let kinds: Vec<_> = updates.iter().map(|u| u["sessionUpdate"].clone()).collect();
    assert_eq!(
        kinds,
        vec![json!("agent_thought_chunk"), json!("agent_message_chunk")]
    );
    assert_eq!(updates[1]["content"]["text"], "hola desde rness");
    // The prompt itself is not echoed back to the client that sent it.
    assert!(seen
        .iter()
        .all(|m| m["params"]["sessionId"] == json!(session)));

    // Durable: the turn is in the log, through the service seam.
    let transcript = client.sessions.transcript(&session).unwrap();
    assert!(transcript.items.len() >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_allow_runs_the_tool_and_reports_results() {
    let ran = Arc::new(AtomicBool::new(false));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(Danger { ran: ran.clone() }));
    let provider = Arc::new(ToolThenDone {
        steps: AtomicUsize::new(0),
    });
    let mut client = start(provider, tools, true);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let session = client.new_session().await;
    let id = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "hazlo"}]}),
        )
        .await;

    // The tool call is announced, then the agent asks.
    let mut seen = Vec::new();
    let permission = loop {
        let message = client.recv().await;
        if message["method"] == "session/request_permission" {
            break message;
        }
        seen.push(message);
    };
    let announced = updates(&seen);
    assert_eq!(announced.last().unwrap()["sessionUpdate"], "tool_call");
    assert_eq!(announced.last().unwrap()["toolCallId"], "call-1");
    assert_eq!(permission["params"]["sessionId"], json!(session));
    assert_eq!(permission["params"]["toolCall"]["toolCallId"], "call-1");
    assert_eq!(
        permission["params"]["options"][0],
        json!({"optionId":"allow-once","name":"Allow once","kind":"allow_once"})
    );
    let request_id = permission["id"].clone();
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}},
        }))
        .await;

    let (rest, response) = client.until_response(id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(ran.load(Ordering::SeqCst));
    let rest = updates(&rest);
    let result = rest
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update")
        .expect("tool result reported");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["content"][0]["content"]["text"], "boom contained");
    assert_eq!(rest.last().unwrap()["content"]["text"], "hecho");
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_reject_never_runs_the_tool() {
    let ran = Arc::new(AtomicBool::new(false));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(Danger { ran: ran.clone() }));
    let provider = Arc::new(ToolThenDone {
        steps: AtomicUsize::new(0),
    });
    let mut client = start(provider, tools, true);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let session = client.new_session().await;
    let id = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "hazlo"}]}),
        )
        .await;
    let permission = loop {
        let message = client.recv().await;
        if message["method"] == "session/request_permission" {
            break message;
        }
    };
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": permission["id"],
            "result": {"outcome": {"outcome": "selected", "optionId": "reject-once"}},
        }))
        .await;
    let (rest, response) = client.until_response(id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert!(!ran.load(Ordering::SeqCst));
    let failed = updates(&rest)
        .into_iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update")
        .expect("rejected call still settles");
    assert_eq!(failed["status"], "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_ends_the_prompt_with_cancelled() {
    let mut client = start(Arc::new(Hang), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let session = client.new_session().await;
    let id = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "espera"}]}),
        )
        .await;
    // Wait until the turn is actually running before cancelling.
    for _ in 0..200 {
        if client.sessions.phase(&session) == rness_engine::inbox::Phase::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A second prompt while one is in flight is refused.
    let overlap = client
        .call(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "otra"}]}),
        )
        .await;
    assert_eq!(
        overlap["error"],
        json!({"code": -32602, "message": "Invalid params: a prompt is already in flight for this session"}),
    );

    client
        .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId": session}}))
        .await;
    let (_, response) = client.until_response(id).await;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
}

#[tokio::test(flavor = "multi_thread")]
async fn load_replays_history_and_list_filters_by_cwd() {
    let mut client = start(Arc::new(Answer), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let session = client.new_session().await;
    client
        .call(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "primera"}]}),
        )
        .await;

    let cwd = client.cwd();
    // Loading needs the session to be inactive on this connection.
    client
        .call("session/close", json!({"sessionId": session}))
        .await;
    let id = client
        .request(
            "session/load",
            json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
        )
        .await;
    let (seen, response) = client.until_response(id).await;
    assert!(response["result"].is_object(), "{response}");
    let kinds: Vec<_> = updates(&seen)
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "user_message_chunk",
            "agent_thought_chunk",
            "agent_message_chunk"
        ]
    );
    client
        .call("session/close", json!({"sessionId": session}))
        .await;

    let listed = client.call("session/list", json!({"cwd": cwd})).await;
    let sessions = listed["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["sessionId"], json!(session));
    assert_eq!(sessions[0]["cwd"], json!(cwd));
    let elsewhere = client.call("session/list", json!({"cwd": "/"})).await;
    assert_eq!(elsewhere["result"]["sessions"], json!([]));

    // A mismatched cwd is refused rather than silently rebinding.
    let wrong = client
        .call(
            "session/resume",
            json!({"sessionId": session, "cwd": "/", "mcpServers": []}),
        )
        .await;
    assert_eq!(
        wrong["error"]["message"],
        "Invalid params: session cwd does not match: /"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_requests_map_to_json_rpc_errors() {
    let mut client = start(Arc::new(Answer), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let cwd = client.cwd();

    let relative = client
        .call("session/new", json!({"cwd": "relative", "mcpServers": []}))
        .await;
    assert_eq!(relative["error"]["code"], -32602);
    let extra = client
        .call(
            "session/new",
            json!({"cwd": cwd, "mcpServers": [], "additionalDirectories": ["/x"]}),
        )
        .await;
    assert_eq!(
        extra["error"],
        json!({"code": -32602, "message": "Invalid params: additionalDirectories is not supported"})
    );
    let unknown = client.call("session/fork", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);
    let missing = client
        .call(
            "session/prompt",
            json!({"sessionId": "../../etc", "prompt": [{"type":"text","text":"x"}]}),
        )
        .await;
    assert_eq!(missing["error"]["code"], -32602);
    let never_opened = client
        .call(
            "session/prompt",
            json!({"sessionId": "01JUNKNOWN", "prompt": [{"type":"text","text":"x"}]}),
        )
        .await;
    assert_eq!(
        never_opened["error"],
        json!({"code": -32602, "message": "Invalid params: unknown session: 01JUNKNOWN"})
    );
    let session = client.new_session().await;
    let audio = client
        .call(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type":"audio","data":"","mimeType":"audio/wav"}]}),
        )
        .await;
    assert_eq!(audio["error"]["code"], -32602);

    client.send(json!("not an object")).await;
    client.writer.write_all(b"{broken\n").await.unwrap();
    let parse = client.recv().await;
    let parse = if parse["error"]["code"] == -32600 {
        client.recv().await
    } else {
        parse
    };
    assert_eq!(parse["error"]["code"], -32700);
}

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

fn alpha() -> Option<(ModelRegistry, ModelSelection)> {
    Some((
        catalog(),
        ModelSelection {
            route: "alpha".into(),
            model: "a1".into(),
        },
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn config_options_group_models_and_expose_reasoning_effort() {
    let mut client = start_with(
        Arc::new(Answer),
        Arc::new(ToolRegistry::default()),
        false,
        alpha(),
    );
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let cwd = client.cwd();
    let new = client
        .call("session/new", json!({"cwd": cwd, "mcpServers": []}))
        .await;
    let session = new["result"]["sessionId"].as_str().unwrap().to_string();
    let options = &new["result"]["configOptions"];
    assert_eq!(
        options[0],
        json!({
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": "alpha/a1",
            "options": [
                {"group": "alpha", "name": "alpha", "options": [
                    {"value": "alpha/a1", "name": "a1"}, {"value": "alpha/a2", "name": "a2"}]},
                {"group": "beta", "name": "beta", "options": [{"value": "beta/b1", "name": "b1"}]},
            ],
        })
    );
    assert_eq!(options[1]["id"], "reasoning_effort");
    assert_eq!(options[1]["category"], "thought_level");
    assert_eq!(options[1]["currentValue"], "");
    assert_eq!(
        options[1]["options"],
        json!([{"value":"","name":"Provider default"},{"value":"low","name":"low"},{"value":"high","name":"high"}])
    );

    let set = |config: &str, value: &str| json!({"sessionId": session, "configId": config, "value": value});
    let high = client
        .call("session/set_config_option", set("reasoning_effort", "high"))
        .await;
    assert_eq!(
        high["result"]["configOptions"][1]["currentValue"], "high",
        "{high}"
    );
    let bad = client
        .call("session/set_config_option", set("reasoning_effort", "max"))
        .await;
    assert_eq!(bad["error"]["code"], -32602);
    let unknown = client
        .call("session/set_config_option", set("model", "alpha/zzz"))
        .await;
    assert_eq!(unknown["error"]["code"], -32602);

    // Switching to a model without declared efforts drops the selector
    // and the stale effort.
    let switched = client
        .call("session/set_config_option", set("model", "beta/b1"))
        .await;
    let options = switched["result"]["configOptions"].as_array().unwrap();
    assert_eq!(options.len(), 1, "{switched}");
    assert_eq!(options[0]["currentValue"], "beta/b1");
    let config = client.sessions.config(&session).unwrap();
    assert_eq!(config.reasoning, None);
    assert_eq!(config.selection.unwrap().route, "beta");
}

#[tokio::test(flavor = "multi_thread")]
async fn image_prompts_follow_the_session_model() {
    let mut client = start_with(
        Arc::new(Answer),
        Arc::new(ToolRegistry::default()),
        false,
        alpha(),
    );
    let init = client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    assert_eq!(
        init["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        true
    );
    let session = client.new_session().await;
    let prompt = |data: &str, mime: &str| {
        json!({"sessionId": session, "prompt": [
            {"type": "text", "text": "mira "},
            {"type": "image", "data": data, "mimeType": mime},
        ]})
    };
    let bad_mime = client
        .call("session/prompt", prompt(PNG, "image/bmp"))
        .await;
    assert_eq!(bad_mime["error"]["code"], -32602, "{bad_mime}");
    let bad_b64 = client
        .call("session/prompt", prompt("not base64!", "image/png"))
        .await;
    assert_eq!(
        bad_b64["error"]["message"],
        "Invalid params: image data must be canonical base64"
    );
    let ok = client
        .call("session/prompt", prompt(PNG, "image/png"))
        .await;
    assert_eq!(ok["result"]["stopReason"], "end_turn", "{ok}");
    let history = client.sessions.store().history(&session).unwrap();
    let user = history
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::UserMessage(m) => Some(m.content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(&user[0], ContentPart::Text { text } if text == "mira "));
    assert!(matches!(&user[1], ContentPart::Image { .. }));

    client
        .call(
            "session/set_config_option",
            json!({"sessionId": session, "configId": "model", "value": "beta/b1"}),
        )
        .await;
    let refused = client
        .call("session/prompt", prompt(PNG, "image/png"))
        .await;
    assert_eq!(
        refused["error"]["message"],
        "Invalid params: model \"beta/b1\" does not declare image input"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn images_are_refused_when_not_advertised() {
    let mut client = start(Arc::new(Answer), Arc::new(ToolRegistry::default()), false);
    let init = client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    assert_eq!(
        init["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        false
    );
    let session = client.new_session().await;
    let refused = client
        .call(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type":"image","data":PNG,"mimeType":"image/png"}]}),
        )
        .await;
    assert_eq!(
        refused["error"]["message"],
        "Invalid params: inline image prompts were not advertised by this connection"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn active_and_delegated_sessions_are_not_resumable_or_listed() {
    let mut client = start(Arc::new(Answer), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let cwd = client.cwd();
    let open = client.new_session().await;
    let again = client
        .call(
            "session/resume",
            json!({"sessionId": open, "cwd": cwd, "mcpServers": []}),
        )
        .await;
    assert_eq!(
        again["error"]["message"],
        format!("Invalid params: session is already active: {open}")
    );
    let listed = client.call("session/list", json!({"cwd": cwd})).await;
    assert_eq!(
        listed["result"]["sessions"],
        json!([]),
        "open sessions are not listed"
    );

    let child = client
        .sessions
        .create_delegated(
            Some(cwd.clone()),
            rness_protocol::branch::Delegation {
                parent: open.clone(),
                call: None,
                depth: 1,
                mode: Default::default(),
            },
        )
        .unwrap();
    let delegated = client
        .call(
            "session/load",
            json!({"sessionId": child, "cwd": cwd, "mcpServers": []}),
        )
        .await;
    assert_eq!(
        delegated["error"]["message"],
        format!("Invalid params: session is not resumable: {child}")
    );
    let missing = client
        .call(
            "session/resume",
            json!({"sessionId": "01JMISSING", "cwd": cwd}),
        )
        .await;
    assert_eq!(missing["error"]["code"], -32602);

    client
        .call("session/close", json!({"sessionId": open}))
        .await;
    let listed = client.call("session/list", json!({"cwd": cwd})).await;
    let ids: Vec<_> = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sessionId"].clone())
        .collect();
    assert_eq!(ids, vec![json!(open)], "closed root listed, child never");
}

/// Two calls; each tool hangs until the dispatcher abandons it.
struct TwoHangingCalls {
    steps: AtomicUsize,
}

#[async_trait]
impl Provider for TwoHangingCalls {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _r: StepRequest<'_>, _c: &CancellationToken) -> StepOutcome {
        assert_eq!(
            self.steps.fetch_add(1, Ordering::SeqCst),
            0,
            "no step after cancel"
        );
        let call = |id: &str| ContentPart::ToolUse {
            call: id.into(),
            name: "Wait".into(),
            args: json!({}),
        };
        message(
            vec![call("call-wait"), call("call-next")],
            StopReason::ToolUse,
        )
    }
}

struct Wait {
    started: Arc<AtomicUsize>,
}

#[async_trait]
impl rness_engine::tools::Tool for Wait {
    fn name(&self) -> &str {
        "Wait"
    }
    async fn execute(&self, _args: Value) -> Result<String, String> {
        self.started.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_settles_pending_tool_calls_as_failed() {
    let started = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(Wait {
        started: started.clone(),
    }));
    let provider = Arc::new(TwoHangingCalls {
        steps: AtomicUsize::new(0),
    });
    let mut client = start(provider, tools, false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let session = client.new_session().await;
    let id = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "espera"}]}),
        )
        .await;
    for _ in 0..300 {
        if started.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(started.load(Ordering::SeqCst) > 0, "a tool started");
    client
        .send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}}),
        )
        .await;
    let (seen, response) = client.until_response(id).await;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    let seen = updates(&seen);
    for call in ["call-wait", "call-next"] {
        let started = seen
            .iter()
            .any(|u| u["sessionUpdate"] == "tool_call" && u["toolCallId"] == call);
        assert!(started, "{call} was announced: {seen:?}");
        let settled = seen
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == call)
            .unwrap_or_else(|| panic!("{call} settles: {seen:?}"));
        assert_eq!(settled["status"], "failed", "{settled}");
    }
}

// -- client-attached MCP servers ---------------------------------------------

/// Stdio MCP fixture. Tools: `pwd` (cwd + $GREETING), `grow` (announces
/// `tools/list_changed`; `extra` appears afterwards), `hang` (never answers).
/// $PIDFILE gets the pid, $CALLS one line per call, $HANG_INIT stalls the
/// handshake.
const MCP_SERVER: &str = r#"
import json, os, sys, time
if os.environ.get("PIDFILE"):
    with open(os.environ["PIDFILE"], "w") as f:
        f.write(str(os.getpid()))
grown = False
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
def tool(name):
    return {"name": name, "description": name, "inputSchema": {"type": "object"}}
for line in sys.stdin:
    msg = json.loads(line)
    mid, method = msg.get("id"), msg.get("method")
    if method == "initialize":
        if os.environ.get("HANG_INIT"):
            time.sleep(3600)
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {"listChanged": True}}, "serverInfo": {"name": "fake", "version": "0"}}})
    elif method == "tools/list":
        names = ["pwd", "grow", "hang"] + (["extra"] if grown else [])
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [tool(n) for n in names]}})
    elif method == "tools/call":
        name = msg["params"]["name"]
        if os.environ.get("CALLS"):
            with open(os.environ["CALLS"], "a") as f:
                f.write(name + "\n")
        if name == "hang":
            continue
        if name == "grow":
            grown = True
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        text = os.getcwd() + " " + os.environ.get("GREETING", "")
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": text}]}})
"#;

fn python3() -> String {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("python3"))
        .find(|path| path.is_file())
        .expect("python3 on PATH")
        .display()
        .to_string()
}

/// Each step either calls the named tool or (None / exhausted) answers
/// "ok"; records the tools advertised on every request.
struct Steps {
    steps: std::sync::Mutex<std::collections::VecDeque<Option<&'static str>>>,
    seen: std::sync::Mutex<Vec<Vec<String>>>,
    calls: AtomicUsize,
}

impl Steps {
    fn new(steps: &[Option<&'static str>]) -> Arc<Self> {
        Arc::new(Self {
            steps: std::sync::Mutex::new(steps.iter().copied().collect()),
            seen: Default::default(),
            calls: AtomicUsize::new(0),
        })
    }

    fn advertised(&self) -> Vec<Vec<String>> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for Steps {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, _c: &CancellationToken) -> StepOutcome {
        let names = request.tools.iter().map(|t| t.name.clone()).collect();
        self.seen.lock().unwrap().push(names);
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        match self.steps.lock().unwrap().pop_front().flatten() {
            Some(tool) => message(
                vec![ContentPart::ToolUse {
                    call: format!("call-{n}"),
                    name: tool.into(),
                    args: json!({}),
                }],
                StopReason::ToolUse,
            ),
            None => message(
                vec![ContentPart::Text { text: "ok".into() }],
                StopReason::EndTurn,
            ),
        }
    }
}

impl Client {
    /// A stdio entry for the fixture server named `name`.
    fn mcp_server(&self, name: &str, env: &[(&str, String)]) -> Value {
        let script = self._dir.path().join("server.py");
        if !script.exists() {
            std::fs::write(&script, MCP_SERVER).unwrap();
        }
        let env: Vec<Value> = env
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect();
        json!({"name": name, "command": python3(), "args": [script], "env": env})
    }

    fn file(&self, name: &str) -> String {
        self._dir.path().join(name).display().to_string()
    }

    async fn open_with(&mut self, servers: Value) -> String {
        let cwd = self.cwd();
        let response = self
            .call("session/new", json!({"cwd": cwd, "mcpServers": servers}))
            .await;
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new failed: {response}"))
            .to_string()
    }

    async fn prompt(&mut self, session: &str) -> (Vec<Value>, Value) {
        let id = self
            .request(
                "session/prompt",
                json!({"sessionId": session, "prompt": [{"type": "text", "text": "go"}]}),
            )
            .await;
        self.until_response(id).await
    }

    fn overlay(&self, session: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .sessions
            .tool_registry()
            .session_tools
            .get(&session.into())
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        names.sort();
        names
    }
}

fn tool_result(seen: &[Value]) -> Value {
    updates(seen)
        .into_iter()
        .rev()
        .find(|u| u["sessionUpdate"] == "tool_call_update")
        .expect("tool result reported")
        .clone()
}

async fn read_pid(path: &str) -> u32 {
    for _ in 0..200 {
        if let Ok(pid) = std::fs::read_to_string(path) {
            if let Ok(pid) = pid.trim().parse() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server never wrote {path}");
}

fn alive(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

async fn assert_dies(pid: u32) {
    for _ in 0..200 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("MCP server {pid} still running");
}

const MCP_TOOLS: [&str; 3] = ["mcp__fake__grow", "mcp__fake__hang", "mcp__fake__pwd"];

#[tokio::test(flavor = "multi_thread")]
async fn mcp_servers_are_mounted_per_session_in_the_workspace() {
    let provider = Steps::new(&[Some("mcp__fake__pwd"), None, None]);
    let mut client = start(provider.clone(), Arc::new(ToolRegistry::default()), false);
    let init = client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    assert_eq!(
        init["result"]["agentCapabilities"]["mcpCapabilities"],
        json!({"http": true, "sse": false})
    );
    let pidfile = client.file("pid");
    let server = client.mcp_server(
        "fake",
        &[("GREETING", "hola".into()), ("PIDFILE", pidfile.clone())],
    );
    let session = client.open_with(json!([server])).await;
    let pid = read_pid(&pidfile).await;
    assert_eq!(client.overlay(&session), MCP_TOOLS);

    let (seen, response) = client.prompt(&session).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let result = tool_result(&seen);
    assert_eq!(result["status"], "completed", "{result}");
    let cwd = client.cwd();
    assert_eq!(
        result["content"][0]["content"]["text"],
        format!("{cwd} hola")
    );

    // Another session of the same connection never sees it.
    let other = client.new_session().await;
    client.prompt(&other).await;
    let advertised = provider.advertised();
    assert_eq!(advertised[0], MCP_TOOLS);
    assert!(advertised[2].is_empty(), "{advertised:?}");

    // Closing detaches the tools and stops the server.
    client
        .call("session/close", json!({"sessionId": session}))
        .await;
    assert!(client.overlay(&session).is_empty());
    assert_dies(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_list_changed_rebridges_inside_the_session() {
    let provider = Steps::new(&[Some("mcp__fake__grow"), None, None]);
    let mut client = start(provider.clone(), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let server = client.mcp_server("fake", &[]);
    let session = client.open_with(json!([server])).await;
    let (_, response) = client.prompt(&session).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    // The re-bridge runs after the notification; wait for it, then the
    // next turn advertises the new tool.
    for _ in 0..200 {
        if client
            .overlay(&session)
            .contains(&"mcp__fake__extra".into())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    client.prompt(&session).await;
    let advertised = provider.advertised();
    assert_eq!(
        advertised.last().unwrap(),
        &[
            "mcp__fake__extra",
            "mcp__fake__grow",
            "mcp__fake__hang",
            "mcp__fake__pwd"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn load_and_resume_take_mcp_servers_again() {
    let provider = Steps::new(&[]);
    let mut client = start(provider.clone(), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let server = client.mcp_server("fake", &[]);
    let session = client.open_with(json!([server.clone()])).await;
    client.prompt(&session).await;
    client
        .call("session/close", json!({"sessionId": session}))
        .await;
    let cwd = client.cwd();

    // Load with the server: it is mounted again.
    let loaded = client
        .call(
            "session/load",
            json!({"sessionId": session, "cwd": cwd, "mcpServers": [server]}),
        )
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert_eq!(client.overlay(&session), MCP_TOOLS);
    client.prompt(&session).await;
    client
        .call("session/close", json!({"sessionId": session}))
        .await;

    // Resume without it: nothing is restored from the log.
    let resumed = client
        .call(
            "session/resume",
            json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
        )
        .await;
    assert!(resumed.get("result").is_some(), "{resumed}");
    assert!(client.overlay(&session).is_empty());
    client.prompt(&session).await;
    client
        .call("session/close", json!({"sessionId": session}))
        .await;
    let advertised = provider.advertised();
    assert_eq!(advertised[0], MCP_TOOLS);
    assert_eq!(advertised[1], MCP_TOOLS);
    assert!(advertised[2].is_empty(), "{advertised:?}");

    // A server that fails on load: error, and the session stays closed.
    let failing = client
        .call(
            "session/load",
            json!({"sessionId": session, "cwd": cwd,
                   "mcpServers": [{"name": "x", "command": "/usr/bin/false"}]}),
        )
        .await;
    assert_eq!(failing["error"]["code"], -32603, "{failing}");
    let prompt = client
        .call(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "x"}]}),
        )
        .await;
    assert_eq!(prompt["error"]["code"], -32602, "{prompt}");
    let listed = client.call("session/list", json!({})).await;
    assert_eq!(listed["result"]["sessions"][0]["sessionId"], json!(session));
}

#[tokio::test(flavor = "multi_thread")]
async fn subagents_do_not_inherit_session_mcp_tools() {
    let provider = Steps::new(&[]);
    let mut client = start(provider.clone(), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let server = client.mcp_server("fake", &[]);
    let session = client.open_with(json!([server])).await;
    let child = client
        .sessions
        .create_delegated(
            None,
            rness_protocol::branch::Delegation {
                parent: session.as_str().into(),
                call: None,
                depth: 1,
                mode: Default::default(),
            },
        )
        .unwrap();
    client
        .sessions
        .send(
            &child,
            UserIntent::Followup,
            vec![ContentPart::Text { text: "hi".into() }],
        )
        .unwrap();
    client.sessions.join(&child).await;
    client.prompt(&session).await;
    let advertised = provider.advertised();
    assert!(
        advertised[0].is_empty(),
        "child sees nothing: {advertised:?}"
    );
    assert_eq!(advertised[1], MCP_TOOLS);
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_rules_apply_to_mcp_tools_by_public_name() {
    let provider = Steps::new(&[Some("mcp__fake__pwd"), None]);
    let tools = Arc::new(ToolRegistry::default());
    let mut client = start(provider, Arc::clone(&tools), true);
    tools.approvals().set_rules(
        [(
            "mcp__fake__pwd".into(),
            rness_engine::approval::ToolPolicy::Ask,
        )]
        .into(),
    );
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let calls = client.file("calls");
    let server = client.mcp_server("fake", &[("CALLS", calls.clone())]);
    let session = client.open_with(json!([server])).await;
    let id = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "go"}]}),
        )
        .await;
    let permission = loop {
        let message = client.recv().await;
        if message["method"] == "session/request_permission" {
            break message;
        }
    };
    assert_eq!(permission["params"]["toolCall"]["toolCallId"], "call-0");
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": permission["id"],
            "result": {"outcome": {"outcome": "selected", "optionId": "reject-once"}},
        }))
        .await;
    let (seen, response) = client.until_response(id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(tool_result(&seen)["status"], "failed");
    assert!(
        !std::path::Path::new(&calls).exists(),
        "server never called"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_during_a_prompt_stops_the_server() {
    let provider = Steps::new(&[Some("mcp__fake__hang")]);
    let mut client = start(provider, Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let pidfile = client.file("pid");
    let server = client.mcp_server("fake", &[("PIDFILE", pidfile.clone())]);
    let session = client.open_with(json!([server])).await;
    let pid = read_pid(&pidfile).await;
    let prompt = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "go"}]}),
        )
        .await;
    loop {
        let message = client.recv().await;
        if message["params"]["update"]["sessionUpdate"] == "tool_call" {
            break;
        }
    }
    let close = client
        .request("session/close", json!({"sessionId": session}))
        .await;
    let mut answered = (false, false);
    while answered != (true, true) {
        let message = client.recv().await;
        if message["id"] == json!(prompt) && message.get("method").is_none() {
            assert_eq!(message["result"]["stopReason"], "cancelled", "{message}");
            answered.0 = true;
        }
        if message["id"] == json!(close) && message.get("method").is_none() {
            answered.1 = true;
        }
    }
    assert_dies(pid).await;
    assert!(client.overlay(&session).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn input_eof_stops_running_and_starting_servers() {
    let mut client = start(Steps::new(&[]), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let (running, starting) = (client.file("running"), client.file("starting"));
    let server = client.mcp_server("fake", &[("PIDFILE", running.clone())]);
    client.open_with(json!([server])).await;
    // A second session whose server never finishes its handshake.
    let stalled = client.mcp_server(
        "fake",
        &[("PIDFILE", starting.clone()), ("HANG_INIT", "1".into())],
    );
    let cwd = client.cwd();
    client
        .request("session/new", json!({"cwd": cwd, "mcpServers": [stalled]}))
        .await;
    let (running, starting) = (read_pid(&running).await, read_pid(&starting).await);
    assert!(alive(running) && alive(starting));
    client.writer.shutdown().await.unwrap();
    assert_dies(running).await;
    assert_dies(starting).await;
}

/// Streamable-HTTP MCP fixture that requires `Authorization: Bearer acp-secret`.
async fn http_mcp(
    headers: axum::http::HeaderMap,
    axum::Json(message): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some("Bearer acp-secret") {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    let result = match message["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}})
        }
        Some("tools/list") => {
            json!({"tools": [{"name": "ping", "inputSchema": {"type": "object"}}]})
        }
        Some("tools/call") => json!({"content": [{"type": "text", "text": "pong"}]}),
        _ => return axum::http::StatusCode::ACCEPTED.into_response(),
    };
    axum::Json(json!({"jsonrpc": "2.0", "id": message["id"], "result": result})).into_response()
}

#[tokio::test(flavor = "multi_thread")]
async fn http_mcp_servers_send_headers_and_run_tools() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(http_mcp).delete(|| async { axum::http::StatusCode::NO_CONTENT }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let provider = Steps::new(&[Some("mcp__remote__ping"), None]);
    let mut client = start(provider.clone(), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let cwd = client.cwd();
    let entry = |token: &str| {
        json!({"type": "http", "name": "remote", "url": url,
               "headers": [{"name": "Authorization", "value": format!("Bearer {token}")}]})
    };
    let refused = client
        .call(
            "session/new",
            json!({"cwd": cwd, "mcpServers": [entry("wrong")]}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32603, "{refused}");
    assert!(
        !refused.to_string().contains("wrong"),
        "header values never echoed: {refused}"
    );

    let session = client.open_with(json!([entry("acp-secret")])).await;
    let (seen, response) = client.prompt(&session).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let result = tool_result(&seen);
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["content"][0]["content"]["text"], "pong");
    assert_eq!(provider.advertised()[0], ["mcp__remote__ping"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_or_failing_mcp_servers_create_no_session() {
    let mut client = start(Arc::new(Answer), Arc::new(ToolRegistry::default()), false);
    client
        .call("initialize", json!({"protocolVersion": 1}))
        .await;
    let cwd = client.cwd();
    let invalid = client
        .call(
            "session/new",
            json!({"cwd": cwd, "mcpServers": [{"name": "x", "command": "relative"}]}),
        )
        .await;
    assert_eq!(invalid["error"]["code"], -32602);
    assert_eq!(
        invalid["error"]["message"],
        "Invalid params: mcpServers[0].command must be an absolute path"
    );
    // A server that exits before the handshake: internal error, nothing kept.
    let failing = client
        .call(
            "session/new",
            json!({"cwd": cwd, "mcpServers": [{"name": "x", "command": "/usr/bin/false"}]}),
        )
        .await;
    assert_eq!(failing["error"]["code"], -32603, "{failing}");
    let listed = client.call("session/list", json!({})).await;
    assert_eq!(listed["result"]["sessions"], json!([]));
}
