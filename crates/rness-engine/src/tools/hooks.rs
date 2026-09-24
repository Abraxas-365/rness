//! Tool-pipeline interception seam (dsh `tools/pre-execute`, `ctx.tools.guard`,
//! `tools/post-execute`, `tools/result`).
//!
//! Every dispatched call of a registered, permitted tool follows:
//!   pre_tool → [ask → approval] → guard → approval policy →
//!   tool_execute(next = execute) → post_tool → tool_result
//!
//! - `pre_tool` returns a typed decision: allow (the chain default, "no
//!   objection" — it never bypasses the configured approval policy), deny,
//!   or ask. A granted ask counts as this call's approval.
//! - `guard` runs after the whole pre-tool chain resolved to allow; it can
//!   only deny or abstain, and a failing guard denies (fail closed).
//! - Denied calls still reach `post_tool`, like dsh.
//! - A failing `pre_tool`/`post_tool` becomes the call's final error result.
//! - `tool_execute` wraps the tool body (dsh `tools/execute`): `next` runs
//!   the body and may be called again (retry) or not at all (short-circuit).
//!   A failing wrapper becomes an error result that still reaches post_tool.
//! - `tool_result` observes the final result; it cannot change it.
//!
//! Arguments are frozen before policy (no rewrite), so the log, the UI and
//! the tool body agree on what ran.

use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use rness_protocol::events::{SessionId, ToolCallId, ToolResult, ToolResultContentPart};
use tokio_util::sync::CancellationToken;

/// Immutable facts about one call, shared by every hook phase.
#[derive(Debug, Clone)]
pub struct ToolHookEvent {
    pub session: SessionId,
    pub call: ToolCallId,
    pub tool: String,
    pub args: serde_json::Value,
    /// Turn number for audit correlation. 0 when dispatched outside a turn loop.
    pub turn: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PreToolDecision {
    Allow,
    Deny { reason: String },
    Ask { reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PostToolDecision {
    /// Keep the result; optionally replace the model-visible content.
    Accept {
        content: Option<Vec<ToolResultContentPart>>,
        additional_contexts: Vec<String>,
    },
    /// Replace the result with corrective feedback marked as an error.
    Block {
        feedback: Vec<ToolResultContentPart>,
        additional_contexts: Vec<String>,
    },
}

impl Default for PostToolDecision {
    fn default() -> Self {
        Self::Accept {
            content: None,
            additional_contexts: Vec::new(),
        }
    }
}

/// The model-visible outcome of one tool-body run, as seen and returned by
/// `tool_execute` wrappers.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecuteOutcome {
    pub content: Vec<ToolResultContentPart>,
    pub is_error: bool,
}

pub type ExecuteFuture = Pin<Box<dyn Future<Output = ExecuteOutcome> + Send>>;
/// Runs the tool body once per call; each call is a fresh execution.
pub type ExecuteNext = dyn Fn() -> ExecuteFuture + Send + Sync;

/// Composition-owned hook implementation (the Lua host in the CLI).
#[async_trait]
pub trait ToolHooks: Send + Sync {
    async fn pre_tool(
        &self,
        event: &ToolHookEvent,
        cancel: &CancellationToken,
    ) -> Result<PreToolDecision, String>;

    /// `Ok(Some(reason))` denies; `Err` also denies (fail closed).
    async fn guard(
        &self,
        event: &ToolHookEvent,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, String>;

    /// Around-execution wrapper. The default runs the body once.
    async fn tool_execute(
        &self,
        event: &ToolHookEvent,
        next: &ExecuteNext,
        cancel: &CancellationToken,
    ) -> Result<ExecuteOutcome, String> {
        let _ = (event, cancel);
        Ok(next().await)
    }

    async fn post_tool(
        &self,
        event: &ToolHookEvent,
        result: &ToolResult,
        cancel: &CancellationToken,
    ) -> Result<PostToolDecision, String>;

    /// Observe-only notification of the final result.
    fn tool_result(&self, event: &ToolHookEvent, result: &ToolResult);
}

/// Model-visible context a `post_tool` hook attached to a call. The turn
/// loop commits these as sourced user messages right after the step's
/// tool results, so they are durable before the next request.
#[derive(Debug, Clone, PartialEq)]
pub struct HookContext {
    pub call: ToolCallId,
    pub text: String,
}
