//! Live streaming frames — the ephemeral display channel.
//!
//! Frames are what a frontend renders WHILE a turn runs; events are what
//! the log commits. Disjoint types on purpose (invariant #9): frames are
//! never persisted, and nothing durable is ever derived from them. When a
//! step commits, frontends reconcile against the durable event.

use serde::{Deserialize, Serialize};

use crate::events::{ChunkDelta, EventId, SessionId, ToolCallId};

/// One live frame, pushed over SSE / channel to attached frontends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// A model request opened.
    StepStarted { session: SessionId, turn: u32 },
    /// Streaming delta from the model (text/thinking/tool args).
    Delta { session: SessionId, chunk: ChunkDelta },
    /// A tool started executing.
    ToolStarted { session: SessionId, call: ToolCallId, name: String },
    /// Incremental tool output (e.g. bash streaming).
    ToolOutput { session: SessionId, call: ToolCallId, output: String },
    /// The step committed; `event` is the durable assistant/message id —
    /// the frontend's reconcile point.
    StepCommitted { session: SessionId, event: EventId },
    /// The turn is over (committed, cancelled, or failed); idle again.
    TurnIdle { session: SessionId },
    /// A compaction summary is running; its source event count and estimated
    /// input tokens let frontends show progress before the durable checkpoint.
    CompactionStarted { session: SessionId, events: usize, estimated_tokens: u64 },
    /// A compaction summary ended, whether it wrote a checkpoint or failed.
    CompactionFinished { session: SessionId, changed: bool },
    /// Durable history changed outside a turn (compaction): reload.
    HistoryChanged { session: SessionId },
    /// A sensitive tool call paused for a one-shot decision (`ask`
    /// policy). Ephemeral like every frame — late clients reconcile the
    /// still-pending set from `GET /api/approvals`.
    ApprovalRequested {
        session: SessionId,
        call: ToolCallId,
        tool: String,
        args: serde_json::Value,
    },
    /// The question is gone (answered, or withdrawn by cancellation);
    /// every attached client drops its prompt.
    ApprovalResolved { session: SessionId, call: ToolCallId },
}
