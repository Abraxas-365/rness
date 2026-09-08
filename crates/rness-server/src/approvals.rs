//! Remote approvals: the server-side answerer for the `ask` policy.
//!
//! The engine pauses a sensitive call and asks the composed answerer;
//! this one publishes the question as an ephemeral frame
//! (`approval_requested`) and parks until some HTTP client answers via
//! `POST /api/approvals/:call`. Late/reconnecting clients reconcile the
//! still-pending set from `GET /api/approvals` — same contract as
//! frames vs history.
//!
//! Fail-closed like every answerer: if the waiting future is dropped
//! (turn cancelled, engine shutdown) the question is withdrawn, an
//! `approval_resolved` frame tells clients to drop their prompt, and
//! the decision resolves as `Cancelled`.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use rness_engine::approval::{Answerer, Decision};
use rness_protocol::api::{ApprovalDecision, ApprovalRequest};
use rness_protocol::events::ToolCallId;
use rness_protocol::frames::Frame;
use tokio::sync::{broadcast, oneshot};

struct Pending {
    request: ApprovalRequest,
    respond: oneshot::Sender<Decision>,
}

/// Shared pending-question table + the frame fan-out to announce them.
pub struct RemoteApprovals {
    pending: Mutex<HashMap<ToolCallId, Pending>>,
    frames: broadcast::Sender<Frame>,
}

impl RemoteApprovals {
    pub fn new(frames: broadcast::Sender<Frame>) -> Self {
        Self { pending: Mutex::new(HashMap::new()), frames }
    }

    /// Snapshot of every unanswered question (the reconcile endpoint).
    pub fn pending(&self) -> Vec<ApprovalRequest> {
        self.pending.lock().expect("pending lock").values().map(|p| p.request.clone()).collect()
    }

    /// Answer one question. `false` if the call is unknown — already
    /// answered, withdrawn, or never asked.
    pub fn resolve(&self, call: &str, decision: ApprovalDecision) -> bool {
        let Some(p) = self.pending.lock().expect("pending lock").remove(call) else {
            return false;
        };
        let verdict = match decision {
            ApprovalDecision::Allowed => Decision::Allowed,
            ApprovalDecision::Rejected => Decision::Rejected,
        };
        // A dropped waiter means the turn died first; nothing to do.
        let _ = p.respond.send(verdict);
        true
    }

    fn withdraw(&self, call: &ToolCallId) -> Option<ApprovalRequest> {
        self.pending.lock().expect("pending lock").remove(call).map(|p| p.request)
    }
}

#[async_trait]
impl Answerer for RemoteApprovals {
    async fn answer(&self, request: &ApprovalRequest) -> Decision {
        let (respond, answered) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending lock")
            .insert(request.call.clone(), Pending { request: request.clone(), respond });
        let _ = self.frames.send(Frame::ApprovalRequested {
            session: request.session.clone(),
            call: request.call.clone(),
            tool: request.tool.clone(),
            args: request.args.clone(),
        });

        // If this future is dropped mid-await (turn cancelled), the guard
        // withdraws the question so the table can't leak a dead entry.
        let guard = WithdrawGuard { approvals: self, call: request.call.clone() };
        let decision = answered.await.unwrap_or(Decision::Cancelled);
        drop(guard);

        let _ = self.frames.send(Frame::ApprovalResolved {
            session: request.session.clone(),
            call: request.call.clone(),
        });
        decision
    }
}

struct WithdrawGuard<'a> {
    approvals: &'a RemoteApprovals,
    call: ToolCallId,
}

impl Drop for WithdrawGuard<'_> {
    fn drop(&mut self) {
        // Normal resolution already removed the entry; this only fires
        // work when the question died unanswered.
        if let Some(request) = self.approvals.withdraw(&self.call) {
            let _ = self.approvals.frames.send(Frame::ApprovalResolved {
                session: request.session,
                call: self.call.clone(),
            });
        }
    }
}
