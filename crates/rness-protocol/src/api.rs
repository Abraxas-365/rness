//! Client↔engine request/response types. The TUI (and any frontend)
//! drives the engine EXCLUSIVELY through these shapes plus the streams
//! in [`crate::frames`] — never through engine internals.
//!
//! Transport-agnostic on purpose: in-process today (CLI wires a
//! ClientHandle to SessionService), SSE/HTTP later without changing the
//! frontend.

use serde::{Deserialize, Serialize};

use crate::events::{ContentPart, Envelope, SessionId, ToolCallId, UserIntent};

/// What a frontend can ask of the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    /// Send user content into a session (intent decides queueing).
    Send { session: SessionId, intent: UserIntent, content: Vec<ContentPart> },
    /// Continue an idle session whose final event is a failed turn, without new user content.
    Retry { session: SessionId },
    /// Cancel the running turn, if any.
    Cancel { session: SessionId },
}

/// A sensitive tool call paused for the user's one-shot decision
/// (only under the `ask` approval policy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub session: SessionId,
    pub call: ToolCallId,
    pub tool: String,
    pub args: serde_json::Value,
}

/// The user's answer to an [`ApprovalRequest`]. One-shot: applies to
/// that call only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allowed,
    Rejected,
}

/// A session's full durable history: committed envelopes in log order
/// (prefix events of forks included). The frontend derives everything
/// it renders from this plus live frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct History {
    pub session: SessionId,
    pub envelopes: Vec<Envelope>,
}
