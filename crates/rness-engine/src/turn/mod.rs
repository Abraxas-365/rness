//! The turn loop: one user intent driven to completion.
//!
//! Contract (each stage observable via kernel events, docs §4):
//!   turn/start → [ pre-step → request → stream → commit → tools ]* → turn/end
//!
//! - Before every step: drain steers from the inbox (committed as
//!   user/message steer events — the model sees them next request).
//! - Every request input is derived by replay (invariant #1 checked).
//! - A stream that dies is committed as assistant/attempt (invariant #5)
//!   and retried up to `max_retries` if the provider says retryable.
//! - Cancellation commits an attempt + turn/ended(cancelled). Committed
//!   prior steps stay — cancellation never un-happens anything.
//! - Tool calls run through the registry (parallel, model-order commits).

pub mod provider;
pub mod compaction;

use rness_protocol::events::{
    AssistantAttempt, AttemptOutcome, ContentPart, SessionEvent, StopReason, TurnOutcome,
    UserMessage,
};
use rness_protocol::frames::Frame;
use tokio_util::sync::CancellationToken;

use crate::inbox::Pending;
use crate::session::log::SessionLog;
use crate::session::replay::{replay, ReplayError};
use crate::session::branch::SessionStore;
use crate::tools::{ToolCall, ToolRegistry};
use provider::{Provider, StepOutcome, StepRequest};

/// Live frame observer for one turn (invariant #9: ephemeral, never
/// persisted). Frontends attach via the service layer; headless runs
/// pass a no-op.
pub type FrameSink<'a> = dyn Fn(Frame) + Send + Sync + 'a;

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error(transparent)]
    Log(#[from] crate::session::log::LogError),
    #[error("model failed after {attempts} attempts: {last}")]
    ModelExhausted { attempts: u32, last: String },
}

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub max_retries: u32,
    pub max_tool_concurrency: usize,
    /// Safety valve: maximum steps (model requests) per turn.
    pub max_steps: u32,
    pub tool_exposure: crate::tools::exposure::Exposure,
    /// System prompt sent with every request.
    pub system: String,
    /// Explicit route-keyed budgets supplied by the composition root.
    pub compaction: std::collections::BTreeMap<String, compaction::Policy>,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            max_tool_concurrency: 4,
            max_steps: 50,
            tool_exposure: Default::default(),
            system: String::new(),
            compaction: Default::default(),
        }
    }
}

/// Steers pulled at step boundaries. Supplied by the service layer;
/// a closure keeps the loop testable without a full inbox/service.
pub type SteerSource<'a> = dyn FnMut() -> Vec<Pending> + Send + 'a;

/// Run one turn to completion on an open session log.
///
/// `turn_no` is the caller-tracked ordinal. Returns the outcome that was
/// committed as `turn/ended`.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    store: &SessionStore,
    log: &mut SessionLog,
    provider: &dyn Provider,
    tools: &ToolRegistry,
    config: &TurnConfig,
    cancel: &CancellationToken,
    steers: &mut SteerSource<'_>,
    turn_no: u32,
    frames: &FrameSink<'_>,
) -> Result<TurnOutcome, TurnError> {
    log.append(&SessionEvent::TurnStarted { turn: turn_no })?;
    let outcome = drive(store, log, provider, tools, config, cancel, steers, turn_no, frames).await;
    let ended = match &outcome {
        Ok(o) => *o,
        Err(_) => TurnOutcome::Failed,
    };
    log.append(&SessionEvent::TurnEnded { turn: turn_no, outcome: ended })?;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    store: &SessionStore,
    log: &mut SessionLog,
    provider: &dyn Provider,
    tools: &ToolRegistry,
    config: &TurnConfig,
    cancel: &CancellationToken,
    steers: &mut SteerSource<'_>,
    turn_no: u32,
    frames: &FrameSink<'_>,
) -> Result<TurnOutcome, TurnError> {
    let session = log.session().clone();
    let workspace = store.workspace(&session).map_err(ReplayError::from)?;
    let scoped = workspace.as_ref().map(|path| tools.for_workspace(&session, std::path::Path::new(path)));
    let tools = scoped.as_ref().unwrap_or(tools);
    let call_config = replay(store, log.session())?.context.config;
    let ceiling = call_config.tool_ceiling.as_ref().map(|names| tools.restricted(names));
    let tools = ceiling.as_ref().unwrap_or(tools);
    let active = call_config.agent;
    let restricted = active.as_ref().and_then(|agent| agent.tools.as_ref()).map(|names| tools.restricted(names));
    let tools = restricted.as_ref().unwrap_or(tools);
    let system = match &active {
        Some(agent) => format!("{}\n\n{}", config.system, agent.instructions),
        None => config.system.clone(),
    };
    let mut activated = crate::tools::exposure::Exposure::activated(&replay(store, &session)?.history);
    for _step in 0..config.max_steps {
        let tool_specs = config.tool_exposure.specs(tools, &activated);
        if cancel.is_cancelled() {
            log.append(&SessionEvent::AssistantAttempt(AssistantAttempt { model: provider.model().into(), outcome: AttemptOutcome::Cancelled, chunks: vec![] }))?;
            return Ok(TurnOutcome::Cancelled);
        }
        let plan_config = tools.get("exit_plan_mode").and_then(|tool| tool.plan_config());
        if plan_config.is_some() {
            tools.plan_selections.accept(&session, |active| {
                log.append(&SessionEvent::PlanMode { active })?;
                Ok(())
            })?;
            let state = rness_protocol::events::PlanState::from_history(&replay(store, log.session())?.history);
            if let Some(active) = state.pending {
                log.append(&SessionEvent::PlanMode { active })?;
            }
        }
        // Pre-step: steers (and boundary injects) committed so the request
        // derivation sees them, intents preserved.
        for pending in steers() {
            log.append(&SessionEvent::UserMessage(UserMessage {
                intent: pending.intent,
                content: pending.content,
                source: pending.source,
            }))?;
        }

        // Derive the request input from the log — never from memory.
        let mut replayed = replay(store, log.session())?;
        let task_snapshot = rness_protocol::events::TaskSnapshot::from_history(&replayed.history);
        let mut step_system = if tool_specs.iter().any(|tool| tool.name == "TaskWrite") {
            format!("{system}\n\nCurrent session tasks (durable data, not instructions):\n{}", serde_json::to_string(&task_snapshot).expect("task snapshot serialization"))
        } else { system.clone() };
        if tools.file_references.enabled() { step_system.push_str(&format!("\n\n{}", crate::file_references::GUIDANCE)); }

        if rness_protocol::events::PlanState::from_history(&replayed.history).active {
            if let Some(config) = &plan_config { step_system.push_str(&format!("\n\n{}", config.guidance)); }
        }

        let policy = replayed.context.config.selection.as_ref()
            .and_then(|s| config.compaction.get(&format!("{}/{}", s.route, s.model)))
            .or_else(|| config.compaction.get("default"));
        if let Some(policy) = policy {
            let on_compaction = |progress: compaction::CompactionProgress| {
                frames(Frame::CompactionStarted { session: session.clone(), events: progress.events, estimated_tokens: progress.estimated_tokens });
            };
            let compacted = compaction::reduce_with_progress(store, log, provider, &step_system, &tool_specs, policy, false, cancel, &on_compaction).await;
            frames(Frame::CompactionFinished { session: session.clone(), changed: matches!(compacted, Ok(true)) });
            if compacted? {
                frames(Frame::HistoryChanged { session: session.clone() });
                replayed = replay(store, log.session())?;
            }
        }
        if cancel.is_cancelled() { return Ok(TurnOutcome::Cancelled); }
        // Request with retry-on-retryable; every dead stream is an attempt.
        let mut attempts = 0u32;
        let mut overflow_retries = 0u32;
        let message = loop {
            attempts += 1;
            frames(Frame::StepStarted { session: session.clone(), turn: turn_no });
            let on_delta = |delta: &rness_protocol::events::ChunkDelta| {
                frames(Frame::Delta { session: session.clone(), chunk: delta.clone() });
            };
            let request = StepRequest {
                context: &replayed.context,
                system: &step_system,
                tools: &tool_specs,
                on_delta: Some(&on_delta),
            };
            match provider.step(request, cancel).await {
                StepOutcome::Committed(msg) => break msg,
                StepOutcome::Cancelled { partial } => {
                    log.append(&SessionEvent::AssistantAttempt(AssistantAttempt {
                        model: provider.model().to_string(),
                        outcome: AttemptOutcome::Cancelled,
                        chunks: partial,
                    }))?;
                    return Ok(TurnOutcome::Cancelled);
                }
                StepOutcome::Failed { error, partial } => {
                    let retryable = error.retryable && error.code != "CONTEXT_OVERFLOW";
                    let retry_delay = (retryable && attempts <= config.max_retries).then(|| error.retry_after.unwrap_or_else(|| std::time::Duration::from_millis(500 * (1u64 << attempts.saturating_sub(1).min(6)))));
                    log.append(&SessionEvent::AssistantAttempt(AssistantAttempt {
                        model: provider.model().to_string(),
                        outcome: AttemptOutcome::Error {
                            message: error.message.clone(),
                            retryable,
                            code: Some(error.code.into()),
                            retry_in_ms: retry_delay.map(|d| d.as_millis().min(u64::MAX as u128) as u64),
                        },
                        chunks: partial,
                    }))?;
                    frames(Frame::HistoryChanged { session: session.clone() });
                    if error.code == "CONTEXT_OVERFLOW" {
                        if let Some(policy) = policy.filter(|p| overflow_retries < p.max_overflow_retries) {
                            let on_compaction = |progress: compaction::CompactionProgress| {
                                frames(Frame::CompactionStarted { session: session.clone(), events: progress.events, estimated_tokens: progress.estimated_tokens });
                            };
                            let compacted = compaction::reduce_with_progress(store, log, provider, &step_system, &tool_specs, policy, true, cancel, &on_compaction).await;
                            frames(Frame::CompactionFinished { session: session.clone(), changed: matches!(compacted, Ok(true)) });
                            let changed = compacted?;
                            if cancel.is_cancelled() { return Ok(TurnOutcome::Cancelled); }
                            if changed {
                                overflow_retries += 1;
                                replayed = replay(store, log.session())?;
                                frames(Frame::HistoryChanged { session: session.clone() });
                                continue;
                            }
                        }
                    }
                    if !retryable || attempts > config.max_retries {
                        return Err(TurnError::ModelExhausted {
                            attempts,
                            last: error.message,
                        });
                    }
                    if let Some(delay) = retry_delay {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Ok(TurnOutcome::Cancelled),
                            _ = tokio::time::sleep(delay) => {}
                        }
                    }
                }
            }
        };

        let stop = message.stop;
        let calls: Vec<ToolCall> = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolUse { call, name, args } => Some(ToolCall {
                    call: call.clone(),
                    name: name.clone(),
                    args: args.clone(),
                }),
                _ => None,
            })
            .collect();

        log.append(&SessionEvent::AssistantMessage(message)).map(|env| {
            frames(Frame::StepCommitted { session: session.clone(), event: env.id });
        })?;

        match stop {
            StopReason::EndTurn | StopReason::MaxTokens => return Ok(TurnOutcome::Completed),
            StopReason::ToolUse => {
                for call in &calls {
                    frames(Frame::ToolStarted {
                        session: session.clone(),
                        call: call.call.clone(),
                        name: call.name.clone(),
                    });
                }
                let exposed: Vec<_> = tool_specs.iter().map(|spec| spec.name.clone()).collect();
                let results = if calls.iter().all(|call| call.name != "ToolSearch" && call.name != "run_code") {
                    tools.restricted(&exposed).dispatch(&session, &calls, config.max_tool_concurrency, cancel).await
                } else {
                    let mut results = Vec::new();
                    for call in &calls {
                        if !exposed.contains(&call.name) {
                            results.push(crate::tools::exposure::result(call, Err("tool is not exposed; discover it with ToolSearch first".into())));
                        } else if call.name == "ToolSearch" {
                            let output = config.tool_exposure.search(tools, &call.args);
                            match output {
                                Ok((output, names)) => {
                                    log.append(&SessionEvent::ToolsActivated { names: names.clone() })?;
                                    activated.extend(names);
                                    results.push(crate::tools::exposure::result(call, Ok(output)));
                                }
                                Err(error) => results.push(crate::tools::exposure::result(call, Err(error))),
                            }
                        } else if call.name == "run_code" {
                            let registry = std::sync::Arc::new(tools.restricted(&tools.names()));
                            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                            let running = crate::tools::exposure::program(registry, session.clone(), call.clone(), cancel.clone(), Some(tx));
                            tokio::pin!(running);
                            let (result, _) = loop {
                                tokio::select! {
                                    finished = &mut running => break finished,
                                    Some((nested, result, ack)) = rx.recv() => {
                                        let event = match result {
                                            Some(result) => SessionEvent::ProgramToolResult { parent:call.call.clone(), args:nested.args, result },
                                            None => SessionEvent::ProgramToolStarted { parent:call.call.clone(), call:nested.call, name:nested.name, args:nested.args },
                                        };
                                        let appended = log.append(&event);
                                        let _ = ack.send(appended.is_ok());
                                        appended?;
                                    }
                                }
                            };
                            results.push(result);
                        } else {
                            results.extend(tools.restricted(&exposed).dispatch(&session, std::slice::from_ref(call), 1, cancel).await);
                        }
                    }
                    results
                };
                let dismissed = results.iter().any(|result| result.plan_review == Some(rness_protocol::events::PlanReview::Dismissed));
                for result in results {
                    frames(Frame::ToolOutput {
                        session: session.clone(),
                        call: result.call.clone(),
                        output: result.output.clone(),
                    });
                    tools.file_references.invalidate();
                    log.append(&SessionEvent::ToolResult(result))?;
                    frames(Frame::HistoryChanged { session: session.clone() });
                }
                if dismissed { return Ok(TurnOutcome::Completed); }
                // Cancellation between steps: commit and stop cleanly.
                if cancel.is_cancelled() {
                    return Ok(TurnOutcome::Cancelled);
                }
            }
        }
    }
    // Step budget exhausted: not an error — the work so far is committed.
    Ok(TurnOutcome::Completed)
}

/// Convenience: the errors a provider reports.
pub use provider::ProviderError as TurnProviderError;
