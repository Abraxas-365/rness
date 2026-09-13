//! Durable session-log events — format v1.
//!
//! THE WIRE FORMAT. Every line of a `session.v1.jsonl` file is one
//! [`Envelope`]. Changes here are format changes: bump
//! [`FORMAT_VERSION`], add a migration module, never mutate committed
//! generations (invariant #3).
//!
//! Design rules:
//! - Committed assistant messages embed the exact timed chunk stream that
//!   produced them ("every run is traceable").
//! - Failed/cancelled/retried model calls are `assistant/attempt` events —
//!   preserved, but never part of derived model context (invariant #5).
//! - The first line of every log is `session/header`. A forked session
//!   carries its parent reference there; prefix events are replayed from
//!   the parent, never copied.
//! - Everything is `deny_unknown_fields`-free on read (forward-tolerant),
//!   but writers only emit what is defined here.

use serde::{Deserialize, Serialize};

use crate::branch::{Delegation, ForkRef};

/// Bumped on any breaking change to the shapes in this module.
pub const FORMAT_VERSION: u32 = 1;

/// ULID string. Sortable, unique per event within a session.
pub type EventId = String;
/// ULID string identifying a session (and its directory).
pub type SessionId = String;
/// RFC 3339 UTC timestamp with millisecond precision.
pub type Timestamp = String;

/// One line of the log: identity + time + the event itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// ULID — unique, monotonically sortable within the session.
    pub id: EventId,
    /// Wall-clock time the event was committed.
    pub at: Timestamp,
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// The durable event vocabulary, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    /// Always the first line of a log file.
    #[serde(rename = "session/header")]
    Header(Header),

    /// A user message entered the conversation (any intent).
    #[serde(rename = "user/message")]
    UserMessage(UserMessage),

    /// A committed assistant step: final content plus the exact stream
    /// that produced it.
    #[serde(rename = "assistant/message")]
    AssistantMessage(AssistantMessage),

    /// A model call that did not commit (error, cancellation, retry).
    /// Kept for traceability; never replayed into model context.
    #[serde(rename = "assistant/attempt")]
    AssistantAttempt(AssistantAttempt),

    /// A tool finished and its result was committed, in model order.
    #[serde(rename = "tool/result")]
    ToolResult(ToolResult),

    #[serde(rename = "tools/activated")]
    ToolsActivated { names: Vec<String> },
    /// Nested program calls are auditable, not standalone model messages.
    #[serde(rename = "tools/program_started")]
    ProgramToolStarted { parent: ToolCallId, call: ToolCallId, name: String, args: serde_json::Value },
    #[serde(rename = "tools/program_result")]
    ProgramToolResult { parent: ToolCallId, args: serde_json::Value, result: ToolResult },

    /// A turn opened (one user intent -> agent until idle).
    #[serde(rename = "turn/started")]
    TurnStarted { turn: u32 },

    /// The turn closed.
    #[serde(rename = "turn/ended")]
    TurnEnded { turn: u32, outcome: TurnOutcome },

    /// A deterministic tool-result prune (dsh tool-result-pruner):
    /// `replaces` names one tool/result event, `result` is the same
    /// result with its output middle removed. Model-free, replayed in
    /// the original's place. The log stays append-only.
    #[serde(rename = "compaction/prune")]
    Prune(Prune),

    /// A compaction summary committed: `replaces` names the shadowed
    /// prefix of earlier events; projections replay `summary` in their
    /// place (dsh checkpoint model). The log stays append-only — the
    /// shadowed events remain, only derivation skips them. A later
    /// compaction may shadow an earlier one (its id goes in `replaces`).
    #[serde(rename = "compaction/summary")]
    Compaction(Compaction),

    /// Audit only: never inserted into model-visible history.
    #[serde(rename = "compaction/started")]
    CompactionStarted { model: String, sources: Vec<EventId>, estimated_input: u64, request: serde_json::Value },
    #[serde(rename = "compaction/request")]
    CompactionRequest { started: EventId, body: String },
    #[serde(rename = "compaction/finished")]
    CompactionFinished {
        started: EventId,
        outcome: String,
        usage: Usage,
        chunks: Vec<TimedChunk>,
    },

    /// Request-header state changed (dsh request/header model): call
    /// config applied to every LATER model request. Durable so resume
    /// restores the last explicit choice; the latest event wins.
    #[serde(rename = "request/config")]
    RequestConfig(CallConfig),

    #[serde(rename = "plan/mode")]
    PlanMode { active: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReview { Approved, KeepPlanning, Dismissed }

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanState {
    pub active: bool,
    pub pending: Option<bool>,
}
impl PlanState {
    pub fn from_history(history: &[Envelope]) -> Self {
        let mut state = Self::default();
        for env in history {
            match &env.event {
                SessionEvent::PlanMode { active } => { state.active = *active; state.pending = None; }
                SessionEvent::ToolResult(result) | SessionEvent::ProgramToolResult { result, .. } if !result.is_error && result.plan_review == Some(PlanReview::Approved) => state.pending = Some(false),
                _ => {}
            }
        }
        state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus { Pending, InProgress, Completed }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskItem {
    pub id: String,
    pub content: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSnapshot {
    pub tasks: Vec<TaskItem>,
}

impl TaskSnapshot {
    /// Latest successful snapshot in full fork-resolved history, including compacted events.
    pub fn from_history(history: &[Envelope]) -> Self {
        history.iter().rev().find_map(|env| match &env.event {
            SessionEvent::ToolResult(result) | SessionEvent::ProgramToolResult { result, .. } if !result.is_error => result.tasks.clone(),
            _ => None,
        }).unwrap_or_default()
    }
}

/// A durable provider route and model selection. Credentials never belong
/// here: composition roots resolve this public identifier at request time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub route: String,
    pub model: String,
}

/// Explicit reasoning control. The tagged wire form avoids interpreting a
/// numeric-looking effort as a thinking budget. Deserialization also accepts
/// the legacy string form written by older logs (numbers become budgets;
/// every other string becomes an effort) without rewriting those logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Reasoning {
    BudgetTokens { tokens: u32 },
    Effort { effort: String },
}

impl<'de> Deserialize<'de> for Reasoning {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Tagged(Tagged),
            Legacy(String),
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", tag = "kind")]
        enum Tagged {
            BudgetTokens { tokens: u32 },
            Effort { effort: String },
        }
        match Wire::deserialize(deserializer)? {
            Wire::Tagged(Tagged::BudgetTokens { tokens }) => Ok(Self::BudgetTokens { tokens }),
            Wire::Tagged(Tagged::Effort { effort }) => Ok(Self::Effort { effort }),
            Wire::Legacy(value) => match value.parse() {
                Ok(tokens) => Ok(Self::BudgetTokens { tokens }),
                Err(_) => Ok(Self::Effort { effort: value }),
            },
        }
    }
}

/// Per-conversation request controls. Every field is optional — absent
/// means "do not send the knob, keep the provider's own behavior"
/// (dsh: omitting preserves the provider default; nothing is invented).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallConfig {
    /// Delegated authority; service updates preserve this ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_ceiling: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentSnapshot>,
    /// Immutable filesystem policy selected at session creation or role selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<crate::sandbox::SandboxMode>,
    /// Durable route/model choice used by the service's provider resolver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ModelSelection>,
    /// Name of the profile that produced the current selection, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Provider-independent reasoning mode, mapped by each adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// Maximum generated output tokens. Omitted retains the adapter default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature. Omitted retains the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub name: String,
    pub instructions: String,
    pub tools: Option<Vec<String>>,
}

/// A committed tool-result prune.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prune {
    /// The original tool/result event this prune shadows.
    pub replaces: EventId,
    /// The pruned replacement (same call id, shorter output).
    pub result: ToolResult,
}

/// A committed compaction checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// Ids of every envelope in the shadowed span, in log order.
    pub replaces: Vec<EventId>,
    /// Replayed to the model in place of the shadowed events.
    pub summary: String,
    /// Model that produced the summary.
    pub model: String,
}

/// First line of every session log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// [`FORMAT_VERSION`] at write time.
    pub version: u32,
    pub session: SessionId,
    /// Present iff this session is a fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ForkRef>,
    /// Present iff this session was created BY an agent (a subagent run).
    /// Orthogonal to `parent`: spawn runs have delegation but no fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Delegation>,
    /// Workspace root the session operates in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// How user input entered the conversation (inbox intent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserIntent {
    /// Normal prompt while idle, or queued for after the current turn.
    Followup,
    /// Redirect the agent mid-turn; visible to the model at the next step.
    Steer,
    /// Content added to context without triggering a turn.
    Inject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub intent: UserIntent,
    pub content: Vec<ContentPart>,
    /// Who authored this message. Engine-injected context (workspace
    /// instructions) is marked so projections can find/replace it; a
    /// plain user prompt carries no source (dsh sourced-message model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MessageSource>,
}

/// Provenance of an engine-injected user message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageSource {
    JobCompletion { id: String },
    /// Workspace instruction baseline (AGENTS.md chain). `identity`
    /// fingerprints discovery inputs+content: a visible baseline with a
    /// matching identity is current; mismatch or absence → re-inject.
    Instructions { identity: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub model: String,
    pub content: Vec<ContentPart>,
    pub stop: StopReason,
    pub usage: Usage,
    /// The exact timed stream that produced `content`.
    pub chunks: Vec<TimedChunk>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantAttempt {
    pub model: String,
    pub outcome: AttemptOutcome,
    /// Whatever streamed before the attempt died.
    pub chunks: Vec<TimedChunk>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AttemptOutcome {
    Error {
        message: String,
        retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_in_ms: Option<u64>,
    },
    Cancelled,
}

/// Content-addressed image reference; never a filesystem path or remote URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    pub id: String,
    pub media_type: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

/// Message content, mirroring provider shapes closely enough to
/// reconstruct requests exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ContentPart {
    Image { attachment: ImageRef },
    Text { text: String },
    /// Model-internal reasoning (persisted; provider adapters decide
    /// whether it is replayed).
    Thinking {
        text: String,
        /// Provider integrity signature (Anthropic): must be replayed
        /// with the block or the API rejects the request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// The model requested a tool call.
    ToolUse { call: ToolCallId, name: String, #[serde(default)] args: serde_json::Value },
}

pub type ToolCallId = String;

/// Ordered content emitted by a tool. Unlike message content, tool output
/// cannot contain model reasoning or further tool calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ToolResultContentPart {
    Text { text: String },
    Image { attachment: ImageRef },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call: ToolCallId,
    /// Tool name, denormalized for greppability.
    pub name: String,
    /// Ordered durable tool output. Empty is accepted when reading pre-rich
    /// logs, whose text-only output remains in `output`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolResultContentPart>,
    /// Text projection retained for wire and extension compatibility. New
    /// writers set this to the concatenated text parts of `content`.
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
    /// Milliseconds the tool ran (wall clock).
    pub duration_ms: u64,
    /// State and successful tool result commit in the same JSONL envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<TaskSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_review: Option<PlanReview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<serde_json::Value>,
}

impl ToolResult {
    /// Rich content when present, otherwise a lossless view of old text-only
    /// logs. Keeping fallback here makes replay migrations transparent.
    pub fn effective_content(&self) -> Vec<ToolResultContentPart> {
        if self.content.is_empty() && !self.output.is_empty() {
            vec![ToolResultContentPart::Text { text: self.output.clone() }]
        } else {
            self.content.clone()
        }
    }

    pub fn text_output(content: &[ToolResultContentPart]) -> String {
        content
            .iter()
            .filter_map(|part| match part {
                ToolResultContentPart::Text { text } => Some(text.as_str()),
                ToolResultContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// One streaming delta, timed relative to request start — enough to
/// re-render the stream exactly as it happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedChunk {
    /// Milliseconds since the model request started.
    pub ms: u64,
    #[serde(flatten)]
    pub delta: ChunkDelta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "d")]
pub enum ChunkDelta {
    Text { t: String },
    Thinking { t: String },
    /// Incremental tool-call argument JSON.
    ToolArgs { call: ToolCallId, t: String },
}
