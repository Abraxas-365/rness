//! [`SessionService`] — the ONLY public API frontends use (docs §3).
//!
//! Owns the store, one [`Inbox`] per live session, and the turn tasks.
//! TUI, CLI, server, and Lua all talk to sessions through this seam;
//! none of them touch logs or the turn loop directly.
//!
//! Concurrency model: all user input funnels through [`SessionService::send`],
//! which classifies via the inbox under a per-session lock. A turn "burst"
//! (initial turn + queued followups) runs as one tokio task that owns the
//! session's writer log for its whole lifetime; the end-of-burst idle
//! transition happens under the inbox lock, so a followup queued at the
//! last instant is either seen by the burst or starts a new one — never
//! lost.
//!
//! Lifecycle notifications are kernel bus events (`session/turn-started`,
//! `session/turn-ended`, `session/idle`) — frames, not log events. The
//! durable record is written by the turn loop itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use rness_kernel::{Context, Event, EventBus, Plugin};
use rness_protocol::events::{
    CallConfig, Compaction, ContentPart, MessageSource, ModelSelection, Prune, SessionEvent,
    SessionId, TurnOutcome, UserIntent, UserMessage,
};
use rness_protocol::frames::Frame;
use tokio_util::sync::CancellationToken;

use crate::inbox::{Disposition, Inbox, Phase};
use crate::session::branch::{BranchError, SessionStore};
use crate::session::log::LogError;
use crate::session::projection::{transcript, Transcript};
use crate::session::replay::{replay, ReplayError, Replayed};
use crate::tools::ToolRegistry;
use crate::turn::provider::Provider;
use crate::turn::{run_turn, TurnConfig};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Branch(#[from] BranchError),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error("session is busy — wait for the active operation to finish and retry")]
    Busy,
    #[error("nothing to compact")]
    NothingToCompact,
    #[error("summarizer failed: {0}")]
    Summarizer(String),
    #[error("no provider resolver configured for selection {route}/{model}")]
    NoProviderResolver { route: String, model: String },
    #[error("provider resolver failed for {route}/{model}: {message}")]
    ProviderResolution {
        route: String,
        model: String,
        message: String,
    },
    #[error("invalid request config: {0}")]
    InvalidConfig(String),
}

// -- bus events (live notifications, invariant #9: never persisted) --------

#[derive(Debug, Clone)]
pub struct TurnNotice {
    pub session: SessionId,
    pub turn: u32,
}

#[derive(Debug, Clone)]
pub struct TurnEndNotice {
    pub session: SessionId,
    pub turn: u32,
    pub outcome: TurnOutcome,
}

pub struct TurnStartedEv;
impl Event for TurnStartedEv {
    const NAME: &'static str = "session/turn-started";
    type Payload = TurnNotice;
}

pub struct TurnEndedEv;
impl Event for TurnEndedEv {
    const NAME: &'static str = "session/turn-ended";
    type Payload = TurnEndNotice;
}

pub struct SessionIdleEv;
impl Event for SessionIdleEv {
    const NAME: &'static str = "session/idle";
    type Payload = SessionId;
}

/// Fired when a brand-new session is created (not resumed).
#[derive(Debug, Clone)]
pub struct SessionCreatedNotice {
    pub session: SessionId,
    pub workspace: Option<String>,
    pub delegation: Option<rness_protocol::branch::Delegation>,
}
pub struct SessionCreatedEv;
impl Event for SessionCreatedEv {
    const NAME: &'static str = "session/created";
    type Payload = SessionCreatedNotice;
}

/// Fired when a session's first turn begins. Delegation info is included so
/// listeners can distinguish root sessions from subagent children.
#[derive(Debug, Clone)]
pub struct SessionStartNotice {
    pub session: SessionId,
    pub workspace: Option<String>,
    pub delegation: Option<rness_protocol::branch::Delegation>,
    /// `"startup"` (first run, turns_so_far==0), `"resume"` (subsequent).
    pub source: String,
}
pub struct SessionStartEv;
impl Event for SessionStartEv {
    const NAME: &'static str = "session/start";
    type Payload = SessionStartNotice;
}

/// Fired when a subagent child is created (before its first turn).
#[derive(Debug, Clone)]
pub struct SubagentStartNotice {
    pub parent: SessionId,
    pub child: SessionId,
    pub agent: Option<String>,
    pub mode: rness_protocol::branch::DelegationMode,
    pub depth: u32,
}
pub struct SubagentStartEv;
impl Event for SubagentStartEv {
    const NAME: &'static str = "subagent/start";
    type Payload = SubagentStartNotice;
}

/// Fired when a continuable subagent settles (idles after a turn).
#[derive(Debug, Clone)]
pub struct SubagentStopNotice {
    pub parent: SessionId,
    pub child: SessionId,
    pub outcome: String,
}
pub struct SubagentStopEv;
impl Event for SubagentStopEv {
    const NAME: &'static str = "subagent/stop";
    type Payload = SubagentStopNotice;
}

/// Emitted by the hook host to durably record hook invocations/results.
/// The burst loop subscribes and appends to the session log.
/// `session` allows filtering when multiple sessions share a bus.
#[derive(Debug, Clone)]
pub struct HookAuditNotice {
    pub session: String,
    pub event: HookAuditEvent,
}

#[derive(Debug, Clone)]
pub enum HookAuditEvent {
    Invoked(rness_protocol::events::HookInvoked),
    Result(rness_protocol::events::HookResult),
}
pub struct HookAuditEv;
impl Event for HookAuditEv {
    const NAME: &'static str = "hook/audit";
    type Payload = HookAuditNotice;
}

/// Live streaming frames (deltas, tool progress, commits) for attached
/// frontends. Ephemeral by contract — reconcile on `StepCommitted`.
pub struct FrameEv;
impl Event for FrameEv {
    const NAME: &'static str = "session/frame";
    type Payload = rness_protocol::frames::Frame;
}

// -- live session state ----------------------------------------------------

struct Live {
    operation: Arc<tokio::sync::Mutex<()>>,
    command: Mutex<Option<CancellationToken>>,
    inbox: Mutex<Inbox>,
    job_pending: Mutex<std::collections::HashSet<String>>,
    /// Token of the current (or last) burst. Replaced on each new burst.
    cancel: Mutex<CancellationToken>,
    /// Handle of the current burst task, for [`SessionService::join`].
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            operation: Arc::new(tokio::sync::Mutex::new(())),
            command: Mutex::new(None),
            inbox: Mutex::new(Inbox::default()),
            job_pending: Default::default(),
            cancel: Mutex::new(CancellationToken::new()),
            handle: Mutex::new(None),
        }
    }
}

/// An admitted command holds session and extension reservations even before execution.
/// Dropping it (including during unwinding) releases both reservations.
struct CommandReservation {
    live: Arc<Live>,
    _operation: tokio::sync::OwnedMutexGuard<()>,
    _activity: tokio::sync::OwnedRwLockReadGuard<()>,
}

#[derive(Clone)]
pub struct CommandPermit(std::sync::Weak<CommandReservation>);

pub struct PreparedCommand {
    live: Arc<Live>,
    reservation: Arc<CommandReservation>,
    command: Arc<dyn crate::interaction::Command>,
    session: SessionId,
    raw_input: String,
    cancel: CancellationToken,
}

impl PreparedCommand {
    pub fn complete(self, service: &SessionService) -> Result<Vec<String>, ServiceError> {
        if self.cancel.is_cancelled() {
            return Err(ServiceError::InvalidConfig("completion cancelled".into()));
        }
        self.command.complete(
            service,
            crate::interaction::CommandInvocation {
                session: &self.session,
                raw_input: &self.raw_input,
                cancel: self.cancel.clone(),
                permit: CommandPermit(Arc::downgrade(&self.reservation)),
            },
        )
    }

    pub fn complete_items(
        self,
        service: &SessionService,
    ) -> Result<Vec<(String, String)>, ServiceError> {
        if self.cancel.is_cancelled() {
            return Err(ServiceError::InvalidConfig("completion cancelled".into()));
        }
        self.command.complete_items(
            service,
            crate::interaction::CommandInvocation {
                session: &self.session,
                raw_input: &self.raw_input,
                cancel: self.cancel.clone(),
                permit: CommandPermit(Arc::downgrade(&self.reservation)),
            },
        )
    }

    pub fn execute(self, service: &SessionService) -> Result<Disposition, ServiceError> {
        if self.cancel.is_cancelled() {
            return Err(ServiceError::InvalidConfig("command cancelled".into()));
        }
        self.command
            .execute(
                service,
                crate::interaction::CommandInvocation {
                    session: &self.session,
                    raw_input: &self.raw_input,
                    cancel: self.cancel.clone(),
                    permit: CommandPermit(Arc::downgrade(&self.reservation)),
                },
            )
            .map(Disposition::Command)
    }
}

impl Drop for PreparedCommand {
    fn drop(&mut self) {
        *self.live.command.lock().unwrap() = None;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Notice {
    None,
    Job,
    Subagent,
}

// -- the service -----------------------------------------------------------

pub type ProviderResolver =
    dyn Fn(&ModelSelection) -> Result<Arc<dyn Provider>, String> + Send + Sync;

pub type InputResolver = dyn Fn(Option<&std::path::Path>, Vec<ContentPart>) -> Result<Vec<ContentPart>, String>
    + Send
    + Sync;

pub struct SessionService {
    images: Arc<std::sync::OnceLock<Arc<crate::images::ImageStore>>>,
    store: Arc<SessionStore>,
    /// Legacy/default provider retained for direct construction in tests and
    /// embeddings that deliberately do not resolve durable selections.
    provider: Arc<dyn Provider>,
    resolver: Option<Arc<ProviderResolver>>,
    creation_seed: CallConfig,
    agents: std::collections::BTreeMap<String, crate::config::AgentDefinition>,
    sandbox: crate::sandbox::SandboxConfig,
    models: crate::config::ModelRegistry,
    tools: Arc<ToolRegistry>,
    config: TurnConfig,
    bus: Arc<EventBus>,
    live: Mutex<HashMap<SessionId, Arc<Live>>>,
    lifecycle: Arc<tokio::sync::RwLock<()>>,
    /// Serializes teardown marking with automatic child-result admission.
    closing: Mutex<std::collections::HashSet<SessionId>>,
    default_workspace: Mutex<Option<String>>,
    commands: crate::interaction::CommandRegistry,
    input_resolver: Mutex<Option<Arc<InputResolver>>>,
    /// Workspace instruction policy (None = feature off). Mechanism in
    /// [`crate::instructions`]; this only holds the caller's choices.
    instructions: Mutex<Option<crate::instructions::InstructionsConfig>>,
    loop_hooks: RwLock<Option<Arc<dyn crate::turn::hooks::LoopHooks>>>,
}

impl SessionService {
    pub fn plan(
        &self,
        session: &SessionId,
    ) -> Result<rness_protocol::events::PlanState, ServiceError> {
        let mut state =
            rness_protocol::events::PlanState::from_history(&self.store.history(session)?);
        if let Some(active) = self.tools.plan_selections.pending(session) {
            state.pending = Some(active);
        }
        Ok(state)
    }

    pub fn select_plan(&self, session: &SessionId, active: bool) -> Result<(), ServiceError> {
        let _activity = self
            .lifecycle
            .clone()
            .try_read_owned()
            .map_err(|_| ServiceError::Busy)?;
        self.store.history(session)?;
        if self
            .tools
            .get("exit_plan_mode")
            .and_then(|tool| tool.plan_config())
            .is_none()
        {
            return Err(ServiceError::InvalidConfig(
                "plan plugin is not enabled".into(),
            ));
        }
        self.tools.plan_selections.select(session, active);
        Ok(())
    }

    pub fn plan_store(&self) -> Arc<SessionStore> {
        self.store.clone()
    }

    pub fn file_references(
        &self,
        session: &SessionId,
        query: &crate::file_references::Query,
        cancel: &CancellationToken,
    ) -> Result<Vec<crate::file_references::Candidate>, ServiceError> {
        let workspace = self
            .store
            .workspace(session)?
            .ok_or_else(|| ServiceError::InvalidConfig("session has no workspace".into()))?;
        self.tools
            .file_references
            .list(std::path::Path::new(&workspace), query, cancel)
            .map_err(ServiceError::InvalidConfig)
    }

    pub fn reference_service(&self) -> &Arc<crate::file_references::FileReferences> {
        &self.tools.file_references
    }

    pub fn tasks(
        &self,
        session: &SessionId,
    ) -> Result<rness_protocol::events::TaskSnapshot, ServiceError> {
        Ok(rness_protocol::events::TaskSnapshot::from_history(
            &self.store.history(session)?,
        ))
    }

    pub fn commands(&self) -> &crate::interaction::CommandRegistry {
        &self.commands
    }

    pub fn set_default_workspace(&self, workspace: String) -> Result<(), ServiceError> {
        *self.default_workspace.lock().unwrap() = Self::normalize_workspace(Some(workspace))?;
        Ok(())
    }

    pub fn set_input_resolver(&self, resolver: Arc<InputResolver>) {
        *self.input_resolver.lock().unwrap() = Some(resolver);
    }
    pub fn with_agents(
        mut self,
        agents: std::collections::BTreeMap<String, crate::config::AgentDefinition>,
        models: crate::config::ModelRegistry,
    ) -> Self {
        self.agents = agents;
        self.models = models;
        self
    }

    pub fn with_sandbox(mut self, sandbox: crate::sandbox::SandboxConfig) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn agents(&self) -> &std::collections::BTreeMap<String, crate::config::AgentDefinition> {
        &self.agents
    }

    pub fn select_agent(&self, session: &SessionId, name: &str) -> Result<(), ServiceError> {
        let _activity = self
            .lifecycle
            .clone()
            .try_read_owned()
            .map_err(|_| ServiceError::Busy)?;
        let live = self.live(session);
        let _operation = live
            .operation
            .clone()
            .try_lock_owned()
            .map_err(|_| ServiceError::Busy)?;
        self.select_agent_reserved(session, name)
    }

    pub(crate) fn select_agent_reserved(
        &self,
        session: &SessionId,
        name: &str,
    ) -> Result<(), ServiceError> {
        let live = self.live(session);
        let inbox = live.inbox.lock().unwrap();
        if inbox.phase() != Phase::Idle {
            return Err(ServiceError::Busy);
        }
        let config = self.agent_config(self.config(session)?, name)?;
        self.require_workspace_for_sandbox(
            &self.store.workspace(session)?,
            self.effective_sandbox(&config),
        )?;
        if self.config(session)? != config {
            self.store
                .open(session)?
                .append(&SessionEvent::RequestConfig(config))?;
        }
        Ok(())
    }

    pub(crate) fn agent_config(
        &self,
        mut config: CallConfig,
        name: &str,
    ) -> Result<CallConfig, ServiceError> {
        let agent = self
            .agents
            .get(name)
            .ok_or_else(|| ServiceError::InvalidConfig(format!("unknown agent: {name}")))?;
        // Resolve before a profile replaces request options: neither a role
        // nor its profile may discard the session's (or parent's) restriction.
        let inherited_sandbox = config.sandbox;
        let sandbox = self
            .effective_sandbox(&config)
            .min(agent.sandbox.unwrap_or(self.sandbox.default));
        if let Some(profile) = &agent.profile {
            let ceiling = config.tool_ceiling;
            config = self
                .models
                .resolve_profile_for(profile, config.selection.as_ref().map(|s| s.route.as_str()))
                .map_err(ServiceError::InvalidConfig)?;
            config.tool_ceiling = ceiling;
        }
        if let Some(names) = &agent.tools {
            for name in names {
                if self.tools.get(name).is_none() {
                    return Err(ServiceError::InvalidConfig(format!("unknown tool: {name}")));
                }
            }
        }
        config.agent = Some(rness_protocol::events::AgentSnapshot {
            name: name.into(),
            instructions: agent.instructions.clone(),
            tools: agent.tools.clone(),
        });
        config.sandbox = (inherited_sandbox.is_some()
            || sandbox != crate::sandbox::SandboxMode::DangerFullAccess)
            .then_some(sandbox);
        self.provider_for(&config)?;
        Self::validate_config(&config)?;
        self.models
            .validate(&config)
            .map_err(ServiceError::InvalidConfig)?;
        Ok(config)
    }

    pub(crate) fn delegated_config(
        &self,
        parent: &SessionId,
        agent: Option<&str>,
    ) -> Result<CallConfig, ServiceError> {
        let mut config = self.config(parent)?;
        let mut allowed = self.tools.names();
        if let Some(ceiling) = &config.tool_ceiling {
            allowed.retain(|name| ceiling.contains(name));
        }
        if let Some(tools) = config.agent.as_ref().and_then(|agent| agent.tools.as_ref()) {
            allowed.retain(|name| tools.contains(name));
        }
        config.tool_ceiling = Some(allowed);
        // Both unnamed and named children retain the parent's restriction;
        // role resolution may only tighten it, including through a profile.
        config.agent = None;
        if let Some(name) = agent {
            if !self.agents.get(name).is_some_and(|agent| agent.subagent) {
                return Err(ServiceError::InvalidConfig(format!(
                    "agent is not enabled for delegation: {name}"
                )));
            }
            config = self.agent_config(config, name)?;
        }
        Ok(config)
    }

    pub fn new(
        store: SessionStore,
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        config: TurnConfig,
        bus: Arc<EventBus>,
    ) -> Self {
        Self {
            images: tools.images.clone(),
            store: Arc::new(store),
            provider,
            resolver: None,
            creation_seed: CallConfig::default(),
            agents: Default::default(),
            sandbox: Default::default(),
            models: Default::default(),
            tools,
            config,
            bus,
            live: Mutex::new(HashMap::new()),
            default_workspace: Mutex::new(None),
            commands: {
                let commands = crate::interaction::CommandRegistry::default();
                commands
                    .register(Arc::new(crate::interaction::HelpCommand))
                    .expect("built-in command");
                commands
                    .register(Arc::new(crate::interaction::AgentCommand))
                    .expect("built-in command");
                commands
            },
            input_resolver: Mutex::new(None),
            lifecycle: Arc::new(tokio::sync::RwLock::new(())),
            closing: Mutex::new(std::collections::HashSet::new()),
            instructions: Mutex::new(None),
            loop_hooks: RwLock::new(None),
        }
    }

    /// Install (or clear) the loop-level hooks used by `run_turn`.
    pub fn set_loop_hooks(&self, hooks: Option<Arc<dyn crate::turn::hooks::LoopHooks>>) {
        *self.loop_hooks.write().expect("loop hooks lock") = hooks;
    }

    fn loop_hooks(&self) -> Option<Arc<dyn crate::turn::hooks::LoopHooks>> {
        self.loop_hooks.read().expect("loop hooks lock").clone()
    }

    pub fn set_images(&self, images: Arc<crate::images::ImageStore>) -> Result<(), ServiceError> {
        self.images
            .set(images)
            .map_err(|_| ServiceError::InvalidConfig("image store already configured".into()))
    }

    pub fn set_image_processor(
        &self,
        processor: Option<(String, Arc<crate::images::ImageProcessor>)>,
    ) -> Result<(), ServiceError> {
        self.images
            .get()
            .ok_or_else(|| ServiceError::InvalidConfig("image storage is not configured".into()))?
            .set_processor(processor)
            .map_err(ServiceError::InvalidConfig)
    }

    pub fn image_policy(
        &self,
        update: Option<serde_json::Value>,
    ) -> Result<crate::images::ImagePolicy, ServiceError> {
        let store = self
            .images
            .get()
            .ok_or_else(|| ServiceError::InvalidConfig("image storage is not configured".into()))?;
        if let Some(update) = update {
            let mut value = serde_json::to_value(store.effective_request_policy(store.policy()))
                .map_err(|e| ServiceError::InvalidConfig(e.to_string()))?;
            let fields = update.as_object().ok_or_else(|| {
                ServiceError::InvalidConfig("image policy must be an object".into())
            })?;
            for (key, field) in fields {
                value[key] = field.clone();
            }
            let policy = serde_json::from_value(value)
                .map_err(|e| ServiceError::InvalidConfig(e.to_string()))?;
            store
                .set_request_policy(Some(policy))
                .map_err(ServiceError::InvalidConfig)?;
        }
        Ok(store.effective_request_policy(store.policy()))
    }

    pub fn admit_image(
        &self,
        session: &SessionId,
        data: &[u8],
        media_type: &str,
    ) -> Result<rness_protocol::events::ImageRef, ServiceError> {
        self.store.history(session)?;
        self.images
            .get()
            .ok_or_else(|| ServiceError::InvalidConfig("image storage is not configured".into()))?
            .admit_for_session(session, data, media_type)
            .map_err(ServiceError::InvalidConfig)
    }

    /// Only session-referenced images may be retrieved through this boundary.
    pub fn read_image(
        &self,
        session: &SessionId,
        id: &str,
    ) -> Result<(String, Vec<u8>), ServiceError> {
        let history = self.store.history(session)?;
        for envelope in history {
            if let SessionEvent::ToolResult(result) = &envelope.event {
                for part in &result.content {
                    if let rness_protocol::events::ToolResultContentPart::Image { attachment } =
                        part
                    {
                        if attachment.id == id {
                            let data = self
                                .images
                                .get()
                                .ok_or_else(|| {
                                    ServiceError::InvalidConfig(
                                        "image storage is not configured".into(),
                                    )
                                })?
                                .read(attachment)
                                .map_err(ServiceError::InvalidConfig)?;
                            return Ok((attachment.media_type.clone(), data));
                        }
                    }
                }
            }
            let content = match &envelope.event {
                SessionEvent::UserMessage(message) => &message.content,
                SessionEvent::AssistantMessage(message) => &message.content,
                _ => continue,
            };
            for part in content {
                if let ContentPart::Image { attachment } = part {
                    if attachment.id == id {
                        let data = self
                            .images
                            .get()
                            .ok_or_else(|| {
                                ServiceError::InvalidConfig(
                                    "image storage is not configured".into(),
                                )
                            })?
                            .read(attachment)
                            .map_err(ServiceError::InvalidConfig)?;
                        return Ok((attachment.media_type.clone(), data));
                    }
                }
            }
        }
        Err(ServiceError::InvalidConfig(
            "image is not referenced by this session".into(),
        ))
    }

    /// Configure the composition-root resolver for durable model selections.
    /// The resolver owns route tables and credentials; neither enters logs.
    pub fn with_provider_resolver(
        mut self,
        creation_seed: CallConfig,
        resolver: Arc<ProviderResolver>,
    ) -> Self {
        self.creation_seed = creation_seed;
        self.resolver = Some(resolver);
        self
    }

    fn provider_for(&self, config: &CallConfig) -> Result<Arc<dyn Provider>, ServiceError> {
        let Some(selection) = &config.selection else {
            return Ok(Arc::clone(&self.provider));
        };
        let resolver = self
            .resolver
            .as_ref()
            .ok_or_else(|| ServiceError::NoProviderResolver {
                route: selection.route.clone(),
                model: selection.model.clone(),
            })?;
        let provider = resolver(selection).map_err(|message| ServiceError::ProviderResolution {
            route: selection.route.clone(),
            model: selection.model.clone(),
            message,
        })?;
        if self
            .models
            .capabilities(selection)
            .is_some_and(|caps| caps.image_input == Some(false))
        {
            Ok(Arc::new(crate::turn::provider::ImageCapabilityProvider {
                inner: provider,
            }))
        } else {
            Ok(provider)
        }
    }

    /// Resolve once for both automatic and manual policy-based compaction.
    fn compaction_provider(
        &self,
        current: &CallConfig,
        policy: &crate::turn::compaction::Policy,
    ) -> Result<Arc<dyn Provider>, ServiceError> {
        policy.validate().map_err(ServiceError::InvalidConfig)?;
        let main = self.provider_for(current)?;
        let mut config = match &policy.summary_profile {
            Some(name) => self
                .models
                .resolve_profile_for(name, current.selection.as_ref().map(|s| s.route.as_str()))
                .map_err(ServiceError::InvalidConfig)?,
            None => current.clone(),
        };
        let summary = if policy.summary_profile.is_some() {
            self.provider_for(&config)?
        } else {
            main.clone()
        };
        config.max_output_tokens = summary
            .supports_max_output_tokens()
            .then_some(policy.summary_tokens);
        Self::validate_config(&config)?;
        self.models
            .validate(&config)
            .map_err(ServiceError::InvalidConfig)?;
        if let (Some(rness_protocol::events::Reasoning::BudgetTokens { tokens }), Some(limit)) =
            (&config.reasoning, config.max_output_tokens)
        {
            if *tokens >= limit {
                return Err(ServiceError::InvalidConfig(
                    "compaction reasoning budget must be less than summary_tokens".into(),
                ));
            }
        }
        Ok(Arc::new(crate::turn::provider::SummaryProvider {
            main,
            summary,
            config,
        }))
    }

    fn validate_config(config: &CallConfig) -> Result<(), ServiceError> {
        if let Some(selection) = &config.selection {
            if selection.route.trim().is_empty() || selection.model.trim().is_empty() {
                return Err(ServiceError::InvalidConfig(
                    "selection route and model must be non-empty".into(),
                ));
            }
        }
        if let Some(rness_protocol::events::Reasoning::BudgetTokens { tokens }) = config.reasoning {
            if tokens < 1024 {
                return Err(ServiceError::InvalidConfig(
                    "reasoning budget must be at least 1024 tokens".into(),
                ));
            }
        }
        if let Some(temperature) = config.temperature {
            if !temperature.is_finite() {
                return Err(ServiceError::InvalidConfig(
                    "temperature must be finite".into(),
                ));
            }
        }
        Ok(())
    }

    /// Enable workspace instructions (AGENTS.md chain). The caller owns
    /// the policy — candidates and budget are explicit, never defaulted.
    pub fn set_instructions(&self, config: crate::instructions::InstructionsConfig) {
        *self.instructions.lock().unwrap() = Some(config);
    }

    /// dsh baseline check: is an instructions baseline with the CURRENT
    /// identity visible in the projected context? If not (first turn,
    /// post-compaction fold, files changed), append a fresh one —
    /// re-read from disk — as a durable sourced user message.
    fn ensure_instructions(
        &self,
        log: &mut crate::session::log::SessionLog,
        session: &SessionId,
    ) -> Result<(), ServiceError> {
        let Some(mut config) = self.instructions.lock().unwrap().clone() else {
            return Ok(());
        };
        if let Some(workspace) = self.store.workspace(session)? {
            config.cwd = workspace.into();
        }
        let Some(baseline) = crate::instructions::render(&config) else {
            return Ok(());
        };

        let replayed = replay(&self.store, session)?;
        let cited: std::collections::HashSet<&str> = replayed
            .context
            .sources
            .iter()
            .map(|s| s.as_str())
            .collect();
        let visible_current = replayed.history.iter().any(|e| {
            cited.contains(e.id.as_str())
                && matches!(
                    &e.event,
                    SessionEvent::UserMessage(UserMessage {
                        source: Some(MessageSource::Instructions { identity }),
                        ..
                    }) if *identity == baseline.identity
                )
        });
        if visible_current {
            return Ok(());
        }

        log.append(&SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Inject,
            content: vec![ContentPart::Text {
                text: baseline.text,
            }],
            source: Some(MessageSource::Instructions {
                identity: baseline.identity,
            }),
        }))?;
        Ok(())
    }

    /// The underlying store, for read-side queries not wrapped here
    /// (ancestry, children, raw history).
    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    /// The kernel bus this service emits lifecycle events on, for
    /// subscribers that need engine-adjacent wiring (subagent settle
    /// notifications).
    pub fn bus(&self) -> &Arc<EventBus> {
        &self.bus
    }

    fn live(&self, session: &SessionId) -> Arc<Live> {
        Arc::clone(
            self.live
                .lock()
                .unwrap()
                .entry(session.clone())
                .or_default(),
        )
    }

    // -- session lifecycle -------------------------------------------------

    /// Create a new root session. Returns its id; no turn starts. The
    /// explicit composition-root seed is committed once, immediately after
    /// the header, so HTTP-created sessions carry the same selection.
    fn normalize_workspace(workspace: Option<String>) -> Result<Option<String>, ServiceError> {
        workspace
            .map(|path| {
                let root = std::fs::canonicalize(&path)
                    .map_err(|e| ServiceError::InvalidConfig(format!("workspace {path}: {e}")))?;
                if !root.is_dir() {
                    return Err(ServiceError::InvalidConfig(format!(
                        "workspace {path} is not a directory"
                    )));
                }
                root.to_str().map(str::to_owned).ok_or_else(|| {
                    ServiceError::InvalidConfig("workspace must be valid UTF-8".into())
                })
            })
            .transpose()
    }

    fn require_workspace_for_sandbox(
        &self,
        workspace: &Option<String>,
        sandbox: crate::sandbox::SandboxMode,
    ) -> Result<(), ServiceError> {
        if workspace.is_none() && sandbox != crate::sandbox::SandboxMode::DangerFullAccess {
            return Err(ServiceError::InvalidConfig(
                "a session workspace is required when sandboxing is configured; set a default workspace or create the session with one".into(),
            ));
        }
        Ok(())
    }

    pub fn create(&self, workspace: Option<String>) -> Result<SessionId, ServiceError> {
        let workspace = workspace.or_else(|| self.default_workspace.lock().unwrap().clone());
        let workspace = Self::normalize_workspace(workspace)?;
        let seed = self.creation_config()?;
        self.require_workspace_for_sandbox(&workspace, self.effective_sandbox(&seed))?;
        let mut log = self.store.create(workspace)?;
        if seed != CallConfig::default() {
            log.append(&SessionEvent::RequestConfig(seed))?;
        }
        let session = log.session().clone();
        self.bus.emit::<SessionCreatedEv>(&SessionCreatedNotice {
            session: session.clone(),
            workspace: self.store.workspace(&session).ok().flatten(),
            delegation: None,
        });
        Ok(session)
    }

    /// Create a delegated fresh session with this service's explicit seed.
    /// Spawned children therefore resolve the same model selection as roots.
    pub fn create_delegated(
        &self,
        workspace: Option<String>,
        delegation: rness_protocol::branch::Delegation,
    ) -> Result<SessionId, ServiceError> {
        let workspace = match workspace {
            Some(path) => Some(path),
            None => self.store.workspace(&delegation.parent)?,
        };
        let workspace = Self::normalize_workspace(workspace)?;
        let mut seed = self.creation_config()?;
        if let Some(parent_sandbox) = self.config(&delegation.parent)?.sandbox {
            seed.sandbox = Some(self.effective_sandbox(&seed).min(parent_sandbox));
        }
        self.require_workspace_for_sandbox(&workspace, self.effective_sandbox(&seed))?;
        let mut log = self.store.create_delegated(workspace, delegation)?;
        if seed != CallConfig::default() {
            log.append(&SessionEvent::RequestConfig(seed))?;
        }
        let session = log.session().clone();
        let delegation = self.store.delegation(&session).ok().flatten();
        self.bus.emit::<SessionCreatedEv>(&SessionCreatedNotice {
            session: session.clone(),
            workspace: self.store.workspace(&session).ok().flatten(),
            delegation,
        });
        Ok(session)
    }

    fn creation_config(&self) -> Result<CallConfig, ServiceError> {
        Self::validate_config(&self.creation_seed)?;
        let mut seed = self.creation_seed.clone();
        let sandbox = self.effective_sandbox(&seed).min(self.sandbox.default);
        // Keep opt-in defaults absent, but always persist restricted policy.
        if seed.sandbox.is_some() || sandbox != crate::sandbox::SandboxMode::DangerFullAccess {
            seed.sandbox = Some(sandbox);
        }
        Ok(seed)
    }

    /// Fork `session` at `at` (or its tip). Prefix request options replay
    /// into the child, but sandbox authority cannot exceed the current parent.
    pub fn fork(&self, session: &SessionId, at: Option<String>) -> Result<SessionId, ServiceError> {
        let mut log = self.store.fork(session, at)?;
        self.tighten_fork_sandbox(&mut log, session)?;
        Ok(log.session().clone())
    }

    /// Fork a delegated child, retaining current source and delegating-parent
    /// sandbox restrictions even when the inherited prefix predates them.
    pub fn fork_delegated(
        &self,
        session: &SessionId,
        at: Option<String>,
        delegation: rness_protocol::branch::Delegation,
    ) -> Result<SessionId, ServiceError> {
        let parent = delegation.parent.clone();
        let mut log = self.store.fork_delegated(session, at, delegation)?;
        self.tighten_fork_sandbox(&mut log, session)?;
        if parent != *session {
            self.tighten_fork_sandbox(&mut log, &parent)?;
        }
        Ok(log.session().clone())
    }

    fn tighten_fork_sandbox(
        &self,
        log: &mut crate::session::log::SessionLog,
        parent: &SessionId,
    ) -> Result<(), ServiceError> {
        let parent_sandbox = self.effective_sandbox(&self.config(parent)?);
        let mut config = self.config(log.session())?;
        if parent_sandbox < self.effective_sandbox(&config) {
            config.sandbox = Some(parent_sandbox);
            log.append(&SessionEvent::RequestConfig(config))?;
        }
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<SessionId>, ServiceError> {
        Ok(self.store.list()?)
    }

    /// List only user-created sessions (excludes subagent children).
    pub fn list_roots(&self) -> Result<Vec<SessionId>, ServiceError> {
        Ok(self.store.list_roots()?)
    }

    pub fn phase(&self, session: &SessionId) -> Phase {
        self.live(session).inbox.lock().unwrap().phase()
    }

    // -- read side ---------------------------------------------------------

    /// Frontend transcript (attempts and all), derived across forks.
    pub fn transcript(&self, session: &SessionId) -> Result<Transcript, ServiceError> {
        Ok(transcript(&self.store.history(session)?))
    }

    /// Invariant-checked replay (history + model context).
    pub fn replay(&self, session: &SessionId) -> Result<Replayed, ServiceError> {
        Ok(replay(&self.store, session)?)
    }

    // -- input -------------------------------------------------------------

    /// Reserve engine quiescence for extension lifecycle changes. Fails rather
    /// than waiting on a turn that may itself be waiting on the plugin host.
    /// Holding the returned guard rejects new sends and compactions.
    pub fn try_extension_maintenance(
        &self,
    ) -> Result<tokio::sync::OwnedRwLockWriteGuard<()>, ServiceError> {
        self.lifecycle
            .clone()
            .try_write_owned()
            .map_err(|_| ServiceError::Busy)
    }

    /// Submit user input. Classification (docs §5):
    /// - idle + followup/steer → committed, a turn burst starts now
    /// - idle + inject → committed, nothing runs
    /// - running + followup → queued; runs as its own turn after this one
    /// - running + steer/inject → committed at the next step boundary
    pub fn send(
        &self,
        session: &SessionId,
        intent: UserIntent,
        content: Vec<ContentPart>,
    ) -> Result<Disposition, ServiceError> {
        self.send_or_retry(session, intent, content, false, Notice::None, None)
    }

    /// Deliver a background completion at the next step, waking an idle owner.
    pub fn notify_job(
        &self,
        session: &SessionId,
        text: String,
    ) -> Result<Disposition, ServiceError> {
        self.send_or_retry(
            session,
            UserIntent::Steer,
            vec![ContentPart::Text { text }],
            false,
            Notice::Job,
            None,
        )
    }

    /// Await reservations and retain them through admission, avoiding a retry race.
    pub async fn notify_job_wait(
        &self,
        session: &SessionId,
        text: String,
    ) -> Result<Disposition, ServiceError> {
        let activity = self.lifecycle.clone().read_owned().await;
        let operation = self.live(session).operation.clone().lock_owned().await;
        self.send_or_retry(
            session,
            UserIntent::Steer,
            vec![ContentPart::Text { text }],
            false,
            Notice::Job,
            Some((activity, operation)),
        )
    }

    /// Record user intervention without waking an idle principal. Await the
    /// invoking command's reservation and use the normal single-writer inbox.
    pub(crate) async fn notify_user_intervention(
        &self,
        session: &SessionId,
        text: String,
    ) -> Result<Disposition, ServiceError> {
        let activity = self.lifecycle.clone().read_owned().await;
        let operation = self.live(session).operation.clone().lock_owned().await;
        self.send_or_retry(
            session,
            UserIntent::Inject,
            vec![ContentPart::Text { text }],
            false,
            Notice::Subagent,
            Some((activity, operation)),
        )
    }

    /// Deliver a continuable child's closing answer. Wait out command
    /// reservations instead of dropping busy notices.
    pub async fn notify_subagent_settled(
        &self,
        session: &SessionId,
        text: String,
    ) -> Result<Disposition, ServiceError> {
        let activity = self.lifecycle.clone().read_owned().await;
        let operation = self.live(session).operation.clone().lock_owned().await;
        let mut lineage = vec![session.clone()];
        while let Some(delegation) = self.store.delegation(lineage.last().unwrap())? {
            if lineage.contains(&delegation.parent) {
                return Err(ServiceError::InvalidConfig(
                    "cyclic subagent lineage".into(),
                ));
            }
            lineage.push(delegation.parent);
        }
        loop {
            {
                // Hold through admission: teardown cannot race the idle wake.
                let closing = self.closing.lock().unwrap();
                let teardown = lineage.iter().any(|id| closing.contains(id));
                if !teardown || self.phase(session) == Phase::Idle {
                    let intent = if teardown {
                        UserIntent::Inject
                    } else {
                        UserIntent::Steer
                    };
                    return self.send_or_retry(
                        session,
                        intent,
                        vec![ContentPart::Text { text }],
                        false,
                        Notice::Subagent,
                        Some((activity, operation)),
                    );
                }
            }
            // Cancelled bursts park queued input. Let the writer retire before
            // logging a teardown notice, so the result remains durable.
            self.cancel(session);
            self.join(session).await;
            // Another joiner may already own the handle; do not spin while
            // waiting for that burst to publish Idle.
            tokio::task::yield_now().await;
        }
    }

    /// Teardown is distinct from interrupting a turn: suppress automatic wakes
    /// for this session and its descendants for the remainder of this host.
    pub fn begin_teardown(&self, session: &SessionId) {
        let mut closing = self.closing.lock().unwrap();
        closing.insert(session.clone());
        self.cancel(session);
    }

    /// Continue a failed turn from durable context without appending user content.
    pub fn retry(&self, session: &SessionId) -> Result<Disposition, ServiceError> {
        self.send_or_retry(
            session,
            UserIntent::Followup,
            vec![],
            true,
            Notice::None,
            None,
        )
    }

    /// Idle-only admission for a durable external queue. The ID is committed in
    /// the same fsynced envelope as the prompt; replay never submits it twice.
    /// false means busy: the caller must retain its durable pending record.
    pub fn deliver_external_once(
        &self,
        session: &SessionId,
        id: &str,
        text: String,
    ) -> Result<bool, ServiceError> {
        if text.trim().is_empty() || text.trim_start().starts_with('/') {
            return Err(ServiceError::InvalidConfig(
                "external prompts must be nonempty chat text, not commands".into(),
            ));
        }
        let activity = self
            .lifecycle
            .clone()
            .try_read_owned()
            .map_err(|_| ServiceError::Busy)?;
        let live = self.live(session);
        let operation = live
            .operation
            .clone()
            .try_lock_owned()
            .map_err(|_| ServiceError::Busy)?;
        if self.store.history(session)?.iter().any(|e| matches!(&e.event,
            SessionEvent::UserMessage(m) if matches!(&m.source, Some(MessageSource::ExternalPrompt { id: key }) if key == id))) {
            return Ok(true);
        }
        if self.closing.lock().unwrap().contains(session)
            || live.inbox.lock().unwrap().phase() != Phase::Idle
        {
            return Ok(false);
        }
        self.send_or_retry_sourced(
            session,
            UserIntent::Followup,
            vec![ContentPart::Text { text }],
            false,
            Notice::None,
            Some((activity, operation)),
            Some(MessageSource::ExternalPrompt { id: id.into() }),
        )?;
        Ok(true)
    }

    pub async fn notify_job_once(
        &self,
        session: &SessionId,
        id: &str,
        text: String,
    ) -> Result<bool, ServiceError> {
        let activity = self.lifecycle.clone().read_owned().await;
        let live = self.live(session);
        let operation = live.operation.clone().lock_owned().await;
        if self.store.history(session)?.iter().any(|e| matches!(&e.event, SessionEvent::UserMessage(message) if matches!(&message.source, Some(rness_protocol::events::MessageSource::JobCompletion { id: key }) if key == id))) { return Ok(true); }
        let mut pending = live.job_pending.lock().unwrap();
        if live.inbox.lock().unwrap().phase() == Phase::Idle {
            pending.remove(id);
        }
        if pending.contains(id) {
            return Ok(false);
        }
        self.send_or_retry_sourced(
            session,
            UserIntent::Steer,
            vec![ContentPart::Text { text }],
            false,
            Notice::Job,
            Some((activity, operation)),
            Some(rness_protocol::events::MessageSource::JobCompletion { id: id.into() }),
        )?;
        pending.insert(id.into());
        Ok(false)
    }

    fn send_or_retry(
        &self,
        session: &SessionId,
        intent: UserIntent,
        content: Vec<ContentPart>,
        retry: bool,
        notice: Notice,
        reservation: Option<(
            tokio::sync::OwnedRwLockReadGuard<()>,
            tokio::sync::OwnedMutexGuard<()>,
        )>,
    ) -> Result<Disposition, ServiceError> {
        self.send_or_retry_sourced(session, intent, content, retry, notice, reservation, None)
    }
    fn send_or_retry_sourced(
        &self,
        session: &SessionId,
        intent: UserIntent,
        content: Vec<ContentPart>,
        retry: bool,
        notice: Notice,
        reservation: Option<(
            tokio::sync::OwnedRwLockReadGuard<()>,
            tokio::sync::OwnedMutexGuard<()>,
        )>,
        source: Option<rness_protocol::events::MessageSource>,
    ) -> Result<Disposition, ServiceError> {
        if notice == Notice::None && !matches!(source, Some(MessageSource::ExternalPrompt { .. })) {
            if let [ContentPart::Text { text }] = content.as_slice() {
                if let Some(command) = self.prepare_command(session, text)? {
                    return command.execute(self);
                }
            }
        }
        let workspace = self.store.workspace(session)?.map(std::path::PathBuf::from);
        if let Some(path) = &workspace {
            if !path.is_absolute() || !path.is_dir() {
                return Err(ServiceError::InvalidConfig(format!(
                    "session workspace is not an available absolute directory: {}",
                    path.display()
                )));
            }
        }
        let resolver = self.input_resolver.lock().unwrap().clone();
        let content = match resolver {
            Some(resolve) => {
                resolve(workspace.as_deref(), content).map_err(ServiceError::InvalidConfig)?
            }
            None => content,
        };
        for part in &content {
            if let ContentPart::Image { attachment } = part {
                let images = self.images.get().ok_or_else(|| {
                    ServiceError::InvalidConfig("image storage is not configured".into())
                })?;
                if !images.admitted_for_session(session, &attachment.id)
                    && self.read_image(session, &attachment.id).is_err()
                {
                    return Err(ServiceError::InvalidConfig(
                        "image was not uploaded to this session".into(),
                    ));
                }
                images
                    .read(attachment)
                    .map_err(ServiceError::InvalidConfig)?;
            }
        }
        let live = self.live(session);
        let (activity, _operation) = match reservation {
            Some(reservation) => reservation,
            None => {
                let activity = self
                    .lifecycle
                    .clone()
                    .try_read_owned()
                    .map_err(|_| ServiceError::Busy)?;
                let operation = live
                    .operation
                    .clone()
                    .try_lock_owned()
                    .map_err(|_| ServiceError::Busy)?;
                (activity, operation)
            }
        };
        let command = live.command.lock().unwrap();
        if command.is_some() {
            return Err(ServiceError::Busy);
        }
        let request_config = self.config(session)?;
        let images_forbidden = request_config
            .selection
            .as_ref()
            .and_then(|selection| self.models.capabilities(selection))
            .is_some_and(|caps| caps.image_input == Some(false));
        if images_forbidden {
            let contains_image = |parts: &[ContentPart]| {
                parts
                    .iter()
                    .any(|part| matches!(part, ContentPart::Image { .. }))
            };
            let context = crate::session::projection::model_context(&self.store.history(session)?);
            let history_has_images = context.turns.iter().any(|turn| match turn {
                crate::session::projection::ModelTurn::User { content }
                | crate::session::projection::ModelTurn::Assistant { content } => {
                    contains_image(content)
                }
                crate::session::projection::ModelTurn::ToolResults { results } => {
                    results.iter().any(|r| {
                        r.content.iter().any(|p| {
                            matches!(
                                p,
                                rness_protocol::events::ToolResultContentPart::Image { .. }
                            )
                        })
                    })
                }
            });
            if contains_image(&content) || history_has_images {
                return Err(ServiceError::InvalidConfig(
                    "selected model explicitly disables image input".into(),
                ));
            }
        }
        let mut inbox = live.inbox.lock().unwrap();
        if retry {
            if inbox.phase() != Phase::Idle {
                return Err(ServiceError::Busy);
            }
            let history = self.store.history(session)?;
            let tip = history
                .iter()
                .rev()
                .find(|e| !matches!(e.event, SessionEvent::RequestConfig(_)));
            if !matches!(
                tip.map(|e| &e.event),
                Some(SessionEvent::TurnEnded {
                    outcome: rness_protocol::events::TurnOutcome::Failed,
                    ..
                })
            ) {
                return Err(ServiceError::InvalidConfig(
                    "retry requires a failed turn at the session tip".into(),
                ));
            }
        }
        // Job completions resume idle owners just like other followups. A
        // lifetime wake count silently strands valid chains of background work;
        // duplicate completions are handled by job settlement / durable IDs.
        let (intent, disposition) = inbox.submit_sourced(intent, content.clone(), source.clone());
        match &disposition {
            Disposition::Command(_) => unreachable!("inbox does not execute commands"),
            Disposition::Queued => {}
            Disposition::LogOnly => {
                drop(inbox);
                let mut log = self.store.open(session)?;
                log.append(&SessionEvent::UserMessage(UserMessage {
                    intent,
                    content,
                    source,
                }))?;
            }
            Disposition::StartTurn => {
                // Resolve before committing the prompt or flipping phase:
                // an unavailable selected provider must not leave a session
                // running with an input it cannot process.
                let request_config = self.config(session)?;
                let policy = request_config
                    .selection
                    .as_ref()
                    .and_then(|s| {
                        self.config
                            .compaction
                            .get(&format!("{}/{}", s.route, s.model))
                    })
                    .or_else(|| self.config.compaction.get("default"));
                let provider = match policy {
                    Some(policy) => self.compaction_provider(&request_config, policy)?,
                    None => self.provider_for(&request_config)?,
                };
                let mut log = self.store.open(session)?;
                crate::turn::compaction::recover(&mut log)?;
                // Workspace instructions precede the prompt that opens
                // the turn (dsh baseline order).
                self.ensure_instructions(&mut log, session)?;
                if !retry {
                    log.append(&SessionEvent::UserMessage(UserMessage {
                        intent,
                        content,
                        source,
                    }))?;
                }
                let turns_so_far = log
                    .read_all()?
                    .iter()
                    .filter(|e| matches!(e.event, SessionEvent::TurnStarted { .. }))
                    .count() as u32;
                inbox.set_phase(Phase::Running);
                drop(inbox);

                let token = CancellationToken::new();
                *live.cancel.lock().unwrap() = token.clone();
                let task = burst(
                    Arc::clone(&self.store),
                    Arc::clone(&live),
                    Arc::clone(&self.bus),
                    provider,
                    Arc::clone(&self.tools),
                    self.config.clone(),
                    log,
                    token,
                    turns_so_far,
                    self.loop_hooks(),
                );
                let handle = tokio::spawn(async move {
                    let _activity = activity;
                    task.await;
                });
                *live.handle.lock().unwrap() = Some(handle);
            }
        }
        Ok(disposition)
    }

    /// Resolve and reserve synchronously, before handing work to a scheduler.
    /// The lifecycle guard prevents unload between lookup and execution.
    pub fn prepare_command(
        &self,
        session: &SessionId,
        text: &str,
    ) -> Result<Option<PreparedCommand>, ServiceError> {
        let activity = self
            .lifecycle
            .clone()
            .try_read_owned()
            .map_err(|_| ServiceError::Busy)?;
        let Some((command, offset)) = self.commands.resolve(text) else {
            return Ok(None);
        };
        self.store.workspace(session)?;
        let live = self.live(session);
        let operation = live
            .operation
            .clone()
            .try_lock_owned()
            .map_err(|_| ServiceError::Busy)?;
        let mut active = live.command.lock().unwrap();
        if active.is_some() || (!command.allow_busy() && self.phase(session) != Phase::Idle) {
            return Err(ServiceError::Busy);
        }
        let cancel = CancellationToken::new();
        *active = Some(cancel.clone());
        drop(active);
        Ok(Some(PreparedCommand {
            reservation: Arc::new(CommandReservation {
                live: live.clone(),
                _operation: operation,
                _activity: activity,
            }),
            live,
            command,
            session: session.clone(),
            raw_input: text[offset..].to_owned(),
            cancel,
        }))
    }

    /// Admission happens when called, not when the returned future is polled.
    pub fn send_async(
        self: &Arc<Self>,
        session: SessionId,
        intent: UserIntent,
        content: Vec<ContentPart>,
    ) -> impl std::future::Future<Output = Result<Disposition, ServiceError>> + Send + 'static {
        let prepared = match content.as_slice() {
            [ContentPart::Text { text }] => self.prepare_command(&session, text),
            _ => Ok(None),
        };
        let service = Arc::clone(self);
        async move {
            let prepared = prepared?;
            tokio::task::spawn_blocking(move || match prepared {
                Some(command) => command.execute(&service),
                None => service.send(&session, intent, content),
            })
            .await
            .map_err(|e| ServiceError::InvalidConfig(format!("submission failed: {e}")))?
        }
    }

    pub fn command_running(&self, session: &SessionId) -> bool {
        self.live(session).command.lock().unwrap().is_some()
    }

    pub fn cancel(&self, session: &SessionId) {
        if let Some(token) = self.live(session).command.lock().unwrap().as_ref() {
            token.cancel();
        }
        self.live(session).cancel.lock().unwrap().cancel();
    }

    /// Compact the session: fold everything before the kept tail into a
    /// model-written summary and commit a `compaction/summary` checkpoint
    /// that shadows the folded span (dsh checkpoint model — the log stays
    /// append-only, projections skip shadowed events).
    ///
    /// `keep_turns`: how many trailing turns stay verbatim (dsh's
    /// stability rule — recent context survives, old context folds).
    /// Idle-only: compacting mid-turn would race the burst's writer log.
    /// Fail-loud when there is nothing to fold.
    pub async fn compact(
        &self,
        session: &SessionId,
        keep_turns: usize,
    ) -> Result<CompactReport, ServiceError> {
        let _activity = self
            .lifecycle
            .clone()
            .try_read_owned()
            .map_err(|_| ServiceError::Busy)?;
        let live = self.live(session);
        let _operation = live
            .operation
            .clone()
            .try_lock_owned()
            .map_err(|_| ServiceError::Busy)?;
        if self.phase(session) != Phase::Idle {
            return Err(ServiceError::Busy);
        }
        let replayed = replay(&self.store, session)?;
        let policy = replayed
            .context
            .config
            .selection
            .as_ref()
            .and_then(|s| {
                self.config
                    .compaction
                    .get(&format!("{}/{}", s.route, s.model))
            })
            .or_else(|| self.config.compaction.get("default"));
        let provider = match policy {
            Some(policy) => self.compaction_provider(&replayed.context.config, policy)?,
            None => self.provider_for(&replayed.context.config)?,
        };
        let history = &replayed.history;

        // The cut: index of the TurnStarted opening the keep_turns-th
        // turn from the end. keep_turns == 0 folds everything.
        let starts: Vec<usize> = history
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.event, SessionEvent::TurnStarted { .. }))
            .map(|(i, _)| i)
            .collect();
        let mut cut = if keep_turns == 0 {
            history.len()
        } else if starts.len() <= keep_turns {
            0
        } else {
            starts[starts.len() - keep_turns]
        };
        // The user message that opens a turn is logged by send() BEFORE
        // the burst writes TurnStarted — keep it with its turn.
        while cut > 0 && matches!(history[cut - 1].event, SessionEvent::UserMessage(_)) {
            cut -= 1;
        }

        // Fold span: model-visible events before the cut that the current
        // projection actually cites (already-shadowed ones are inert;
        // earlier checkpoints DO fold — their summary is model-visible).
        let cited: std::collections::HashSet<&str> = replayed
            .context
            .sources
            .iter()
            .map(|s| s.as_str())
            .collect();
        // Transitive closure: folding a checkpoint also re-claims the
        // span it shadowed — otherwise those events would resurface once
        // their shadower is itself shadowed.
        let replaces: Vec<String> = history[..cut]
            .iter()
            .filter(|e| cited.contains(e.id.as_str()))
            .flat_map(|e| {
                let mut ids = vec![e.id.clone()];
                match &e.event {
                    SessionEvent::Compaction(c) => ids.extend(c.replaces.iter().cloned()),
                    // Folding a prune re-claims its original too —
                    // otherwise the full-length result resurfaces.
                    SessionEvent::Prune(pr) => ids.push(pr.replaces.clone()),
                    _ => {}
                }
                ids
            })
            .collect();
        if replaces.is_empty() {
            return Err(ServiceError::NothingToCompact);
        }

        // Summarize ONLY the folding region, projected exactly as the
        // model saw it (shadowing of prior checkpoints honored).
        let mut fold_ctx = crate::session::projection::model_context(&history[..cut]);
        fold_ctx.config = provider
            .summary_config()
            .cloned()
            .unwrap_or_else(|| replayed.context.config.clone());
        let estimated_tokens = crate::turn::compaction::measure(&fold_ctx, "", &[]);
        self.bus.emit::<FrameEv>(&Frame::CompactionStarted {
            session: session.clone(),
            events: replaces.len(),
            estimated_tokens,
        });
        let summary = summarize(provider.as_ref(), fold_ctx, policy).await;
        self.bus.emit::<FrameEv>(&Frame::CompactionFinished {
            session: session.clone(),
            changed: summary.is_ok(),
        });
        let summary = summary?;

        let shadowed = replaces.len();
        let mut log = self.store.open(session)?;
        log.append(&SessionEvent::Compaction(Compaction {
            replaces,
            summary: summary.clone(),
            model: provider.summary_model().to_string(),
        }))?;
        drop(log);

        self.bus.emit::<FrameEv>(&Frame::HistoryChanged {
            session: session.clone(),
        });
        Ok(CompactReport { shadowed, summary })
    }

    /// Compact a zero-based half-open model-message region, rejecting stale snapshots.
    pub async fn compact_region(
        &self,
        session: &SessionId,
        start: usize,
        end: usize,
        expected_sources: Vec<rness_protocol::events::EventId>,
        policy: crate::turn::compaction::Policy,
    ) -> Result<bool, ServiceError> {
        self.compact_region_with_permit(
            session,
            start,
            end,
            expected_sources,
            policy,
            None,
            CancellationToken::new(),
        )
        .await
    }

    pub async fn compact_region_with_permit(
        &self,
        session: &SessionId,
        start: usize,
        end: usize,
        expected_sources: Vec<rness_protocol::events::EventId>,
        policy: crate::turn::compaction::Policy,
        permit: Option<CommandPermit>,
        cancel: CancellationToken,
    ) -> Result<bool, ServiceError> {
        policy.validate().map_err(ServiceError::InvalidConfig)?;
        let live = self.live(session);
        let reservation = match permit {
            Some(permit) => {
                let reservation = permit.0.upgrade().ok_or(ServiceError::Busy)?;
                if !Arc::ptr_eq(&reservation.live, &live) {
                    return Err(ServiceError::Busy);
                }
                Some(reservation)
            }
            None => None,
        };
        let _activity = if reservation.is_none() {
            Some(
                self.lifecycle
                    .clone()
                    .try_read_owned()
                    .map_err(|_| ServiceError::Busy)?,
            )
        } else {
            None
        };
        let _operation = if reservation.is_none() {
            Some(
                live.operation
                    .clone()
                    .try_lock_owned()
                    .map_err(|_| ServiceError::Busy)?,
            )
        } else {
            None
        };
        if cancel.is_cancelled() {
            return Err(ServiceError::InvalidConfig("command cancelled".into()));
        }
        if self.phase(session) != Phase::Idle {
            return Err(ServiceError::Busy);
        }
        let replayed = replay(&self.store, session)?;
        if replayed.context.sources != expected_sources {
            return Err(ServiceError::InvalidConfig(
                "stale compaction region".into(),
            ));
        }
        let provider = self.compaction_provider(&replayed.context.config, &policy)?;
        let mut log = self.store.open(session)?;
        crate::turn::compaction::recover(&mut log)?;
        let progress_session = session.clone();
        let progress_bus = Arc::clone(&self.bus);
        let on_compaction = move |progress: crate::turn::compaction::CompactionProgress| {
            progress_bus.emit::<FrameEv>(&progress.frame(progress_session.clone()));
        };
        let changed = crate::turn::compaction::reduce_region_with_progress(
            &self.store,
            &mut log,
            provider.as_ref(),
            "",
            &[],
            &policy,
            false,
            Some(start..end),
            &cancel,
            &on_compaction,
        )
        .await
        .map_err(|e| ServiceError::Summarizer(e.to_string()));
        self.bus.emit::<FrameEv>(&Frame::CompactionFinished {
            session: session.clone(),
            changed: matches!(changed, Ok(true)),
        });
        let changed = changed?;
        if changed {
            self.bus.emit::<FrameEv>(&Frame::HistoryChanged {
                session: session.clone(),
            });
        }
        Ok(changed)
    }

    /// Deterministically prune oversized tool results (dsh
    /// tool-result-pruner): every model-visible tool result longer than
    /// `threshold` chars — except those in the last `keep_turns` turns —
    /// is replaced by head + marker + tail via a `compaction/prune`
    /// event. No model call, instant, append-only (the original stays in
    /// the log, shadowed). Idle-only. Returns how many results were
    /// pruned (0 is fine — pruning is opportunistic, unlike compact).
    pub fn prune_tool_results(
        &self,
        session: &SessionId,
        opts: PruneOptions,
    ) -> Result<usize, ServiceError> {
        let _activity = self
            .lifecycle
            .clone()
            .try_read_owned()
            .map_err(|_| ServiceError::Busy)?;
        let live = self.live(session);
        let _operation = live
            .operation
            .clone()
            .try_lock_owned()
            .map_err(|_| ServiceError::Busy)?;
        if self.phase(session) != Phase::Idle {
            return Err(ServiceError::Busy);
        }
        let replayed = replay(&self.store, session)?;
        let history = &replayed.history;

        // Protected tail: events at/after the TurnStarted of the
        // keep_turns-th turn from the end (same cut rule as compact).
        let starts: Vec<usize> = history
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.event, SessionEvent::TurnStarted { .. }))
            .map(|(i, _)| i)
            .collect();
        let cut = if opts.keep_turns == 0 {
            history.len()
        } else if starts.len() <= opts.keep_turns {
            0
        } else {
            starts[starts.len() - opts.keep_turns]
        };

        // Candidates: cited tool results before the cut over threshold.
        // `cited` skips results already shadowed by a checkpoint; the
        // projection replays PRUNED results under the prune event's id,
        // so re-pruning an already-pruned result never triggers (its
        // replayed output is already short).
        let cited: std::collections::HashSet<&str> = replayed
            .context
            .sources
            .iter()
            .map(|s| s.as_str())
            .collect();
        let mut pruned = 0usize;
        let mut log: Option<crate::session::log::SessionLog> = None;
        for env in &history[..cut] {
            let SessionEvent::ToolResult(r) = &env.event else {
                continue;
            };
            if !cited.contains(env.id.as_str()) {
                continue;
            }
            // Do not reorder mixed text/image output using the text-only pruner.
            if r.content.iter().any(|p| {
                matches!(
                    p,
                    rness_protocol::events::ToolResultContentPart::Image { .. }
                )
            }) {
                continue;
            }
            let chars = r.output.chars().count();
            if chars <= opts.threshold_chars {
                continue;
            }
            let head: String = r.output.chars().take(opts.head_chars).collect();
            let tail_start = chars.saturating_sub(opts.tail_chars).max(opts.head_chars);
            let tail: String = r.output.chars().skip(tail_start).collect();
            let removed = tail_start - opts.head_chars.min(tail_start);
            let mut result = r.clone();
            result.output = format!(
                "{head}\n\n[... tool result middle pruned: {removed} chars removed ...]\n\n{tail}"
            );
            result.content.clear();
            let log = match &mut log {
                Some(l) => l,
                None => log.insert(self.store.open(session)?),
            };
            log.append(&SessionEvent::Prune(Prune {
                replaces: env.id.clone(),
                result,
            }))?;
            pruned += 1;
        }
        drop(log);

        if pruned > 0 {
            self.bus.emit::<FrameEv>(&Frame::HistoryChanged {
                session: session.clone(),
            });
        }
        Ok(pruned)
    }

    /// Wait for the current burst to finish (test/CLI convenience; event
    /// consumers subscribe to `session/idle` instead).
    pub async fn join(&self, session: &SessionId) {
        let handle = self.live(session).handle.lock().unwrap().take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// The session's effective request controls (latest `request/config`
    /// event; default when none was ever set).
    pub fn config(&self, session: &SessionId) -> Result<CallConfig, ServiceError> {
        Ok(self
            .store
            .history(session)?
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                SessionEvent::RequestConfig(config) => Some(config.clone()),
                _ => None,
            })
            .unwrap_or_default())
    }

    pub fn model_capabilities(
        &self,
        selection: &rness_protocol::events::ModelSelection,
    ) -> Option<&crate::config::ModelCapabilities> {
        self.models.capabilities(selection)
    }

    pub fn model_names(&self) -> Vec<String> {
        self.models.model_names()
    }

    pub fn profile_names(&self) -> Vec<String> {
        self.models.profile_names()
    }

    pub fn profile_config(
        &self,
        name: &str,
        provider: Option<&str>,
    ) -> Result<CallConfig, ServiceError> {
        self.models
            .resolve_profile_for(name, provider)
            .map_err(ServiceError::InvalidConfig)
    }

    fn effective_sandbox(&self, config: &CallConfig) -> crate::sandbox::SandboxMode {
        // Absence in durable config is the compatibility policy, not the
        // current startup default (which applies only to creation/role choice).
        config
            .sandbox
            .unwrap_or(crate::sandbox::SandboxMode::DangerFullAccess)
    }

    fn validate_sandbox_update(
        &self,
        previous: &CallConfig,
        next: &CallConfig,
    ) -> Result<(), ServiceError> {
        if self.effective_sandbox(next) > self.effective_sandbox(previous) {
            return Err(ServiceError::InvalidConfig(
                "session sandbox cannot be broadened after creation".into(),
            ));
        }
        Ok(())
    }

    /// Commit new request controls (dsh request/header model): a durable
    /// `request/config` event applied to every later model request.
    /// No-op when equal to the effective config — only real changes are
    /// logged (dsh: no silent per-call drift, no redundant snapshots).
    pub fn set_config(&self, session: &SessionId, config: CallConfig) -> Result<(), ServiceError> {
        self.set_config_with_permit(session, config, None)
    }

    pub fn set_config_with_permit(
        &self,
        session: &SessionId,
        config: CallConfig,
        permit: Option<CommandPermit>,
    ) -> Result<(), ServiceError> {
        let live = self.live(session);
        let reservation = match permit {
            Some(permit) => {
                let reservation = permit.0.upgrade().ok_or(ServiceError::Busy)?;
                if !Arc::ptr_eq(&reservation.live, &live) {
                    return Err(ServiceError::Busy);
                }
                Some(reservation)
            }
            None => None,
        };
        let _activity = if reservation.is_none() {
            Some(
                self.lifecycle
                    .clone()
                    .try_read_owned()
                    .map_err(|_| ServiceError::Busy)?,
            )
        } else {
            None
        };
        let _operation = if reservation.is_none() {
            Some(
                live.operation
                    .clone()
                    .try_lock_owned()
                    .map_err(|_| ServiceError::Busy)?,
            )
        } else {
            None
        };
        let inbox = live.inbox.lock().unwrap();
        if inbox.phase() != Phase::Idle {
            return Err(ServiceError::Busy);
        }
        let mut config = config;
        let previous = self.config(session)?;
        // Request options are replaced, but omitting sandbox is not permission
        // to erase durable policy (including when changing model profiles).
        if config.sandbox.is_none() {
            config.sandbox = previous.sandbox;
        }
        if let Some(ceiling) = previous.tool_ceiling.clone() {
            config.tool_ceiling = Some(match config.tool_ceiling {
                Some(requested) => ceiling
                    .into_iter()
                    .filter(|name| requested.contains(name))
                    .collect(),
                None => ceiling,
            });
        }
        Self::validate_config(&config)?;
        self.validate_sandbox_update(&previous, &config)?;
        self.require_workspace_for_sandbox(
            &self.store.workspace(session)?,
            self.effective_sandbox(&config),
        )?;
        self.models
            .validate(&config)
            .map_err(ServiceError::InvalidConfig)?;
        self.provider_for(&config)?;
        if self.config(session)? == config {
            return Ok(());
        }
        let mut log = self.store.open(session)?;
        log.append(&SessionEvent::RequestConfig(config))?;
        drop(log);
        drop(inbox);
        self.bus.emit::<FrameEv>(&Frame::HistoryChanged {
            session: session.clone(),
        });
        Ok(())
    }
}

/// Options for [`SessionService::prune_tool_results`]. No defaults —
/// callers state their budget (dsh reference: threshold 8192, head 4096,
/// tail 1024, keep 2).
#[derive(Debug, Clone, Copy)]
pub struct PruneOptions {
    /// Results longer than this many chars get pruned.
    pub threshold_chars: usize,
    /// Chars kept from the start of the output.
    pub head_chars: usize,
    /// Chars kept from the end.
    pub tail_chars: usize,
    /// Trailing turns never pruned (same stability rule as compact).
    pub keep_turns: usize,
}

/// Outcome of [`SessionService::compact`].
#[derive(Debug, Clone)]
pub struct CompactReport {
    /// How many events the checkpoint shadows.
    pub shadowed: usize,
    pub summary: String,
}

/// Prompt used for the summarization request (dsh compaction-basic's
/// summarizer, condensed).
const SUMMARIZE_SYSTEM: &str = "You are a conversation compactor. Summarize the \
conversation so far into a dense briefing a coding agent can resume from: \
the user's goals and constraints, decisions made, work completed (files \
touched, commands run, results), current state, and what remains. Keep \
exact identifiers (paths, names, ids) verbatim. Output only the summary.";

/// One extra model request that produces the checkpoint text: the folding
/// region is replayed as-is, plus a final user instruction to summarize.
async fn summarize(
    provider: &dyn Provider,
    mut ctx: crate::session::projection::ModelContext,
    policy: Option<&crate::turn::compaction::Policy>,
) -> Result<String, ServiceError> {
    ctx.turns.push(crate::session::projection::ModelTurn::User {
        content: vec![ContentPart::Text {
            text: policy
                .map_or(
                    "Summarize the conversation above now, per the system instructions.",
                    |p| p.prompt.as_str(),
                )
                .to_string(),
        }],
    });
    let request = crate::turn::provider::StepRequest {
        context: &ctx,
        system: policy.map_or(SUMMARIZE_SYSTEM, |p| p.system_prompt.as_str()),
        tools: &[],
        on_delta: None,
    };
    let cancel = CancellationToken::new();
    match provider.summarize_step(request, &cancel).await {
        crate::turn::provider::StepOutcome::Committed(msg) => {
            let text: String = msg
                .content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                return Err(ServiceError::Summarizer("empty summary".into()));
            }
            Ok(text)
        }
        crate::turn::provider::StepOutcome::Cancelled { .. } => {
            Err(ServiceError::Summarizer("cancelled".into()))
        }
        crate::turn::provider::StepOutcome::Failed { error, .. } => {
            Err(ServiceError::Summarizer(error.message))
        }
    }
}

/// One burst: the initial turn plus any followups queued while running.
/// Owns the writer log for its whole lifetime.
#[allow(clippy::too_many_arguments)]
async fn burst(
    store: Arc<SessionStore>,
    live: Arc<Live>,
    bus: Arc<EventBus>,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: TurnConfig,
    log: crate::session::log::SessionLog,
    token: CancellationToken,
    turns_so_far: u32,
    loop_hooks: Option<Arc<dyn crate::turn::hooks::LoopHooks>>,
) {
    let session = log.session().clone();
    let mut log = Some(log);
    let mut turn_no = turns_so_far;

    // Fire session_start once per burst (i.e. when the session transitions
    // from idle to running).
    {
        let delegation = store.delegation(&session).ok().flatten();
        let workspace = store.workspace(&session).ok().flatten();
        let source = if turns_so_far == 0 { "startup" } else { "resume" };
        bus.emit::<SessionStartEv>(&SessionStartNotice {
            session: session.clone(),
            workspace,
            delegation,
            source: source.into(),
        });
    }

    // Collect durable hook audit events; drained into the log at step
    // boundaries so the single-writer invariant is preserved.
    let (audit_tx, audit_rx) = std::sync::mpsc::channel::<rness_protocol::events::SessionEvent>();
    let audit_session = session.clone();
    let _audit_sub = bus.on::<HookAuditEv>(move |notice| {
        if notice.session != audit_session.to_string() {
            return; // Ignore events from other sessions.
        }
        let event = match &notice.event {
            HookAuditEvent::Invoked(h) => {
                rness_protocol::events::SessionEvent::HookInvoked(h.clone())
            }
            HookAuditEvent::Result(h) => {
                rness_protocol::events::SessionEvent::HookResult(h.clone())
            }
        };
        let _ = audit_tx.send(event);
    });

    loop {
        // Flush any hook audit events collected since the last drain.
        if let Some(log_ref) = log.as_mut() {
            while let Ok(event) = audit_rx.try_recv() {
                if let Err(e) = log_ref.append(&event) {
                    tracing::warn!(session = %session, "hook audit append failed: {e}");
                }
            }
        }

        turn_no += 1;
        bus.emit::<TurnStartedEv>(&TurnNotice {
            session: session.clone(),
            turn: turn_no,
        });

        let steer_live = Arc::clone(&live);
        let mut steers = move || steer_live.inbox.lock().unwrap().drain_steers();
        let frame_bus = Arc::clone(&bus);
        let frames = move |frame: rness_protocol::frames::Frame| {
            frame_bus.emit::<FrameEv>(&frame);
        };
        let result = run_turn(
            &store,
            log.as_mut().expect("burst owns the log"),
            provider.as_ref(),
            &tools,
            &config,
            &token,
            &mut steers,
            turn_no,
            &frames,
            loop_hooks.as_deref(),
        )
        .await;

        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(session = %session, turn = turn_no, error = %e, "turn failed");
                TurnOutcome::Failed
            }
        };
        bus.emit::<TurnEndedEv>(&TurnEndNotice {
            session: session.clone(),
            turn: turn_no,
            outcome,
        });

        // Continuation decision under the inbox lock: a followup queued
        // at this exact moment is either popped here or submitted after
        // the phase flips to Idle (and starts its own burst).
        let mut inbox = live.inbox.lock().unwrap();
        let next = if outcome == TurnOutcome::Completed && !token.is_cancelled() {
            inbox.pop_followup()
        } else {
            None
        };
        match next {
            Some(pending) => {
                drop(inbox);
                let append =
                    log.as_mut()
                        .expect("burst owns the log")
                        .append(&SessionEvent::UserMessage(UserMessage {
                            intent: UserIntent::Followup,
                            content: pending.content,
                            source: pending.source,
                        }));
                if let Err(e) = append {
                    tracing::error!(session = %session, error = %e, "followup commit failed");
                    let mut inbox = live.inbox.lock().unwrap();
                    drop(log.take()); // release the writer lock first
                    inbox.set_phase(Phase::Idle);
                    drop(inbox);
                    bus.emit::<FrameEv>(&rness_protocol::frames::Frame::TurnIdle {
                        session: session.clone(),
                    });
                    bus.emit::<SessionIdleEv>(&session);
                    return;
                }
            }
            None => {
                // Late steers: accepted during the final step, after the
                // last boundary drain — without this they'd park in
                // memory invisibly. The turn is over, so idle semantics
                // apply now: injects append silently; a steer degrades
                // to a followup and gets its own turn.
                let leftovers = if outcome == TurnOutcome::Completed && !token.is_cancelled() {
                    inbox.drain_steers()
                } else {
                    // Cancelled/failed: queued input stays parked, same
                    // contract as followups.
                    Vec::new()
                };
                if !leftovers.is_empty() {
                    drop(inbox);
                    let mut start_turn = false;
                    let mut append_failed = false;
                    for pending in leftovers {
                        let intent = match pending.intent {
                            UserIntent::Steer => {
                                start_turn = true;
                                UserIntent::Followup
                            }
                            other => other,
                        };
                        let append = log.as_mut().expect("burst owns the log").append(
                            &SessionEvent::UserMessage(UserMessage {
                                intent,
                                content: pending.content,
                                source: pending.source,
                            }),
                        );
                        if let Err(e) = append {
                            tracing::error!(session = %session, error = %e, "late steer commit failed");
                            append_failed = true;
                            break;
                        }
                    }
                    if start_turn && !append_failed {
                        continue; // the degraded followup runs as its own turn
                    }
                    // Final flush of hook audit events before going idle.
                    if let Some(log_ref) = log.as_mut() {
                        while let Ok(event) = audit_rx.try_recv() {
                            let _ = log_ref.append(&event);
                        }
                    }
                    let mut inbox = live.inbox.lock().unwrap();
                    drop(log.take());
                    inbox.set_phase(Phase::Idle);
                    drop(inbox);
                    bus.emit::<FrameEv>(&rness_protocol::frames::Frame::TurnIdle {
                        session: session.clone(),
                    });
                    bus.emit::<SessionIdleEv>(&session);
                    return;
                }
                // Final flush of hook audit events before going idle.
                if let Some(log_ref) = log.as_mut() {
                    while let Ok(event) = audit_rx.try_recv() {
                        let _ = log_ref.append(&event);
                    }
                }
                drop(log.take()); // release the writer lock before going idle
                inbox.set_phase(Phase::Idle);
                drop(inbox);
                bus.emit::<FrameEv>(&rness_protocol::frames::Frame::TurnIdle {
                    session: session.clone(),
                });
                bus.emit::<SessionIdleEv>(&session);
                return;
            }
        }
    }
}

// -- kernel plugin ---------------------------------------------------------

/// Mounts the session service on a kernel: provides `"sessions"`.
/// The first seam where the kernel (M0) and the engine (M1) meet.
pub struct SessionsPlugin {
    pub root: std::path::PathBuf,
    pub provider: Arc<dyn Provider>,
    /// Optional composition-root resolver; omitted preserves the direct
    /// provider constructor used by engine tests and embeddings.
    pub resolver: Option<Arc<ProviderResolver>>,
    /// Explicit config committed into every new root/spawn session.
    pub creation_seed: CallConfig,
    pub agents: std::collections::BTreeMap<String, crate::config::AgentDefinition>,
    pub sandbox: crate::sandbox::SandboxConfig,
    pub models: crate::config::ModelRegistry,
    pub tools: Arc<ToolRegistry>,
    pub config: TurnConfig,
}

impl Plugin for SessionsPlugin {
    fn name(&self) -> &str {
        "sessions"
    }

    fn apply(&self, ctx: &mut Context<'_>) -> Result<(), String> {
        let mut service = SessionService::new(
            SessionStore::new(self.root.clone()),
            Arc::clone(&self.provider),
            Arc::clone(&self.tools),
            self.config.clone(),
            Arc::clone(ctx.bus()),
        );
        if let Some(resolver) = &self.resolver {
            service =
                service.with_provider_resolver(self.creation_seed.clone(), Arc::clone(resolver));
        }
        let service = Arc::new(
            service
                .with_agents(self.agents.clone(), self.models.clone())
                .with_sandbox(self.sandbox.clone()),
        );
        ctx.provide("sessions", service).map_err(|e| e.to_string())
    }
}
