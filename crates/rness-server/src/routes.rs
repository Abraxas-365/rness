//! REST routes: thin adapters from HTTP to SessionService. No logic
//! here beyond shape mapping — the service is the API.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rness_protocol::api::{ClientRequest, History};
use serde_json::json;

use crate::ServerState;

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/:id", get(history))
        .route("/api/sessions/:id/tasks", get(tasks))
        .route("/api/sessions/:id/phase", get(phase))
        .route("/api/sessions/:id/fork", post(fork))
        .route("/api/sessions/:id/compact", post(compact))
        .route("/api/sessions/:id/prune", post(prune))
        .route("/api/questions", get(pending_questions))
        .route("/api/questions/:session/:call", post(answer_questions).delete(dismiss_questions))
        .route("/api/request", post(request))
        .route("/api/approvals", get(pending_approvals))
        .route("/api/approvals/:call", post(resolve_approval))
        .route("/api/events", get(crate::sse::all_events))
        .route("/api/events/:id", get(crate::sse::session_events))
        .with_state(state)
}

async fn pending_questions(State(s): State<ServerState>) -> Response { Json(s.questions.pending()).into_response() }
async fn answer_questions(State(s): State<ServerState>, Path((session, call)): Path<(String, String)>, Json(answers): Json<rness_engine::questions::Answers>) -> Response {
    match s.questions.resolve(&session, &call, answers) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
    }
}
async fn dismiss_questions(State(s): State<ServerState>, Path((session, call)): Path<(String, String)>) -> Response {
    if s.questions.dismiss(&session, &call) { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }.into_response()
}

/// Service errors become plain-text 4xx/5xx. Unknown sessions are the
/// caller's fault; everything else is ours.
fn err_response(e: impl std::fmt::Display) -> Response {
    let msg = e.to_string();
    let status = if msg.contains("not found") || msg.contains("unknown") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, msg).into_response()
}

async fn list_sessions(State(s): State<ServerState>) -> Response {
    match s.sessions.list() {
        Ok(ids) => Json(ids).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(serde::Deserialize, Default)]
struct CreateBody {
    workspace: Option<String>,
}

async fn create_session(
    State(s): State<ServerState>,
    body: Option<Json<CreateBody>>,
) -> Response {
    let workspace = body.and_then(|Json(b)| b.workspace);
    match s.sessions.create(workspace) {
        Ok(id) => Json(json!({ "session": id })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn history(State(s): State<ServerState>, Path(id): Path<String>) -> Response {
    match s.sessions.store().history(&id) {
        Ok(envelopes) => Json(History { session: id, envelopes }).into_response(),
        Err(e) => err_response(e),
    }
}

async fn tasks(State(s): State<ServerState>, Path(id): Path<String>) -> Response {
    match s.sessions.tasks(&id) {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => err_response(error),
    }
}

async fn phase(State(s): State<ServerState>, Path(id): Path<String>) -> Response {
    let phase = match s.sessions.phase(&id) {
        rness_engine::inbox::Phase::Idle => "idle",
        rness_engine::inbox::Phase::Running => "running",
    };
    Json(json!({ "phase": phase })).into_response()
}

#[derive(serde::Deserialize, Default)]
struct ForkBody {
    at: Option<String>,
}

async fn fork(
    State(s): State<ServerState>,
    Path(id): Path<String>,
    body: Option<Json<ForkBody>>,
) -> Response {
    let at = body.and_then(|Json(b)| b.at);
    match s.sessions.fork(&id, at) {
        Ok(child) => Json(json!({ "session": child })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(serde::Deserialize, Default)]
struct CompactBody {
    keep_turns: Option<usize>,
}

async fn compact(
    State(s): State<ServerState>,
    Path(id): Path<String>,
    body: Option<Json<CompactBody>>,
) -> Response {
    let keep = body.and_then(|Json(b)| b.keep_turns).unwrap_or(2);
    match s.sessions.compact(&id, keep).await {
        Ok(r) => Json(json!({ "shadowed": r.shadowed, "summary": r.summary })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(serde::Deserialize)]
struct PruneBody {
    threshold: usize,
    head: usize,
    tail: usize,
    keep_turns: usize,
}

async fn prune(
    State(s): State<ServerState>,
    Path(id): Path<String>,
    Json(b): Json<PruneBody>,
) -> Response {
    let opts = rness_engine::service::PruneOptions {
        threshold_chars: b.threshold,
        head_chars: b.head,
        tail_chars: b.tail,
        keep_turns: b.keep_turns,
    };
    match s.sessions.prune_tool_results(&id, opts) {
        Ok(n) => Json(json!({ "pruned": n })).into_response(),
        Err(e) => err_response(e),
    }
}

/// Still-pending approval questions (the reconcile point for clients
/// that attached after the `approval_requested` frame flew by).
async fn pending_approvals(State(s): State<ServerState>) -> Response {
    Json(s.approvals.pending()).into_response()
}

#[derive(serde::Deserialize)]
struct ApprovalBody {
    decision: rness_protocol::api::ApprovalDecision,
}

/// Answer one paused call. One-shot: a second answer (or an unknown
/// call id) is the caller's race to lose — 404.
async fn resolve_approval(
    State(s): State<ServerState>,
    Path(call): Path<String>,
    Json(b): Json<ApprovalBody>,
) -> Response {
    if s.approvals.resolve(&call, b.decision) {
        Json(json!({ "status": "resolved" })).into_response()
    } else {
        (StatusCode::NOT_FOUND, "no pending approval for that call").into_response()
    }
}

/// The single mutation endpoint: a [`ClientRequest`], exactly what the
/// TUI's Backend sends in-process.
async fn request(State(s): State<ServerState>, Json(req): Json<ClientRequest>) -> Response {
    match req {
        ClientRequest::Send { session, intent, content } => {
            match s.sessions.send_async(session, intent, content).await {
                Ok(outcome) => {
                    let status = match outcome {
                        rness_engine::inbox::Disposition::Command(result) => return Json(json!({"status":"command", "result":result})).into_response(),
                        rness_engine::inbox::Disposition::StartTurn => "started",
                        rness_engine::inbox::Disposition::Queued => "queued",
                        rness_engine::inbox::Disposition::LogOnly => "logged",
                    };
                    Json(json!({ "status": status })).into_response()
                }
                Err(e) => err_response(e),
            }
        }
        ClientRequest::Retry { session } => match s.sessions.retry(&session) {
            Ok(_) => Json(json!({ "status": "started" })).into_response(),
            Err(e) => err_response(e),
        },
        ClientRequest::Cancel { session } => {
            s.sessions.cancel(&session);
            Json(json!({ "status": "cancelled" })).into_response()
        }
    }
}
