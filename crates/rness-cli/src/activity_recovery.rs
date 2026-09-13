//! Recover historical activity once, with bounded retries after failures.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use rness_engine::{service::SessionService, subagent::SubagentActivity};

const INITIAL_DELAY: Duration = Duration::from_secs(5);
const MAX_DELAY: Duration = Duration::from_secs(60);

struct Attempt {
    retry_at: Option<Instant>, // None means recovery succeeded.
    delay: Duration,
}

#[derive(Default)]
pub(crate) struct ActivityRecovery {
    attempts: HashMap<String, Attempt>,
}

impl ActivityRecovery {
    fn due(&self, session: &str, now: Instant) -> bool {
        self.attempts.get(session).map_or(true, |attempt| {
            attempt.retry_at.is_some_and(|retry_at| now >= retry_at)
        })
    }

    fn finish(&mut self, session: &str, now: Instant, success: bool) {
        let delay = self
            .attempts
            .get(session)
            .map_or(INITIAL_DELAY, |attempt| (attempt.delay * 2).min(MAX_DELAY));
        self.attempts.insert(
            session.to_owned(),
            Attempt {
                retry_at: (!success).then_some(now + delay),
                delay,
            },
        );
    }

    pub(crate) fn recover(
        &mut self,
        activity: &SubagentActivity,
        sessions: &SessionService,
        session: &str,
    ) {
        if !self.due(session, Instant::now()) {
            return;
        }
        let result = activity.recover(sessions, session);
        // Start the delay after the scan, not before it: a slow failure must
        // never make the next attempt immediately due.
        self.finish(session, Instant::now(), result.is_ok());
        if let Err(error) = result {
            tracing::warn!(%session, %error, "subagent activity recovery failed; retry delayed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_recovery_backs_off_and_caps_delay() {
        let mut recovery = ActivityRecovery::default();
        let mut now = Instant::now();
        assert!(recovery.due("parent", now));
        for secs in [5, 10, 20, 40, 60, 60] {
            recovery.finish("parent", now, false);
            assert!(!recovery.due("parent", now + Duration::from_millis(200)));
            assert!(!recovery.due(
                "parent",
                now + Duration::from_secs(secs) - Duration::from_nanos(1)
            ));
            now += Duration::from_secs(secs);
            assert!(recovery.due("parent", now));
        }
    }

    #[test]
    fn success_stops_retries_without_blocking_other_sessions() {
        let mut recovery = ActivityRecovery::default();
        let now = Instant::now();
        recovery.finish("failed", now, false);
        assert!(recovery.due("other", now));
        recovery.finish("failed", now + INITIAL_DELAY, true);
        assert!(!recovery.due("failed", now + Duration::from_secs(3600)));
        assert!(recovery.due("other", now));
    }
}
