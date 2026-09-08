//! Subagent seam: delegation of work to child agents (dsh subagent/).
//!
//! Architecture mirrors dsh's four-layer split, collapsed to one module:
//! - [`SubagentProvider`]: how a child comes to exist (spawn = fresh,
//!   fork = seeded with the parent's completed-turn prefix). Providers
//!   declare capabilities; the runtime validates requests against them
//!   BEFORE delegating — an unsupported option is a loud error, never
//!   silently ignored.
//! - [`SubagentRuntime`]: the registry + entry point (`start`). Enforces
//!   max delegation depth from the durable lineage stamps.
//! - The driver: a child is a NORMAL session in the same
//!   [`SessionService`] — same log, same turn loop, same tools, visible
//!   to session listing/branch queries like any other. `start` sends the
//!   prompt, awaits idle, and settles the run from the child's log.
//!
//! The child's result is the text of its last non-empty assistant
//! message — same convention as dsh's `assistant-output`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rness_kernel::Disposer;
use rness_protocol::branch::{Delegation, DelegationMode};
use rness_protocol::events::{ContentPart, SessionEvent, SessionId, TurnOutcome, UserIntent};

use crate::inbox::Disposition;
use crate::service::{ServiceError, SessionIdleEv, SessionService};
use crate::session::branch::BranchError;

/// What a provider can do. Requests asking for anything a provider does
/// not declare are rejected at `start` — fail loud (dsh invariant).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Child sees the parent's conversation history (fork-style).
    pub inherits_parent_context: bool,
    /// Provider can host continuable children: durable sessions that
    /// accept later messages and interrupts (dsh two-shapes model).
    pub continuable: bool,
}

/// A delegation request from a parent session.
#[derive(Debug, Clone)]
pub struct SubagentRequest {
    pub agent: Option<String>,
    pub parent: SessionId,
    pub prompt: String,
}

/// How the child's run ended, mapped from the session's turn-end
/// vocabulary (dsh stop reasons, minus what we don't produce yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    Aborted,
    Error,
}

/// A settled subagent run.
#[derive(Debug, Clone)]
pub struct SubagentRun {
    pub session: SessionId,
    pub stop: StopReason,
    /// Last non-empty assistant text after the activation boundary.
    pub output: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    #[error("unknown subagent provider '{0}'")]
    UnknownProvider(String),
    #[error("delegation depth {depth} exceeds max {max}")]
    DepthExceeded { depth: u32, max: u32 },
    #[error("provider '{provider}' does not support {capability}")]
    Unsupported { provider: String, capability: &'static str },
    #[error("not authorized: {0}")]
    NotAuthorized(String),
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Branch(#[from] BranchError),
}

/// How a child session comes to exist. The ONLY provider-specific part:
/// everything after creation (drive, await, settle) is shared.
pub trait SubagentProvider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    /// Create the child session (with delegation lineage stamped).
    fn create_child(
        &self,
        sessions: &SessionService,
        request: &SubagentRequest,
        delegation: Delegation,
    ) -> Result<SessionId, SubagentError>;
}

/// Spawn: fresh child, zero parent context.
pub struct SpawnProvider;

impl SubagentProvider for SpawnProvider {
    fn name(&self) -> &str {
        "spawn"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { inherits_parent_context: false, continuable: true }
    }
    fn create_child(
        &self,
        sessions: &SessionService,
        request: &SubagentRequest,
        delegation: Delegation,
    ) -> Result<SessionId, SubagentError> {
        let child = sessions.create_delegated(None, delegation)?;
        sessions.set_config(&child, sessions.config(&request.parent)?)?;
        Ok(child)
    }
}

/// Fork: child seeded with the parent's COMPLETED-turn prefix. The
/// in-flight turn (the one whose tool call is delegating right now) is
/// unbalanced — it cannot replay — so the seed cuts at the last
/// `TurnEnded` (dsh's `completedTurnPrefix`).
pub struct ForkProvider;

impl SubagentProvider for ForkProvider {
    fn name(&self) -> &str {
        "fork"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { inherits_parent_context: true, continuable: true }
    }
    fn create_child(
        &self,
        sessions: &SessionService,
        request: &SubagentRequest,
        delegation: Delegation,
    ) -> Result<SessionId, SubagentError> {
        let history = sessions.store().history(&request.parent)?;
        let last_turn_end = history
            .iter()
            .rev()
            .find(|e| matches!(e.event, SessionEvent::TurnEnded { .. }))
            .map(|e| e.id.clone());
        let child = match last_turn_end {
            Some(at) => sessions.fork_delegated(&request.parent, Some(at), delegation),
            // Parent has no completed turns — start with fresh history.
            None => sessions.create_delegated(None, delegation),
        }?;
        sessions.set_config(&child, sessions.config(&request.parent)?)?;
        Ok(child)
    }
}

/// The seam: provider registry + the one entry point ([`Self::start`]).
pub struct SubagentRuntime {
    sessions: Arc<SessionService>,
    providers: RwLock<HashMap<String, Arc<dyn SubagentProvider>>>,
    /// Hard ceiling on delegation depth (root = 0). Runaway recursive
    /// delegation dies here, not in a stack.
    max_depth: u32,
    /// Live settle-watch subscriptions for continuable children;
    /// dropped with the runtime.
    watchers: std::sync::Mutex<Vec<Disposer>>,
}

impl SubagentRuntime {
    pub fn new(sessions: Arc<SessionService>, max_depth: u32) -> Self {
        let rt = Self {
            sessions: Arc::clone(&sessions),
            providers: RwLock::new(HashMap::new()),
            max_depth,
            watchers: std::sync::Mutex::new(Vec::new()),
        };
        // Settle watch (dsh: "when a resident Activation settles, the
        // manager tells the child's direct parent in the parent's own
        // turn stream"): every idle of a CONTINUABLE child injects a
        // settle notice into its parent. One subscription for the
        // runtime's life; one-shot children are untouched.
        let watch_sessions = Arc::clone(&sessions);
        let disposer = sessions.bus().on::<SessionIdleEv>(move |child: &SessionId| {
            let Ok(Some(d)) = watch_sessions.store().delegation(child) else {
                return;
            };
            if d.mode != DelegationMode::Continuable {
                return;
            }
            let run = match settle_last_turn(&watch_sessions, child) {
                Ok(run) => run,
                Err(e) => {
                    tracing::error!(child = %child, error = %e, "settle read failed");
                    return;
                }
            };
            let stop = match run.stop {
                StopReason::Completed => "completed",
                StopReason::Aborted => "aborted",
                StopReason::Error => "error",
            };
            let text = format!(
                "[subagent {child} settled: {stop}]\n{}",
                if run.output.is_empty() { "(no output)" } else { &run.output }
            );
            if let Err(e) = watch_sessions.send(
                &d.parent,
                UserIntent::Inject,
                vec![ContentPart::Text { text }],
            ) {
                tracing::error!(parent = %d.parent, error = %e, "settle notice failed");
            }
        });
        rt.watchers.lock().expect("watchers lock").push(disposer);
        rt
    }

    pub fn register(&self, provider: Arc<dyn SubagentProvider>) {
        self.providers
            .write()
            .expect("providers lock")
            .insert(provider.name().to_string(), provider);
    }

    pub fn roster(&self) -> std::collections::BTreeMap<String, String> {
        self.sessions.agents().iter().filter(|(_, agent)| agent.subagent)
            .map(|(name, agent)| (name.clone(), agent.description.clone())).collect()
    }

    pub fn provider_names(&self) -> Vec<String> {
        let mut v: Vec<_> =
            self.providers.read().expect("providers lock").keys().cloned().collect();
        v.sort();
        v
    }

    pub fn capabilities(&self, provider: &str) -> Option<Capabilities> {
        self.providers.read().expect("providers lock").get(provider).map(|p| p.capabilities())
    }

    /// Delegate: create the child, send the prompt, await idle, settle.
    /// The child is a normal session — it shows up in `list()`, lineage
    /// queries, and the TUI pickers while it runs.
    pub async fn start(
        &self,
        provider: &str,
        request: SubagentRequest,
    ) -> Result<SubagentRun, SubagentError> {
        let provider = self
            .providers
            .read()
            .expect("providers lock")
            .get(provider)
            .cloned()
            .ok_or_else(|| SubagentError::UnknownProvider(provider.into()))?;

        // Depth: parent's stamped depth + 1, enforced BEFORE creation.
        let parent_depth = self
            .sessions
            .store()
            .delegation(&request.parent)?
            .map(|d| d.depth)
            .unwrap_or(0);
        let depth = parent_depth + 1;
        if depth > self.max_depth {
            return Err(SubagentError::DepthExceeded { depth, max: self.max_depth });
        }
        let delegation =
            Delegation { parent: request.parent.clone(), depth, mode: DelegationMode::OneShot };

        let config = self.sessions.delegated_config(&request.parent, request.agent.as_deref())?;
        let child = provider.create_child(&self.sessions, &request, delegation)?;
        self.sessions.set_config(&child, config)?;

        // Activation boundary: events present BEFORE our prompt (the
        // fork seed). The settled output must come from after it, so a
        // fork never re-reads inherited assistant text as its "result".
        let boundary = self.sessions.store().history(&child)?.len();

        self.sessions.send(
            &child,
            UserIntent::Followup,
            vec![ContentPart::Text { text: request.prompt.clone() }],
        )?;
        self.sessions.join(&child).await;

        Ok(settle(&self.sessions, &child, boundary)?)
    }

    // -- continuable children (dsh two-shapes model) ------------------------

    /// Start a CONTINUABLE child: create it, submit the prompt, and
    /// return its id immediately — no settle await. The child keeps a
    /// durable session that accepts later [`Self::send_message`] calls
    /// and [`Self::interrupt`]; every settle is injected into the
    /// parent's stream by the runtime's settle watch.
    pub fn start_continuable(
        &self,
        provider: &str,
        request: SubagentRequest,
    ) -> Result<SessionId, SubagentError> {
        let provider = self
            .providers
            .read()
            .expect("providers lock")
            .get(provider)
            .cloned()
            .ok_or_else(|| SubagentError::UnknownProvider(provider.into()))?;
        if !provider.capabilities().continuable {
            return Err(SubagentError::Unsupported {
                provider: provider.name().into(),
                capability: "continuable children",
            });
        }

        let parent_depth = self
            .sessions
            .store()
            .delegation(&request.parent)?
            .map(|d| d.depth)
            .unwrap_or(0);
        let depth = parent_depth + 1;
        if depth > self.max_depth {
            return Err(SubagentError::DepthExceeded { depth, max: self.max_depth });
        }
        let delegation = Delegation {
            parent: request.parent.clone(),
            depth,
            mode: DelegationMode::Continuable,
        };

        let config = self.sessions.delegated_config(&request.parent, request.agent.as_deref())?;
        let child = provider.create_child(&self.sessions, &request, delegation)?;
        self.sessions.set_config(&child, config)?;
        self.sessions.send(
            &child,
            UserIntent::Followup,
            vec![ContentPart::Text { text: request.prompt.clone() }],
        )?;
        Ok(child)
    }

    /// Send a message across ONE parent/child edge (dsh: agent-message
    /// authority is exact adjacency). `sender` must be the target's
    /// stamped direct parent, or the target must be the sender's stamped
    /// direct parent (a continuable child steering back up). A working
    /// target sees it at the next step boundary (Steer); an idle target
    /// starts a turn. Returns acceptance, never a reply.
    pub fn send_message(
        &self,
        sender: &SessionId,
        target: &SessionId,
        text: String,
    ) -> Result<Disposition, SubagentError> {
        let target_delegation = self.sessions.store().delegation(target)?;
        let authorized = match &target_delegation {
            // Down edge: target is sender's direct continuable child.
            Some(d) if &d.parent == sender => {
                if d.mode != DelegationMode::Continuable {
                    return Err(SubagentError::NotAuthorized(format!(
                        "{target} is a one-shot child — it cannot accept messages"
                    )));
                }
                true
            }
            // Up edge: sender is a continuable child of the target.
            _ => matches!(
                self.sessions.store().delegation(sender)?,
                Some(d) if &d.parent == target && d.mode == DelegationMode::Continuable
            ),
        };
        if !authorized {
            return Err(SubagentError::NotAuthorized(format!(
                "{sender} and {target} are not direct delegation neighbors"
            )));
        }
        let framed = format!("Agent {sender} sent a message:\n{text}");
        Ok(self.sessions.send(
            target,
            UserIntent::Steer,
            vec![ContentPart::Text { text: framed }],
        )?)
    }

    /// Stop only the target's current turn (dsh interrupt_agent): queued
    /// followups stay parked, descendants keep running, the child stays
    /// available. Caller must be a delegation ancestor of the target.
    /// Interrupting an idle child is an accepted no-op.
    pub fn interrupt(
        &self,
        caller: &SessionId,
        target: &SessionId,
    ) -> Result<(), SubagentError> {
        // Walk the durable delegation chain from target up to caller.
        let mut cursor = target.clone();
        let mut hops = 0u32;
        let authorized = loop {
            match self.sessions.store().delegation(&cursor)? {
                Some(d) if &d.parent == caller => break true,
                Some(d) => {
                    cursor = d.parent;
                    hops += 1;
                    if hops > self.max_depth {
                        break false;
                    }
                }
                None => break false,
            }
        };
        if !authorized {
            return Err(SubagentError::NotAuthorized(format!(
                "{caller} is not a delegation ancestor of {target}"
            )));
        }
        self.sessions.cancel(target);
        Ok(())
    }

    /// The continuable children below `root`: direct children, or the
    /// whole descendant tree in stable pre-order. One-shot children are
    /// intentionally absent — they cannot accept `send_message` (dsh).
    pub fn list_children(
        &self,
        root: &SessionId,
        descendants: bool,
    ) -> Result<Vec<ChildAgent>, SubagentError> {
        // Delegation stamps only name the parent, so build the child
        // index by scanning the store once.
        let mut by_parent: HashMap<SessionId, Vec<(SessionId, Delegation)>> = HashMap::new();
        for id in self.sessions.store().list()? {
            if let Some(d) = self.sessions.store().delegation(&id)? {
                if d.mode == DelegationMode::Continuable {
                    by_parent.entry(d.parent.clone()).or_default().push((id, d));
                }
            }
        }
        for children in by_parent.values_mut() {
            children.sort_by(|a, b| a.0.cmp(&b.0));
        }

        let mut out = Vec::new();
        let mut stack: Vec<SessionId> = vec![root.clone()];
        while let Some(node) = stack.pop() {
            let Some(children) = by_parent.get(&node) else { continue };
            for (id, d) in children {
                out.push(ChildAgent {
                    session: id.clone(),
                    parent: d.parent.clone(),
                    depth: d.depth,
                    running: self.sessions.phase(id) == crate::inbox::Phase::Running,
                });
            }
            if descendants {
                // Pre-order: descend into each child after listing it.
                for (id, _) in children.iter().rev() {
                    stack.push(id.clone());
                }
            }
        }
        Ok(out)
    }
}

/// One continuable child in a discovery listing ([`SubagentRuntime::list_children`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildAgent {
    pub session: SessionId,
    /// Durable direct-parent session id.
    pub parent: SessionId,
    pub depth: u32,
    /// Live phase: true while a turn burst runs.
    pub running: bool,
}

/// Settle from the child's LAST turn only: boundary at the final
/// `TurnStarted`, so a settle notice never re-reads earlier output.
fn settle_last_turn(
    sessions: &SessionService,
    child: &SessionId,
) -> Result<SubagentRun, ServiceError> {
    let history = sessions.store().history(child)?;
    let boundary = history
        .iter()
        .rposition(|e| matches!(e.event, SessionEvent::TurnStarted { .. }))
        .unwrap_or(0);
    settle_events(&history, boundary, child)
}

/// Read the child's log after the boundary and settle the run: stop
/// reason from the last turn end, output from the last non-empty
/// assistant message.
fn settle(
    sessions: &SessionService,
    child: &SessionId,
    boundary: usize,
) -> Result<SubagentRun, ServiceError> {
    let history = sessions.store().history(child)?;
    settle_events(&history, boundary, child)
}

fn settle_events(
    history: &[rness_protocol::events::Envelope],
    boundary: usize,
    child: &SessionId,
) -> Result<SubagentRun, ServiceError> {
    let after = &history[boundary.min(history.len())..];

    let stop = after
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::TurnEnded { outcome, .. } => Some(match outcome {
                TurnOutcome::Completed => StopReason::Completed,
                TurnOutcome::Cancelled => StopReason::Aborted,
                TurnOutcome::Failed => StopReason::Error,
            }),
            _ => None,
        })
        .unwrap_or(StopReason::Error);

    let output = after
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::AssistantMessage(m) => {
                let text: String = m
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        })
        .unwrap_or_default();

    Ok(SubagentRun { session: child.clone(), stop, output })
}
