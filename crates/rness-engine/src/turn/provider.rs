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

/// Enforces the selected model's declared modalities on every request,
/// including requests after tools return images within the same turn.
pub(crate) struct ImageCapabilityProvider {
    pub inner: std::sync::Arc<dyn Provider>,
}

#[async_trait]
impl Provider for ImageCapabilityProvider {
    fn model(&self) -> &str { self.inner.model() }

    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        use crate::session::projection::ModelTurn;
        use rness_protocol::events::{ContentPart, ToolResultContentPart};
        if cancel.is_cancelled() { return StepOutcome::Cancelled { partial: vec![] }; }
        let has_images = request.context.turns.iter().any(|turn| match turn {
            ModelTurn::User { content } | ModelTurn::Assistant { content } => content.iter().any(|p| matches!(p, ContentPart::Image { .. })),
            ModelTurn::ToolResults { results } => results.iter().any(|r| r.content.iter().any(|p| matches!(p, ToolResultContentPart::Image { .. }))),
        });
        if has_images {
            return StepOutcome::Failed {
                error: ProviderError { code: "UNSUPPORTED_CONTENT", retry_after: None,
                    message: "selected model explicitly disables image input".into(), retryable: false },
                partial: vec![],
            };
        }
        self.inner.step(request, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rness_protocol::events::*;
    use crate::session::projection::ModelTurn;

    struct NeverCalled;
    #[async_trait]
    impl Provider for NeverCalled {
        fn model(&self) -> &str { "text-only" }
        async fn step(&self, _: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
            panic!("image request reached text-only provider")
        }
    }

    #[tokio::test]
    async fn tool_images_are_rejected_before_provider_request() {
        let provider = ImageCapabilityProvider { inner: std::sync::Arc::new(NeverCalled) };
        let context = ModelContext { turns: vec![ModelTurn::ToolResults { results: vec![ToolResult {
            call: "camera-1".into(), name: "camera".into(), output: String::new(),
            content: vec![ToolResultContentPart::Image { attachment: ImageRef {
                id: "stored".into(), media_type: "image/png".into(), bytes: 1, width: 1, height: 1,
            } }], is_error: false, duration_ms: 0, tasks: None, plan_review: None, presentation: None,
        }] }], ..Default::default() };
        let request = || StepRequest { context: &context, system: "", tools: &[], on_delta: None };
        let cancel = CancellationToken::new();
        let StepOutcome::Failed { error, partial } = provider.step(request(), &cancel).await else { panic!("expected rejection") };
        assert_eq!(error.code, "UNSUPPORTED_CONTENT");
        assert!(!error.retryable);
        assert!(partial.is_empty());
        cancel.cancel();
        assert!(matches!(provider.step(request(), &cancel).await, StepOutcome::Cancelled { .. }));
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Model identifier recorded on messages and attempts.
    fn model(&self) -> &str;

    /// Configure attachment resolution before the provider is shared.
    fn configure_images(&mut self, _store: std::sync::Arc<crate::images::ImageStore>, _policy: crate::images::ImagePolicy) {}

    /// Execute one model request.
    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome;
}
