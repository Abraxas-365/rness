//! The plugin host: an actor thread that owns the single Lua VM.
//!
//! mlua's Lua is not Sync and registry keys are VM-bound, so the VM
//! lives on one dedicated thread; everyone else talks to it through a
//! cloneable [`LuaHost`] handle over a command channel. This is the v2
//! answer to v1's dual-VM/replay design: one VM, message passing, no
//! neutralized APIs.

#[async_trait::async_trait]
impl rness_kernel::presentation::TextProvider for LuaHost {
    async fn status(&self, context: serde_json::Value) -> Option<serde_json::Value> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::StatusView { context, reply }).ok()?;
        rx.await.ok().flatten()
    }

    async fn text(&self) -> Option<String> {
        self.statusline().await
    }
}

#[async_trait::async_trait]
impl rness_tools::web::WebHooks for LuaHost {
    async fn transform(
        &self,
        operation: &str,
        phase: &str,
        value: serde_json::Value,
        context: serde_json::Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, String> {
        let token = cancel.child_token();
        let _guard = token.clone().drop_guard();
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::WebTransform {
                operation: operation.into(),
                phase: phase.into(),
                value,
                context,
                cancel: token.clone(),
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        tokio::select! {
            biased;
            _ = token.cancelled() => Err("web hook cancelled".into()),
            result = tokio::time::timeout(std::time::Duration::from_secs(30), rx) => result.map_err(|_| "web hook timed out")?.map_err(|_| "lua vm gone")?,
        }
    }
}

impl rness_kernel::presentation::HookSink for LuaHost {
    fn fire_hook(&self, event: &str, payload: serde_json::Value) {
        LuaHost::fire_hook(self, event, payload);
    }
}

/// Upper bound for one interception chain; a stuck handler fails its call.
const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Delegating ancestors of `session`, nearest first. Depth-bounded so a
/// corrupt header cycle cannot hang the actor.
fn delegation_lineage(
    sessions: &rness_engine::service::SessionService,
    session: &str,
) -> Vec<String> {
    let mut lineage = Vec::new();
    let mut current = session.to_owned();
    while lineage.len() < 64 {
        match sessions.store().delegation(&current) {
            Ok(Some(delegation)) => {
                current = delegation.parent.clone();
                lineage.push(delegation.parent);
            }
            _ => break,
        }
    }
    lineage
}

/// Add delegation `parent` + `lineage` (nearest first) for opts.agent scoping.
fn add_lineage(binding: Option<&SessionBinding>, payload: &mut serde_json::Value) {
    let Some(binding) = binding else { return };
    let Some(session) = payload.get("session").and_then(|s| s.as_str()).map(str::to_owned) else { return };
    let lineage = delegation_lineage(&binding.sessions, &session);
    payload["parent"] = lineage.first().cloned().map_or(serde_json::Value::Null, serde_json::Value::String);
    payload["lineage"] = serde_json::json!(lineage);
}

fn hook_payload(event: &rness_engine::tools::hooks::ToolHookEvent) -> serde_json::Value {
    serde_json::json!({
        "session": event.session,
        "call": event.call,
        "tool": event.tool,
        "args": event.args,
        "turn": event.turn,
    })
}

/// `"text"` or `{ "text", {type="text", text=...}, {kind="image", ...} }`.
fn hook_content(
    value: &serde_json::Value,
    field: &str,
) -> Result<Vec<rness_protocol::events::ToolResultContentPart>, String> {
    use rness_protocol::events::ToolResultContentPart as Part;
    match value {
        serde_json::Value::String(text) => Ok(vec![Part::Text { text: text.clone() }]),
        serde_json::Value::Array(parts) => parts
            .iter()
            .map(|part| match part {
                serde_json::Value::String(text) => Ok(Part::Text { text: text.clone() }),
                serde_json::Value::Object(map) if map.get("type").and_then(|t| t.as_str()) == Some("text") => map
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|text| Part::Text { text: text.into() })
                    .ok_or_else(|| format!("{field}: text part needs a string 'text'")),
                other => serde_json::from_value(other.clone()).map_err(|e| format!("{field}: invalid content part: {e}")),
            })
            .collect(),
        _ => Err(format!("{field} must be a string or an array of content parts")),
    }
}

fn hook_contexts(decision: &serde_json::Value) -> Result<Vec<String>, String> {
    match decision.get("additional_contexts") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::String(text)) => Ok(vec![text.clone()]),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(|| "additional_contexts must be strings".to_string()))
            .collect(),
        Some(_) => Err("additional_contexts must be a string or an array of strings".into()),
    }
}

fn hook_kind(decision: &serde_json::Value) -> Result<&str, String> {
    decision
        .get("kind")
        .and_then(|k| k.as_str())
        .ok_or_else(|| "decision must be a table with a string 'kind'".to_string())
}

/// What `next()` returns to a `tool_execute` handler.
fn outcome_value(outcome: &rness_engine::tools::hooks::ExecuteOutcome) -> serde_json::Value {
    serde_json::json!({
        "content": outcome.content,
        "output": rness_protocol::events::ToolResult::text_output(&outcome.content),
        "is_error": outcome.is_error,
    })
}

/// `{content = "text" | parts, is_error = bool}`; `output` is accepted as
/// a text alias so returning `next()`'s table unchanged round-trips.
fn execute_outcome(value: &serde_json::Value) -> Result<rness_engine::tools::hooks::ExecuteOutcome, String> {
    let serde_json::Value::Object(map) = value else {
        return Err("tool_execute must return a table {content, is_error}".into());
    };
    let content = match (map.get("content"), map.get("output")) {
        // Lua cannot tell an empty array from an empty object.
        (Some(serde_json::Value::Object(o)), _) if o.is_empty() => Vec::new(),
        (Some(content), _) => hook_content(content, "content")?,
        (None, Some(output @ serde_json::Value::String(_))) => hook_content(output, "output")?,
        (None, _) => return Err("tool_execute result needs 'content' (string or parts)".into()),
    };
    let is_error = match map.get("is_error") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return Err("tool_execute result 'is_error' must be a boolean".into()),
    };
    Ok(rness_engine::tools::hooks::ExecuteOutcome { content, is_error })
}

/// Await one actor reply, bounded by `cancel` and [`HOOK_TIMEOUT`].
async fn hook_reply<T>(
    rx: tokio::sync::oneshot::Receiver<Result<T, String>>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("tool_execute hook cancelled".into()),
        result = tokio::time::timeout(HOOK_TIMEOUT, rx) => result.map_err(|_| "tool_execute hook timed out")?.map_err(|_| "lua vm gone")?,
    }
}

/// Drops a parked `tool_execute` coroutine on the actor (no-op once done).
struct ExecuteParking {
    tx: std::sync::Arc<mpsc::Sender<Cmd>>,
    id: u64,
}
impl Drop for ExecuteParking {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::ExecuteDrop { id: self.id });
    }
}

impl LuaHost {
    /// Whether any live handler is registered for `event`.
    pub fn has_hook(&self, event: &str) -> bool {
        self.hook_counts.has(event)
    }

    /// Set the event bus for durable hook audit events.
    pub fn set_bus(&mut self, bus: std::sync::Arc<rness_kernel::EventBus>) {
        self.bus = Some(bus);
    }

    /// Run the `event` interception chain on the VM actor. Bounded by
    /// `cancel` and [`HOOK_TIMEOUT`].
    pub async fn intercept(
        &self,
        event: &str,
        payload: serde_json::Value,
        default: serde_json::Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, String> {
        static HOOK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let start = std::time::Instant::now();
        let seq = HOOK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let handler_id = format!(
            "{}-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            seq,
            event,
        );

        let turn = payload.get("turn").and_then(|t| t.as_u64()).unwrap_or(0) as u32;
        let audit_session = payload
            .get("session")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();

        // Emit hook/invoked audit event.
        if let Some(bus) = &self.bus {
            let matcher = payload.get("tool").and_then(|t| t.as_str()).map(str::to_owned);
            bus.emit::<rness_engine::service::HookAuditEv>(
                &rness_engine::service::HookAuditNotice {
                    session: audit_session.clone(),
                    event: rness_engine::service::HookAuditEvent::Invoked(
                        rness_protocol::events::HookInvoked {
                            turn,
                            point: event.to_string(),
                            source: "lua".into(),
                            matcher,
                            handler_id: handler_id.clone(),
                        },
                    ),
                },
            );
        }

        let token = cancel.child_token();
        let _guard = token.clone().drop_guard();
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Intercept { event: event.into(), payload, default, cancel: token.clone(), reply })
            .map_err(|_| "lua vm gone")?;
        let result = tokio::select! {
            biased;
            _ = token.cancelled() => Err(format!("{event} hook cancelled")),
            result = tokio::time::timeout(HOOK_TIMEOUT, rx) => result.map_err(|_| format!("{event} hook timed out"))?.map_err(|_| "lua vm gone")?,
        };

        // Emit hook/result audit event.
        if let Some(bus) = &self.bus {
            let decision = match &result {
                Ok(v) => v.get("kind").and_then(|k| k.as_str()).unwrap_or("ok").to_string(),
                Err(e) => format!("error: {e}"),
            };
            bus.emit::<rness_engine::service::HookAuditEv>(
                &rness_engine::service::HookAuditNotice {
                    session: audit_session,
                    event: rness_engine::service::HookAuditEvent::Result(
                        rness_protocol::events::HookResult {
                            turn,
                            point: event.to_string(),
                            handler_id,
                            decision,
                            exit_code: None,
                            stderr_summary: None,
                            duration_ms: start.elapsed().as_millis() as u64,
                        },
                    ),
                },
            );
        }

        result
    }
}

#[async_trait::async_trait]
impl rness_engine::tools::hooks::ToolHooks for LuaHost {
    async fn pre_tool(
        &self,
        event: &rness_engine::tools::hooks::ToolHookEvent,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<rness_engine::tools::hooks::PreToolDecision, String> {
        use rness_engine::tools::hooks::PreToolDecision;
        if !self.has_hook("pre_tool") {
            return Ok(PreToolDecision::Allow);
        }
        let decision = self
            .intercept("pre_tool", hook_payload(event), serde_json::json!({"kind": "allow"}), cancel)
            .await?;
        let reason = decision.get("reason").and_then(|r| r.as_str()).map(str::to_owned);
        match hook_kind(&decision)? {
            "allow" => Ok(PreToolDecision::Allow),
            "deny" => Ok(PreToolDecision::Deny {
                reason: reason.unwrap_or_else(|| format!("{} was denied by a pre_tool hook", event.tool)),
            }),
            "ask" => Ok(PreToolDecision::Ask { reason }),
            other => Err(format!("pre_tool: unknown decision kind '{other}' (allow, deny, ask)")),
        }
    }

    async fn guard(
        &self,
        event: &rness_engine::tools::hooks::ToolHookEvent,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Option<String>, String> {
        if !self.has_hook(crate::runtime::GUARD_EVENT) {
            return Ok(None);
        }
        let decision = self
            .intercept(crate::runtime::GUARD_EVENT, hook_payload(event), serde_json::Value::Null, cancel)
            .await?;
        if decision.is_null() {
            return Ok(None);
        }
        match hook_kind(&decision)? {
            "abstain" | "allow" => Ok(None),
            "deny" => Ok(Some(
                decision
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{} was denied by a guard", event.tool)),
            )),
            other => Err(format!("guard: unknown decision kind '{other}' (deny or nil)")),
        }
    }

    async fn tool_execute(
        &self,
        event: &rness_engine::tools::hooks::ToolHookEvent,
        next: &rness_engine::tools::hooks::ExecuteNext,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<rness_engine::tools::hooks::ExecuteOutcome, String> {
        use crate::runtime::ExecuteStep;
        if !self.has_hook(crate::runtime::EXECUTE_EVENT) {
            return Ok(next().await);
        }
        let token = cancel.child_token();
        let _cancel_on_drop = token.clone().drop_guard();
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::ExecuteStart { payload: hook_payload(event), cancel: token.clone(), reply })
            .map_err(|_| "lua vm gone")?;
        let (id, mut step) = hook_reply(rx, &token).await?;
        // Release the parked coroutine however this future ends.
        let _park = ExecuteParking { tx: self.tx.clone(), id };
        let mut last = None;
        loop {
            match step {
                ExecuteStep::Body => {
                    let outcome = next().await;
                    let (reply, rx) = tokio::sync::oneshot::channel();
                    self.tx
                        .send(Cmd::ExecuteResume { id, outcome: outcome_value(&outcome), cancel: token.clone(), reply })
                        .map_err(|_| "lua vm gone")?;
                    last = Some(outcome);
                    step = match hook_reply(rx, &token).await {
                        // A body that already ran still commits its result.
                        Err(_) if token.is_cancelled() => return Ok(last.expect("body ran")),
                        other => other?,
                    };
                }
                ExecuteStep::Done(serde_json::Value::Null) => {
                    return last.ok_or_else(|| "tool_execute: the chain returned nil without calling next()".into());
                }
                ExecuteStep::Done(value) => return execute_outcome(&value),
            }
        }
    }

    async fn post_tool(
        &self,
        event: &rness_engine::tools::hooks::ToolHookEvent,
        result: &rness_protocol::events::ToolResult,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<rness_engine::tools::hooks::PostToolDecision, String> {
        use rness_engine::tools::hooks::PostToolDecision;
        if !self.has_hook("post_tool") {
            return Ok(PostToolDecision::default());
        }
        let mut payload = hook_payload(event);
        payload["result"] = serde_json::json!({
            "content": result.content,
            "output": result.output,
            "is_error": result.is_error,
            "duration_ms": result.duration_ms,
        });
        let decision = self
            .intercept("post_tool", payload, serde_json::json!({"kind": "accept"}), cancel)
            .await?;
        let additional_contexts = hook_contexts(&decision)?;
        match hook_kind(&decision)? {
            "accept" => {
                let content = match (decision.get("content"), decision.get("value")) {
                    (Some(_), Some(_)) => return Err("post_tool: accept takes content or value, not both".into()),
                    (Some(content), None) => Some(hook_content(content, "content")?),
                    (None, Some(serde_json::Value::String(text))) => Some(hook_content(&serde_json::Value::String(text.clone()), "value")?),
                    (None, Some(value)) => Some(hook_content(&serde_json::Value::String(value.to_string()), "value")?),
                    (None, None) => None,
                };
                Ok(PostToolDecision::Accept { content, additional_contexts })
            }
            "block" => {
                let feedback = decision
                    .get("feedback")
                    .ok_or_else(|| "post_tool: block needs 'feedback'".to_string())
                    .and_then(|feedback| hook_content(feedback, "feedback"))?;
                Ok(PostToolDecision::Block { feedback, additional_contexts })
            }
            other => Err(format!("post_tool: unknown decision kind '{other}' (accept, block)")),
        }
    }

    fn tool_result(
        &self,
        event: &rness_engine::tools::hooks::ToolHookEvent,
        result: &rness_protocol::events::ToolResult,
    ) {
        if !self.has_hook("tool_result") {
            return;
        }
        let mut payload = hook_payload(event);
        payload["result"] = serde_json::json!({
            "content": result.content,
            "output": result.output,
            "is_error": result.is_error,
            "duration_ms": result.duration_ms,
        });
        self.fire_hook("tool_result", payload);
    }
}

fn loop_payload(event: &rness_engine::turn::hooks::LoopEvent) -> serde_json::Value {
    serde_json::json!({
        "session": event.session,
        "turn": event.turn,
        "step": event.step,
    })
}

fn hook_messages(value: &serde_json::Value) -> Result<Vec<String>, String> {
    match value.get("messages") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::String(s)) => Ok(vec![s.clone()]),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "messages must be strings".to_string())
            })
            .collect(),
        Some(_) => Err("messages must be a string or an array of strings".into()),
    }
}

#[async_trait::async_trait]
impl rness_engine::turn::hooks::LoopHooks for LuaHost {
    async fn pre_step(
        &self,
        event: &rness_engine::turn::hooks::LoopEvent,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<rness_engine::turn::hooks::PreStepDecision, String> {
        use rness_engine::turn::hooks::PreStepDecision;
        if !self.has_hook("pre_step") {
            return Ok(PreStepDecision::Enter);
        }
        let decision = self
            .intercept("pre_step", loop_payload(event), serde_json::json!({"kind": "enter"}), cancel)
            .await?;
        match hook_kind(&decision)? {
            "enter" => {
                let messages = hook_messages(&decision)?;
                if messages.is_empty() {
                    Ok(PreStepDecision::Enter)
                } else {
                    Ok(PreStepDecision::EnterWithMessages { messages })
                }
            }
            "reject" => Ok(PreStepDecision::Reject),
            other => Err(format!("pre_step: unknown decision kind '{other}' (enter, reject)")),
        }
    }

    async fn request(
        &self,
        event: &rness_engine::turn::hooks::LoopEvent,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), String> {
        if !self.has_hook("request") {
            return Ok(());
        }
        let _ = self
            .intercept("request", loop_payload(event), serde_json::Value::Null, cancel)
            .await?;
        Ok(())
    }

    async fn request_error(
        &self,
        event: &rness_engine::turn::hooks::LoopEvent,
        error: &rness_engine::turn::hooks::RequestError,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<rness_engine::turn::hooks::RequestErrorAction, String> {
        use rness_engine::turn::hooks::RequestErrorAction;
        if !self.has_hook("request_error") {
            return Ok(RequestErrorAction::Default);
        }
        let mut payload = loop_payload(event);
        payload["error"] = serde_json::json!({
            "code": error.code,
            "message": error.message,
            "retryable": error.retryable,
            "attempt": error.attempt,
            "max_retries": error.max_retries,
        });
        let decision = self
            .intercept("request_error", payload, serde_json::Value::Null, cancel)
            .await?;
        match decision.get("kind").and_then(|k| k.as_str()) {
            Some("retry") => Ok(RequestErrorAction::Retry),
            _ => Ok(RequestErrorAction::Default),
        }
    }

    async fn turn_stopping(
        &self,
        event: &rness_engine::turn::hooks::LoopEvent,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<rness_engine::turn::hooks::TurnStoppingAction, String> {
        use rness_engine::turn::hooks::TurnStoppingAction;
        if !self.has_hook("turn_stopping") {
            return Ok(TurnStoppingAction::Stop);
        }
        let decision = self
            .intercept("turn_stopping", loop_payload(event), serde_json::json!({"kind": "stop"}), cancel)
            .await?;
        match hook_kind(&decision)? {
            "continue" => {
                let messages = hook_messages(&decision)?;
                if messages.is_empty() {
                    Ok(TurnStoppingAction::Stop)
                } else {
                    Ok(TurnStoppingAction::Continue { messages })
                }
            }
            "stop" => Ok(TurnStoppingAction::Stop),
            other => Err(format!("turn_stopping: unknown decision kind '{other}' (stop, continue)")),
        }
    }
}

#[async_trait::async_trait]
impl rness_kernel::presentation::Applications for LuaHost {
    async fn app_specs(&self) -> Vec<LuaAppSpec> {
        LuaHost::app_specs(self).await
    }
    async fn app_view(&self, name: &str, ctx: serde_json::Value) -> Result<Vec<String>, String> {
        LuaHost::app_view(self, name, ctx).await
    }
    async fn app_key(
        &self,
        name: &str,
        key: &str,
        ctx: serde_json::Value,
    ) -> Result<AppKeyOutcome, String> {
        LuaHost::app_key(self, name, key, ctx).await
    }
}

#[async_trait::async_trait]
impl rness_engine::presentation::ToolCards for LuaHost {
    async fn tool_card(
        &self,
        name: &str,
        args: serde_json::Value,
        output: &str,
        is_error: bool,
    ) -> Option<Vec<crate::runtime::StyledLine>> {
        LuaHost::tool_card(self, name, args, output, is_error).await
    }

    async fn tool_card_presented(
        &self,
        name: &str,
        args: serde_json::Value,
        output: &str,
        is_error: bool,
        presentation: Option<serde_json::Value>,
    ) -> Option<Vec<crate::runtime::StyledLine>> {
        LuaHost::tool_card_presented(self, name, args, output, is_error, presentation).await
    }
}

use std::sync::mpsc;

use crate::runtime::{AppKeyOutcome, LuaAppSpec, LuaRuntime, LuaToolSpec};

/// The engine services `rness.session` bridges to. Kept by the actor so
/// every fresh VM (hot reload) gets the same injection.
#[derive(Clone)]
pub struct SessionBinding {
    pub sessions: std::sync::Arc<rness_engine::service::SessionService>,
    pub subagents: std::sync::Arc<rness_engine::subagent::SubagentRuntime>,
    pub registry: std::sync::Arc<rness_engine::tools::ToolRegistry>,
    pub mcp: crate::api::mcp::McpConnections,
    pub rt: tokio::runtime::Handle,
    /// Active "<provider>/<model>" selection, surfaced as `rness.model`.
    pub model: String,
}

/// Remaining presentation declarations after coordinated teardown.
pub struct UnloadSnapshot {
    pub apps: Vec<LuaAppSpec>,
    pub keymap_binds: Vec<(String, Option<String>)>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReloadError {
    #[error("extension runtime is busy")]
    Busy,
    #[error("{0}")]
    Failed(String),
}

enum Cmd {
    /// Run an interception chain (`rness.hook.on(event, fn(ev, next))`).
    Intercept {
        event: String,
        payload: serde_json::Value,
        default: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    },
    /// Start a `tool_execute` chain; parks it when it calls `next()`.
    ExecuteStart {
        payload: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        reply: tokio::sync::oneshot::Sender<Result<(u64, crate::runtime::ExecuteStep), String>>,
    },
    /// Resume a parked `tool_execute` chain with the body's outcome.
    ExecuteResume {
        id: u64,
        outcome: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        reply: tokio::sync::oneshot::Sender<Result<crate::runtime::ExecuteStep, String>>,
    },
    /// The caller abandoned a parked chain.
    ExecuteDrop {
        id: u64,
    },
    WebTransform {
        operation: String,
        phase: String,
        value: serde_json::Value,
        context: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    },
    ValidateBindings {
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    ActionSpecs {
        reply: tokio::sync::oneshot::Sender<Vec<crate::runtime::LuaActionSpec>>,
    },
    BindingSpecs {
        reply: tokio::sync::oneshot::Sender<Vec<crate::runtime::LuaBindingSpec>>,
    },
    Action {
        guard: Box<dyn FnOnce() -> bool + Send>,
        generation: u64,
        name: String,
        scope: String,
        context: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<Vec<crate::runtime::UiActionOperation>, String>>,
    },
    Complete {
        name: String,
        context: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        reply: mpsc::Sender<Result<Vec<(String, String)>, String>>,
    },
    Command {
        name: String,
        context: serde_json::Value,
        permit: rness_engine::service::CommandPermit,
        cancel: tokio_util::sync::CancellationToken,
        reply: mpsc::Sender<Result<rness_engine::interaction::CommandResult, String>>,
    },
    ResumeCommand {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    PluginNames {
        reply: tokio::sync::oneshot::Sender<Vec<String>>,
    },
    CoordinatedUnload {
        name: String,
        installed: crate::api::tools::InstalledTools,
        apply_ui: Box<dyn FnOnce(UnloadSnapshot) + Send>,
        reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    Load {
        name: String,
        source: String,
        dependencies: Vec<String>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Unload {
        name: String,
        reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    ToolSpecs {
        reply: tokio::sync::oneshot::Sender<Vec<LuaToolSpec>>,
    },
    ResumeTool {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    CallTool {
        context: serde_json::Value,
        name: String,
        args: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<(String, Option<serde_json::Value>), String>>,
    },
    FireHook {
        event: String,
        payload: serde_json::Value,
    },
    /// A `rness.timer` deadline elapsed; run its callback on the VM thread.
    FireTimer { id: u64 },
    StatusView {
        context: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Option<serde_json::Value>>,
    },
    Statusline {
        reply: tokio::sync::oneshot::Sender<Option<String>>,
    },
    ToolCard {
        name: String,
        args: serde_json::Value,
        output: String,
        is_error: bool,
        presentation: Option<serde_json::Value>,
        reply: tokio::sync::oneshot::Sender<Option<Vec<crate::runtime::StyledLine>>>,
    },
    AppSpecs {
        reply: tokio::sync::oneshot::Sender<Vec<LuaAppSpec>>,
    },
    KeymapBinds {
        reply: tokio::sync::oneshot::Sender<Vec<(String, Option<String>)>>,
    },
    AppView {
        guard: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
        name: String,
        ctx: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<Vec<String>, String>>,
    },
    AppKey {
        guard: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
        name: String,
        key: String,
        ctx: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<AppKeyOutcome, String>>,
    },
    /// Surface engine services as `rness.session` (post-mount; the VM
    /// boots before the kernel). The actor keeps the binding and
    /// re-installs it into every reloaded VM.
    InstallSession {
        binding: SessionBinding,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    InstallJobs {
        jobs: rness_tools::jobs::JobRegistry,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Inject the shared questions broker so plugins can dynamically
    /// enable/disable the AskUser tool via `rness.questions.enable()`.
    InstallQuestions {
        questions: std::sync::Arc<rness_engine::questions::Questions>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Reload runtime declarations in the retained VM. Failures preserve the
    /// previous registrations; startup is never evaluated again.
    Reload {
        sources: Vec<crate::loader::PluginSource>,
        reconcile: Option<Box<dyn FnOnce(Vec<LuaToolSpec>) + Send>>,
        reply: tokio::sync::oneshot::Sender<Result<Vec<(String, String)>, ReloadError>>,
    },
}

/// Cloneable handle to the VM actor. All methods are async (they await
/// the actor's reply) except [`LuaHost::fire_hook`], which is
/// fire-and-forget so event fan-out never blocks on Lua.
#[derive(Clone)]
pub struct LuaHost {
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx: std::sync::Arc<mpsc::Sender<Cmd>>,
    hook_counts: crate::runtime::HookCounts,
    /// Optional bus reference for emitting hook audit events.
    bus: Option<std::sync::Arc<rness_kernel::EventBus>>,
}

struct LuaCommand {
    name: String,
    usage: String,
    allow_busy: bool,
    arguments: Vec<(String, String)>,
    description: String,
    tx: std::sync::Weak<mpsc::Sender<Cmd>>,
}

impl rness_engine::interaction::Command for LuaCommand {
    fn name(&self) -> &str {
        &self.name
    }
    fn usage(&self) -> &str {
        &self.usage
    }
    fn allow_busy(&self) -> bool {
        self.allow_busy
    }
    fn arguments(&self) -> Vec<(String, String)> {
        self.arguments.clone()
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn complete(
        &self,
        service: &rness_engine::service::SessionService,
        input: rness_engine::interaction::CommandInvocation<'_>,
    ) -> Result<Vec<String>, rness_engine::service::ServiceError> {
        self.complete_items(service, input)
            .map(|items| items.into_iter().map(|(value, _)| value).collect())
    }
    fn complete_items(
        &self,
        service: &rness_engine::service::SessionService,
        input: rness_engine::interaction::CommandInvocation<'_>,
    ) -> Result<Vec<(String, String)>, rness_engine::service::ServiceError> {
        use rness_engine::service::ServiceError;
        if std::thread::current().name() == Some("lua-vm") {
            return Err(ServiceError::InvalidConfig(
                "recursive Lua completion".into(),
            ));
        }
        let workspace = service.store().workspace(input.session)?;
        let (reply, receive) = mpsc::channel();
        self.tx.upgrade().ok_or_else(|| ServiceError::InvalidConfig("Lua host unavailable".into()))?.send(Cmd::Complete {
            name: self.name.clone(), context: serde_json::json!({"session":input.session,"workspace":workspace,"raw_input":input.raw_input}), cancel: input.cancel, reply,
        }).map_err(|_| ServiceError::InvalidConfig("Lua host unavailable".into()))?;
        receive
            .recv()
            .map_err(|_| ServiceError::InvalidConfig("Lua host unavailable".into()))?
            .map_err(ServiceError::InvalidConfig)
    }
    fn execute(
        &self,
        service: &rness_engine::service::SessionService,
        input: rness_engine::interaction::CommandInvocation<'_>,
    ) -> Result<rness_engine::interaction::CommandResult, rness_engine::service::ServiceError> {
        use rness_engine::service::ServiceError;
        if std::thread::current().name() == Some("lua-vm") {
            return Err(ServiceError::InvalidConfig(
                "Lua commands cannot recursively invoke Lua commands".into(),
            ));
        }
        let workspace = service.store().workspace(input.session)?;
        let (reply, receive) = mpsc::channel();
        self.tx.upgrade().ok_or_else(|| ServiceError::InvalidConfig("Lua host unavailable".into()))?.send(Cmd::Command { name: self.name.clone(), context: serde_json::json!({"session":input.session,"raw_input":input.raw_input,"workspace":workspace}), permit: input.permit, cancel: input.cancel, reply })
            .map_err(|_| ServiceError::InvalidConfig("Lua host unavailable".into()))?;
        receive
            .recv()
            .map_err(|_| ServiceError::InvalidConfig("Lua host unavailable".into()))?
            .map_err(ServiceError::InvalidConfig)
    }
}

fn sync_commands(
    rt: &LuaRuntime,
    binding: &SessionBinding,
    tx: &std::sync::Weak<mpsc::Sender<Cmd>>,
    installed: &mut Vec<std::sync::Arc<dyn rness_engine::interaction::Command>>,
) -> Result<(), String> {
    let specs = rt.command_specs();
    installed.retain(|command| {
        if specs.iter().any(|(name, _)| name == command.name()) {
            return true;
        }
        binding.sessions.commands().unregister_if_current(command);
        false
    });
    for (name, description) in specs {
        if installed.iter().any(|c| c.name() == name) {
            continue;
        }
        let (usage, arguments, allow_busy) = rt.command_metadata(&name);
        let command: std::sync::Arc<dyn rness_engine::interaction::Command> =
            std::sync::Arc::new(LuaCommand {
                name,
                usage,
                arguments,
                allow_busy,
                description,
                tx: tx.clone(),
            });
        binding.sessions.commands().register(command.clone())?;
        installed.push(command);
    }
    Ok(())
}

struct PendingTool {
    thread: crate::runtime::CommandThread,
    reply: tokio::sync::oneshot::Sender<Result<(String, Option<serde_json::Value>), String>>,
}

struct ToolCompletion {
    tx: std::sync::Arc<mpsc::Sender<Cmd>>,
    id: u64,
    result: Option<Result<serde_json::Value, String>>,
}
impl Drop for ToolCompletion {
    fn drop(&mut self) {
        let result = self
            .result
            .take()
            .unwrap_or_else(|| Err("session query task stopped".into()));
        let _ = self.tx.send(Cmd::ResumeTool {
            id: self.id,
            result,
        });
    }
}
fn tool_step(
    id: u64,
    tool: PendingTool,
    step: Result<crate::runtime::ToolStep, String>,
    pending: &mut std::collections::HashMap<u64, PendingTool>,
    runtime: Option<&tokio::runtime::Handle>,
    tx: &std::sync::Weak<mpsc::Sender<Cmd>>,
) {
    match step {
        Ok(crate::runtime::ToolStep::Pending(request)) => {
            let Some((runtime, tx)) = runtime.zip(tx.upgrade()) else {
                let _ = tool
                    .reply
                    .send(Err("session query runtime unavailable".into()));
                return;
            };
            pending.insert(id, tool);
            let mut completion = ToolCompletion {
                tx,
                id,
                result: None,
            };
            runtime.spawn(async move {
                completion.result = Some(
                    match tokio::time::timeout(std::time::Duration::from_secs(60), request.0).await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            Err("session query timed out; index worker may still finish".into())
                        }
                    },
                );
                drop(completion);
            });
        }
        Ok(crate::runtime::ToolStep::Complete(result)) => {
            let _ = tool.reply.send(Ok(result));
        }
        Err(error) => {
            let _ = tool.reply.send(Err(error));
        }
    }
}

struct PendingCommand {
    thread: crate::runtime::CommandThread,
    permit: rness_engine::service::CommandPermit,
    cancel: tokio_util::sync::CancellationToken,
    // Keeping this unanswered keeps PreparedCommand (and its engine locks) alive.
    reply: mpsc::Sender<Result<rness_engine::interaction::CommandResult, String>>,
}

struct CompactionCompletion {
    tx: std::sync::Arc<mpsc::Sender<Cmd>>,
    id: u64,
    result: Option<Result<serde_json::Value, String>>,
}

impl Drop for CompactionCompletion {
    fn drop(&mut self) {
        let result = self
            .result
            .take()
            .unwrap_or_else(|| Err("compaction task stopped".into()));
        let _ = self.tx.send(Cmd::ResumeCommand {
            id: self.id,
            result,
        });
    }
}

fn command_step(
    id: u64,
    command: PendingCommand,
    step: Result<crate::runtime::CommandStep, String>,
    pending: &mut std::collections::HashMap<u64, PendingCommand>,
    runtime: Option<&tokio::runtime::Handle>,
    tx: &std::sync::Weak<mpsc::Sender<Cmd>>,
) {
    match step {
        Ok(crate::runtime::CommandStep::Pending(future)) => {
            let Some((runtime, tx)) = runtime.zip(tx.upgrade()) else {
                let _ = command
                    .reply
                    .send(Err("compaction runtime unavailable".into()));
                return;
            };
            pending.insert(id, command);
            // A dropped/panicking runtime task must also release the parked command.
            let mut completion = CompactionCompletion {
                tx,
                id,
                result: None,
            };
            runtime.spawn(async move {
                completion.result = Some(future.await);
                drop(completion);
            });
        }
        result => {
            let result = result.map(|step| match step {
                crate::runtime::CommandStep::Complete(result) => result,
                crate::runtime::CommandStep::Pending(_) => unreachable!(),
            });
            let _ = command.reply.send(result);
        }
    }
}

impl LuaHost {
    /// Spawn the VM actor. Fails fast if the VM can't be built.
    pub fn spawn() -> Result<Self, String> {
        Self::spawn_with_config(crate::api::config::StartupConfig::default())
    }

    pub fn spawn_with_config(config: crate::api::config::StartupConfig) -> Result<Self, String> {
        Self::spawn_inner(config, None).map(|(host, _)| host)
    }

    pub fn spawn_from_init(
        path: std::path::PathBuf,
    ) -> Result<(Self, crate::api::config::StartupConfig), String> {
        Self::spawn_inner(crate::api::config::StartupConfig::default(), Some(path))
    }

    fn spawn_inner(
        mut config: crate::api::config::StartupConfig,
        init: Option<std::path::PathBuf>,
    ) -> Result<(Self, crate::api::config::StartupConfig), String> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let tx = std::sync::Arc::new(tx);
        let command_tx = std::sync::Arc::downgrade(&tx);
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let actor_generation = generation.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("lua-vm".into())
            .spawn(move || {
                let mut rt = match LuaRuntime::new() {
                    Ok(mut rt) => {
                        // Before init.lua runs, so timers it creates are delivered.
                        let timer_tx = command_tx.clone();
                        rt.set_timer_sink(Box::new(move |id| {
                            if let Some(tx) = timer_tx.upgrade() {
                                let _ = tx.send(Cmd::FireTimer { id });
                            }
                        }));
                        if let Some(path) = &init {
                            match rt.startup(path) {
                                Ok(startup) => config = startup,
                                Err(error) => { let _ = ready_tx.send(Err(error)); return; }
                            }
                        }
                        if let Err(e) = rt.install_config(&config) {
                            let _ = ready_tx.send(Err(e.to_string()));
                            return;
                        }
                        let _ = ready_tx.send(Ok((config.clone(), rt.hook_counts())));
                        rt
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let mut disabled_plugins = std::collections::HashSet::new();
                let mut installed_commands = Vec::new();
                let mut session_binding: Option<SessionBinding> = None;
                let mut questions_ref: Option<std::sync::Arc<rness_engine::questions::Questions>> = None;
                let mut pending_tools = std::collections::HashMap::new();
                let mut next_tool_id = 0u64;
                let mut pending_commands = std::collections::HashMap::new();
                let mut next_command_id = 0u64;
                let mut pending_executes: std::collections::HashMap<u64, crate::runtime::ExecuteThread> = std::collections::HashMap::new();
                let mut next_execute_id = 0u64;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::ValidateBindings { reply } => { let _ = reply.send(rt.validate_bindings(false)); }
                        Cmd::Complete { name, context, cancel, reply } => {
                            let token = cancel.clone();
                            rt.lua().set_app_data(cancel.clone());
                            rt.lua().set_hook(mlua::HookTriggers::new().every_nth_instruction(1000), move |_, _| {
                                if token.is_cancelled() { Err(mlua::Error::runtime("completion cancelled")) } else { Ok(mlua::VmState::Continue) }
                            });
                            let result = if cancel.is_cancelled() { Err("completion cancelled".into()) } else { rt.complete_command_items(&name, context) };
                            rt.lua().remove_hook();
                            rt.lua().remove_app_data::<tokio_util::sync::CancellationToken>();
                            let _ = reply.send(result);
                        }
                        Cmd::Command { name, context, permit, cancel, reply } => {
                            use mlua::LuaSerdeExt;
                            let thread = match rt.command_thread(&name) {
                                Ok(thread) => thread,
                                Err(error) => { let _ = reply.send(Err(error)); continue; }
                            };
                            let id = next_command_id;
                            next_command_id = next_command_id.checked_add(1).expect("command ID exhausted");
                            let mut command = PendingCommand { thread, permit, cancel, reply };
                            let step = rt.lua().to_value(&context).map_err(|e| e.to_string()).and_then(|args|
                                rt.resume_command(&mut command.thread, args, command.permit.clone(), &command.cancel));
                            command_step(id, command, step, &mut pending_commands,
                                session_binding.as_ref().map(|b| &b.rt), &command_tx);
                        }
                        Cmd::ResumeCommand { id, result } => {
                            let Some(mut command) = pending_commands.remove(&id) else { continue; };
                            let step = match result {
                                Ok(value) => {
                                    use mlua::LuaSerdeExt;
                                    let lua_val = rt.lua().to_value(&value);
                                    match lua_val {
                                        Ok(v) => rt.resume_command(&mut command.thread, (true, v), command.permit.clone(), &command.cancel),
                                        Err(e) => rt.resume_command(&mut command.thread, (false, e.to_string()), command.permit.clone(), &command.cancel),
                                    }
                                }
                                Err(error) => rt.resume_command(&mut command.thread, (false, error), command.permit.clone(), &command.cancel),
                            };
                            command_step(id, command, step, &mut pending_commands,
                                session_binding.as_ref().map(|b| &b.rt), &command_tx);
                        }
                        Cmd::Load { name, source, dependencies, reply } => {
                            if !pending_commands.is_empty() || !pending_tools.is_empty() {
                                let _ = reply.send(Err("extension runtime is busy".into()));
                                continue;
                            }
                            let staged = questions_ref.as_ref().map(|qs| {
                                let staged = std::sync::Arc::new(rness_engine::questions::Questions::default());
                                staged.set_overlay_config(qs.overlay_config());
                                staged.set_owner(qs.owner());
                                staged.set_available(qs.is_available());
                                staged
                            });
                            if let Some(staged) = &staged {
                                if let Err(error) = rt.install_questions(staged.clone()) { let _ = reply.send(Err(error.to_string())); continue; }
                            }
                            let r = rt.load_with_dependencies(&name, &source, &dependencies).map_err(|e| e.to_string()).and_then(|()| {
                                if let Some(binding) = &session_binding {
                                    if let Err(error) = sync_commands(&rt, binding, &command_tx, &mut installed_commands) {
                                        let _ = rt.unload(&name);
                                        let _ = sync_commands(&rt, binding, &command_tx, &mut installed_commands);
                                        return Err(error);
                                    }
                                }
                                Ok(())
                            });
                            if let (Some(qs), Some(staged)) = (&questions_ref, staged) {
                                rt.install_questions(qs.clone()).expect("questions API installation");
                                if r.is_ok() {
                                    qs.set_overlay_config(staged.overlay_config());
                                    qs.set_owner(staged.owner());
                                    qs.set_available(staged.is_available());
                                }
                            }
                            if r.is_ok() { actor_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst); if let Some(binding) = &session_binding { binding.sessions.reference_service().configure(rt.reference_config()); } }
                            let _ = reply.send(r);
                        }
                        Cmd::CoordinatedUnload { name, installed, apply_ui, reply } => {
                            if !pending_tools.is_empty() {
                                let _ = reply.send(Err("extension runtime is busy".into()));
                                continue;
                            }
                            let result = (|| {
                                let binding = session_binding.as_ref().ok_or("coordinated unload requires a mounted host")?;
                                let _maintenance = binding.sessions.try_extension_maintenance().map_err(|e| e.to_string())?;
                                let removed = rt.unload(&name).map_err(|e| e.to_string())?;
                                if removed {
                                    actor_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    disabled_plugins.insert(name.clone());
                                    binding.sessions.reference_service().configure(rt.reference_config());
                                    sync_commands(&rt, binding, &command_tx, &mut installed_commands)?;
                                    let remaining = rt.tool_specs();
                                    for tool in installed {
                                        if !remaining.iter().any(|spec| spec.name == tool.name()) {
                                            binding.registry.unregister_if_current(&tool);
                                        }
                                    }
                                    apply_ui(UnloadSnapshot {
                                        apps: rt.app_specs(),
                                        keymap_binds: rt.keymap_binds(),
                                    });
                                }
                                Ok(removed)
                            })();
                            let _ = reply.send(result);
                        }
                        Cmd::Unload { name, reply } => {
                            let result = if session_binding.is_some() {
                                Err("mounted hosts require coordinated engine/UI teardown; restart rness to disable a plugin".into())
                            } else {
                                rt.unload(&name).map_err(|e| e.to_string()).map(|removed| {
                                    if removed { actor_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst); disabled_plugins.insert(name.clone()); }
                                    removed
                                })
                            };
                            let _ = reply.send(result);
                        }
                        Cmd::PluginNames { reply } => { let _ = reply.send(rt.plugin_names()); }
                        Cmd::ToolSpecs { reply } => {
                            let _ = reply.send(rt.tool_specs());
                        }
                        Cmd::Intercept { event, mut payload, default, cancel, reply } => {
                            add_lineage(session_binding.as_ref(), &mut payload);
                            let token = cancel.clone();
                            let deadline = std::time::Instant::now() + HOOK_TIMEOUT;
                            rt.lua().set_hook(mlua::HookTriggers::new().every_nth_instruction(1000), move |_, _| {
                                if token.is_cancelled() || std::time::Instant::now() >= deadline { Err(mlua::Error::runtime("hook cancelled or timed out")) } else { Ok(mlua::VmState::Continue) }
                            });
                            let result = if cancel.is_cancelled() || reply.is_closed() {
                                Err("hook cancelled".into())
                            } else {
                                rt.intercept(&event, &payload, &default)
                            };
                            rt.lua().remove_hook();
                            let _ = reply.send(result.map_err(|e| format!("{event} hook: {e}")));
                        }
                        Cmd::ExecuteStart { mut payload, cancel, reply } => {
                            use crate::runtime::ExecuteStep;
                            add_lineage(session_binding.as_ref(), &mut payload);
                            let result = if cancel.is_cancelled() || reply.is_closed() {
                                Err("tool_execute hook cancelled".into())
                            } else {
                                rt.execute_thread(&payload).and_then(|mut exec| {
                                    let step = rt.resume_execute(&mut exec, None, &cancel, std::time::Instant::now() + HOOK_TIMEOUT)?;
                                    let id = next_execute_id;
                                    next_execute_id = next_execute_id.checked_add(1).expect("execute ID exhausted");
                                    if matches!(step, ExecuteStep::Body) { pending_executes.insert(id, exec); }
                                    Ok((id, step))
                                })
                            };
                            let _ = reply.send(result);
                        }
                        Cmd::ExecuteResume { id, outcome, cancel, reply } => {
                            use crate::runtime::ExecuteStep;
                            let result = match pending_executes.remove(&id) {
                                None => Err("tool_execute chain is gone".into()),
                                Some(mut exec) => rt.resume_execute(&mut exec, Some(outcome), &cancel, std::time::Instant::now() + HOOK_TIMEOUT).inspect(|step| {
                                    if matches!(step, ExecuteStep::Body) { pending_executes.insert(id, exec); }
                                }),
                            };
                            let _ = reply.send(result);
                        }
                        Cmd::ExecuteDrop { id } => {
                            pending_executes.remove(&id);
                        }
                        Cmd::WebTransform { operation, phase, value, context, cancel, reply } => {
                            use mlua::LuaSerdeExt;
                            let token = cancel.clone();
                            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                            rt.lua().set_app_data(cancel.clone());
                            rt.lua().set_hook(mlua::HookTriggers::new().every_nth_instruction(1000), move |_, _| {
                                if token.is_cancelled() || std::time::Instant::now() >= deadline { Err(mlua::Error::runtime("web hook cancelled or timed out")) } else { Ok(mlua::VmState::Continue) }
                            });
                            let result = (|| -> mlua::Result<serde_json::Value> {
                                if cancel.is_cancelled() || reply.is_closed() { return Err(mlua::Error::runtime("web hook cancelled")); }
                                let Some(callback) = rt.web_hook(&operation, &phase)? else { return Ok(value); };
                                let returned: mlua::Value = callback.call((rt.lua().to_value(&value)?, rt.lua().to_value(&context)?))?;
                                if !matches!(returned, mlua::Value::Table(_)) { return Err(mlua::Error::runtime("web hook must return a table")); }
                                rt.lua().from_value(returned)
                            })().map_err(|e| format!("web.{operation}.{phase}: {e}"));
                            rt.lua().remove_hook();
                            rt.lua().remove_app_data::<tokio_util::sync::CancellationToken>();
                            let _ = reply.send(result);
                        }
                        Cmd::CallTool { name, args, context, reply } => {
                            use mlua::LuaSerdeExt;
                            let mut thread = match rt.tool_thread(&name) {
                                Ok(thread) => thread,
                                Err(error) => { let _ = reply.send(Err(error)); continue; }
                            };
                            let id = next_tool_id;
                            next_tool_id = next_tool_id.checked_add(1).expect("tool ID exhausted");
                            let step = rt.lua().to_value(&args).and_then(|args|
                                Ok((args, rt.lua().to_value(&context)?))).map_err(|e| e.to_string())
                                .and_then(|args| rt.resume_tool(&mut thread, args));
                            tool_step(id, PendingTool { thread, reply }, step, &mut pending_tools,
                                session_binding.as_ref().map(|b| &b.rt), &command_tx);
                        }
                        Cmd::ResumeTool { id, result } => {
                            use mlua::LuaSerdeExt;
                            let Some(mut tool) = pending_tools.remove(&id) else { continue; };
                            if tool.reply.is_closed() { continue; }
                            let step = match result {
                                Ok(value) => rt.lua().to_value(&value).map_err(|e| e.to_string())
                                    .and_then(|value| rt.resume_tool(&mut tool.thread, (true, value))),
                                Err(error) => rt.resume_tool(&mut tool.thread, (false, error)),
                            };
                            tool_step(id, tool, step, &mut pending_tools,
                                session_binding.as_ref().map(|b| &b.rt), &command_tx);
                        }
                        Cmd::FireHook { event, mut payload } => {
                            // Add delegation lineage for opts.agent scoping
                            // on any hook payload that carries a session field.
                            add_lineage(session_binding.as_ref(), &mut payload);
                            for err in rt.fire_hook(&event, &payload) {
                                tracing::warn!(target: "lua", "hook '{event}' failed: {err}");
                            }
                        }
                        Cmd::FireTimer { id } => {
                            let deadline = std::time::Instant::now() + HOOK_TIMEOUT;
                            rt.lua().set_hook(mlua::HookTriggers::new().every_nth_instruction(1000), move |_, _| {
                                if std::time::Instant::now() >= deadline { Err(mlua::Error::runtime("timer callback timed out")) } else { Ok(mlua::VmState::Continue) }
                            });
                            let result = rt.fire_timer(id);
                            rt.lua().remove_hook();
                            if let Err(err) = result {
                                tracing::warn!(target: "lua", "timer {id} failed: {err}");
                            }
                        }
                        Cmd::StatusView { context, reply } => {
                            let _ = reply.send(rt.status_view(context));
                        }
                        Cmd::Statusline { reply } => {
                            let _ = reply.send(rt.statusline());
                        }
                        Cmd::ToolCard { name, args, output, is_error, presentation, reply } => {
                            let _ = reply.send(rt.tool_card_presented(&name, &args, &output, is_error, presentation.as_ref()));
                        }
                        Cmd::BindingSpecs { reply } => {
                            let _ = reply.send(rt.binding_specs());
                        }
                        Cmd::ActionSpecs { reply } => {
                            let _ = reply.send(rt.action_specs());
                        }
                        Cmd::Action { guard, generation, name, scope, context, reply } => {
                            let result = if guard() && generation == actor_generation.load(std::sync::atomic::Ordering::SeqCst) {
                                rt.call_action(&name, &scope, context)
                            } else { Err("plugin action generation expired".into()) };
                            let _ = reply.send(result);
                        }
                        Cmd::AppSpecs { reply } => {
                            let _ = reply.send(rt.app_specs());
                        }
                        Cmd::KeymapBinds { reply } => {
                            let _ = reply.send(rt.keymap_binds());
                        }
                        Cmd::AppView { guard, name, ctx, reply } => {
                            let result = if guard.as_ref().is_some_and(|guard| !guard()) {
                                Err("stale app request".into())
                            } else { rt.app_view(&name, &ctx).map_err(|e| e.to_string()) };
                            let _ = reply.send(result);
                        }
                        Cmd::AppKey { guard, name, key, ctx, reply } => {
                            let result = if guard.as_ref().is_some_and(|guard| !guard()) {
                                Err("stale app request".into())
                            } else { rt.app_key(&name, &key, &ctx).map_err(|e| e.to_string()) };
                            let _ = reply.send(result);
                        }
                        Cmd::InstallSession { binding, reply } => {
                            let r = rt
                                .install_session(
                                    std::sync::Arc::clone(&binding.sessions),
                                    std::sync::Arc::clone(&binding.subagents),
                                    std::sync::Arc::clone(&binding.registry),
                                    std::sync::Arc::clone(&binding.mcp),
                                    binding.rt.clone(),
                                    binding.model.clone(),
                                )
                                .map_err(|e| e.to_string());
                            let r = r.and_then(|()| sync_commands(&rt, &binding, &command_tx, &mut installed_commands));
                            if r.is_ok() { binding.sessions.reference_service().configure(rt.reference_config()); }
                            session_binding = Some(binding);
                            let _ = reply.send(r);
                        }
                        Cmd::InstallJobs { jobs, reply } => {
                            let _ = reply.send(rt.install_jobs(jobs).map_err(|e| e.to_string()));
                        }
                        Cmd::InstallQuestions { questions, reply } => {
                            questions_ref = Some(questions.clone());
                            let r = rt.install_questions(questions).map_err(|e| e.to_string());
                            let _ = reply.send(r);
                        }
                        Cmd::Reload { mut sources, reconcile, reply } => {
                            if !pending_tools.is_empty() {
                                let _ = reply.send(Err(ReloadError::Busy));
                                continue;
                            }
                            // Validate the declared graph before applying session-local unloads.
                            // Failed consumers may never have loaded, so unloading their
                            // prerequisite is valid; keep them skipped on subsequent reloads.
                            if let Err(error) = crate::loader::ordered_sources(&sources) {
                                let _ = reply.send(Err(ReloadError::Failed(error)));
                                continue;
                            }
                            let mut excluded = disabled_plugins.clone();
                            loop {
                                let mut changed = false;
                                for source in &sources {
                                    if !excluded.contains(&source.name) && source.dependencies.iter().any(|dependency| excluded.contains(dependency)) {
                                        tracing::warn!(target: "lua", "skipping plugin '{}' on reload: dependency unloaded", source.name);
                                        excluded.insert(source.name.clone());
                                        changed = true;
                                    }
                                }
                                if !changed { break; }
                            }
                            sources.retain(|source| !excluded.contains(&source.name));
                            let _maintenance = match session_binding.as_ref().map(|b| b.sessions.try_extension_maintenance()).transpose() {
                                Ok(guard) => guard,
                                Err(_) => { let _ = reply.send(Err(ReloadError::Busy)); continue; }
                            };
                            let staged_questions = std::sync::Arc::new(rness_engine::questions::Questions::default());
                            if questions_ref.is_some() {
                                rt.install_questions(staged_questions.clone()).expect("questions API installation");
                            }
                            let r = rt.reload_plugins(&sources, |fresh| {
                                if let Some(binding) = &session_binding {
                                    let replacements = fresh.command_specs().into_iter().map(|(name, description)| {
                                        let (usage, arguments, allow_busy) = fresh.command_metadata(&name);
                                        std::sync::Arc::new(LuaCommand { name, description, usage, arguments, allow_busy, tx: command_tx.clone() }) as std::sync::Arc<dyn rness_engine::interaction::Command>
                                    }).collect::<Vec<_>>();
                                    binding.sessions.commands().replace_owned(&installed_commands, &replacements)?;
                                    installed_commands = replacements;
                                }
                                Ok(())
                            });
                            if let Some(qs) = &questions_ref {
                                rt.install_questions(qs.clone()).expect("questions API installation");
                                if r.is_ok() {
                                    qs.set_overlay_config(staged_questions.overlay_config());
                                    qs.set_owner(staged_questions.owner());
                                    qs.set_available(staged_questions.is_available());
                                }
                            }
                            if r.is_ok() {
                                actor_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                if let Some(binding) = &session_binding { binding.sessions.reference_service().configure(rt.reference_config()); }
                                if let Some(reconcile) = reconcile { reconcile(rt.tool_specs()); }
                            }
                            let _ = reply.send(r.map(|()| Vec::new()).map_err(ReloadError::Failed));
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let (config, hook_counts) = ready_rx
            .recv()
            .map_err(|_| "lua vm thread died".to_string())??;
        Ok((Self { tx, generation, hook_counts, bus: None }, config))
    }

    pub async fn load(&self, name: &str, source: &str) -> Result<(), String> {
        self.load_with_dependencies(name, source, &[]).await
    }

    pub async fn load_with_dependencies(
        &self,
        name: &str,
        source: &str,
        dependencies: &[String],
    ) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Load {
                name: name.into(),
                source: source.into(),
                dependencies: dependencies.to_vec(),
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Unload in an unmounted host. Mounted hosts reject this operation
    /// until engine and frontend teardown can be coordinated.
    pub async fn unload(&self, name: &str) -> Result<bool, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Unload {
                name: name.into(),
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Teardown under an engine maintenance reservation on the VM actor.
    /// `installed` must contain the current handles owned by this host.
    /// `apply_ui` must synchronously invalidate apps/cards/statusline and rebuild
    /// keymaps from the snapshot. It must not block, panic, or call this host.
    /// Once enqueued, teardown completes even if the caller drops its future.
    pub async fn unload_coordinated(
        &self,
        name: &str,
        installed: crate::api::tools::InstalledTools,
        apply_ui: impl FnOnce(UnloadSnapshot) + Send + 'static,
    ) -> Result<bool, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::CoordinatedUnload {
                name: name.into(),
                installed,
                apply_ui: Box::new(apply_ui),
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    pub async fn plugin_names(&self) -> Vec<String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::PluginNames { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn tool_specs(&self) -> Vec<LuaToolSpec> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::ToolSpecs { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        self.call_tool_context(name, args, serde_json::json!({}))
            .await
    }

    pub async fn call_tool_context(
        &self,
        name: &str,
        args: serde_json::Value,
        context: serde_json::Value,
    ) -> Result<String, String> {
        self.call_tool_presented(name, args, context)
            .await
            .map(|(output, _)| output)
    }

    pub async fn call_tool_presented(
        &self,
        name: &str,
        args: serde_json::Value,
        context: serde_json::Value,
    ) -> Result<(String, Option<serde_json::Value>), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::CallTool {
                name: name.into(),
                args,
                context,
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Fire-and-forget: never blocks the caller on Lua execution.
    pub fn fire_hook(&self, event: &str, payload: serde_json::Value) {
        let _ = self.tx.send(Cmd::FireHook {
            event: event.into(),
            payload,
        });
    }

    pub async fn statusline(&self) -> Option<String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::Statusline { reply }).ok()?;
        rx.await.ok().flatten()
    }

    /// Render a finished tool call through the Lua card renderer.
    /// None = no renderer / declined / VM gone — use the built-in card.
    pub async fn tool_card(
        &self,
        name: &str,
        args: serde_json::Value,
        output: &str,
        is_error: bool,
    ) -> Option<Vec<crate::runtime::StyledLine>> {
        self.tool_card_presented(name, args, output, is_error, None)
            .await
    }

    pub async fn tool_card_presented(
        &self,
        name: &str,
        args: serde_json::Value,
        output: &str,
        is_error: bool,
        presentation: Option<serde_json::Value>,
    ) -> Option<Vec<crate::runtime::StyledLine>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::ToolCard {
                name: name.into(),
                args,
                output: output.into(),
                is_error,
                presentation,
                reply,
            })
            .ok()?;
        rx.await.ok().flatten()
    }

    pub async fn action_specs(&self) -> Vec<crate::runtime::LuaActionSpec> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::ActionSpecs { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn binding_specs(&self) -> Vec<crate::runtime::LuaBindingSpec> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::BindingSpecs { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn validate_bindings(&self) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::ValidateBindings { reply })
            .map_err(|_| "Lua host stopped".to_owned())?;
        rx.await.map_err(|_| "Lua host stopped".to_owned())?
    }

    pub fn action_generation(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.generation.clone()
    }

    pub async fn call_action(
        &self,
        name: &str,
        scope: &str,
        context: serde_json::Value,
    ) -> Result<Vec<crate::runtime::UiActionOperation>, String> {
        self.call_action_at(
            self.generation.load(std::sync::atomic::Ordering::SeqCst),
            name,
            scope,
            context,
        )
        .await
    }

    pub async fn call_action_at(
        &self,
        generation: u64,
        name: &str,
        scope: &str,
        context: serde_json::Value,
    ) -> Result<Vec<crate::runtime::UiActionOperation>, String> {
        self.call_action_guarded(generation, name, scope, context, || true)
            .await
    }

    pub async fn call_action_guarded(
        &self,
        generation: u64,
        name: &str,
        scope: &str,
        context: serde_json::Value,
        guard: impl FnOnce() -> bool + Send + 'static,
    ) -> Result<Vec<crate::runtime::UiActionOperation>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Action {
                guard: Box::new(guard),
                generation,
                name: name.into(),
                scope: scope.into(),
                context,
                reply,
            })
            .map_err(|_| "Lua host stopped".to_owned())?;
        rx.await.map_err(|_| "Lua host stopped".to_owned())?
    }

    pub async fn app_specs(&self) -> Vec<LuaAppSpec> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::AppSpecs { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Declared host keymap binds, in order (chord, action|None=unbind).
    pub async fn keymap_binds(&self) -> Vec<(String, Option<String>)> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::KeymapBinds { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn app_view(
        &self,
        name: &str,
        ctx: serde_json::Value,
    ) -> Result<Vec<String>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::AppView {
                guard: None,
                name: name.into(),
                ctx,
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    pub async fn app_key(
        &self,
        name: &str,
        key: &str,
        ctx: serde_json::Value,
    ) -> Result<AppKeyOutcome, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::AppKey {
                guard: None,
                name: name.into(),
                key: key.into(),
                ctx,
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Check activation/context on the VM thread immediately before calling Lua.
    pub async fn app_view_guarded(
        &self,
        name: &str,
        ctx: serde_json::Value,
        guard: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<Vec<String>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::AppView {
                guard: Some(guard),
                name: name.into(),
                ctx,
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// A queued key must not run side effects after its activation expires.
    pub async fn app_key_guarded(
        &self,
        name: &str,
        key: &str,
        ctx: serde_json::Value,
        guard: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<AppKeyOutcome, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::AppKey {
                guard: Some(guard),
                name: name.into(),
                key: key.into(),
                ctx,
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Replace runtime registrations from `sources`, preserving startup state.
    /// Any load failure leaves the old registrations live.
    pub async fn reload(
        &self,
        sources: Vec<crate::loader::PluginSource>,
    ) -> Result<Vec<(String, String)>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Reload {
                sources,
                reconcile: None,
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await
            .map_err(|_| "lua vm gone")?
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn reload_reconciled(
        &self,
        sources: Vec<crate::loader::PluginSource>,
        reconcile: impl FnOnce(Vec<LuaToolSpec>) + Send + 'static,
    ) -> Result<Vec<(String, String)>, ReloadError> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Reload {
                sources,
                reconcile: Some(Box::new(reconcile)),
                reply,
            })
            .map_err(|_| ReloadError::Failed("lua vm gone".into()))?;
        rx.await
            .map_err(|_| ReloadError::Failed("lua vm gone".into()))?
    }

    /// Inject shared background jobs. Sticky across retained-VM hot reloads.
    pub async fn install_jobs(&self, jobs: rness_tools::jobs::JobRegistry) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::InstallJobs { jobs, reply })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Inject the shared questions broker. Sticky across hot reloads.
    pub async fn install_questions(
        &self,
        questions: std::sync::Arc<rness_engine::questions::Questions>,
    ) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::InstallQuestions { questions, reply })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Inject `rness.session` (post-mount). Sticky across hot reloads.
    pub async fn install_session(
        &self,
        sessions: std::sync::Arc<rness_engine::service::SessionService>,
        subagents: std::sync::Arc<rness_engine::subagent::SubagentRuntime>,
        registry: std::sync::Arc<rness_engine::tools::ToolRegistry>,
        mcp: crate::api::mcp::McpConnections,
        rt: tokio::runtime::Handle,
        model: String,
    ) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::InstallSession {
                binding: SessionBinding {
                    sessions,
                    subagents,
                    registry,
                    mcp,
                    rt,
                    model,
                },
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn guarded_app_callbacks_reject_stale_requests_before_lua_side_effects() {
        let host = LuaHost::spawn().unwrap();
        host.load(
            "probe",
            r#"
            local calls = 0
            rness.ui.app { name = 'probe', slot = 'overlay',
                view = function() calls = calls + 1; return {tostring(calls)} end,
                on_key = function() calls = calls + 1; return true end }
        "#,
        )
        .await
        .unwrap();
        let guard: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new(|| false);
        assert_eq!(
            host.app_key_guarded("probe", "enter", json!({}), guard.clone())
                .await
                .unwrap_err(),
            "stale app request"
        );
        assert_eq!(
            host.app_view_guarded("probe", json!({}), guard)
                .await
                .unwrap_err(),
            "stale app request"
        );
        // Both rejected callbacks must leave Lua state untouched.
        assert_eq!(host.app_view("probe", json!({})).await.unwrap(), vec!["1"]);
        let generation = host.action_generation();
        let captured = generation.load(std::sync::atomic::Ordering::SeqCst);
        let guard = std::sync::Arc::new(move || {
            generation.load(std::sync::atomic::Ordering::SeqCst) == captured
        });
        host.load("other", "").await.unwrap();
        assert_eq!(
            host.app_key_guarded("probe", "enter", json!({}), guard)
                .await
                .unwrap_err(),
            "stale app request"
        );
        assert_eq!(host.app_view("probe", json!({})).await.unwrap(), vec!["2"]);
    }

    #[tokio::test]
    async fn queued_action_rejects_reloaded_registration_generation() {
        let host = LuaHost::spawn().unwrap();
        let source = "local p = __rness_plugin_context(); p.action('run', {scope='promptbox', description='Run', run=function(ctx) ctx.promptbox.insert('new') end})";
        host.load("review", source).await.unwrap();
        let generation = host
            .action_generation()
            .load(std::sync::atomic::Ordering::SeqCst);
        host.reload(vec![crate::loader::PluginSource {
            dependencies: vec![],
            name: "review".into(),
            source: source.into(),
        }])
        .await
        .unwrap();
        assert!(host
            .call_action_at(generation, "review.run", "promptbox", json!({}))
            .await
            .unwrap_err()
            .contains("generation expired"));
        assert_eq!(
            host.call_action("review.run", "promptbox", json!({}))
                .await
                .unwrap()
                .len(),
            1
        );
        let generation = host
            .action_generation()
            .load(std::sync::atomic::Ordering::SeqCst);
        assert!(host
            .reload(vec![crate::loader::PluginSource {
                dependencies: vec![],
                name: "review".into(),
                source: "error('failed')".into()
            }])
            .await
            .is_err());
        assert!(host
            .call_action_at(generation, "review.run", "promptbox", json!({}))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn reload_does_not_resurrect_session_unloaded_plugins() {
        let host = LuaHost::spawn().unwrap();
        let source = crate::loader::PluginSource {
            dependencies: vec![],
            name: "disabled".into(),
            source: "rness.tool.register{name='owned', run=function() return 'ok' end}".into(),
        };
        host.load(&source.name, &source.source).await.unwrap();
        assert!(host.unload("disabled").await.unwrap());
        assert!(host.reload(vec![source]).await.unwrap().is_empty());
        assert!(host.tool_specs().await.is_empty());
        assert!(host.plugin_names().await.is_empty());
    }

    #[tokio::test]
    async fn unmounted_host_unloads_across_cloned_handles() {
        let host = LuaHost::spawn().unwrap();
        host.load(
            "plugin",
            "rness.tool.register{name='owned', run=function() return 'ok' end}",
        )
        .await
        .unwrap();
        assert!(host.clone().unload("plugin").await.unwrap());
        assert!(host.tool_specs().await.is_empty());
        assert!(host.call_tool("owned", json!({})).await.is_err());
        assert!(!host.unload("plugin").await.unwrap());
        host.load(
            "plugin",
            "rness.tool.register{name='owned', run=function() return 'new' end}",
        )
        .await
        .unwrap();
        assert_eq!(host.call_tool("owned", json!({})).await.unwrap(), "new");
    }

    #[tokio::test]
    async fn host_loads_and_calls_tools_across_threads() {
        let host = LuaHost::spawn().unwrap();
        host.load(
            "p",
            r#"rness.tool.register{ name = "add", run = function(a) return tostring(a.x + a.y) end }"#,
        )
        .await
        .unwrap();

        let specs = host.tool_specs().await;
        assert_eq!(specs[0].name, "add");
        assert_eq!(
            host.call_tool("add", json!({"x": 2, "y": 3})).await,
            Ok("5".into())
        );

        // Cloned handles talk to the same VM.
        let clone = host.clone();
        assert_eq!(
            clone.call_tool("add", json!({"x": 1, "y": 1})).await,
            Ok("2".into())
        );
    }

    #[tokio::test]
    async fn hooks_fire_without_blocking_and_mutate_vm_state() {
        let host = LuaHost::spawn().unwrap();
        host.load(
            "p",
            r#"
            count = 0
            rness.hook.on("tick", function() count = count + 1 end)
            rness.tool.register{ name = "count", run = function() return tostring(count) end }
            "#,
        )
        .await
        .unwrap();

        host.fire_hook("tick", json!({}));
        host.fire_hook("tick", json!({}));
        // Commands are processed in order: the tool call observes both.
        assert_eq!(host.call_tool("count", json!({})).await, Ok("2".into()));
    }

    #[tokio::test]
    async fn lua_tool_hooks_drive_engine_dispatch() {
        use rness_engine::tools::{hooks::HookContext, ToolCall, ToolRegistry};
        let host = LuaHost::spawn().unwrap();
        host.load(
            "hooks",
            r#"
            results = {}
            rness.tool.register{ name = "echo", run = function(args) return args.say end }
            rness.hook.on("pre_tool", {match = "echo"}, function(ev, next)
              if ev.args.say == "secret" then return {kind = "deny", reason = "no secrets"} end
              return next()
            end)
            rness.hook.guard(function(ev) if ev.args.say == "guarded" then return {kind = "deny", reason = "guarded"} end end)
            rness.hook.on("post_tool", function(ev, next)
              local d = next()
              if ev.result.is_error then return d end
              return {kind = "accept", content = ev.result.output .. "!", additional_contexts = {"checked " .. ev.call}}
            end)
            rness.hook.on("tool_result", function(ev) results[#results+1] = ev.call .. "=" .. ev.result.output end)
            rness.tool.register{ name = "seen", run = function() return table.concat(results, ",") end }
            "#,
        )
        .await
        .unwrap();
        let registry = ToolRegistry::default();
        crate::api::tools::sync_lua_tools(&registry, &host, &[]).await;
        registry.set_hooks(Some(std::sync::Arc::new(host.clone())));
        let call = |id: &str, say: &str| ToolCall { call: id.into(), name: "echo".into(), args: json!({"say": say}) };
        let results = registry
            .dispatch(&"s".into(), &[call("a", "hi"), call("b", "secret"), call("c", "guarded")], 1, &tokio_util::sync::CancellationToken::new())
            .await;
        assert_eq!((results[0].output.as_str(), results[0].is_error), ("hi!", false));
        assert_eq!((results[1].output.as_str(), results[1].is_error), ("no secrets", true));
        assert_eq!((results[2].output.as_str(), results[2].is_error), ("guarded", true));
        assert_eq!(registry.take_hook_contexts(&"s".into()), [HookContext { call: "a".into(), text: "checked a".into() }]);
        // tool_result is fire-and-forget but ordered before this call.
        assert_eq!(host.call_tool("seen", json!({})).await, Ok("a=hi!,b=no secrets,c=guarded".into()));
    }

    #[tokio::test]
    async fn lua_hook_failures_become_error_results() {
        use rness_engine::tools::{ToolCall, ToolRegistry};
        let host = LuaHost::spawn().unwrap();
        host.load(
            "hooks",
            r#"
            rness.tool.register{ name = "echo", run = function(args) return args.say end }
            rness.hook.on("pre_tool", function(ev) if ev.args.say == "bad" then return {kind = "maybe"} end end)
            "#,
        )
        .await
        .unwrap();
        let registry = ToolRegistry::default();
        crate::api::tools::sync_lua_tools(&registry, &host, &[]).await;
        registry.set_hooks(Some(std::sync::Arc::new(host.clone())));
        let call = |say: &str| ToolCall { call: say.into(), name: "echo".into(), args: json!({"say": say}) };
        let cancel = tokio_util::sync::CancellationToken::new();
        let bad = registry.dispatch(&"s".into(), &[call("bad")], 1, &cancel).await.remove(0);
        assert!(bad.is_error && bad.output.contains("unknown decision kind 'maybe'"), "{}", bad.output);
        // A nil return from the chain means the default (allow).
        let ok = registry.dispatch(&"s".into(), &[call("ok")], 1, &cancel).await.remove(0);
        assert_eq!((ok.output.as_str(), ok.is_error), ("ok", false));
    }

    #[tokio::test]
    async fn lua_tool_execute_wraps_engine_dispatch() {
        use rness_engine::tools::{ToolCall, ToolRegistry};
        let host = LuaHost::spawn().unwrap();
        host.load(
            "exec",
            r#"
            runs = 0
            rness.tool.register{ name = "flaky", run = function(args)
              runs = runs + 1
              if runs < 2 then error("transient") end
              return "ran " .. runs
            end }
            rness.tool.register{ name = "count", run = function() return tostring(runs) end }
            rness.hook.on("tool_execute", {match = "flaky"}, function(ev, next)
              if ev.args.mode == "skip" then return {content = "skipped", is_error = false} end
              if ev.args.mode == "nil" then return nil end
              local r = next()
              if r.is_error then r = next() end
              return r
            end)
            "#,
        )
        .await
        .unwrap();
        let registry = ToolRegistry::default();
        crate::api::tools::sync_lua_tools(&registry, &host, &[]).await;
        registry.set_hooks(Some(std::sync::Arc::new(host.clone())));
        let cancel = tokio_util::sync::CancellationToken::new();
        let call = |name: &str, mode: &str| ToolCall { call: mode.into(), name: name.into(), args: json!({"mode": mode}) };
        // Retry: first run errors, the wrapper runs the body again.
        let retried = registry.dispatch(&"s".into(), &[call("flaky", "retry")], 1, &cancel).await.remove(0);
        assert_eq!((retried.output.as_str(), retried.is_error), ("ran 2", false));
        // Short-circuit: the body never runs.
        let skipped = registry.dispatch(&"s".into(), &[call("flaky", "skip")], 1, &cancel).await.remove(0);
        assert_eq!((skipped.output.as_str(), skipped.is_error), ("skipped", false));
        assert_eq!(host.call_tool("count", json!({})).await, Ok("2".into()));
        // nil without next() is a wrapper bug, not a silent success.
        let bad = registry.dispatch(&"s".into(), &[call("flaky", "nil")], 1, &cancel).await.remove(0);
        assert!(bad.is_error && bad.output.contains("without calling next()"), "{}", bad.output);
        // Unmatched tools bypass the wrapper.
        let other = registry.dispatch(&"s".into(), &[call("count", "x")], 1, &cancel).await.remove(0);
        assert_eq!(other.output, "2");
    }

    #[tokio::test]
    async fn load_error_reports_source_name() {
        let host = LuaHost::spawn().unwrap();
        let err = host
            .load("broken.lua", "this is not lua")
            .await
            .unwrap_err();
        assert!(err.contains("broken.lua"), "{err}");
    }

    #[tokio::test]
    async fn questions_lifecycle_through_host() {
        let host = LuaHost::spawn().unwrap();
        let qs = std::sync::Arc::new(rness_engine::questions::Questions::default());
        host.install_questions(qs.clone()).await.unwrap();

        // Plugin enables questions.
        host.load(
            "q-plugin",
            "rness.questions.enable { height = 25, title = 'Decisions' }",
        )
        .await
        .unwrap();
        assert!(qs.is_available());
        assert_eq!(qs.owner().as_deref(), Some("q-plugin"));
        assert_eq!(qs.overlay_config().height, 25);
        assert_eq!(qs.overlay_config().title, "Decisions");

        // Unload via unmounted host disables questions.
        assert!(host.unload("q-plugin").await.unwrap());
        assert!(!qs.is_available());
        assert!(qs.owner().is_none());

        // Re-enable works after unload.
        host.load("q-plugin", "rness.questions.enable()")
            .await
            .unwrap();
        assert!(qs.is_available());

        // Explicit disable from Lua.
        host.load("off", "rness.questions.disable()").await.unwrap();
        assert!(!qs.is_available());
    }
}
