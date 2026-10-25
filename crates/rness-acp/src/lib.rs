//! Agent Client Protocol (ACP v1) frontend.
//!
//! `rness --acp` speaks JSON-RPC 2.0 as newline-delimited JSON on
//! stdin/stdout, so ACP clients (Zed, channel gateways, other harnesses)
//! can drive rness sessions like they drive any ACP agent. Like the HTTP
//! server, this is a frontend over [`SessionService`]: it never mutates
//! durable state except through the service seam (invariant #12).
//!
//! Scope (modelled on deepseek-harness's automation-oriented ACP server):
//!
//! * `initialize`, `authenticate` (no methods), `session/new`,
//!   `session/load` (replays committed history), `session/resume`,
//!   `session/list`, `session/close`, `session/prompt`, `session/cancel`,
//!   `session/set_config_option` (the model selector).
//! * Updates are **committed facts** read back from the session log —
//!   reasoning, assistant text, tool calls, tool results, usage — never
//!   live token deltas, so a replay shows exactly what a live client saw.
//! * Sensitive tool calls under the `ask` policy become
//!   `session/request_permission` (one-shot allow/reject, fail closed).
//! * Not supported: client filesystem/terminal delegation, modes, plans,
//!   per-session MCP servers (rness's MCP servers come from init.lua),
//!   `additionalDirectories`, audio, blob resources.

mod mcp;
pub mod rpc;
pub mod updates;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use rness_engine::approval::{Answerer, Decision};
use rness_engine::config::ModelCapabilities;
use rness_engine::inbox::{Disposition, Phase};
use rness_engine::service::{FrameEv, ServiceError, SessionIdleEv, SessionService, TurnEndedEv};
use rness_engine::session::branch::BranchError;
use rness_engine::session::log::LogError;
use rness_kernel::Disposer;
use rness_protocol::api::ApprovalRequest;
use rness_protocol::events::{
    AttemptOutcome, CallConfig, ContentPart, EventId, ModelSelection, Reasoning, SessionEvent,
    SessionId, StopReason, TurnOutcome, UserIntent,
};
use rness_protocol::frames::Frame;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use rpc::{Peer, RpcError};

pub const PROTOCOL_VERSION: u64 = 1;
const LIST_PAGE: usize = 100;

/// Identity and capability switches chosen by the composition root.
#[derive(Debug, Clone)]
pub struct AcpConfig {
    pub name: String,
    pub version: String,
    /// Advertise image prompts (an image store is configured).
    pub images: bool,
    /// Per-request timeout for client-attached MCP servers (dsh: 60 s).
    pub mcp_timeout: std::time::Duration,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            name: "rness".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            images: false,
            mcp_timeout: std::time::Duration::from_secs(60),
        }
    }
}

/// A session this connection has opened (new/load/resume).
#[derive(Default)]
struct Open {
    /// Last session-log event already reported to the client. The lock
    /// also serializes flushes so updates leave in log order.
    cursor: Mutex<Option<EventId>>,
    /// A `session/prompt` is in flight (ACP allows one per session).
    prompting: AtomicBool,
    /// The client sent `session/cancel` for the in-flight prompt.
    cancelled: AtomicBool,
    /// A flush is already queued for the pump.
    wake: AtomicBool,
    /// MCP servers the client attached to this session.
    mcp: Option<mcp::Mounted>,
}

pub struct AcpAgent {
    sessions: Arc<SessionService>,
    config: AcpConfig,
    peer: Peer,
    open: Mutex<HashMap<SessionId, Arc<Open>>>,
    /// Input closed: no session may be opened any more (set under `open`).
    closed: AtomicBool,
    /// Cancelled when input closes: aborts MCP mounts still connecting
    /// (dropping a half-made mount kills its stdio children).
    shutdown: tokio_util::sync::CancellationToken,
    wake_tx: mpsc::UnboundedSender<SessionId>,
    wake_rx: Mutex<Option<mpsc::UnboundedReceiver<SessionId>>>,
    out_rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    subscriptions: Mutex<Vec<Disposer>>,
}

impl Drop for AcpAgent {
    fn drop(&mut self) {
        for dispose in self.subscriptions.get_mut().unwrap().drain(..) {
            dispose();
        }
    }
}

impl AcpAgent {
    pub fn new(sessions: Arc<SessionService>, config: AcpConfig) -> Arc<Self> {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let agent = Arc::new(Self {
            sessions,
            config,
            peer: Peer::new(out_tx),
            open: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            shutdown: Default::default(),
            wake_tx,
            wake_rx: Mutex::new(Some(wake_rx)),
            out_rx: Mutex::new(Some(out_rx)),
            subscriptions: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&agent);
        let sub = agent.sessions.bus().on::<FrameEv>(move |frame| {
            if let Some(agent) = weak.upgrade() {
                agent.on_frame(frame);
            }
        });
        agent.subscriptions.lock().unwrap().push(sub);
        agent
    }

    /// Serve one ACP connection until the client closes its input.
    pub async fn serve<R, W>(self: &Arc<Self>, reader: R, mut writer: W) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut out_rx = self
            .out_rx
            .lock()
            .unwrap()
            .take()
            .expect("an AcpAgent serves one connection");
        let writer_task = tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                writer.write_all(line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await?;
            }
            Ok::<_, std::io::Error>(())
        });
        let mut wake_rx = self.wake_rx.lock().unwrap().take().expect("one pump");
        let pump = {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                while let Some(session) = wake_rx.recv().await {
                    let Some(agent) = weak.upgrade() else { break };
                    if let Some(open) = agent.opened(&session) {
                        open.wake.store(false, Ordering::SeqCst);
                        agent.flush(&session, &open);
                    }
                }
            })
        };

        let mut lines = BufReader::new(reader).lines();
        let result = loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            };
            if line.trim().is_empty() {
                continue;
            }
            self.dispatch_line(&line);
        };

        // Input closed: stop every turn this connection started, then let
        // the writer drain what is already queued.
        let open: Vec<_> = {
            let mut open = self.open.lock().unwrap();
            self.closed.store(true, Ordering::SeqCst);
            open.drain().collect()
        };
        self.shutdown.cancel();
        for (session, open) in open {
            self.sessions.cancel(&session);
            self.release(&session, &open).await;
        }
        pump.abort();
        self.peer.close();
        let _ = writer_task.await;
        result
    }

    fn dispatch_line(self: &Arc<Self>, line: &str) {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(e) => {
                self.peer.respond(
                    Value::Null,
                    Err(RpcError::new(rpc::PARSE_ERROR, format!("Parse error: {e}"))),
                );
                return;
            }
        };
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to one of our requests.
            match id {
                Some(id) => {
                    let result = match message.get("error") {
                        Some(error) => Err(rpc::error_from_json(error)),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    self.peer.resolve(&id, result);
                }
                None => self.peer.respond(
                    Value::Null,
                    Err(RpcError::new(rpc::INVALID_REQUEST, "Invalid request")),
                ),
            }
            return;
        };
        let method = method.to_string();
        let params = message.get("params").cloned().unwrap_or(json!({}));
        match id {
            None => self.notification(&method, &params),
            Some(id) => {
                let agent = Arc::clone(self);
                tokio::spawn(async move {
                    let result = agent.request(&method, params).await;
                    agent.peer.respond(id, result);
                });
            }
        }
    }

    fn notification(&self, method: &str, params: &Value) {
        if method == "session/cancel" {
            if let Some(session) = params.get("sessionId").and_then(Value::as_str) {
                self.cancel(session);
            }
        }
        // Unknown notifications (including `$/cancel_request`) are ignored.
    }

    async fn request(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(self.initialize(&params)),
            "authenticate" => Ok(json!({})),
            "session/new" => self.new_session(&params).await,
            "session/load" => self.load_session(&params, true).await,
            "session/resume" => self.load_session(&params, false).await,
            "session/list" => self.list_sessions(&params),
            "session/close" => self.close_session(&params).await,
            "session/prompt" => self.prompt(&params).await,
            "session/set_config_option" => self.set_config_option(&params),
            other => Err(RpcError::method_not_found(other)),
        }
    }

    // -- handshake ---------------------------------------------------------

    fn initialize(&self, _params: &Value) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentInfo": { "name": self.config.name, "version": self.config.version },
            "agentCapabilities": {
                "loadSession": true,
                "mcpCapabilities": { "http": true, "sse": false },
                "promptCapabilities": {
                    "image": self.config.images,
                    "audio": false,
                    "embeddedContext": true,
                },
                "sessionCapabilities": { "close": {}, "list": {}, "resume": {} },
            },
            "authMethods": [],
        })
    }

    // -- session setup -----------------------------------------------------

    async fn new_session(&self, params: &Value) -> Result<Value, RpcError> {
        let cwd = workspace_params(params)?;
        // Validate and connect every MCP server before the session exists,
        // so a failure leaves nothing behind (dsh: mounted before publish).
        let mounted = self.mount(params, &cwd).await?;
        let sessions = Arc::clone(&self.sessions);
        let created = tokio::task::spawn_blocking(move || sessions.create(Some(cwd)))
            .await
            .map_err(RpcError::internal)
            .and_then(|created| created.map_err(service_error));
        let session = match created {
            Ok(session) => session,
            Err(error) => {
                if let Some(mounted) = mounted {
                    mounted.close().await;
                }
                return Err(error);
            }
        };
        let options = match self.config_options(&session) {
            Ok(options) => options,
            Err(error) => {
                if let Some(mounted) = mounted {
                    mounted.close().await;
                }
                return Err(error);
            }
        };
        self.track(&session, mounted).await?;
        Ok(json!({ "sessionId": session, "configOptions": options }))
    }

    /// Validate `params.mcpServers` and connect them. `None` = no servers.
    async fn mount(&self, params: &Value, cwd: &str) -> Result<Option<mcp::Mounted>, RpcError> {
        let specs = mcp::parse(
            params.get("mcpServers"),
            std::path::Path::new(cwd),
            self.config.mcp_timeout,
        )?;
        if specs.is_empty() {
            return Ok(None);
        }
        let mounted = tokio::select! {
            mounted = mcp::Mounted::connect(specs, self.sessions.tool_registry()) => mounted?,
            () = self.shutdown.cancelled() => {
                return Err(RpcError::internal("connection closed while starting MCP servers"));
            }
        };
        Ok((!mounted.is_empty()).then_some(mounted))
    }

    async fn load_session(&self, params: &Value, replay: bool) -> Result<Value, RpcError> {
        let cwd = workspace_params(params)?;
        let session = session_param(params)?;
        if self.opened(&session).is_some() || self.sessions.phase(&session) != Phase::Idle {
            return Err(RpcError::invalid_params(format!(
                "session is already active: {session}"
            )));
        }
        let not_resumable =
            || RpcError::invalid_params(format!("session is not resumable: {session}"));
        // Subagent sessions belong to their parent's tree, not to a client.
        if !matches!(self.sessions.store().delegation(&session), Ok(None)) {
            return Err(not_resumable());
        }
        let workspace = self
            .sessions
            .store()
            .workspace(&session)
            .map_err(|_| not_resumable())?;
        if !same_directory(workspace.as_deref(), &cwd) {
            return Err(RpcError::invalid_params(format!(
                "session cwd does not match: {cwd}"
            )));
        }
        let mounted = self.mount(params, &cwd).await?;
        if replay {
            let history = match self.sessions.store().history(&session) {
                Ok(history) => history,
                Err(error) => {
                    if let Some(mounted) = mounted {
                        mounted.close().await;
                    }
                    return Err(branch_error(error));
                }
            };
            let translate = self.translator(&session, true, workspace.as_deref());
            for envelope in &history {
                for update in translate.updates(envelope) {
                    self.update(&session, update);
                }
            }
        }
        let options = match self.config_options(&session) {
            Ok(options) => options,
            Err(error) => {
                if let Some(mounted) = mounted {
                    mounted.close().await;
                }
                return Err(error);
            }
        };
        self.track(&session, mounted).await?;
        Ok(json!({ "configOptions": options }))
    }

    fn list_sessions(&self, params: &Value) -> Result<Value, RpcError> {
        let cwd = match params.get("cwd").and_then(Value::as_str) {
            Some(cwd) if !std::path::Path::new(cwd).is_absolute() => {
                return Err(RpcError::invalid_params(format!(
                    "cwd must be an absolute path: {cwd}"
                )))
            }
            other => other.map(str::to_owned),
        };
        let start = match params.get("cursor").and_then(Value::as_str) {
            Some(cursor) => cursor
                .parse::<usize>()
                .map_err(|_| RpcError::invalid_params("invalid cursor"))?,
            None => 0,
        };
        let store = self.sessions.store();
        let mut ids = store.list_roots().map_err(branch_error)?;
        // ULIDs sort by creation time: newest first.
        ids.reverse();
        let mut matching = Vec::new();
        for id in ids {
            if self.opened(&id).is_some() || self.sessions.phase(&id) != Phase::Idle {
                continue;
            }
            let Ok(Some(workspace)) = store.workspace(&id) else {
                continue;
            };
            if cwd
                .as_deref()
                .is_some_and(|cwd| !same_directory(Some(&workspace), cwd))
            {
                continue;
            }
            matching.push((id, workspace));
        }
        let page: Vec<Value> = matching
            .iter()
            .skip(start)
            .take(LIST_PAGE)
            .map(|(id, workspace)| {
                let mut info = json!({ "sessionId": id, "cwd": workspace });
                if let Ok(Some(title)) = self.sessions.title(id) {
                    info["title"] = json!(title);
                }
                info
            })
            .collect();
        let mut result = json!({ "sessions": page });
        if start + LIST_PAGE < matching.len() {
            result["nextCursor"] = json!((start + LIST_PAGE).to_string());
        }
        Ok(result)
    }

    async fn close_session(&self, params: &Value) -> Result<Value, RpcError> {
        let session = session_param(params)?;
        let open = self.open.lock().unwrap().remove(&session);
        if let Some(open) = open {
            if open.prompting.load(Ordering::SeqCst) {
                open.cancelled.store(true, Ordering::SeqCst);
                self.sessions.cancel(&session);
            }
            self.release(&session, &open).await;
        }
        Ok(json!({}))
    }

    /// Detach the session's MCP tools and stop its servers.
    async fn release(&self, session: &SessionId, open: &Open) {
        if let Some(mounted) = &open.mcp {
            self.sessions.clear_session_tools(session);
            mounted.close().await;
        }
    }

    // -- model selector ----------------------------------------------------

    fn config_options(&self, session: &SessionId) -> Result<Value, RpcError> {
        let config = self.sessions.config(session).map_err(service_error)?;
        Ok(config_options(
            &self.sessions.model_names(),
            &config,
            config
                .selection
                .as_ref()
                .and_then(|selection| self.sessions.model_capabilities(selection)),
        ))
    }

    fn set_config_option(&self, params: &Value) -> Result<Value, RpcError> {
        let session = session_param(params)?;
        self.require_open(&session)?;
        let value = params
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("value must be a string"))?;
        let mut config = self.sessions.config(&session).map_err(service_error)?;
        match params.get("configId").and_then(Value::as_str) {
            Some(MODEL_OPTION) => {
                let (route, model) = value
                    .split_once('/')
                    .filter(|(route, model)| !route.is_empty() && !model.is_empty())
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!("unknown model option: {value}"))
                    })?;
                let names = self.sessions.model_names();
                let current = config.selection.as_ref().map(selection_value);
                if !names.iter().any(|name| name == value) && current.as_deref() != Some(value) {
                    return Err(RpcError::invalid_params(format!(
                        "unknown model option: {value}"
                    )));
                }
                let selection = ModelSelection {
                    route: route.into(),
                    model: model.into(),
                };
                // Keep the reasoning choice only while the new model declares it.
                let efforts = self
                    .sessions
                    .model_capabilities(&selection)
                    .and_then(|caps| caps.reasoning.as_ref())
                    .and_then(|reasoning| reasoning.efforts.clone());
                if let Some(Reasoning::Effort { effort }) = &config.reasoning {
                    if efforts.is_none_or(|efforts| !efforts.contains(effort)) {
                        config.reasoning = None;
                    }
                }
                if matches!(config.reasoning, Some(Reasoning::BudgetTokens { .. })) {
                    config.reasoning = None;
                }
                config.selection = Some(selection);
            }
            Some(REASONING_OPTION) => {
                let efforts = config
                    .selection
                    .as_ref()
                    .and_then(|selection| self.sessions.model_capabilities(selection))
                    .and_then(|caps| caps.reasoning.as_ref())
                    .and_then(|reasoning| reasoning.efforts.clone())
                    .unwrap_or_default();
                config.reasoning = match value {
                    PROVIDER_DEFAULT => None,
                    effort if efforts.iter().any(|known| known == effort) => {
                        Some(Reasoning::Effort {
                            effort: effort.into(),
                        })
                    }
                    _ => {
                        return Err(RpcError::invalid_params(format!(
                            "unknown reasoning effort: {value}"
                        )))
                    }
                };
            }
            Some(other) => {
                return Err(RpcError::invalid_params(format!(
                    "unknown session config option: {other}"
                )))
            }
            None => return Err(RpcError::invalid_params("configId is required")),
        }
        config.profile = None;
        self.sessions
            .set_config(&session, config)
            .map_err(service_error)?;
        Ok(json!({ "configOptions": self.config_options(&session)? }))
    }

    // -- prompt turn -------------------------------------------------------

    async fn prompt(self: &Arc<Self>, params: &Value) -> Result<Value, RpcError> {
        let session = session_param(params)?;
        let open = self.require_open(&session)?;
        let blocks = params
            .get("prompt")
            .and_then(Value::as_array)
            .ok_or_else(|| RpcError::invalid_params("prompt must be an array"))?;
        let content = self.prompt_content(&session, blocks)?;
        if open.prompting.swap(true, Ordering::SeqCst) {
            return Err(RpcError::invalid_params(
                "a prompt is already in flight for this session",
            ));
        }
        let _done = ClearOnDrop(&open.prompting);
        open.cancelled.store(false, Ordering::SeqCst);
        // Everything committed before this prompt was already reported.
        self.reset_cursor(&session, &open);

        // Subscribe before sending so the end of the burst cannot be missed.
        let (end_tx, mut end_rx) = mpsc::unbounded_channel::<Option<TurnOutcome>>();
        let bus = self.sessions.bus();
        let (turn_sub, idle_sub) = {
            let (s1, tx1) = (session.clone(), end_tx.clone());
            let turn = bus.on::<TurnEndedEv>(move |notice| {
                if notice.session == s1 {
                    let _ = tx1.send(Some(notice.outcome));
                }
            });
            let (s2, tx2) = (session.clone(), end_tx);
            let idle = bus.on::<SessionIdleEv>(move |idle| {
                if *idle == s2 {
                    let _ = tx2.send(None);
                }
            });
            (turn, idle)
        };
        let _subs = DisposeOnDrop(vec![turn_sub, idle_sub]);

        let disposition = self
            .sessions
            .send_async(session.clone(), UserIntent::Followup, content)
            .await;
        let disposition = match disposition {
            Ok(disposition) => disposition,
            Err(e) => return Err(service_error(e)),
        };
        let mut outcome = None;
        match disposition {
            Disposition::Command(result) => {
                self.flush(&session, &open);
                if !result.message.is_empty() {
                    self.update(
                        &session,
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": result.message },
                        }),
                    );
                }
                return Ok(json!({ "stopReason": "end_turn" }));
            }
            Disposition::LogOnly => {}
            Disposition::StartTurn | Disposition::Queued => loop {
                match end_rx.recv().await {
                    Some(Some(turn)) => outcome = Some(turn),
                    // An idle from a burst that ended just before ours
                    // started must not end this prompt early.
                    Some(None) if self.sessions.phase(&session) != Phase::Idle => {}
                    Some(None) | None => break,
                }
            },
        }
        // The burst is over: report the rest of the log, then answer.
        let committed = self.flush(&session, &open);
        if open.cancelled.load(Ordering::SeqCst) || outcome == Some(TurnOutcome::Cancelled) {
            return Ok(json!({ "stopReason": "cancelled" }));
        }
        if outcome == Some(TurnOutcome::Failed) {
            let reason = committed
                .iter()
                .rev()
                .find_map(|event| match event {
                    SessionEvent::AssistantAttempt(attempt) => match &attempt.outcome {
                        AttemptOutcome::Error { message, .. } => Some(message.clone()),
                        AttemptOutcome::Cancelled => None,
                    },
                    _ => None,
                })
                .unwrap_or_else(|| "see the rness log".into());
            return Err(RpcError::internal(format!("turn failed: {reason}")));
        }
        let max_tokens = committed.iter().rev().find_map(|event| match event {
            SessionEvent::AssistantMessage(message) => Some(message.stop == StopReason::MaxTokens),
            _ => None,
        });
        Ok(json!({
            "stopReason": if max_tokens == Some(true) { "max_tokens" } else { "end_turn" }
        }))
    }

    fn cancel(&self, session: &str) {
        if let Some(open) = self.opened(session) {
            if open.prompting.load(Ordering::SeqCst) {
                open.cancelled.store(true, Ordering::SeqCst);
                self.sessions.cancel(&session.to_string());
            }
        }
    }

    fn prompt_content(
        &self,
        session: &SessionId,
        blocks: &[Value],
    ) -> Result<Vec<ContentPart>, RpcError> {
        // Validate every block before admitting any image, so a bad block
        // never leaves attachments behind.
        let mut parts = Vec::new();
        for block in blocks {
            parts.push(prompt_part(block, self.config.images)?);
        }
        if parts.iter().any(|part| matches!(part, Part::Image(..))) {
            self.require_image_model(session)?;
        }
        let mut content: Vec<ContentPart> = Vec::new();
        for part in parts {
            match part {
                // Adjacent text blocks become one part, like dsh.
                Part::Text(text) => match content.last_mut() {
                    Some(ContentPart::Text { text: last }) => last.push_str(&text),
                    _ => content.push(ContentPart::Text { text }),
                },
                Part::Image(bytes, mime) => {
                    let attachment = self
                        .sessions
                        .admit_image(session, &bytes, mime)
                        .map_err(service_error)?;
                    content.push(ContentPart::Image { attachment });
                }
            }
        }
        let meaningful = content.iter().any(|part| match part {
            ContentPart::Text { text } => !text.trim().is_empty(),
            _ => true,
        });
        if !meaningful {
            return Err(RpcError::invalid_params("empty prompt"));
        }
        Ok(content)
    }

    /// Image prompts need a model that does not declare `image_input = false`.
    fn require_image_model(&self, session: &SessionId) -> Result<(), RpcError> {
        let selection = self
            .sessions
            .config(session)
            .map_err(service_error)?
            .selection;
        let refused = selection
            .as_ref()
            .and_then(|selection| self.sessions.model_capabilities(selection))
            .is_some_and(|caps| caps.image_input == Some(false));
        if refused {
            let name = selection.as_ref().map(selection_value).unwrap_or_default();
            return Err(RpcError::invalid_params(format!(
                "model \"{name}\" does not declare image input"
            )));
        }
        Ok(())
    }

    // -- update plumbing ---------------------------------------------------

    fn update(&self, session: &str, update: Value) {
        self.peer.notify(
            "session/update",
            json!({ "sessionId": session, "update": update }),
        );
    }

    fn on_frame(&self, frame: &Frame) {
        let session = match frame {
            Frame::StepStarted { session, .. }
            | Frame::StepCommitted { session, .. }
            | Frame::ToolStarted { session, .. }
            | Frame::HistoryChanged { session }
            | Frame::TurnIdle { session } => session,
            Frame::TitleChanged { session, title } => {
                if self.opened(session).is_some() {
                    self.update(
                        session,
                        json!({ "sessionUpdate": "session_info_update", "title": title }),
                    );
                }
                return;
            }
            _ => return,
        };
        if let Some(open) = self.opened(session) {
            if open.prompting.load(Ordering::SeqCst) && !open.wake.swap(true, Ordering::SeqCst) {
                let _ = self.wake_tx.send(session.clone());
            }
        }
    }

    fn translator<'a>(
        &'a self,
        session: &'a SessionId,
        include_user: bool,
        workspace: Option<&'a str>,
    ) -> updates::Translate<'a> {
        let context_window = self
            .sessions
            .config(session)
            .ok()
            .and_then(|config| config.selection)
            .and_then(|selection| self.sessions.model_capabilities(&selection).cloned())
            .and_then(|caps| caps.context_window);
        updates::Translate {
            include_user,
            workspace,
            context_window,
            images: Box::new(move |id| self.sessions.read_image(session, id).ok()),
        }
    }

    fn reset_cursor(&self, session: &SessionId, open: &Open) {
        let last = self
            .sessions
            .store()
            .read_session(session)
            .ok()
            .and_then(|events| events.last().map(|e| e.id.clone()));
        *open.cursor.lock().unwrap() = last;
    }

    /// Report every event committed since the cursor; returns them.
    fn flush(&self, session: &SessionId, open: &Open) -> Vec<SessionEvent> {
        let mut cursor = open.cursor.lock().unwrap();
        let fresh = match cursor.as_deref() {
            Some(after) => self.sessions.store().history_after(session, after),
            None => self
                .sessions
                .store()
                .read_session(session)
                .map(Some)
                .map_err(Into::into),
        };
        let fresh = match fresh {
            Ok(Some(fresh)) => fresh,
            Ok(None) | Err(_) => return Vec::new(),
        };
        let Some(last) = fresh.last() else {
            return Vec::new();
        };
        *cursor = Some(last.id.clone());
        let workspace = self.sessions.store().workspace(session).ok().flatten();
        let translate = self.translator(session, false, workspace.as_deref());
        for envelope in &fresh {
            for update in translate.updates(envelope) {
                self.update(session, update);
            }
        }
        drop(translate);
        // A slash command (e.g. /model) changed the session's config.
        if fresh
            .iter()
            .any(|envelope| matches!(envelope.event, SessionEvent::RequestConfig(_)))
        {
            if let Ok(options) = self.config_options(session) {
                self.update(
                    session,
                    json!({ "sessionUpdate": "config_option_update", "configOptions": options }),
                );
            }
        }
        fresh
            .iter()
            .map(|envelope| envelope.event.clone())
            .collect()
    }

    // -- bookkeeping -------------------------------------------------------

    fn opened(&self, session: &str) -> Option<Arc<Open>> {
        self.open.lock().unwrap().get(session).cloned()
    }

    fn require_open(&self, session: &str) -> Result<Arc<Open>, RpcError> {
        self.opened(session)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown session: {session}")))
    }

    /// Open `session` on this connection with its MCP servers. A racing
    /// open of the same session (two concurrent loads) loses and has its
    /// servers stopped.
    async fn track(&self, session: &SessionId, mcp: Option<mcp::Mounted>) -> Result<(), RpcError> {
        let rejected = {
            let mut open = self.open.lock().unwrap();
            match open.entry(session.clone()) {
                _ if self.closed.load(Ordering::SeqCst) => Some(mcp),
                std::collections::hash_map::Entry::Occupied(_) => Some(mcp),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    if let Some(mounted) = &mcp {
                        self.sessions
                            .set_session_tools(session, Arc::clone(&mounted.registry));
                    }
                    slot.insert(Arc::new(Open {
                        mcp,
                        ..Open::default()
                    }));
                    None
                }
            }
        };
        match rejected {
            None => Ok(()),
            Some(mounted) => {
                if let Some(mounted) = mounted {
                    mounted.close().await;
                }
                Err(RpcError::invalid_params(format!(
                    "session is already active: {session}"
                )))
            }
        }
    }

    /// The opened session a (possibly delegated) session belongs to.
    fn owner(&self, session: &SessionId) -> Option<SessionId> {
        let mut current = session.clone();
        for _ in 0..64 {
            if self.opened(&current).is_some() {
                return Some(current);
            }
            current = self.sessions.store().delegation(&current).ok()??.parent;
        }
        None
    }
}

#[async_trait]
impl Answerer for AcpAgent {
    async fn answer(&self, request: &ApprovalRequest) -> Decision {
        let Some(owner) = self.owner(&request.session) else {
            return Decision::Unavailable;
        };
        // The client must see the tool call before the question about it.
        if owner == request.session {
            if let Some(open) = self.opened(&owner) {
                self.flush(&owner, &open);
            }
        }
        let mut tool_call = {
            let workspace = self
                .sessions
                .store()
                .workspace(&request.session)
                .ok()
                .flatten();
            let translate = updates::Translate {
                include_user: false,
                workspace: workspace.as_deref(),
                context_window: None,
                images: Box::new(|_| None),
            };
            translate.tool_call(&request.call, &request.tool, &request.args)
        };
        if let Some(object) = tool_call.as_object_mut() {
            object.remove("sessionUpdate");
            object.insert("status".into(), json!("pending"));
        }
        let response = self
            .peer
            .request(
                "session/request_permission",
                json!({
                    "sessionId": owner,
                    "toolCall": tool_call,
                    "options": [
                        { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                        { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" },
                    ],
                }),
            )
            .await;
        let Ok(response) = response else {
            return Decision::Unavailable;
        };
        let outcome = &response["outcome"];
        match (outcome["outcome"].as_str(), outcome["optionId"].as_str()) {
            (Some("cancelled"), _) => Decision::Cancelled,
            (Some("selected"), Some("allow-once")) => Decision::Allowed,
            (Some("selected"), Some("reject-once")) => Decision::Rejected,
            // Anything else never becomes a grant.
            _ => Decision::Rejected,
        }
    }
}

struct ClearOnDrop<'a>(&'a AtomicBool);

impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

struct DisposeOnDrop(Vec<Disposer>);

impl Drop for DisposeOnDrop {
    fn drop(&mut self) {
        for dispose in self.0.drain(..) {
            dispose();
        }
    }
}

// -- prompt content ------------------------------------------------------------

enum Part {
    Text(String),
    Image(Vec<u8>, &'static str),
}

const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

fn prompt_part(block: &Value, images: bool) -> Result<Part, RpcError> {
    let field = |key: &str| block.get(key).and_then(Value::as_str);
    match field("type").unwrap_or("") {
        "text" => field("text")
            .map(|text| Part::Text(text.into()))
            .ok_or_else(|| RpcError::invalid_params("text block without text")),
        "resource_link" => {
            let uri = field("uri")
                .ok_or_else(|| RpcError::invalid_params("resource_link without uri"))?;
            let name = field("name").unwrap_or(uri);
            Ok(Part::Text(format!("[{name}]({uri})")))
        }
        "resource" => {
            let resource = block.get("resource").unwrap_or(&Value::Null);
            let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
            let body = resource
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("only text resources are supported"))?;
            Ok(Part::Text(format!(
                "<resource uri=\"{uri}\">\n{body}\n</resource>"
            )))
        }
        "image" => {
            if !images {
                return Err(RpcError::invalid_params(
                    "inline image prompts were not advertised by this connection",
                ));
            }
            let mime = field("mimeType")
                .and_then(|mime| IMAGE_TYPES.iter().find(|known| **known == mime))
                .ok_or_else(|| {
                    RpcError::invalid_params(
                        "image mimeType must be image/png, image/jpeg, image/webp, or image/gif",
                    )
                })?;
            let data = field("data").unwrap_or("");
            let engine = &base64::engine::general_purpose::STANDARD;
            let bytes = engine
                .decode(data)
                .ok()
                .filter(|bytes| !bytes.is_empty() && engine.encode(bytes) == data)
                .ok_or_else(|| RpcError::invalid_params("image data must be canonical base64"))?;
            Ok(Part::Image(bytes, mime))
        }
        "audio" => Err(RpcError::invalid_params(
            "audio prompt content is not supported",
        )),
        other => Err(RpcError::invalid_params(format!(
            "unsupported prompt content: {other}"
        ))),
    }
}

// -- config options ------------------------------------------------------------

const MODEL_OPTION: &str = "model";
const REASONING_OPTION: &str = "reasoning_effort";
/// Opaque value for "no explicit effort"; efforts are never empty strings.
const PROVIDER_DEFAULT: &str = "";

fn selection_value(selection: &ModelSelection) -> String {
    format!("{}/{}", selection.route, selection.model)
}

/// ACP config options for one session: the model selector, grouped by
/// provider (like dsh), plus a reasoning-effort selector when the selected
/// model declares its efforts.
fn config_options(
    catalog: &[String],
    config: &CallConfig,
    caps: Option<&ModelCapabilities>,
) -> Value {
    let current = config.selection.as_ref().map(selection_value);
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    let mut push = |value: &str, front: bool| {
        let Some((provider, model)) = value.split_once('/') else {
            return;
        };
        let option = json!({ "value": value, "name": model });
        match groups.iter_mut().find(|(group, _)| group == provider) {
            Some((_, options)) if front => options.insert(0, option),
            Some((_, options)) => options.push(option),
            None => groups.push((provider.to_string(), vec![option])),
        }
    };
    for name in catalog {
        push(name, false);
    }
    if let Some(current) = &current {
        if !catalog.contains(current) {
            push(current, true);
        }
    }
    let Some(current) = current.or_else(|| catalog.first().cloned()) else {
        return json!([]);
    };
    let groups: Vec<Value> = groups
        .into_iter()
        .map(|(group, options)| json!({ "group": group, "name": group, "options": options }))
        .collect();
    let mut options = vec![json!({
        "id": MODEL_OPTION,
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": current,
        "options": groups,
    })];
    let efforts = caps
        .and_then(|caps| caps.reasoning.as_ref())
        .and_then(|reasoning| reasoning.efforts.as_ref())
        .filter(|efforts| !efforts.is_empty());
    if let Some(efforts) = efforts {
        let current = match &config.reasoning {
            Some(Reasoning::Effort { effort }) => effort.as_str(),
            _ => PROVIDER_DEFAULT,
        };
        let mut choices = vec![json!({ "value": PROVIDER_DEFAULT, "name": "Provider default" })];
        choices.extend(
            efforts
                .iter()
                .map(|effort| json!({ "value": effort, "name": effort })),
        );
        options.push(json!({
            "id": REASONING_OPTION,
            "name": "Reasoning effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": current,
            "options": choices,
        }));
    }
    Value::Array(options)
}

// -- parameter helpers -------------------------------------------------------

fn session_param(params: &Value) -> Result<SessionId, RpcError> {
    let session = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("sessionId is required"))?;
    // Session ids name directories under the store root: never a path.
    if session.is_empty()
        || session.len() > 64
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(RpcError::invalid_params(format!(
            "unknown session: {session}"
        )));
    }
    Ok(session.to_string())
}

fn workspace_params(params: &Value) -> Result<String, RpcError> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("cwd is required"))?;
    if !std::path::Path::new(cwd).is_absolute() {
        return Err(RpcError::invalid_params(format!(
            "cwd must be an absolute path: {cwd}"
        )));
    }
    if params
        .get("additionalDirectories")
        .and_then(Value::as_array)
        .is_some_and(|dirs| !dirs.is_empty())
    {
        return Err(RpcError::invalid_params(
            "additionalDirectories is not supported",
        ));
    }
    Ok(cwd.to_string())
}

fn same_directory(workspace: Option<&str>, cwd: &str) -> bool {
    let Some(workspace) = workspace else {
        return false;
    };
    if workspace == cwd {
        return true;
    }
    match (std::fs::canonicalize(workspace), std::fs::canonicalize(cwd)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn branch_error(error: BranchError) -> RpcError {
    match error {
        BranchError::Log(LogError::NotFound(session)) => {
            RpcError::invalid_params(format!("unknown session: {session}"))
        }
        other => RpcError::internal(other),
    }
}

fn service_error(error: ServiceError) -> RpcError {
    match error {
        ServiceError::Branch(error) => branch_error(error),
        ServiceError::Log(LogError::NotFound(session)) => {
            RpcError::invalid_params(format!("unknown session: {session}"))
        }
        ServiceError::InvalidConfig(message) => RpcError::invalid_params(message),
        other => RpcError::internal(other),
    }
}
