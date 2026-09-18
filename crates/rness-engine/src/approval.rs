//! Approval seam (dsh user-approval pattern, fail-open composition).
//!
//! Policy decides BEFORE any answerer runs:
//! - `Allow` (rness default): every tool runs unconditionally — no
//!   questions exist, nothing to fail.
//! - `Ask`: sensitive tool calls pause for a one-shot decision from the
//!   composed answerer. WITHIN this mode the seam is fail-closed, like
//!   dsh: no answerer mounted, or an answerer that drops the question,
//!   blocks the call instead of silently allowing it.
//! - `Never`: sensitive calls are rejected deterministically (strict
//!   headless/CI stance); the answerer is never consulted.
//!
//! Grants are one-shot: a decision applies to that call only. The
//! decision's evidence lands in the durable log as the tool result.

use std::sync::RwLock;

use async_trait::async_trait;
/// The request shape is protocol-owned: frontends see the SAME type.
pub use rness_protocol::api::ApprovalRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    #[default]
    Allow,
    Ask,
    Never,
}

impl std::str::FromStr for Policy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(Policy::Allow),
            "ask" => Ok(Policy::Ask),
            "never" => Ok(Policy::Never),
            other => Err(format!(
                "unknown approval policy '{other}' (allow|ask|never)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolPolicy {
    Allow,
    Ask,
    Deny,
}

/// The one-shot verdict on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allowed — for this call only.
    Allowed,
    /// The user said no.
    Rejected,
    /// The question was withdrawn (UI gone, turn cancelled).
    Cancelled,
    /// No answerer could decide — fail closed.
    Unavailable,
}

/// Whoever can answer: the TUI overlay, a test, an automation bridge.
#[async_trait]
pub trait Answerer: Send + Sync {
    async fn answer(&self, request: &ApprovalRequest) -> Decision;
}

/// The composed approval service. `Default` = `Allow` with no answerer,
/// which behaves exactly like not having the seam at all.
#[derive(Default)]
pub struct Approvals {
    rules: RwLock<std::collections::BTreeMap<String, ToolPolicy>>,
    policy: RwLock<Policy>,
    answerer: RwLock<Option<std::sync::Arc<dyn Answerer>>>,
}

impl Approvals {
    pub fn with_policy(policy: Policy) -> Self {
        Self {
            policy: RwLock::new(policy),
            ..Default::default()
        }
    }

    pub fn policy(&self) -> Policy {
        *self.policy.read().expect("policy lock")
    }

    pub fn set_policy(&self, policy: Policy) {
        *self.policy.write().expect("policy lock") = policy;
    }

    /// Mount the (single, terminal) answerer.
    pub fn set_answerer(&self, answerer: std::sync::Arc<dyn Answerer>) {
        *self.answerer.write().expect("answerer lock") = Some(answerer);
    }

    /// Decide whether `request` may run. Policy first, answerer second.
    pub fn set_rules(&self, rules: std::collections::BTreeMap<String, ToolPolicy>) {
        *self.rules.write().expect("rules lock") = rules;
    }

    pub async fn check_tool(&self, request: &ApprovalRequest, sensitive: bool) -> Decision {
        let rule = self
            .rules
            .read()
            .expect("rules lock")
            .get(&request.tool)
            .copied();
        let policy = match rule {
            Some(ToolPolicy::Allow) => Policy::Allow,
            Some(ToolPolicy::Ask) => Policy::Ask,
            Some(ToolPolicy::Deny) => Policy::Never,
            None if sensitive => self.policy(),
            None => Policy::Allow,
        };
        self.decide(request, policy).await
    }

    pub async fn check(&self, request: &ApprovalRequest) -> Decision {
        self.decide(request, self.policy()).await
    }

    async fn decide(&self, request: &ApprovalRequest, policy: Policy) -> Decision {
        match policy {
            Policy::Allow => Decision::Allowed,
            Policy::Never => Decision::Rejected,
            Policy::Ask => {
                let answerer = self.answerer.read().expect("answerer lock").clone();
                match answerer {
                    None => Decision::Unavailable, // fail closed within ask
                    Some(a) => a.answer(request).await,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Always(Decision);
    #[async_trait]
    impl Answerer for Always {
        async fn answer(&self, _request: &ApprovalRequest) -> Decision {
            self.0
        }
    }

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            session: "s1".into(),
            call: "c1".into(),
            tool: "Bash".into(),
            args: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn explicit_rules_override_sensitivity_and_global_policy() {
        let approvals = Approvals::default();
        for sensitive in [false, true] {
            assert_eq!(
                approvals.check_tool(&req(), sensitive).await,
                Decision::Allowed
            );
        }
        approvals.set_rules([("Bash".into(), ToolPolicy::Deny)].into());
        assert_eq!(
            approvals.check_tool(&req(), false).await,
            Decision::Rejected
        );
        approvals.set_rules([("Bash".into(), ToolPolicy::Ask)].into());
        assert_eq!(
            approvals.check_tool(&req(), false).await,
            Decision::Unavailable
        );
        approvals.set_answerer(Arc::new(Always(Decision::Allowed)));
        assert_eq!(approvals.check_tool(&req(), false).await, Decision::Allowed);
        approvals.set_answerer(Arc::new(Always(Decision::Rejected)));
        assert_eq!(
            approvals.check_tool(&req(), false).await,
            Decision::Rejected
        );
        approvals.set_policy(Policy::Never);
        approvals.set_rules([("Bash".into(), ToolPolicy::Allow)].into());
        assert_eq!(approvals.check_tool(&req(), true).await, Decision::Allowed);
        approvals.set_rules(Default::default());
        assert_eq!(approvals.check_tool(&req(), true).await, Decision::Rejected);
        assert_eq!(approvals.check_tool(&req(), false).await, Decision::Allowed);
    }

    #[tokio::test]
    async fn allow_never_consults_the_answerer() {
        let approvals = Approvals::default(); // default policy = Allow
        approvals.set_answerer(Arc::new(Always(Decision::Rejected)));
        assert_eq!(approvals.check(&req()).await, Decision::Allowed);
    }

    #[tokio::test]
    async fn never_rejects_without_consulting() {
        let approvals = Approvals::with_policy(Policy::Never);
        approvals.set_answerer(Arc::new(Always(Decision::Allowed)));
        assert_eq!(approvals.check(&req()).await, Decision::Rejected);
    }

    #[tokio::test]
    async fn ask_without_answerer_fails_closed() {
        let approvals = Approvals::with_policy(Policy::Ask);
        assert_eq!(approvals.check(&req()).await, Decision::Unavailable);
    }

    #[tokio::test]
    async fn ask_delegates_to_the_answerer() {
        let approvals = Approvals::with_policy(Policy::Ask);
        approvals.set_answerer(Arc::new(Always(Decision::Allowed)));
        assert_eq!(approvals.check(&req()).await, Decision::Allowed);
        approvals.set_answerer(Arc::new(Always(Decision::Rejected)));
        assert_eq!(approvals.check(&req()).await, Decision::Rejected);
    }
}
