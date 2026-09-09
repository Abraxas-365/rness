//! The provider seam. Adapters (anthropic, openai, …) implement this;
//! the turn loop consumes it. The engine owns the trait — provider crates
//! are leaf plugins (docs §7 rule 4).
//!
//! A provider's `step` consumes an invariant-checked [`ModelContext`] and
//! returns a fully-committed message OR a preserved failure. Streaming to
//! frontends happens inside the adapter (it emits live frames while
//! accumulating chunks); the engine only sees the durable result —
//! keeping the frames/events split (invariant #9) at the seam.

use async_trait::async_trait;
use rness_protocol::events::{AssistantMessage, ChunkDelta, TimedChunk};
use tokio_util::sync::CancellationToken;

use crate::session::projection::ModelContext;
use crate::tools::ToolSpec;

/// Live-delta observer: called by the adapter for every streamed chunk,
/// in arrival order. Ephemeral by contract (invariant #9) — the durable
/// record is the chunk list on the committed message/attempt.
pub type DeltaSink<'a> = &'a (dyn Fn(&ChunkDelta) + Send + Sync);

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub code: &'static str,
    pub retry_after: Option<std::time::Duration>,
    pub message: String,
    pub retryable: bool,
}

/// Everything an adapter needs to build one model request.
pub struct StepRequest<'a> {
    /// Invariant-checked, replay-derived conversation.
    pub context: &'a ModelContext,
    /// System prompt (may be empty).
    pub system: &'a str,
    /// Tools to advertise, name-sorted.
    pub tools: &'a [ToolSpec],
    /// Where live deltas go while the stream runs. `None` = headless.
    pub on_delta: Option<DeltaSink<'a>>,
}

/// Result of one model request.
pub enum StepOutcome {
    /// Stream completed; the message embeds its exact chunk record.
    Committed(AssistantMessage),
    /// Cancelled mid-stream; whatever streamed is preserved.
    Cancelled { partial: Vec<TimedChunk> },
    /// The request/stream failed; partial chunks preserved.
    Failed { error: ProviderError, partial: Vec<TimedChunk> },
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Model identifier recorded on messages and attempts.
    fn model(&self) -> &str;

    /// Execute one model request.
    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome;
}
