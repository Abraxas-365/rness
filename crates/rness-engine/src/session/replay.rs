//! Replay: the one entry point the turn loop uses to derive a model
//! request's input. history (across forks) -> projection -> invariant
//! check, in one call. If the invariant fails, NO request is built.

use std::sync::Arc;

use rness_protocol::events::{Envelope, SessionEvent, SessionId};

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
    pub history: Vec<Arc<Envelope>>,
    pub context: ModelContext,
    /// Distinct `type`s of events this build does not know (written by a
    /// newer rness), in first-seen order. Empty for normal sessions; the
    /// turn loop refuses to run a model turn otherwise.
    pub unknown: Vec<String>,
}

/// Distinct unknown event types in `history`, first-seen order.
pub fn unknown_kinds(history: &[impl std::borrow::Borrow<Envelope>]) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for env in history {
        if let SessionEvent::Unknown(unknown) = &env.borrow().event {
            if !kinds.contains(&unknown.kind) {
                kinds.push(unknown.kind.clone());
            }
        }
    }
    kinds
}

pub fn replay(store: &SessionStore, session: &SessionId) -> Result<Replayed, ReplayError> {
    let history = store.history(session)?;
    let context = model_context(&history);
    assert_model_visible_logged(&context, &history)?;
    let unknown = unknown_kinds(&history);
    Ok(Replayed {
        history,
        context,
        unknown,
    })
}
