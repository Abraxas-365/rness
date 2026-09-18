//! The inbox: ALL user input enters through here, classified by intent
//! (followup/steer/inject semantics).
//!
//! Semantics by phase:
//! - `Idle`: a followup starts a turn immediately. Steer degrades to
//!   followup (nothing to steer). Inject appends to the log silently.
//! - `Running`: followups QUEUE (drained when the turn ends and each
//!   starts a new turn). Steers are delivered at the NEXT step boundary —
//!   the model sees them mid-turn. Injects join the steers at the next
//!   boundary too: the running turn owns the log (single writer), and a
//!   boundary append is model-visibly identical to an immediate one.
//!
//! The inbox owns classification and queueing only; the turn loop drains
//! it at the boundaries it defines. Everything accepted is committed to
//! the log as a `user/message` with its intent — replay needs no inbox.

use std::collections::VecDeque;

use rness_protocol::events::{ContentPart, UserIntent};

/// What the engine is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Running,
}

/// One accepted input, waiting for its delivery point.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub source: Option<rness_protocol::events::MessageSource>,
    pub intent: UserIntent,
    pub content: Vec<ContentPart>,
}

/// What the caller should do with an input it just submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    Command(crate::interaction::CommandResult),
    /// Start a turn with it now (it was committed as followup).
    StartTurn,
    /// Queued; a later drain will surface it.
    Queued,
    /// Append to the log only; no turn implications.
    LogOnly,
}

/// Phase-aware input queue. Single-session; the service owns one per
/// live session. Not thread-safe by itself — the service serializes.
#[derive(Debug, Default)]
pub struct Inbox {
    phase: Phase,
    /// Followups waiting for the current turn to end.
    followups: VecDeque<Pending>,
    /// Steers waiting for the next step boundary.
    steers: VecDeque<Pending>,
}

impl Inbox {
    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
    }

    /// Classify an input. The caller commits the event to the log first
    /// (with the possibly-degraded intent returned here), then acts on
    /// the disposition.
    pub fn submit(
        &mut self,
        intent: UserIntent,
        content: Vec<ContentPart>,
    ) -> (UserIntent, Disposition) {
        self.submit_sourced(intent, content, None)
    }
    pub fn submit_sourced(
        &mut self,
        intent: UserIntent,
        content: Vec<ContentPart>,
        source: Option<rness_protocol::events::MessageSource>,
    ) -> (UserIntent, Disposition) {
        match (self.phase, intent) {
            (Phase::Idle, UserIntent::Followup | UserIntent::Steer) => {
                // Steering an idle agent is just a prompt.
                (UserIntent::Followup, Disposition::StartTurn)
            }
            (Phase::Idle, UserIntent::Inject) => (UserIntent::Inject, Disposition::LogOnly),
            (Phase::Running, UserIntent::Followup) => {
                self.followups.push_back(Pending {
                    source,
                    intent: UserIntent::Followup,
                    content,
                });
                (UserIntent::Followup, Disposition::Queued)
            }
            (Phase::Running, UserIntent::Steer) => {
                self.steers.push_back(Pending {
                    source,
                    intent: UserIntent::Steer,
                    content,
                });
                (UserIntent::Steer, Disposition::Queued)
            }
            (Phase::Running, UserIntent::Inject) => {
                // The running turn owns the log; deliver at the next
                // boundary, intent preserved.
                self.steers.push_back(Pending {
                    source,
                    intent: UserIntent::Inject,
                    content,
                });
                (UserIntent::Inject, Disposition::Queued)
            }
        }
    }

    /// Steers due at a step boundary. Called by the turn loop before each
    /// model request.
    pub fn drain_steers(&mut self) -> Vec<Pending> {
        self.steers.drain(..).collect()
    }

    /// Next queued followup, surfaced when a turn ends. Each starts its
    /// own turn.
    pub fn pop_followup(&mut self) -> Option<Pending> {
        self.followups.pop_front()
    }

    pub fn has_steers(&self) -> bool {
        !self.steers.is_empty()
    }

    pub fn has_followups(&self) -> bool {
        !self.followups.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: t.into() }]
    }

    #[test]
    fn idle_semantics() {
        let mut inbox = Inbox::default();
        assert_eq!(
            inbox.submit(UserIntent::Followup, text("go")),
            (UserIntent::Followup, Disposition::StartTurn)
        );
        // Steer while idle degrades to followup.
        assert_eq!(
            inbox.submit(UserIntent::Steer, text("go")),
            (UserIntent::Followup, Disposition::StartTurn)
        );
        assert_eq!(
            inbox.submit(UserIntent::Inject, text("ctx")),
            (UserIntent::Inject, Disposition::LogOnly)
        );
    }

    #[test]
    fn running_semantics_and_drains() {
        let mut inbox = Inbox::default();
        inbox.set_phase(Phase::Running);

        assert_eq!(
            inbox.submit(UserIntent::Followup, text("next")).1,
            Disposition::Queued
        );
        assert_eq!(
            inbox.submit(UserIntent::Steer, text("stop that")).1,
            Disposition::Queued
        );
        assert_eq!(
            inbox.submit(UserIntent::Steer, text("also this")).1,
            Disposition::Queued
        );
        assert_eq!(
            inbox.submit(UserIntent::Inject, text("fyi")).1,
            Disposition::Queued
        );

        // Step boundary: steers + injects drain in order, once.
        let steers = inbox.drain_steers();
        assert_eq!(steers.len(), 3);
        assert_eq!(steers[2].intent, UserIntent::Inject);
        assert!(inbox.drain_steers().is_empty());

        // Turn end: followups pop one at a time.
        inbox.set_phase(Phase::Idle);
        assert!(inbox.pop_followup().is_some());
        assert!(inbox.pop_followup().is_none());
    }
}
