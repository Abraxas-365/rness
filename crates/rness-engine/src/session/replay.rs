//! Replay: the one entry point the turn loop uses to derive a model
//! request's input. history (across forks) -> projection -> invariant
//! check, in one call. If the invariant fails, NO request is built.

use rness_protocol::events::{Envelope, SessionId};

use crate::invariants::{assert_model_visible_logged, InvariantViolation};
use crate::session::branch::{BranchError, SessionStore};
use crate::session::projection::{model_context, ModelContext};

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error(transparent)]
    Branch(#[from] BranchError),
    #[error("invariant violation: {0}")]
    Invariant(#[from] InvariantViolation),
}

/// A session's full history plus the model context derived from it,
/// invariant-checked. What the request builder consumes.
pub struct Replayed {
    pub history: Vec<Envelope>,
    pub context: ModelContext,
}

pub fn replay(store: &SessionStore, session: &SessionId) -> Result<Replayed, ReplayError> {
    let history = store.history(session)?;
    let context = model_context(&history);
    assert_model_visible_logged(&context, &history)?;
    Ok(Replayed { history, context })
}
