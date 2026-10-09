//! Durable session-log events — format v1.
//!
//! THE WIRE FORMAT. Every line of a `session.v1.jsonl` file is one
//! [`Envelope`]. Changes here are format changes: bump
//! [`FORMAT_VERSION`], add a migration module, never mutate committed
//! generations (invariant #3).
//!
//! Design rules:
//! - Committed assistant messages carry their result (`content`); the exact
//!   timed chunk stream that produced it is kept only with
//!   `rness.record_stream = true` (otherwise `chunks` is written as `[]`,
//!   never omitted, so older readers still parse it). Failed/cancelled
//!   attempts always keep what streamed.
//! - Failed/cancelled/retried model calls are `assistant/attempt` events —
//!   preserved, but never part of derived model context (invariant #5).
//! - The first line of every log is `session/header`. A forked session
//!   carries its parent reference there; prefix events are replayed from
//!   the parent, never copied.
//! - Everything is `deny_unknown_fields`-free on read (forward-tolerant),
//!   but writers only emit what is defined here.
//! - Forward compatibility: a line whose `type` this build does not know
//!   (written by a newer rness) reads as [`SessionEvent::Unknown`] with its
//!   JSON kept verbatim, instead of failing the whole log. A line with a
//!   KNOWN `type` but malformed fields is still an error (real corruption).
//!   Projections ignore unknown events; the turn loop refuses to run a model
//!   turn on a session that contains any (it could be model-visible).
//! - `session/repair` is a pure audit event: the writer quarantined an
//!   invalid trailing region to a sidecar file on open.

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
///
/// Deserialization never fails on an unknown `type`: see
/// [`SessionEvent::Unknown`]. Log readers use [`Envelope::parse_line`]
/// (fast path for known types).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Envelope {
    /// ULID — unique, monotonically sortable within the session.
    pub id: EventId,
    /// Wall-clock time the event was committed.
    pub at: Timestamp,
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// Every `type` tag this build understands (all [`SessionEvent`] variants
/// except [`SessionEvent::Unknown`]). A line with any other tag parses as
/// `Unknown`.
pub const KNOWN_TYPES: &[&str] = &[
    "session/header",
    "user/message",
    "assistant/message",
    "assistant/attempt",
    "tool/result",
    "tools/activated",
    "tools/program_started",
    "tools/program_result",
    "turn/started",
    "turn/ended",
    "compaction/prune",
    "compaction/summary",
    "compaction/started",
    "compaction/request",
    "compaction/finished",
    "request/config",
    "plan/mode",
    "hook/invoked",
    "hook/result",
    "session/title",
    "session/repair",
];

/// Derived fast path for log lines of a known type.
#[derive(Deserialize)]
struct KnownEnvelope {
    id: EventId,
    at: Timestamp,
    #[serde(flatten, deserialize_with = "SessionEvent::deserialize_known")]
    event: SessionEvent,
}

impl Envelope {
    /// Parse one log line (without or with its trailing newline). Known
    /// types take the derived fast path; a JSON object whose `type` is not
    /// in [`KNOWN_TYPES`] becomes [`SessionEvent::Unknown`]. Anything else
    /// (invalid JSON, known type with bad fields, missing `id`/`at`) is the
    /// original parse error.
    pub fn parse_line(line: &[u8]) -> Result<Envelope, serde_json::Error> {
        match serde_json::from_slice::<KnownEnvelope>(line) {
            Ok(known) => Ok(Envelope {
                id: known.id,
                at: known.at,
                event: known.event,
            }),
            Err(error) => match serde_json::from_slice::<serde_json::Value>(line) {
                Ok(value) => match Self::unknown_from_value(value) {
                    Some(envelope) => Ok(envelope),
                    None => Err(error),
                },
                Err(_) => Err(error),
            },
        }
    }

    /// `Some` iff `value` is an envelope-shaped object (string `id`, `at`
    /// and `type`) whose `type` this build does not know.
    fn unknown_from_value(value: serde_json::Value) -> Option<Envelope> {
        let serde_json::Value::Object(mut map) = value else {
            return None;
        };
        let kind = map.get("type")?.as_str()?;
        if KNOWN_TYPES.contains(&kind) {
            return None;
        }
        let kind = kind.to_owned();
        if !(map.get("id")?.is_string() && map.get("at")?.is_string()) {
            return None;
        }
        let Some(serde_json::Value::String(id)) = map.remove("id") else {
            return None;
        };
        let Some(serde_json::Value::String(at)) = map.remove("at") else {
            return None;
        };
        Some(Envelope {
            id,
            at,
            event: SessionEvent::Unknown(UnknownEvent {
                kind,
                raw: serde_json::Value::Object(map),
            }),
        })
    }
}

impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let known = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|kind| KNOWN_TYPES.contains(&kind));
        if known {
            let known = KnownEnvelope::deserialize(value).map_err(D::Error::custom)?;
            return Ok(Envelope {
                id: known.id,
                at: known.at,
                event: known.event,
            });
        }
        Self::unknown_from_value(value)
            .ok_or_else(|| D::Error::custom("envelope requires string `id` and `at` fields"))
    }
}

/// An event of a type this build does not know (written by a newer rness).
/// Kept verbatim; never rewritten, never model-visible here.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownEvent {
    /// The event's `type` tag.
    pub kind: String,
    /// The event's JSON object, `type` included (without the envelope's
    /// `id`/`at` when read from a log line). Serialized back verbatim.
    pub raw: serde_json::Value,
}

/// Payload of `session/repair`: the writer quarantined an invalid trailing
/// region of the log (never an acknowledged append) on open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRepair {
    /// Bytes moved out of the log.
    pub bytes: u64,
    /// Lines in the quarantined region.
    pub lines: u64,
    /// Sidecar file (relative to the session directory) holding the bytes.
    pub sidecar: String,
    pub reason: String,
}

impl Serialize for SessionEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SessionEvent::Unknown(unknown) => unknown.raw.serialize(serializer),
            known => SessionEvent::serialize_known(known, serializer),
        }
    }
}

impl<'de> Deserialize<'de> for SessionEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some(kind) if !KNOWN_TYPES.contains(&kind) => Ok(SessionEvent::Unknown(UnknownEvent {
                kind: kind.to_owned(),
                raw: value,
            })),
            _ => SessionEvent::deserialize_known(value).map_err(D::Error::custom),
        }
    }
}

impl SessionEvent {
    /// The derived (known-types-only) serializer.
    fn serialize_known<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SessionEvent::serialize(self, serializer)
    }

    /// The derived (known-types-only) deserializer.
    fn deserialize_known<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error> {
        SessionEvent::deserialize(deserializer)
    }

    /// The `type` tag of this event (exhaustive: a new variant must add its
    /// tag here and to [`KNOWN_TYPES`]).
    pub fn kind(&self) -> &str {
        match self {
            SessionEvent::Header(_) => "session/header",
            SessionEvent::UserMessage(_) => "user/message",
            SessionEvent::AssistantMessage(_) => "assistant/message",
            SessionEvent::AssistantAttempt(_) => "assistant/attempt",
            SessionEvent::ToolResult(_) => "tool/result",
            SessionEvent::ToolsActivated { .. } => "tools/activated",
            SessionEvent::ProgramToolStarted { .. } => "tools/program_started",
            SessionEvent::ProgramToolResult { .. } => "tools/program_result",
            SessionEvent::TurnStarted { .. } => "turn/started",
            SessionEvent::TurnEnded { .. } => "turn/ended",
            SessionEvent::Prune(_) => "compaction/prune",
            SessionEvent::Compaction(_) => "compaction/summary",
            SessionEvent::CompactionStarted { .. } => "compaction/started",
            SessionEvent::CompactionRequest { .. } => "compaction/request",
            SessionEvent::CompactionFinished { .. } => "compaction/finished",
            SessionEvent::RequestConfig(_) => "request/config",
            SessionEvent::PlanMode { .. } => "plan/mode",
            SessionEvent::HookInvoked(_) => "hook/invoked",
            SessionEvent::HookResult(_) => "hook/result",
            SessionEvent::Title(_) => "session/title",
            SessionEvent::Repair(_) => "session/repair",
            SessionEvent::Unknown(unknown) => &unknown.kind,
        }
    }
}

/// The durable event vocabulary, tagged by `type`.
///
/// `remote = "Self"`: the derives generate inherent `serialize` /
/// `deserialize` over the known variants; the trait impls above wrap them
/// with the [`SessionEvent::Unknown`] fallback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self", tag = "type")]
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
    ProgramToolStarted {
        parent: ToolCallId,
        call: ToolCallId,
        name: String,
        args: serde_json::Value,
    },
    #[serde(rename = "tools/program_result")]
    ProgramToolResult {
        parent: ToolCallId,
        args: serde_json::Value,
        result: ToolResult,
    },

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
    CompactionStarted {
        model: String,
        sources: Vec<EventId>,
        estimated_input: u64,
        /// Legacy: older logs stored the full summarizer input here. It is
        /// reconstructable from the log (`sources`) plus code, so new
        /// writers leave it null and omit it (dsh stores no request either).
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        request: serde_json::Value,
    },
    /// Legacy, read-only: the raw HTTP body of a summarizer request. No
    /// longer written (it duplicated `compaction/started.request`); kept so
    /// older logs still parse.
    #[serde(rename = "compaction/request")]
    CompactionRequest { started: EventId, body: String },
    #[serde(rename = "compaction/finished")]
    CompactionFinished {
        started: EventId,
        outcome: String,
        usage: Usage,
        /// The summarizer's timed stream. Kept for failed, cancelled and
        /// rejected runs (their only record of the reply); empty for
        /// `committed` unless `rness.record_stream` is on.
        chunks: Vec<TimedChunk>,
    },

    /// Request-header state changed (dsh request/header model): call
    /// config applied to every LATER model request. Durable so resume
    /// restores the last explicit choice; the latest event wins.
    #[serde(rename = "request/config")]
    RequestConfig(CallConfig),

    #[serde(rename = "plan/mode")]
    PlanMode { active: bool },

    /// Audit: a hook handler was invoked. Paired with `HookResult` by `handler_id`.
    #[serde(rename = "hook/invoked")]
    HookInvoked(HookInvoked),

    /// Audit: a hook handler returned/failed. Paired with `HookInvoked` by `handler_id`.
    #[serde(rename = "hook/result")]
    HookResult(HookResult),

    /// A human-readable session title (last-wins). Written by a title
    /// plugin (automatic: `fallback`/`model`) or by an explicit rename.
    #[serde(rename = "session/title")]
    Title(SessionTitle),

    /// Audit only: an invalid trailing region (never an acknowledged
    /// append) was moved to a sidecar file when the writer opened the log.
    #[serde(rename = "session/repair")]
    Repair(LogRepair),

    /// Read-side only: an event whose `type` this build does not know.
    /// Never constructed by writers; serialized back verbatim.
    #[serde(skip)]
    Unknown(UnknownEvent),
}

/// Payload for the `session/title` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTitle {
    pub title: String,
    /// How the title was produced.
    #[serde(default = "default_title_source")]
    pub source: TitleSource,
}

fn default_title_source() -> TitleSource {
    TitleSource::Fallback
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TitleSource {
    /// Explicitly set by the user.
    User,
    /// Auto-generated by an LLM call.
    Model,
    /// Deterministic fallback (first N words of first user message).
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReview {
    Approved,
    KeepPlanning,
    Dismissed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanState {
    pub active: bool,
    pub pending: Option<bool>,
}
impl PlanState {
    pub fn from_history(history: &[impl std::borrow::Borrow<Envelope>]) -> Self {
        let mut state = Self::default();
        for env in history {
            match &env.borrow().event {
                SessionEvent::PlanMode { active } => {
                    state.active = *active;
                    state.pending = None;
                }
                SessionEvent::ToolResult(result)
                | SessionEvent::ProgramToolResult { result, .. }
                    if !result.is_error && result.plan_review == Some(PlanReview::Approved) =>
                {
                    state.pending = Some(false)
                }
                _ => {}
            }
        }
        state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

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
    pub fn from_history(history: &[impl std::borrow::Borrow<Envelope>]) -> Self {
        history
            .iter()
            .rev()
            .find_map(|env| match &env.borrow().event {
                SessionEvent::ToolResult(result)
                | SessionEvent::ProgramToolResult { result, .. }
                    if !result.is_error =>
                {
                    result.tasks.clone()
                }
                _ => None,
            })
            .unwrap_or_default()
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
    /// Ids of the envelopes this checkpoint directly folds, in log order:
    /// the model-visible events of the span plus any earlier checkpoint or
    /// prune whose replacement was visible there. What those earlier
    /// checkpoints folded in turn is NOT repeated here — readers expand it
    /// with [`shadowed_by_checkpoints`]. Older logs wrote the expanded
    /// list; the expansion is idempotent, so both shapes read alike.
    pub replaces: Vec<EventId>,
    /// Replayed to the model in place of the shadowed events.
    pub summary: String,
    /// Model that produced the summary.
    pub model: String,
}

/// One live checkpoint's full shadow, expanded through the checkpoints and
/// prunes it folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointShadow {
    /// The `compaction/summary` event.
    pub checkpoint: EventId,
    /// The checkpoint's summary text.
    pub summary: String,
    /// Every event the checkpoint stands in for, in log order.
    pub shadowed: Vec<EventId>,
}

/// The shadow of every LIVE checkpoint in `history` (header first, log
/// order), newest checkpoint first — the one place `replaces` is expanded.
///
/// A checkpoint is live unless a later one folded it. Each live
/// checkpoint's shadow is its `replaces` plus, transitively, the shadow of
/// every checkpoint it lists and the original of every prune it lists.
/// The shadows of live checkpoints are disjoint. Expansion is idempotent:
/// a checkpoint whose `replaces` was already expanded at write time
/// (older logs) yields the same shadow.
pub fn shadowed_by_checkpoints(
    history: &[impl std::borrow::Borrow<Envelope>],
) -> Vec<CheckpointShadow> {
    use std::collections::HashMap;
    // Work in log positions: one hash lookup per listed id, then plain
    // indexing (ULID strings are slow to hash and sessions hold 10^4-10^5
    // events).
    let mut position: HashMap<&str, usize> = HashMap::with_capacity(history.len());
    for (index, env) in history.iter().enumerate() {
        position.insert(env.borrow().id.as_str(), index);
    }
    // Per event: what it folds (checkpoint -> its replaces, prune -> its
    // original), as positions. Ids not in this history are kept as strings.
    let mut folds: Vec<Option<Vec<usize>>> = vec![None; history.len()];
    let mut foreign: Vec<Vec<&str>> = vec![Vec::new(); history.len()];
    for (index, env) in history.iter().enumerate() {
        let env = env.borrow();
        let listed: Vec<&str> = match &env.event {
            SessionEvent::Compaction(c) => c.replaces.iter().map(String::as_str).collect(),
            SessionEvent::Prune(p) => vec![p.replaces.as_str()],
            _ => continue,
        };
        let mut positions = Vec::with_capacity(listed.len());
        for id in listed {
            match position.get(id) {
                Some(&p) => positions.push(p),
                None => foreign[index].push(id),
            }
        }
        folds[index] = Some(positions);
    }
    let mut claimed = vec![false; history.len()];
    let mut shadow = vec![false; history.len()];
    let mut out = Vec::new();
    // Later checkpoints win: walk newest first; a checkpoint already
    // claimed by a newer one is inert.
    for (index, env) in history.iter().enumerate().rev() {
        let env = env.borrow();
        let SessionEvent::Compaction(c) = &env.event else {
            continue;
        };
        if claimed[index] {
            continue;
        }
        let mut members: Vec<usize> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut unknown: Vec<&str> = foreign[index].clone();
        // Mark before pushing: older logs' expanded lists repeat every id
        // many times over, and the stack must not grow with those repeats.
        for &p in folds[index].as_deref().unwrap_or_default() {
            if !shadow[p] {
                shadow[p] = true;
                members.push(p);
                stack.push(p);
            }
        }
        while let Some(p) = stack.pop() {
            unknown.extend(foreign[p].iter().copied());
            for &inner in folds[p].as_deref().unwrap_or_default() {
                if !shadow[inner] {
                    shadow[inner] = true;
                    members.push(inner);
                    stack.push(inner);
                }
            }
        }
        for &p in &members {
            claimed[p] = true;
            shadow[p] = false;
        }
        members.sort_unstable();
        let mut shadowed: Vec<EventId> = members
            .into_iter()
            .map(|p| history[p].borrow().id.clone())
            .collect();
        unknown.sort_unstable();
        unknown.dedup();
        shadowed.extend(unknown.into_iter().map(String::from));
        out.push(CheckpointShadow {
            checkpoint: env.id.clone(),
            summary: c.summary.clone(),
            shadowed,
        });
    }
    out
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
    ExternalPrompt {
        id: String,
    },
    JobCompletion {
        id: String,
    },
    /// Workspace instruction baseline (AGENTS.md chain). `identity`
    /// fingerprints discovery inputs+content: a visible baseline with a
    /// matching identity is current; mismatch or absence → re-inject.
    Instructions {
        identity: String,
    },
    /// Plugin-maintained context block (`rness.context.ensure`), e.g. a
    /// memory index. Same contract as `Instructions`: `name` identifies the
    /// renderer, `identity` fingerprints the content; the engine re-injects
    /// when no visible block matches (first turn, compaction fold, change).
    Context {
        name: String,
        identity: String,
    },
    /// Context a tool-pipeline hook attached to a call (`post_tool`
    /// `additional_contexts`). Committed after the step's tool results.
    Hook {
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<ToolCallId>,
        /// Optional plugin-chosen label (e.g. "time") so renderers can
        /// style/hide specific hooks. Additive and optional: older readers
        /// ignore it, older logs omit it — no format bump.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub model: String,
    pub content: Vec<ContentPart>,
    pub stop: StopReason,
    pub usage: Usage,
    /// Meter-estimated input tokens of the request that produced this
    /// message (0 = unknown/legacy). Recorded at commit so later turns can
    /// calibrate the heuristic meter against `usage`'s real counts.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub estimated_input: u64,
    /// The exact timed stream that produced `content`. Empty unless
    /// `rness.record_stream` is on (older logs always have it).
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
    Image {
        attachment: ImageRef,
    },
    Text {
        text: String,
    },
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
    ToolUse {
        call: ToolCallId,
        name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
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
            vec![ToolResultContentPart::Text {
                text: self.output.clone(),
            }]
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
    Text {
        t: String,
    },
    Thinking {
        t: String,
    },
    /// Incremental tool-call argument JSON.
    ToolArgs {
        call: ToolCallId,
        t: String,
    },
}

// ── hook audit events ─────────────────────────────────────────────────

/// Recorded when a hook handler is invoked. Paired with [`HookResult`]
/// by `handler_id`. Audit-only: never inserted into model-visible history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookInvoked {
    pub turn: u32,
    /// Hook point name, e.g. `"pre_tool"`, `"session_start"`.
    pub point: String,
    /// Source type: `"lua"` or `"command"` (hooks.json bridge).
    pub source: String,
    /// Optional tool/event matcher that selected this handler.
    pub matcher: Option<String>,
    /// Correlation key linking this invocation to its result.
    pub handler_id: String,
}

/// Recorded when a hook handler returns or fails.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookResult {
    pub turn: u32,
    pub point: String,
    /// Same key as the paired [`HookInvoked`].
    pub handler_id: String,
    /// Outcome summary: `"allow"`, `"deny"`, `"ask"`, `"enter"`, `"reject"`,
    /// `"retry"`, `"stop"`, `"continue"`, `"error"`, etc.
    pub decision: String,
    /// Shell exit code (command hooks only).
    pub exit_code: Option<i32>,
    /// Bounded stderr summary (command hooks only).
    pub stderr_summary: Option<String>,
    /// Wall-clock milliseconds.
    pub duration_ms: u64,
}

#[cfg(test)]
mod checkpoint_shadow_tests {
    use super::*;

    fn env(id: &str, event: SessionEvent) -> Envelope {
        Envelope {
            id: id.into(),
            at: "2026-01-01T00:00:00.000Z".into(),
            event,
        }
    }
    fn user(id: &str) -> Envelope {
        env(
            id,
            SessionEvent::UserMessage(UserMessage {
                intent: UserIntent::Followup,
                content: vec![ContentPart::Text { text: id.into() }],
                source: None,
            }),
        )
    }
    fn tool(id: &str) -> Envelope {
        env(
            id,
            SessionEvent::ToolResult(ToolResult {
                call: "c".into(),
                name: "t".into(),
                content: vec![],
                output: "long".into(),
                is_error: false,
                duration_ms: 0,
                tasks: None,
                plan_review: None,
                presentation: None,
            }),
        )
    }
    fn prune(id: &str, target: &str) -> Envelope {
        let SessionEvent::ToolResult(mut result) = tool(target).event else {
            unreachable!()
        };
        result.output = "short".into();
        env(
            id,
            SessionEvent::Prune(Prune {
                replaces: target.into(),
                result,
            }),
        )
    }
    fn checkpoint(id: &str, replaces: &[&str]) -> Envelope {
        env(
            id,
            SessionEvent::Compaction(Compaction {
                replaces: replaces.iter().map(|s| s.to_string()).collect(),
                summary: format!("summary {id}"),
                model: "m".into(),
            }),
        )
    }
    fn ids(shadow: &CheckpointShadow) -> Vec<&str> {
        shadow.shadowed.iter().map(String::as_str).collect()
    }

    #[test]
    fn direct_sources_expand_through_folded_checkpoints_and_prunes() {
        // a b [S1 = a b] c t [P = t] d [S2 = S1 c P d]  e
        let history = vec![
            user("a"),
            user("b"),
            checkpoint("S1", &["a", "b"]),
            user("c"),
            tool("t"),
            prune("P", "t"),
            user("d"),
            checkpoint("S2", &["S1", "c", "P", "d"]),
            user("e"),
        ];
        let shadows = shadowed_by_checkpoints(&history);
        assert_eq!(shadows.len(), 1, "S1 is folded by S2, so only S2 is live");
        assert_eq!(shadows[0].checkpoint, "S2");
        assert_eq!(shadows[0].summary, "summary S2");
        // Expanded, in log order; the anchor (first) is the oldest event.
        assert_eq!(ids(&shadows[0]), ["a", "b", "S1", "c", "t", "P", "d"]);
    }

    #[test]
    fn expanded_lists_from_older_logs_read_identically() {
        let direct = vec![
            user("a"),
            user("b"),
            checkpoint("S1", &["a", "b"]),
            user("c"),
            checkpoint("S2", &["S1", "c"]),
        ];
        let mut expanded = direct.clone();
        expanded[4] = checkpoint("S2", &["a", "b", "S1", "c"]);
        let mut mixed = direct.clone();
        mixed.push(user("d"));
        // Old writer after new ones: lists everything again.
        mixed.push(checkpoint("S3", &["a", "b", "S1", "c", "S2", "d"]));
        let mut mixed_new = mixed.clone();
        mixed_new[6] = checkpoint("S3", &["S2", "d"]);

        assert_eq!(
            shadowed_by_checkpoints(&direct),
            shadowed_by_checkpoints(&expanded)
        );
        assert_eq!(
            shadowed_by_checkpoints(&mixed),
            shadowed_by_checkpoints(&mixed_new)
        );
        assert_eq!(
            ids(&shadowed_by_checkpoints(&mixed_new)[0]),
            ["a", "b", "S1", "c", "S2", "d"]
        );
    }

    #[test]
    fn independent_checkpoints_stay_live_and_disjoint() {
        // A manual region fold S2 over [c d] beside an earlier S1 over [a b].
        let history = vec![
            user("a"),
            user("b"),
            checkpoint("S1", &["a", "b"]),
            user("c"),
            user("d"),
            checkpoint("S2", &["c", "d"]),
            user("e"),
        ];
        let shadows = shadowed_by_checkpoints(&history);
        assert_eq!(shadows.len(), 2);
        assert_eq!(shadows[0].checkpoint, "S2");
        assert_eq!(ids(&shadows[0]), ["c", "d"]);
        assert_eq!(shadows[1].checkpoint, "S1");
        assert_eq!(ids(&shadows[1]), ["a", "b"]);
    }

    #[test]
    fn empty_and_unknown_ids_are_harmless() {
        let history = vec![user("a"), checkpoint("S1", &["a", "ghost"])];
        let shadows = shadowed_by_checkpoints(&history);
        assert_eq!(ids(&shadows[0]), ["a", "ghost"]);
        assert!(shadowed_by_checkpoints(&[user("a")]).is_empty());
    }
}
