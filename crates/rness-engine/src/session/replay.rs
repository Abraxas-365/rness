//! Replay: the one entry point the turn loop uses to derive a model
//! request's input. history (across forks) -> projection -> invariant
//! check, in one call. If the invariant fails, NO request is built.

use std::sync::Arc;

use rness_protocol::events::{Envelope, SessionEvent, SessionId};

use crate::invariants::{assert_model_visible_logged, InvariantViolation};
use crate::session::branch::{BranchError, SessionStore};
use crate::session::log::SessionLog;
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

/// A session holds events this build does not understand (written by a
/// newer rness). Any of them could be model-visible or change what a
/// compaction folds, so every path that builds a model request or commits a
/// compaction/prune must refuse (invariant #1). One check, many callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewerEvents(pub Vec<String>);

/// `Err` iff `history` contains an unknown event type.
pub fn require_known(history: &[impl std::borrow::Borrow<Envelope>]) -> Result<(), NewerEvents> {
    match unknown_kinds(history) {
        kinds if kinds.is_empty() => Ok(()),
        kinds => Err(NewerEvents(kinds)),
    }
}

impl Replayed {
    /// See [`NewerEvents`].
    pub fn require_known(&self) -> Result<(), NewerEvents> {
        if self.unknown.is_empty() {
            Ok(())
        } else {
            Err(NewerEvents(self.unknown.clone()))
        }
    }
}

pub fn replay(store: &SessionStore, session: &SessionId) -> Result<Replayed, ReplayError> {
    store.count_replay();
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

/// The latest [`replay`] of the session a writer handle appends to, reused
/// until that handle writes again. Every append bumps
/// [`SessionLog::generation`], so a cached value can never miss an event
/// committed through the handle; the handle holds the writer lock, so no
/// other writer exists. One replay per step instead of one per call site.
#[derive(Default)]
pub struct ReplayCache {
    cached: Option<(SessionId, u64, Arc<Replayed>)>,
}

impl ReplayCache {
    /// The replay as of `log`'s latest write (re-derived if it wrote since).
    pub fn get(
        &mut self,
        store: &SessionStore,
        log: &SessionLog,
    ) -> Result<Arc<Replayed>, ReplayError> {
        if let Some((session, generation, replayed)) = &self.cached {
            if session == log.session() && *generation == log.generation() {
                // Stale-cache guard: nothing may have reached the log
                // except through this handle.
                debug_assert_eq!(
                    store
                        .history(session)
                        .ok()
                        .and_then(|h| h.last().map(|e| e.id.clone())),
                    replayed.history.last().map(|e| e.id.clone()),
                    "session log changed behind the writer handle"
                );
                return Ok(replayed.clone());
            }
        }
        let replayed = Arc::new(replay(store, log.session())?);
        self.cached = Some((log.session().clone(), log.generation(), replayed.clone()));
        Ok(replayed)
    }
}
