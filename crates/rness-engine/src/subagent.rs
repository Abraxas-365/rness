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

mod activity;
pub use activity::{SubagentActivity, ToolStreamEv};

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

/// Observer for the child id right after creation.
pub type OnStart = Box<dyn FnOnce(&SessionId) + Send>;

/// Optional controls for [`SubagentRuntime::start_with`].
#[derive(Default)]
pub struct RunOptions {
    /// `(parent tool call, card args)` linking the child to a tool card.
    pub presentation: Option<(String, serde_json::Value)>,
    /// Structured-output schema (object-rooted subset).
    pub output_schema: Option<serde_json::Value>,
    /// Cancelling this token cancels the child's turn.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// Called with the child id right after creation.
    pub on_start: Option<OnStart>,
    /// Parent tool call recorded on the delegation link when there is no
    /// `presentation` (the child is attributed to that call in `/agents`,
    /// but gets no subagent card of its own).
    pub call: Option<String>,
    /// Tools removed from the child's ceiling (inherited by its own
    /// delegations). Workflow members use it to forbid nested workflows.
    pub withhold_tools: Vec<String>,
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
    /// Validated `structured_output` value, when a schema was requested
    /// and the child captured one. Never durable.
    pub structured: Option<serde_json::Value>,
    /// Why the run failed: the last provider error recorded after the
    /// activation boundary (`code: message`, at most 500 chars).
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    #[error("unknown subagent provider '{0}'")]
    UnknownProvider(String),
    #[error("delegation depth {depth} exceeds max {max}")]
    DepthExceeded { depth: u32, max: u32 },
    #[error("provider '{provider}' does not support {capability}")]
    Unsupported {
        provider: String,
        capability: &'static str,
    },
    #[error("not authorized: {0}")]
    NotAuthorized(String),
    #[error("unsupported output schema: {0}")]
    InvalidSchema(String),
    /// The run's cancel token fired before the child existed.
    #[error("cancelled before the child started")]
    Cancelled,
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
        Capabilities {
            inherits_parent_context: false,
            continuable: true,
        }
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
        Capabilities {
            inherits_parent_context: true,
            continuable: true,
        }
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
    allow_generic: bool,
    /// Per-root user aliases, retained as tombstones when sessions disappear.
    user_aliases: std::sync::Mutex<HashMap<SessionId, Vec<SessionId>>>,
    pub activity: SubagentActivity,
    /// Live settle-watch subscriptions for continuable children;
    /// dropped with the runtime.
    watchers: std::sync::Mutex<Vec<Disposer>>,
    /// Roots whose tree [`Self::spawn_reconcile_tree`] already visited.
    reconciled: std::sync::Mutex<std::collections::HashSet<SessionId>>,
}

impl SubagentRuntime {
    pub fn new(sessions: Arc<SessionService>, max_depth: u32) -> Self {
        let rt = Self {
            sessions: Arc::clone(&sessions),
            providers: RwLock::new(HashMap::new()),
            max_depth,
            allow_generic: false,
            user_aliases: Default::default(),
            activity: SubagentActivity::default(),
            watchers: std::sync::Mutex::new(Vec::new()),
            reconciled: Default::default(),
        };
        // Settle watch (dsh: "when a resident Activation settles, the
        // manager tells the child's direct parent in the parent's own
        // turn stream"): every idle of a CONTINUABLE child delivers its
        // final output to the parent, steering a busy turn or waking an idle
        // parent. Teardown only logs the notice. One-shot jobs are untouched.
        let watch_sessions = Arc::clone(&sessions);
        let disposer = sessions
            .bus()
            .on::<SessionIdleEv>(move |child: &SessionId| {
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

                watch_sessions.bus().emit::<crate::service::SubagentStopEv>(
                    &crate::service::SubagentStopNotice {
                        parent: d.parent.clone(),
                        child: child.clone(),
                        outcome: stop.to_string(),
                        mode: DelegationMode::Continuable,
                    },
                );

                let text = format!(
                    "[subagent {child} settled: {stop}]\n{}",
                    match (&run.error, run.output.is_empty()) {
                        (Some(error), true) => format!("error: {error}"),
                        (_, true) => "(no output)".into(),
                        (_, false) => run.output.clone(),
                    }
                );
                // Never block the child's idle event on a parent reservation (the
                // parent may itself be waiting for this child to finish).
                let sessions = Arc::clone(&watch_sessions);
                tokio::spawn(async move {
                    if let Err(e) = sessions.notify_subagent_settled(&d.parent, text).await {
                        tracing::error!(parent = %d.parent, error = %e, "settle notice failed");
                    }
                });
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

    /// Startup policy shared by all callers and descendants.
    pub fn with_allow_generic(mut self, allow: bool) -> Self {
        self.allow_generic = allow;
        self
    }

    pub fn allows_generic(&self) -> bool {
        self.allow_generic
    }

    /// Validate before creating a child (or accepting a background job).
    pub fn validate_agent(&self, agent: Option<&str>) -> Result<(), SubagentError> {
        match agent {
            None if !self.allow_generic => Err(SubagentError::NotAuthorized(
                "generic subagents are disabled (rness.agents.allow_generic = false); select a configured agent enabled for delegation. If no role fits, do not delegate".into(),
            )),
            Some(name) if !self.sessions.agents().get(name).is_some_and(|agent| agent.subagent) => {
                Err(SubagentError::NotAuthorized(format!("agent is not enabled for delegation: {name}")))
            }
            _ => Ok(()),
        }
    }

    pub fn roster(&self) -> std::collections::BTreeMap<String, String> {
        self.sessions
            .agents()
            .iter()
            .filter(|(_, agent)| agent.subagent)
            .map(|(name, agent)| (name.clone(), agent.description.clone()))
            .collect()
    }

    pub fn provider_names(&self) -> Vec<String> {
        let mut v: Vec<_> = self
            .providers
            .read()
            .expect("providers lock")
            .keys()
            .cloned()
            .collect();
        v.sort();
        v
    }

    pub fn capabilities(&self, provider: &str) -> Option<Capabilities> {
        self.providers
            .read()
            .expect("providers lock")
            .get(provider)
            .map(|p| p.capabilities())
    }

    /// Delegate: create the child, send the prompt, await idle, settle.
    /// The child is a normal session — it shows up in `list()`, lineage
    /// queries, and the TUI pickers while it runs.
    pub async fn start(
        &self,
        provider: &str,
        request: SubagentRequest,
    ) -> Result<SubagentRun, SubagentError> {
        self.start_presented(provider, request, None).await
    }

    pub async fn start_presented(
        &self,
        provider: &str,
        request: SubagentRequest,
        presentation: Option<(String, serde_json::Value)>,
    ) -> Result<SubagentRun, SubagentError> {
        self.start_with(
            provider,
            request,
            RunOptions {
                presentation,
                ..RunOptions::default()
            },
        )
        .await
    }

    /// [`Self::start_presented`] with the full option set. An
    /// `output_schema` (dsh `outputSchema`) is checked against the enforced
    /// subset BEFORE any child exists; the child gets an in-memory
    /// `structured_output` capture tool for exactly this run, and a child
    /// that completes without a valid capture settles as
    /// [`StopReason::Error`]. `cancel` cancels the child's turn (it settles
    /// as [`StopReason::Aborted`]); `on_start` observes the child id once
    /// the session exists, before its prompt is sent.
    pub async fn start_with(
        &self,
        provider: &str,
        request: SubagentRequest,
        options: RunOptions,
    ) -> Result<SubagentRun, SubagentError> {
        let RunOptions {
            presentation,
            output_schema,
            cancel,
            on_start,
            call,
            withhold_tools,
        } = options;
        if let Some(schema) = &output_schema {
            crate::structured::check_object_schema(schema)
                .map_err(|violations| SubagentError::InvalidSchema(violations.join("; ")))?;
        }
        self.validate_agent(request.agent.as_deref())?;
        let provider = self
            .providers
            .read()
            .expect("providers lock")
            .get(provider)
            .cloned()
            .ok_or_else(|| SubagentError::UnknownProvider(provider.into()))?;
        // Killed before launch: never create a child that nobody can stop.
        if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
            return Err(SubagentError::Cancelled);
        }

        // Depth: parent's stamped depth + 1, enforced BEFORE creation.
        let parent_depth = self
            .sessions
            .store()
            .delegation(&request.parent)?
            .map(|d| d.depth)
            .unwrap_or(0);
        let depth = parent_depth + 1;
        if depth > self.max_depth {
            return Err(SubagentError::DepthExceeded {
                depth,
                max: self.max_depth,
            });
        }
        let delegation = Delegation {
            parent: request.parent.clone(),
            call: presentation.as_ref().map(|(call, _)| call.clone()).or(call),
            depth,
            mode: DelegationMode::OneShot,
        };

        let mut config = self
            .sessions
            .delegated_config(&request.parent, request.agent.as_deref())?;
        if let Some(ceiling) = config.tool_ceiling.as_mut() {
            ceiling.retain(|name| !withhold_tools.contains(name));
        }
        let child = provider.create_child(&self.sessions, &request, delegation)?;
        self.sessions.set_config(&child, config)?;
        // Detaches on every exit path (settle, error, cancellation).
        let attached =
            output_schema.map(|schema| self.sessions.structured_outputs().attach(&child, schema));
        if let Some(on_start) = on_start {
            on_start(&child);
        }

        self.sessions.bus().emit::<crate::service::SubagentStartEv>(
            &crate::service::SubagentStartNotice {
                parent: request.parent.clone(),
                child: child.clone(),
                agent: request.agent.clone(),
                mode: DelegationMode::OneShot,
                depth,
            },
        );

        // Activation boundary: events present BEFORE our prompt (the
        // fork seed). The settled output must come from after it, so a
        // fork never re-reads inherited assistant text as its "result".
        let boundary = self.sessions.store().history(&child)?.len();
        if let Some((call, args)) = presentation {
            self.activity
                .register(&self.sessions, &request.parent, &call, &child, args);
        }

        self.sessions
            .send(
                &child,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: request.prompt.clone(),
                }],
            )
            .map_err(|error| {
                self.activity.failed(&child);
                error
            })?;
        // A watcher (not a select over `join`): `join` takes the burst
        // handle, so abandoning it midway would lose the settle wait.
        // `send` installed the burst's token synchronously, so a token that
        // fired before this point still cancels the turn immediately.
        let watcher = cancel.map(|token| {
            let (sessions, child) = (Arc::clone(&self.sessions), child.clone());
            tokio::spawn(async move {
                token.cancelled().await;
                sessions.cancel(&child);
            })
        });
        self.sessions.join(&child).await;
        if let Some(watcher) = watcher {
            watcher.abort();
        }

        let mut run = settle(&self.sessions, &child, boundary)?;
        if let Some(attached) = attached {
            run.structured = attached.attachment().captured();
            // dsh readResult: asked for structure, completed without it.
            if run.structured.is_none() && run.stop == StopReason::Completed {
                run.stop = StopReason::Error;
            }
        }

        let outcome = match run.stop {
            StopReason::Completed => "completed",
            StopReason::Aborted => "aborted",
            StopReason::Error => "error",
        };
        self.sessions.bus().emit::<crate::service::SubagentStopEv>(
            &crate::service::SubagentStopNotice {
                parent: request.parent.clone(),
                child: child.clone(),
                outcome: outcome.to_string(),
                mode: DelegationMode::OneShot,
            },
        );

        Ok(run)
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
        self.start_continuable_presented(provider, request, None)
    }

    pub fn start_continuable_presented(
        &self,
        provider: &str,
        request: SubagentRequest,
        presentation: Option<(String, serde_json::Value)>,
    ) -> Result<SessionId, SubagentError> {
        self.validate_agent(request.agent.as_deref())?;
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
            return Err(SubagentError::DepthExceeded {
                depth,
                max: self.max_depth,
            });
        }
        let delegation = Delegation {
            parent: request.parent.clone(),
            call: presentation.as_ref().map(|(call, _)| call.clone()),
            depth,
            mode: DelegationMode::Continuable,
        };

        let config = self
            .sessions
            .delegated_config(&request.parent, request.agent.as_deref())?;
        let child = provider.create_child(&self.sessions, &request, delegation)?;
        self.sessions.set_config(&child, config)?;

        self.sessions.bus().emit::<crate::service::SubagentStartEv>(
            &crate::service::SubagentStartNotice {
                parent: request.parent.clone(),
                child: child.clone(),
                agent: request.agent.clone(),
                mode: DelegationMode::Continuable,
                depth,
            },
        );

        if let Some((call, args)) = presentation {
            self.activity
                .register(&self.sessions, &request.parent, &call, &child, args);
        }
        self.sessions
            .send(
                &child,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: request.prompt.clone(),
                }],
            )
            .map_err(|error| {
                self.activity.failed(&child);
                error
            })?;
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

    /// User intervention, deliberately separate from model-facing send_message.
    /// Trusted frontends supply the invoking conversation, not an agent sender.
    /// Existing user-message content durably records the user origin and scope.
    pub fn steer_user(
        &self,
        caller: &SessionId,
        target: &SessionId,
        text: String,
    ) -> Result<Disposition, SubagentError> {
        if text.trim().is_empty() {
            return Err(
                ServiceError::InvalidConfig("user steering requires a message".into()).into(),
            );
        }
        self.authorize_descendant(caller, target)?;
        if self
            .sessions
            .store()
            .delegation(target)?
            .is_none_or(|d| d.mode != DelegationMode::Continuable)
        {
            return Err(SubagentError::NotAuthorized(
                "user steering requires a continuable subagent; one-shot children cannot receive messages".into(),
            ));
        }
        let disposition = self.sessions.send(
            target,
            UserIntent::Steer,
            vec![ContentPart::Text {
                text: format!("User steering from conversation {caller}:\n{text}"),
            }],
        )?;
        // The invoking command owns caller's operation reservation. Reuse the
        // reservation-aware notice path asynchronously, never wait on the Lua
        // actor and never impersonate the principal agent.
        let sessions = Arc::clone(&self.sessions);
        let caller = caller.clone();
        let notice = format!("User intervened in subagent {target} with steering:\n{text}");
        tokio::spawn(async move {
            if let Err(error) = sessions.notify_user_intervention(&caller, notice).await {
                tracing::error!(session = %caller, %error, "user intervention notice failed");
            }
        });
        Ok(disposition)
    }

    /// Close a killed ONE-SHOT child for good: its run is over, so its tree
    /// is cancelled and nothing (e.g. a grandchild's job notice) wakes it
    /// again. Returns the child's delegated descendants.
    pub fn close_one_shot(&self, child: &SessionId) -> Vec<SessionId> {
        self.sessions.close_tree(child)
    }

    /// Startup/resume reconcile for children whose host died mid-turn
    /// (their own log ends in `turn/started` with no `turn/ended`, and no
    /// live burst will ever emit the idle that drives the settle watch).
    /// Only the delegation tree of `root` is visited: the session this host
    /// opened or resumed. Other roots in the store may belong to another
    /// live rness process, or be long finished, and are never touched.
    /// Shallowest first, each open child turn gets `turn/ended{cancelled}`.
    /// A CONTINUABLE child's direct parent is told first, once
    /// (`[subagent <id> settled: interrupted]`, deduplicated by the durable
    /// source id `settle:<child>:<turn>`), so a crash between the two stays
    /// idempotent. The notice is a log-only Inject: it never starts a turn
    /// (a parent running here sees it at its next step). One-shot children
    /// get no notice here: job recovery reports their background job as
    /// `interrupted`. Sessions that are live here or whose log another
    /// process holds are skipped. Open turns are found from the log tail
    /// only. Returns how many turns were closed.
    pub async fn reconcile_tree(&self, root: &SessionId) -> Result<usize, SubagentError> {
        /// Look this far back from the end of a child's log for its last
        /// turn marker; a child whose open turn started earlier is skipped.
        const TAIL_BYTES: u64 = 16 * 1024 * 1024;
        let sessions = Arc::clone(&self.sessions);
        let root = root.clone();
        let mut open: Vec<(SessionId, Delegation, u32, String)> =
            tokio::task::spawn_blocking(move || -> Result<_, SubagentError> {
                let mut by_parent: HashMap<SessionId, Vec<(SessionId, Delegation)>> =
                    HashMap::new();
                for (child, d) in sessions.store().delegations()? {
                    by_parent
                        .entry(d.parent.clone())
                        .or_default()
                        .push((child, d));
                }
                let mut tree = Vec::new();
                let mut queue = std::collections::VecDeque::from([root]);
                while let Some(node) = queue.pop_front() {
                    for (child, d) in by_parent.remove(&node).unwrap_or_default() {
                        queue.push_back(child.clone());
                        tree.push((child, d));
                    }
                }
                let mut open = Vec::new();
                for (child, d) in tree {
                    if sessions.phase(&child) != crate::inbox::Phase::Idle {
                        continue;
                    }
                    let tail = crate::session::log::read_tail_turn(
                        sessions.store().root(),
                        &child,
                        TAIL_BYTES,
                    );
                    let (turn, events) = match tail {
                        Ok(crate::session::log::TailTurn::Open { turn, events }) => (turn, events),
                        Ok(_) => continue,
                        Err(error) => {
                            tracing::warn!(child = %child, %error, "child log tail unreadable");
                            continue;
                        }
                    };
                    let events: Vec<_> = events.into_iter().map(Arc::new).collect();
                    let run = settle_events(&events, 0, &child)?;
                    open.push((child, d, turn, run.output));
                }
                Ok(open)
            })
            .await
            .map_err(|e| ServiceError::InvalidConfig(format!("reconcile failed: {e}")))??;
        open.sort_by_key(|(child, d, ..)| (d.depth, child.clone()));
        let mut reconciled = 0;
        for (child, d, turn, output) in open {
            // Re-check: a turn may have started since the scan.
            if self.sessions.phase(&child) != crate::inbox::Phase::Idle {
                continue;
            }
            // A writer lock held by another process means the child is live
            // there. Probe it, but never hold it across the parent notice.
            if let Err(error) = self.sessions.store().open(&child) {
                tracing::debug!(child = %child, %error, "interrupted child not reconciled");
                continue;
            }
            let text = format!(
                "[subagent {child} settled: interrupted]\nThe host stopped while this child's \
                 turn was running; the turn was closed as cancelled.\n{}",
                if output.is_empty() {
                    "(no output)"
                } else {
                    &output
                }
            );
            if d.mode == DelegationMode::Continuable {
                if let Err(error) = self
                    .sessions
                    .inject_job_once(&d.parent, &format!("settle:{child}:{turn}"), text)
                    .await
                {
                    tracing::warn!(parent = %d.parent, %error, "interrupted child notice failed");
                    continue;
                }
            }
            if self.sessions.phase(&child) != crate::inbox::Phase::Idle {
                continue;
            }
            let closed = (|| -> Result<bool, ServiceError> {
                let mut log = match self.sessions.store().open(&child) {
                    Ok(log) => log,
                    Err(_) => return Ok(false),
                };
                // Still the same open turn now that we hold the writer?
                match crate::session::log::read_tail_turn(
                    self.sessions.store().root(),
                    &child,
                    TAIL_BYTES,
                )? {
                    crate::session::log::TailTurn::Open { turn: t, .. } if t == turn => {}
                    _ => return Ok(false),
                }
                log.append(&SessionEvent::TurnEnded {
                    turn,
                    outcome: TurnOutcome::Cancelled,
                })?;
                Ok(true)
            })()?;
            if !closed {
                continue;
            }
            self.sessions.bus().emit::<crate::service::SubagentStopEv>(
                &crate::service::SubagentStopNotice {
                    parent: d.parent.clone(),
                    child: child.clone(),
                    outcome: "interrupted".into(),
                    mode: d.mode,
                },
            );
            reconciled += 1;
        }
        Ok(reconciled)
    }

    /// [`Self::reconcile_tree`] for the delegation root of `session` on a
    /// background task, at most once per root per process, so host startup
    /// never waits on it. Idempotent; failures are logged.
    pub fn spawn_reconcile_tree(self: &Arc<Self>, session: SessionId) {
        let rt = Arc::clone(self);
        tokio::spawn(async move {
            let sessions = Arc::clone(&rt.sessions);
            let start = session.clone();
            let root = tokio::task::spawn_blocking(move || {
                let mut node = start;
                for _ in 0..1024 {
                    match sessions.store().delegation(&node) {
                        Ok(Some(d)) => node = d.parent,
                        _ => break,
                    }
                }
                node
            })
            .await;
            let Ok(root) = root else { return };
            let first = {
                let mut seen = rt.reconciled.lock().unwrap();
                let first = seen.insert(root.clone());
                seen.insert(session);
                first
            };
            if !first {
                return;
            }
            match rt.reconcile_tree(&root).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(%root, closed = n, "closed interrupted child turns"),
                Err(error) => tracing::warn!(%root, %error, "interrupted child reconcile failed"),
            }
        });
    }

    /// Reconcile a tree ([`Self::spawn_reconcile_tree`]) the first time this
    /// process starts a turn anywhere in it, i.e. when this host opens or
    /// resumes it. Trees nobody runs here are never touched.
    pub fn reconcile_on_resume(self: &Arc<Self>) {
        let rt = Arc::downgrade(self);
        let disposer = self
            .sessions
            .bus()
            .on::<crate::service::SessionStartEv>(move |n| {
                let Some(rt) = rt.upgrade() else { return };
                if rt.reconciled.lock().unwrap().contains(&n.session) {
                    return;
                }
                if tokio::runtime::Handle::try_current().is_ok() {
                    rt.spawn_reconcile_tree(n.session.clone());
                }
            });
        self.watchers.lock().expect("watchers lock").push(disposer);
    }

    /// Stop only the target's current turn (dsh interrupt_agent): queued
    /// followups stay parked, descendants keep running, the child stays
    /// available. Caller must be a delegation ancestor of the target.
    /// Interrupting an idle child is an accepted no-op.
    pub fn interrupt(&self, caller: &SessionId, target: &SessionId) -> Result<(), SubagentError> {
        self.authorize_descendant(caller, target)?;
        self.sessions.cancel(target);
        Ok(())
    }

    fn authorize_descendant(
        &self,
        caller: &SessionId,
        target: &SessionId,
    ) -> Result<(), SubagentError> {
        // Follow durable ancestry, not caller-supplied depth or UI membership.
        // A visited set rejects malformed cycles without relying on today's
        // max-depth setting (which may differ from the creation-time setting).
        let mut cursor = target.clone();
        let mut seen = std::collections::HashSet::from([target.clone()]);
        while let Some(d) = self.sessions.store().delegation(&cursor)? {
            if !seen.insert(d.parent.clone()) {
                break;
            }
            if &d.parent == caller {
                return Ok(());
            }
            cursor = d.parent;
        }
        Err(SubagentError::NotAuthorized(format!(
            "{caller} is not a delegation ancestor of {target}"
        )))
    }

    /// The continuable children below `root`: direct children, or the
    /// whole descendant tree in stable pre-order. One-shot children are
    /// intentionally absent — they cannot accept `send_message` (dsh).
    pub fn list_children(
        &self,
        root: &SessionId,
        descendants: bool,
    ) -> Result<Vec<ChildAgent>, SubagentError> {
        self.list_delegated(root, descendants, false)
    }

    /// All descendants for user controls with append-only aliases per root.
    /// First observation sorts full IDs; later discoveries append, never reuse
    /// deleted entries. Aliases last for this runtime, full IDs across restarts.
    /// Model-facing list_children intentionally retains its tree ordering.
    pub fn list_agents(&self, root: &SessionId) -> Result<Vec<ChildAgent>, SubagentError> {
        let mut aliases = self.user_aliases.lock().expect("user aliases lock");
        let ids = aliases.entry(root.clone()).or_default();
        let mut agents = self.list_delegated(root, true, true)?;
        agents.sort_by(|a, b| a.session.cmp(&b.session));
        for child in &agents {
            if !ids.contains(&child.session) {
                ids.push(child.session.clone());
            }
        }
        agents.sort_by_key(|child| ids.iter().position(|id| id == &child.session).unwrap());
        for child in &mut agents {
            child.alias = Some(format!(
                "a{}",
                ids.iter().position(|id| id == &child.session).unwrap() + 1
            ));
        }
        Ok(agents)
    }

    fn list_delegated(
        &self,
        root: &SessionId,
        descendants: bool,
        include_one_shot: bool,
    ) -> Result<Vec<ChildAgent>, SubagentError> {
        // One directory scan for the whole tree, then walk it in memory.
        let mut by_parent: std::collections::HashMap<SessionId, Vec<(SessionId, Delegation)>> =
            std::collections::HashMap::new();
        for (id, d) in self.sessions.store().delegations()? {
            by_parent.entry(d.parent.clone()).or_default().push((id, d));
        }
        let mut out = Vec::new();
        let mut stack: Vec<SessionId> = vec![root.clone()];
        while let Some(node) = stack.pop() {
            let children = by_parent.remove(&node).unwrap_or_default();
            let mut node_children: Vec<(SessionId, Delegation)> = children
                .into_iter()
                .filter(|(_, d)| include_one_shot || d.mode == DelegationMode::Continuable)
                .collect();
            node_children.sort_by(|a, b| a.0.cmp(&b.0));
            for (id, d) in &node_children {
                out.push(ChildAgent {
                    alias: None,
                    session: id.clone(),
                    parent: d.parent.clone(),
                    depth: d.depth,
                    running: self.sessions.phase(id) == crate::inbox::Phase::Running,
                });
            }
            if descendants {
                // Pre-order: descend into each child after listing it.
                for (id, _) in node_children.iter().rev() {
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
    /// User-list alias only, stable per root for this runtime's lifetime.
    pub alias: Option<String>,
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
    history: &[std::sync::Arc<rness_protocol::events::Envelope>],
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

    let error = (stop == StopReason::Error)
        .then(|| {
            after.iter().rev().find_map(|e| match &e.event {
                SessionEvent::AssistantAttempt(rness_protocol::events::AssistantAttempt {
                    outcome: rness_protocol::events::AttemptOutcome::Error { message, code, .. },
                    ..
                }) => Some(truncate_chars(
                    &match code {
                        Some(code) => format!("{code}: {message}"),
                        None => message.clone(),
                    },
                    500,
                )),
                _ => None,
            })
        })
        .flatten();

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

    Ok(SubagentRun {
        session: child.clone(),
        stop,
        output,
        structured: None,
        error,
    })
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}
