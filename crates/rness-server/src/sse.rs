//! SSE frame streams. Frames are ephemeral by contract (protocol
//! invariant #9): a lagging client LOSES frames rather than stalling
//! anyone, and reconciles from durable history via GET /api/sessions/:id.

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_core::Stream;
use rness_protocol::frames::Frame;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::ServerState;

fn frame_event(frame: &Frame) -> Event {
    // Serialization of protocol types cannot fail.
    Event::default().data(serde_json::to_string(frame).expect("frame serializes"))
}

/// Every frame from every session (a dashboard's feed).
pub async fn all_events(
    State(s): State<ServerState>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = BroadcastStream::new(s.frames.subscribe()).filter_map(|item| match item {
        Ok(frame) => Some(Ok(frame_event(&frame))),
        // Lagged: frames dropped for this client; it reconciles later.
        Err(BroadcastStreamRecvError::Lagged(_)) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Frames filtered to one session (a chat view's feed).
pub async fn session_events(
    State(s): State<ServerState>,
    Path(id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = BroadcastStream::new(s.frames.subscribe()).filter_map(move |item| match item {
        Ok(frame) if frame_session(&frame) == id => Some(Ok(frame_event(&frame))),
        Ok(_) => None,
        Err(BroadcastStreamRecvError::Lagged(_)) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn frame_session(frame: &Frame) -> &str {
    match frame {
        Frame::StepStarted { session, .. }
        | Frame::Delta { session, .. }
        | Frame::ToolStarted { session, .. }
        | Frame::ToolOutput { session, .. }
        | Frame::StepCommitted { session, .. }
        | Frame::TurnIdle { session }
        | Frame::HistoryChanged { session }
        | Frame::ApprovalRequested { session, .. }
        | Frame::ApprovalResolved { session, .. } => session,
    }
}
