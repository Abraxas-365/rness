//! Runtime-asserted invariants (docs/invariants.md). Cheap enough to run
//! on every model request in debug AND release — correctness beats
//! nanoseconds here.

use rness_protocol::events::{Envelope, SessionEvent};

use crate::session::projection::ModelContext;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvariantViolation {
    /// Invariant #1: model-visible means logged. A model context cited a
    /// source event that is not in the session's history.
    #[error("model context cites event '{0}' which is not in the log")]
    UnloggedSource(String),
    /// Invariant #5: attempts never reach model context.
    #[error("model context cites attempt event '{0}'")]
    AttemptInContext(String),
}

/// Assert that everything in a derived model context traces back to a
/// committed, non-attempt event in `history`. Called before every model
/// request (the request builder refuses to send on violation).
pub fn assert_model_visible_logged(
    ctx: &ModelContext,
    history: &[Envelope],
) -> Result<(), InvariantViolation> {
    for source in &ctx.sources {
        match history.iter().find(|e| &e.id == source) {
            None => return Err(InvariantViolation::UnloggedSource(source.clone())),
            Some(env) => {
                if matches!(env.event, SessionEvent::AssistantAttempt(_)) {
                    return Err(InvariantViolation::AttemptInContext(source.clone()));
                }
            }
        }
    }
    Ok(())
}
