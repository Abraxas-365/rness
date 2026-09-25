//! Loop-level interception hooks (dsh `agent/pre-step`, `agent/request`,
//! `agent/request-error`, `agent/turn-stopping`).
//!
//! These run inside the turn loop, between step boundaries. The engine
//! calls them when a hook implementation is provided; otherwise the loop
//! behaves exactly as before (zero-cost default path).

use async_trait::async_trait;
use rness_protocol::events::SessionId;
use tokio_util::sync::CancellationToken;

/// Payload for every loop hook.
#[derive(Debug, Clone)]
pub struct LoopEvent {
    pub session: SessionId,
    pub turn: u32,
    pub step: u32,
}

// ── pre_step ──────────────────────────────────────────────────────────

/// One hook-injected message. `tag` is an optional plugin-chosen label
/// persisted on `MessageSource::Hook` so renderers can style/hide it.
#[derive(Debug, Clone, PartialEq)]
pub struct HookMessage {
    pub text: String,
    pub tag: Option<String>,
}

impl From<String> for HookMessage {
    fn from(text: String) -> Self {
        Self { text, tag: None }
    }
}

impl From<&str> for HookMessage {
    fn from(text: &str) -> Self {
        Self { text: text.into(), tag: None }
    }
}

/// Decision returned by a `pre_step` handler.
#[derive(Debug, Clone, PartialEq)]
pub enum PreStepDecision {
    /// Proceed with the step (default).
    Enter,
    /// Inject additional context visible to the model this step.
    /// The messages are committed as `UserMessage{intent: Inject}` before
    /// the request is built.
    EnterWithMessages { messages: Vec<HookMessage> },
    /// Reject the step — the turn ends as completed (no model call).
    Reject,
}

impl Default for PreStepDecision {
    fn default() -> Self {
        Self::Enter
    }
}

// ── request ───────────────────────────────────────────────────────────

// `request` is currently a no-op placeholder; rness does not yet expose
// per-step model config overrides. We define the trait method as returning
// `()` so listeners can observe the step without blocking, and a future
// phase can extend it to return config tweaks.

// ── request_error ─────────────────────────────────────────────────────

/// Outcome of the `request_error` chain.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestErrorAction {
    /// Retry the request (the loop still respects its own retry budget).
    Retry,
    /// Let the loop apply its built-in retry/fail logic (default).
    Default,
}

impl Default for RequestErrorAction {
    fn default() -> Self {
        Self::Default
    }
}

/// Facts about one failed model request.
#[derive(Debug, Clone)]
pub struct RequestError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub attempt: u32,
    pub max_retries: u32,
}

// ── turn_stopping ─────────────────────────────────────────────────────

/// After the model says "end turn" and before the turn actually closes,
/// `turn_stopping` lets listeners inject one last steer. If the handler
/// returns messages, the turn does NOT close — it continues with those
/// messages visible to the model on the next step.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnStoppingAction {
    /// Close the turn normally (default).
    Stop,
    /// Continue the turn with these messages injected.
    Continue { messages: Vec<HookMessage> },
}

impl Default for TurnStoppingAction {
    fn default() -> Self {
        Self::Stop
    }
}

// ── trait ──────────────────────────────────────────────────────────────

/// Loop-level hook implementation. Supplied by the composition root
/// (the Lua host in the CLI). Every method has a no-op default so
/// callers can provide a partial implementation.
#[async_trait]
pub trait LoopHooks: Send + Sync {
    /// Called before each step's model request is built.
    async fn pre_step(
        &self,
        event: &LoopEvent,
        cancel: &CancellationToken,
    ) -> Result<PreStepDecision, String> {
        let _ = (event, cancel);
        Ok(PreStepDecision::default())
    }

    /// Called just before the model request is sent. Currently observe-only;
    /// a future phase will let it return config overrides.
    async fn request(
        &self,
        event: &LoopEvent,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let _ = (event, cancel);
        Ok(())
    }

    /// Called when a model request fails, before the loop's own retry logic.
    async fn request_error(
        &self,
        event: &LoopEvent,
        error: &RequestError,
        cancel: &CancellationToken,
    ) -> Result<RequestErrorAction, String> {
        let _ = (event, error, cancel);
        Ok(RequestErrorAction::default())
    }

    /// Called when the model ends a turn (EndTurn or truncation with no
    /// tool calls). Return `Continue` with messages to extend the turn.
    async fn turn_stopping(
        &self,
        event: &LoopEvent,
        cancel: &CancellationToken,
    ) -> Result<TurnStoppingAction, String> {
        let _ = (event, cancel);
        Ok(TurnStoppingAction::default())
    }
}
