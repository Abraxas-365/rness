//! HTTP/SSE server exposing SessionService. ONE wire protocol (lesson
//! from deepseek's RPC+SDK+ACP+headless sprawl: pick one).
//!
//! The wire IS `rness-protocol`: requests are [`ClientRequest`],
//! history is [`History`], live streaming is [`Frame`] over SSE. A
//! remote frontend and the in-process TUI consume the exact same
//! shapes — the transport is the only difference.
//!
//! Surface (deliberately small):
//!   GET  /api/sessions            -> ["id", ...]
//!   POST /api/sessions            {workspace?} -> {session}
//!   GET  /api/sessions/:id        -> History (log-order envelopes)
//!   GET  /api/sessions/:id/phase  -> {phase: "idle"|"running"}
//!   POST /api/sessions/:id/fork   {at?} -> {session}
//!   POST /api/request             ClientRequest -> {status}
//!   GET  /api/approvals           -> [ApprovalRequest] still pending
//!   POST /api/approvals/:call     {decision: "allowed"|"rejected"}
//!   GET  /api/events              SSE: every Frame, all sessions
//!   GET  /api/events/:id          SSE: frames filtered to one session
//!
//! The CLI requires bearer authentication for non-loopback binds; custom hosts
//! install `auth::authorize` explicitly. TLS termination remains operator-owned.

pub mod approvals;
pub mod auth;
pub mod routes;
pub mod sse;

use std::sync::Arc;

use rness_engine::service::SessionService;
use tokio::sync::broadcast;

/// Shared server state: the engine service + a frame fan-out.
///
/// The broadcast channel decouples the engine's synchronous EventBus
/// from any number of (possibly slow) SSE clients: the bus callback
/// only does a non-blocking `send`, laggy receivers drop frames (they
/// reconcile from durable history — frames are ephemeral by contract).
#[derive(Clone)]
pub struct ServerState {
    pub questions: Arc<rness_engine::questions::Questions>,
    pub sessions: Arc<SessionService>,
    pub frames: broadcast::Sender<rness_protocol::frames::Frame>,
    /// Remote answerer for the `ask` policy. Always constructed; it only
    /// receives questions if the composition root mounts it on the
    /// engine's approval seam (`tools.approvals().set_answerer(...)`).
    pub approvals: Arc<approvals::RemoteApprovals>,
}

impl ServerState {
    pub fn new(sessions: Arc<SessionService>) -> Self {
        let (frames, _) = broadcast::channel(1024);
        let approvals = Arc::new(approvals::RemoteApprovals::new(frames.clone()));
        Self {
            sessions,
            frames,
            approvals,
            questions: Arc::new(Default::default()),
        }
    }

    /// The callback to hang on the kernel bus (`FrameEv`).
    pub fn frame_sink(&self) -> impl Fn(&rness_protocol::frames::Frame) + Send + Sync + 'static {
        let tx = self.frames.clone();
        move |frame| {
            let _ = tx.send(frame.clone());
        }
    }
}

/// Build the axum router over the state.
pub fn router(state: ServerState) -> axum::Router {
    routes::router(state)
}
