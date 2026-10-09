//! Runtime-asserted invariants (docs/invariants.md). Cheap enough to run
//! on every model request in debug AND release — correctness beats
//! nanoseconds here.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

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

/// Multiply-rotate hasher for event ids (FxHash-style). Ids are local log
/// data, not attacker-chosen keys, and SipHash dominated the check at 100k
/// events.
#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            let word = u64::from_le_bytes(*word);
            self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(K);
        }
        for &byte in rest {
            self.0 = (self.0.rotate_left(5) ^ u64::from(byte)).wrapping_mul(K);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Assert that everything in a derived model context traces back to a
/// committed, non-attempt event in `history`. Called before every model
/// request (the request builder refuses to send on violation).
///
/// O(history + sources): the history is indexed by id once per call. The
/// first event with a given id wins, exactly like a front-to-back scan.
pub fn assert_model_visible_logged(
    ctx: &ModelContext,
    history: &[impl std::borrow::Borrow<Envelope>],
) -> Result<(), InvariantViolation> {
    if ctx.sources.is_empty() {
        return Ok(());
    }
    let mut by_id: HashMap<&str, &Envelope, BuildHasherDefault<IdHasher>> =
        HashMap::with_capacity_and_hasher(history.len(), Default::default());
    for env in history {
        let env = env.borrow();
        by_id.entry(env.id.as_str()).or_insert(env);
    }
    for source in &ctx.sources {
        match by_id.get(source.as_str()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rness_protocol::events::{
        AssistantAttempt, AttemptOutcome, ContentPart, UserIntent, UserMessage,
    };

    fn env(id: &str, event: SessionEvent) -> Envelope {
        Envelope {
            id: id.into(),
            at: "t".into(),
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

    fn attempt(id: &str) -> Envelope {
        env(
            id,
            SessionEvent::AssistantAttempt(AssistantAttempt {
                model: "m".into(),
                outcome: AttemptOutcome::Cancelled,
                chunks: vec![],
            }),
        )
    }

    fn ctx(sources: &[&str]) -> ModelContext {
        ModelContext {
            sources: sources.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn logged_sources_pass() {
        let history = vec![user("a"), user("b")];
        assert_eq!(
            assert_model_visible_logged(&ctx(&["a", "b"]), &history),
            Ok(())
        );
        assert_eq!(assert_model_visible_logged(&ctx(&[]), &history), Ok(()));
    }

    #[test]
    fn unlogged_source_fails() {
        let history = vec![user("a")];
        assert_eq!(
            assert_model_visible_logged(&ctx(&["a", "zz"]), &history),
            Err(InvariantViolation::UnloggedSource("zz".into()))
        );
    }

    #[test]
    fn attempt_source_fails() {
        let history = vec![user("a"), attempt("b")];
        assert_eq!(
            assert_model_visible_logged(&ctx(&["b"]), &history),
            Err(InvariantViolation::AttemptInContext("b".into()))
        );
    }

    #[test]
    fn duplicate_id_first_occurrence_wins() {
        // Same semantics as the former front-to-back `find`.
        let history = vec![user("d"), attempt("d")];
        assert_eq!(assert_model_visible_logged(&ctx(&["d"]), &history), Ok(()));
        let history = vec![attempt("d"), user("d")];
        assert_eq!(
            assert_model_visible_logged(&ctx(&["d"]), &history),
            Err(InvariantViolation::AttemptInContext("d".into()))
        );
    }

    #[test]
    fn large_check_is_linear() {
        let history: Vec<Envelope> = (0..200_000).map(|i| user(&format!("e{i}"))).collect();
        let sources: Vec<String> = (0..200_000).step_by(2).map(|i| format!("e{i}")).collect();
        let ctx = ModelContext {
            sources,
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        assert_eq!(assert_model_visible_logged(&ctx, &history), Ok(()));
        // Quadratic was minutes here; generous bound for debug builds.
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            t0.elapsed()
        );
    }
}
