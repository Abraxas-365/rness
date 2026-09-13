//! The plugin host: an actor thread that owns the single Lua VM.
//!
//! mlua's Lua is not Sync and registry keys are VM-bound, so the VM
//! lives on one dedicated thread; everyone else talks to it through a
//! cloneable [`LuaHost`] handle over a command channel. This is the v2
//! answer to v1's dual-VM/replay design: one VM, message passing, no
//! neutralized APIs.

#[async_trait::async_trait]
impl rness_kernel::presentation::TextProvider for LuaHost {
    async fn status(&self, context: serde_json::Value) -> Option<serde_json::Value> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::StatusView { context, reply }).ok()?;
        rx.await.ok().flatten()
    }

    async fn text(&self) -> Option<String> {
        self.statusline().await
    }
}

#[async_trait::async_trait]
impl rness_tools::web::WebHooks for LuaHost {
    async fn transform(&self, operation: &str, phase: &str, value: serde_json::Value, context: serde_json::Value, cancel: &tokio_util::sync::CancellationToken) -> Result<serde_json::Value, String> {
        let token = cancel.child_token();
        let _guard = token.clone().drop_guard();
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::WebTransform { operation: operation.into(), phase: phase.into(), value, context, cancel: token.clone(), reply }).map_err(|_| "lua vm gone")?;
        tokio::select! {
            biased;
            _ = token.cancelled() => Err("web hook cancelled".into()),
            result = tokio::time::timeout(std::time::Duration::from_secs(30), rx) => result.map_err(|_| "web hook timed out")?.map_err(|_| "lua vm gone")?,
        }
    }
}

impl rness_kernel::presentation::HookSink for LuaHost {
    fn fire_hook(&self, event: &str, payload: serde_json::Value) { LuaHost::fire_hook(self, event, payload); }
}

#[async_trait::async_trait]
impl rness_kernel::presentation::Applications for LuaHost {
    async fn app_specs(&self) -> Vec<LuaAppSpec> { LuaHost::app_specs(self).await }
    async fn app_view(&self, name: &str, ctx: serde_json::Value) -> Result<Vec<String>, String> {
        LuaHost::app_view(self, name, ctx).await
    }
    async fn app_key(&self, name: &str, key: &str, ctx: serde_json::Value) -> Result<AppKeyOutcome, String> {
        LuaHost::app_key(self, name, key, ctx).await
    }
}

#[async_trait::async_trait]
impl rness_engine::presentation::ToolCards for LuaHost {
    async fn tool_card(&self, name: &str, args: serde_json::Value, output: &str, is_error: bool)
        -> Option<Vec<crate::runtime::StyledLine>> {
        LuaHost::tool_card(self, name, args, output, is_error).await
    }

    async fn tool_card_presented(&self, name: &str, args: serde_json::Value, output: &str, is_error: bool, presentation: Option<serde_json::Value>)
        -> Option<Vec<crate::runtime::StyledLine>> {
        LuaHost::tool_card_presented(self, name, args, output, is_error, presentation).await
    }
}

use std::sync::mpsc;

use crate::runtime::{AppKeyOutcome, LuaAppSpec, LuaRuntime, LuaToolSpec};

/// The engine services `rness.session` bridges to. Kept by the actor so
/// every fresh VM (hot reload) gets the same injection.
#[derive(Clone)]
pub struct SessionBinding {
    pub sessions: std::sync::Arc<rness_engine::service::SessionService>,
    pub subagents: std::sync::Arc<rness_engine::subagent::SubagentRuntime>,
    pub registry: std::sync::Arc<rness_engine::tools::ToolRegistry>,
    pub mcp: crate::api::mcp::McpConnections,
    pub rt: tokio::runtime::Handle,
    /// Active "<provider>/<model>" selection, surfaced as `rness.model`.
    pub model: String,
}

/// Remaining presentation declarations after coordinated teardown.
pub struct UnloadSnapshot {
    pub apps: Vec<LuaAppSpec>,
    pub keymap_binds: Vec<(String, Option<String>)>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReloadError {
    #[error("extension runtime is busy")]
    Busy,
    #[error("{0}")]
    Failed(String),
}

enum Cmd {
    WebTransform { operation: String, phase: String, value: serde_json::Value, context: serde_json::Value, cancel: tokio_util::sync::CancellationToken, reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>> },
    ValidateBindings { reply: tokio::sync::oneshot::Sender<Result<(), String>> },
    ActionSpecs { reply: tokio::sync::oneshot::Sender<Vec<crate::runtime::LuaActionSpec>> },
    BindingSpecs { reply: tokio::sync::oneshot::Sender<Vec<crate::runtime::LuaBindingSpec>> },
    Action { guard: Box<dyn FnOnce() -> bool + Send>, generation: u64, name: String, scope: String, context: serde_json::Value, reply: tokio::sync::oneshot::Sender<Result<Vec<crate::runtime::UiActionOperation>, String>> },
    Complete { name: String, context: serde_json::Value, cancel: tokio_util::sync::CancellationToken, reply: mpsc::Sender<Result<Vec<String>, String>> },
    Command { name: String, context: serde_json::Value, permit: rness_engine::service::CommandPermit, cancel: tokio_util::sync::CancellationToken, reply: mpsc::Sender<Result<rness_engine::interaction::CommandResult, String>> },
    ResumeCommand { id: u64, result: Result<bool, String> },
    PluginNames { reply: tokio::sync::oneshot::Sender<Vec<String>> },
    CoordinatedUnload {
        name: String,
        installed: crate::api::tools::InstalledTools,
        apply_ui: Box<dyn FnOnce(UnloadSnapshot) + Send>,
        reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    Load {
        name: String,
        source: String,
        dependencies: Vec<String>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Unload {
        name: String,
        reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    ToolSpecs {
        reply: tokio::sync::oneshot::Sender<Vec<LuaToolSpec>>,
    },
    CallTool {
        context: serde_json::Value,
        name: String,
        args: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<(String, Option<serde_json::Value>), String>>,
    },
    FireHook {
        event: String,
        payload: serde_json::Value,
    },
    StatusView {
        context: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Option<serde_json::Value>>,
    },
    Statusline {
        reply: tokio::sync::oneshot::Sender<Option<String>>,
    },
    ToolCard {
        name: String,
        args: serde_json::Value,
        output: String,
        is_error: bool,
        presentation: Option<serde_json::Value>,
        reply: tokio::sync::oneshot::Sender<Option<Vec<crate::runtime::StyledLine>>>,
    },
    AppSpecs {
        reply: tokio::sync::oneshot::Sender<Vec<LuaAppSpec>>,
    },
    KeymapBinds {
        reply: tokio::sync::oneshot::Sender<Vec<(String, Option<String>)>>,
    },
    AppView {
        name: String,
        ctx: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<Vec<String>, String>>,
    },
    AppKey {
        name: String,
        key: String,
        ctx: serde_json::Value,
        reply: tokio::sync::oneshot::Sender<Result<AppKeyOutcome, String>>,
    },
    /// Surface engine services as `rness.session` (post-mount; the VM
    /// boots before the kernel). The actor keeps the binding and
    /// re-installs it into every reloaded VM.
    InstallSession {
        binding: SessionBinding,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Inject the shared questions broker so plugins can dynamically
    /// enable/disable the AskUser tool via `rness.questions.enable()`.
    InstallQuestions {
        questions: std::sync::Arc<rness_engine::questions::Questions>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Reload runtime declarations in the retained VM. Failures preserve the
    /// previous registrations; startup is never evaluated again.
    Reload {
        sources: Vec<crate::loader::PluginSource>,
        reconcile: Option<Box<dyn FnOnce(Vec<LuaToolSpec>) + Send>>,
        reply: tokio::sync::oneshot::Sender<Result<Vec<(String, String)>, ReloadError>>,
    },
}

/// Cloneable handle to the VM actor. All methods are async (they await
/// the actor's reply) except [`LuaHost::fire_hook`], which is
/// fire-and-forget so event fan-out never blocks on Lua.
#[derive(Clone)]
pub struct LuaHost {
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx: std::sync::Arc<mpsc::Sender<Cmd>>,
}

struct LuaCommand {
    name: String,
    usage: String,
    allow_busy: bool,
    arguments: Vec<(String, String)>,
    description: String,
    tx: std::sync::Weak<mpsc::Sender<Cmd>>,
}

impl rness_engine::interaction::Command for LuaCommand {
    fn name(&self) -> &str { &self.name }
    fn usage(&self) -> &str { &self.usage }
    fn allow_busy(&self) -> bool { self.allow_busy }
    fn arguments(&self) -> Vec<(String, String)> { self.arguments.clone() }
    fn description(&self) -> &str { &self.description }
    fn complete(&self, service: &rness_engine::service::SessionService, input: rness_engine::interaction::CommandInvocation<'_>) -> Result<Vec<String>, rness_engine::service::ServiceError> {
        use rness_engine::service::ServiceError;
        if std::thread::current().name() == Some("lua-vm") { return Err(ServiceError::InvalidConfig("recursive Lua completion".into())); }
        let workspace = service.store().workspace(input.session)?;
        let (reply, receive) = mpsc::channel();
        self.tx.upgrade().ok_or_else(|| ServiceError::InvalidConfig("Lua host unavailable".into()))?.send(Cmd::Complete {
            name: self.name.clone(), context: serde_json::json!({"session":input.session,"workspace":workspace,"raw_input":input.raw_input}), cancel: input.cancel, reply,
        }).map_err(|_| ServiceError::InvalidConfig("Lua host unavailable".into()))?;
        receive.recv().map_err(|_| ServiceError::InvalidConfig("Lua host unavailable".into()))?.map_err(ServiceError::InvalidConfig)
    }
    fn execute(&self, service: &rness_engine::service::SessionService, input: rness_engine::interaction::CommandInvocation<'_>) -> Result<rness_engine::interaction::CommandResult, rness_engine::service::ServiceError> {
        use rness_engine::service::ServiceError;
        if std::thread::current().name() == Some("lua-vm") {
            return Err(ServiceError::InvalidConfig("Lua commands cannot recursively invoke Lua commands".into()));
        }
        let workspace = service.store().workspace(input.session)?;
        let (reply, receive) = mpsc::channel();
        self.tx.upgrade().ok_or_else(|| ServiceError::InvalidConfig("Lua host unavailable".into()))?.send(Cmd::Command { name: self.name.clone(), context: serde_json::json!({"session":input.session,"raw_input":input.raw_input,"workspace":workspace}), permit: input.permit, cancel: input.cancel, reply })
            .map_err(|_| ServiceError::InvalidConfig("Lua host unavailable".into()))?;
        receive.recv().map_err(|_| ServiceError::InvalidConfig("Lua host unavailable".into()))?
            .map_err(ServiceError::InvalidConfig)
    }
}

fn sync_commands(rt: &LuaRuntime, binding: &SessionBinding, tx: &std::sync::Weak<mpsc::Sender<Cmd>>, installed: &mut Vec<std::sync::Arc<dyn rness_engine::interaction::Command>>) -> Result<(), String> {
    let specs = rt.command_specs();
    installed.retain(|command| {
        if specs.iter().any(|(name, _)| name == command.name()) { return true; }
        binding.sessions.commands().unregister_if_current(command);
        false
    });
    for (name, description) in specs {
        if installed.iter().any(|c| c.name() == name) { continue; }
        let (usage, arguments, allow_busy) = rt.command_metadata(&name);
        let command: std::sync::Arc<dyn rness_engine::interaction::Command> = std::sync::Arc::new(LuaCommand { name, usage, arguments, allow_busy, description, tx: tx.clone() });
        binding.sessions.commands().register(command.clone())?;
        installed.push(command);
    }
    Ok(())
}

struct PendingCommand {
    thread: crate::runtime::CommandThread,
    permit: rness_engine::service::CommandPermit,
    cancel: tokio_util::sync::CancellationToken,
    // Keeping this unanswered keeps PreparedCommand (and its engine locks) alive.
    reply: mpsc::Sender<Result<rness_engine::interaction::CommandResult, String>>,
}

struct CompactionCompletion {
    tx: std::sync::Arc<mpsc::Sender<Cmd>>,
    id: u64,
    result: Option<Result<bool, String>>,
}

impl Drop for CompactionCompletion {
    fn drop(&mut self) {
        let result = self.result.take().unwrap_or_else(|| Err("compaction task stopped".into()));
        let _ = self.tx.send(Cmd::ResumeCommand { id: self.id, result });
    }
}

fn command_step(
    id: u64, command: PendingCommand, step: Result<crate::runtime::CommandStep, String>,
    pending: &mut std::collections::HashMap<u64, PendingCommand>,
    runtime: Option<&tokio::runtime::Handle>, tx: &std::sync::Weak<mpsc::Sender<Cmd>>,
) {
    match step {
        Ok(crate::runtime::CommandStep::Pending(future)) => {
            let Some((runtime, tx)) = runtime.zip(tx.upgrade()) else {
                let _ = command.reply.send(Err("compaction runtime unavailable".into()));
                return;
            };
            pending.insert(id, command);
            // A dropped/panicking runtime task must also release the parked command.
            let mut completion = CompactionCompletion { tx, id, result: None };
            runtime.spawn(async move {
                completion.result = Some(future.await);
                drop(completion);
            });
        }
        result => {
            let result = result.map(|step| match step {
                crate::runtime::CommandStep::Complete(result) => result,
                crate::runtime::CommandStep::Pending(_) => unreachable!(),
            });
            let _ = command.reply.send(result);
        }
    }
}

impl LuaHost {
    /// Spawn the VM actor. Fails fast if the VM can't be built.
    pub fn spawn() -> Result<Self, String> {
        Self::spawn_with_config(crate::api::config::StartupConfig::default())
    }

    pub fn spawn_with_config(config: crate::api::config::StartupConfig) -> Result<Self, String> {
        Self::spawn_inner(config, None).map(|(host, _)| host)
    }

    pub fn spawn_from_init(path: std::path::PathBuf) -> Result<(Self, crate::api::config::StartupConfig), String> {
        Self::spawn_inner(crate::api::config::StartupConfig::default(), Some(path))
    }

    fn spawn_inner(mut config: crate::api::config::StartupConfig, init: Option<std::path::PathBuf>) -> Result<(Self, crate::api::config::StartupConfig), String> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let tx = std::sync::Arc::new(tx);
        let command_tx = std::sync::Arc::downgrade(&tx);
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let actor_generation = generation.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("lua-vm".into())
            .spawn(move || {
                let mut rt = match LuaRuntime::new() {
                    Ok(mut rt) => {
                        if let Some(path) = &init {
                            match rt.startup(path) {
                                Ok(startup) => config = startup,
                                Err(error) => { let _ = ready_tx.send(Err(error)); return; }
                            }
                        }
                        if let Err(e) = rt.install_config(&config) {
                            let _ = ready_tx.send(Err(e.to_string()));
                            return;
                        }
                        let _ = ready_tx.send(Ok(config.clone()));
                        rt
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let mut disabled_plugins = std::collections::HashSet::new();
                let mut installed_commands = Vec::new();
                let mut session_binding: Option<SessionBinding> = None;
                let mut questions_ref: Option<std::sync::Arc<rness_engine::questions::Questions>> = None;
                let mut pending_commands = std::collections::HashMap::new();
                let mut next_command_id = 0u64;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::ValidateBindings { reply } => { let _ = reply.send(rt.validate_bindings(false)); }
                        Cmd::Complete { name, context, cancel, reply } => {
                            let token = cancel.clone();
                            rt.lua().set_app_data(cancel.clone());
                            rt.lua().set_hook(mlua::HookTriggers::new().every_nth_instruction(1000), move |_, _| {
                                if token.is_cancelled() { Err(mlua::Error::runtime("completion cancelled")) } else { Ok(mlua::VmState::Continue) }
                            });
                            let result = if cancel.is_cancelled() { Err("completion cancelled".into()) } else { rt.complete_command(&name, context) };
                            rt.lua().remove_hook();
                            rt.lua().remove_app_data::<tokio_util::sync::CancellationToken>();
                            let _ = reply.send(result);
                        }
                        Cmd::Command { name, context, permit, cancel, reply } => {
                            use mlua::LuaSerdeExt;
                            let thread = match rt.command_thread(&name) {
                                Ok(thread) => thread,
                                Err(error) => { let _ = reply.send(Err(error)); continue; }
                            };
                            let id = next_command_id;
                            next_command_id = next_command_id.checked_add(1).expect("command ID exhausted");
                            let mut command = PendingCommand { thread, permit, cancel, reply };
                            let step = rt.lua().to_value(&context).map_err(|e| e.to_string()).and_then(|args|
                                rt.resume_command(&mut command.thread, args, command.permit.clone(), &command.cancel));
                            command_step(id, command, step, &mut pending_commands,
                                session_binding.as_ref().map(|b| &b.rt), &command_tx);
                        }
                        Cmd::ResumeCommand { id, result } => {
                            let Some(mut command) = pending_commands.remove(&id) else { continue; };
                            let step = match result {
                                Ok(result) => rt.resume_command(&mut command.thread, (true, result), command.permit.clone(), &command.cancel),
                                Err(error) => rt.resume_command(&mut command.thread, (false, error), command.permit.clone(), &command.cancel),
                            };
                            command_step(id, command, step, &mut pending_commands,
                                session_binding.as_ref().map(|b| &b.rt), &command_tx);
                        }
                        Cmd::Load { name, source, dependencies, reply } => {
                            if !pending_commands.is_empty() {
                                let _ = reply.send(Err("extension runtime is busy".into()));
                                continue;
                            }
                            let staged = questions_ref.as_ref().map(|qs| {
                                let staged = std::sync::Arc::new(rness_engine::questions::Questions::default());
                                staged.set_overlay_config(qs.overlay_config());
                                staged.set_owner(qs.owner());
                                staged.set_available(qs.is_available());
                                staged
                            });
                            if let Some(staged) = &staged {
                                if let Err(error) = rt.install_questions(staged.clone()) { let _ = reply.send(Err(error.to_string())); continue; }
                            }
                            let r = rt.load_with_dependencies(&name, &source, &dependencies).map_err(|e| e.to_string()).and_then(|()| {
                                if let Some(binding) = &session_binding {
                                    if let Err(error) = sync_commands(&rt, binding, &command_tx, &mut installed_commands) {
                                        let _ = rt.unload(&name);
                                        let _ = sync_commands(&rt, binding, &command_tx, &mut installed_commands);
                                        return Err(error);
                                    }
                                }
                                Ok(())
                            });
                            if let (Some(qs), Some(staged)) = (&questions_ref, staged) {
                                rt.install_questions(qs.clone()).expect("questions API installation");
                                if r.is_ok() {
                                    qs.set_overlay_config(staged.overlay_config());
                                    qs.set_owner(staged.owner());
                                    qs.set_available(staged.is_available());
                                }
                            }
                            if r.is_ok() { actor_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst); if let Some(binding) = &session_binding { binding.sessions.reference_service().configure(rt.reference_config()); } }
                            let _ = reply.send(r);
                        }
                        Cmd::CoordinatedUnload { name, installed, apply_ui, reply } => {
                            let result = (|| {
                                let binding = session_binding.as_ref().ok_or("coordinated unload requires a mounted host")?;
                                let _maintenance = binding.sessions.try_extension_maintenance().map_err(|e| e.to_string())?;
                                let removed = rt.unload(&name).map_err(|e| e.to_string())?;
                                if removed {
                                    actor_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    disabled_plugins.insert(name.clone());
                                    binding.sessions.reference_service().configure(rt.reference_config());
                                    sync_commands(&rt, binding, &command_tx, &mut installed_commands)?;
                                    let remaining = rt.tool_specs();
                                    for tool in installed {
                                        if !remaining.iter().any(|spec| spec.name == tool.name()) {
                                            binding.registry.unregister_if_current(&tool);
                                        }
                                    }
                                    apply_ui(UnloadSnapshot {
                                        apps: rt.app_specs(),
                                        keymap_binds: rt.keymap_binds(),
                                    });
                                }
                                Ok(removed)
                            })();
                            let _ = reply.send(result);
                        }
                        Cmd::Unload { name, reply } => {
                            let result = if session_binding.is_some() {
                                Err("mounted hosts require coordinated engine/UI teardown; restart rness to disable a plugin".into())
                            } else {
                                rt.unload(&name).map_err(|e| e.to_string()).map(|removed| {
                                    if removed { actor_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst); disabled_plugins.insert(name.clone()); }
                                    removed
                                })
                            };
                            let _ = reply.send(result);
                        }
                        Cmd::PluginNames { reply } => { let _ = reply.send(rt.plugin_names()); }
                        Cmd::ToolSpecs { reply } => {
                            let _ = reply.send(rt.tool_specs());
                        }
                        Cmd::WebTransform { operation, phase, value, context, cancel, reply } => {
                            use mlua::LuaSerdeExt;
                            let token = cancel.clone();
                            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                            rt.lua().set_app_data(cancel.clone());
                            rt.lua().set_hook(mlua::HookTriggers::new().every_nth_instruction(1000), move |_, _| {
                                if token.is_cancelled() || std::time::Instant::now() >= deadline { Err(mlua::Error::runtime("web hook cancelled or timed out")) } else { Ok(mlua::VmState::Continue) }
                            });
                            let result = (|| -> mlua::Result<serde_json::Value> {
                                if cancel.is_cancelled() || reply.is_closed() { return Err(mlua::Error::runtime("web hook cancelled")); }
                                let Some(callback) = rt.web_hook(&operation, &phase)? else { return Ok(value); };
                                let returned: mlua::Value = callback.call((rt.lua().to_value(&value)?, rt.lua().to_value(&context)?))?;
                                if !matches!(returned, mlua::Value::Table(_)) { return Err(mlua::Error::runtime("web hook must return a table")); }
                                rt.lua().from_value(returned)
                            })().map_err(|e| format!("web.{operation}.{phase}: {e}"));
                            rt.lua().remove_hook();
                            rt.lua().remove_app_data::<tokio_util::sync::CancellationToken>();
                            let _ = reply.send(result);
                        }
                        Cmd::CallTool { name, args, context, reply } => {
                            let r = match rt.call_tool_presented(&name, &args, &context) {
                                Ok(r) => r,
                                Err(e) => Err(e.to_string()),
                            };
                            let _ = reply.send(r);
                        }
                        Cmd::FireHook { event, payload } => {
                            for err in rt.fire_hook(&event, &payload) {
                                tracing::warn!(target: "lua", "hook '{event}' failed: {err}");
                            }
                        }
                        Cmd::StatusView { context, reply } => {
                            let _ = reply.send(rt.status_view(context));
                        }
                        Cmd::Statusline { reply } => {
                            let _ = reply.send(rt.statusline());
                        }
                        Cmd::ToolCard { name, args, output, is_error, presentation, reply } => {
                            let _ = reply.send(rt.tool_card_presented(&name, &args, &output, is_error, presentation.as_ref()));
                        }
                        Cmd::BindingSpecs { reply } => {
                            let _ = reply.send(rt.binding_specs());
                        }
                        Cmd::ActionSpecs { reply } => {
                            let _ = reply.send(rt.action_specs());
                        }
                        Cmd::Action { guard, generation, name, scope, context, reply } => {
                            let result = if guard() && generation == actor_generation.load(std::sync::atomic::Ordering::SeqCst) {
                                rt.call_action(&name, &scope, context)
                            } else { Err("plugin action generation expired".into()) };
                            let _ = reply.send(result);
                        }
                        Cmd::AppSpecs { reply } => {
                            let _ = reply.send(rt.app_specs());
                        }
                        Cmd::KeymapBinds { reply } => {
                            let _ = reply.send(rt.keymap_binds());
                        }
                        Cmd::AppView { name, ctx, reply } => {
                            let _ = reply.send(rt.app_view(&name, &ctx).map_err(|e| e.to_string()));
                        }
                        Cmd::AppKey { name, key, ctx, reply } => {
                            let _ =
                                reply.send(rt.app_key(&name, &key, &ctx).map_err(|e| e.to_string()));
                        }
                        Cmd::InstallSession { binding, reply } => {
                            let r = rt
                                .install_session(
                                    std::sync::Arc::clone(&binding.sessions),
                                    std::sync::Arc::clone(&binding.subagents),
                                    std::sync::Arc::clone(&binding.registry),
                                    std::sync::Arc::clone(&binding.mcp),
                                    binding.rt.clone(),
                                    binding.model.clone(),
                                )
                                .map_err(|e| e.to_string());
                            let r = r.and_then(|()| sync_commands(&rt, &binding, &command_tx, &mut installed_commands));
                            if r.is_ok() { binding.sessions.reference_service().configure(rt.reference_config()); }
                            session_binding = Some(binding);
                            let _ = reply.send(r);
                        }
                        Cmd::InstallQuestions { questions, reply } => {
                            questions_ref = Some(questions.clone());
                            let r = rt.install_questions(questions).map_err(|e| e.to_string());
                            let _ = reply.send(r);
                        }
                        Cmd::Reload { mut sources, reconcile, reply } => {
                            // Validate the declared graph before applying session-local unloads.
                            // Failed consumers may never have loaded, so unloading their
                            // prerequisite is valid; keep them skipped on subsequent reloads.
                            if let Err(error) = crate::loader::ordered_sources(&sources) {
                                let _ = reply.send(Err(ReloadError::Failed(error)));
                                continue;
                            }
                            let mut excluded = disabled_plugins.clone();
                            loop {
                                let mut changed = false;
                                for source in &sources {
                                    if !excluded.contains(&source.name) && source.dependencies.iter().any(|dependency| excluded.contains(dependency)) {
                                        tracing::warn!(target: "lua", "skipping plugin '{}' on reload: dependency unloaded", source.name);
                                        excluded.insert(source.name.clone());
                                        changed = true;
                                    }
                                }
                                if !changed { break; }
                            }
                            sources.retain(|source| !excluded.contains(&source.name));
                            let _maintenance = match session_binding.as_ref().map(|b| b.sessions.try_extension_maintenance()).transpose() {
                                Ok(guard) => guard,
                                Err(_) => { let _ = reply.send(Err(ReloadError::Busy)); continue; }
                            };
                            let staged_questions = std::sync::Arc::new(rness_engine::questions::Questions::default());
                            if questions_ref.is_some() {
                                rt.install_questions(staged_questions.clone()).expect("questions API installation");
                            }
                            let r = rt.reload_plugins(&sources, |fresh| {
                                if let Some(binding) = &session_binding {
                                    let replacements = fresh.command_specs().into_iter().map(|(name, description)| {
                                        let (usage, arguments, allow_busy) = fresh.command_metadata(&name);
                                        std::sync::Arc::new(LuaCommand { name, description, usage, arguments, allow_busy, tx: command_tx.clone() }) as std::sync::Arc<dyn rness_engine::interaction::Command>
                                    }).collect::<Vec<_>>();
                                    binding.sessions.commands().replace_owned(&installed_commands, &replacements)?;
                                    installed_commands = replacements;
                                }
                                Ok(())
                            });
                            if let Some(qs) = &questions_ref {
                                rt.install_questions(qs.clone()).expect("questions API installation");
                                if r.is_ok() {
                                    qs.set_overlay_config(staged_questions.overlay_config());
                                    qs.set_owner(staged_questions.owner());
                                    qs.set_available(staged_questions.is_available());
                                }
                            }
                            if r.is_ok() {
                                actor_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                if let Some(binding) = &session_binding { binding.sessions.reference_service().configure(rt.reference_config()); }
                                if let Some(reconcile) = reconcile { reconcile(rt.tool_specs()); }
                            }
                            let _ = reply.send(r.map(|()| Vec::new()).map_err(ReloadError::Failed));
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let config = ready_rx.recv().map_err(|_| "lua vm thread died".to_string())??;
        Ok((Self { tx, generation }, config))
    }

    pub async fn load(&self, name: &str, source: &str) -> Result<(), String> {
        self.load_with_dependencies(name, source, &[]).await
    }

    pub async fn load_with_dependencies(&self, name: &str, source: &str, dependencies: &[String]) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Load { name: name.into(), source: source.into(), dependencies: dependencies.to_vec(), reply })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Unload in an unmounted host. Mounted hosts reject this operation
    /// until engine and frontend teardown can be coordinated.
    pub async fn unload(&self, name: &str) -> Result<bool, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::Unload { name: name.into(), reply }).map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Teardown under an engine maintenance reservation on the VM actor.
    /// `installed` must contain the current handles owned by this host.
    /// `apply_ui` must synchronously invalidate apps/cards/statusline and rebuild
    /// keymaps from the snapshot. It must not block, panic, or call this host.
    /// Once enqueued, teardown completes even if the caller drops its future.
    pub async fn unload_coordinated(
        &self,
        name: &str,
        installed: crate::api::tools::InstalledTools,
        apply_ui: impl FnOnce(UnloadSnapshot) + Send + 'static,
    ) -> Result<bool, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::CoordinatedUnload {
            name: name.into(), installed, apply_ui: Box::new(apply_ui), reply,
        }).map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    pub async fn plugin_names(&self) -> Vec<String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::PluginNames { reply }).is_err() { return Vec::new(); }
        rx.await.unwrap_or_default()
    }

    pub async fn tool_specs(&self) -> Vec<LuaToolSpec> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::ToolSpecs { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, String> {
        self.call_tool_context(name, args, serde_json::json!({})).await
    }

    pub async fn call_tool_context(&self, name: &str, args: serde_json::Value, context: serde_json::Value) -> Result<String, String> {
        self.call_tool_presented(name, args, context).await.map(|(output, _)| output)
    }

    pub async fn call_tool_presented(&self, name: &str, args: serde_json::Value, context: serde_json::Value) -> Result<(String, Option<serde_json::Value>), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::CallTool { name: name.into(), args, context, reply })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Fire-and-forget: never blocks the caller on Lua execution.
    pub fn fire_hook(&self, event: &str, payload: serde_json::Value) {
        let _ = self.tx.send(Cmd::FireHook { event: event.into(), payload });
    }

    pub async fn statusline(&self) -> Option<String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::Statusline { reply }).ok()?;
        rx.await.ok().flatten()
    }

    /// Render a finished tool call through the Lua card renderer.
    /// None = no renderer / declined / VM gone — use the built-in card.
    pub async fn tool_card(
        &self,
        name: &str,
        args: serde_json::Value,
        output: &str,
        is_error: bool,
    ) -> Option<Vec<crate::runtime::StyledLine>> {
        self.tool_card_presented(name, args, output, is_error, None).await
    }

    pub async fn tool_card_presented(
        &self,
        name: &str,
        args: serde_json::Value,
        output: &str,
        is_error: bool,
        presentation: Option<serde_json::Value>,
    ) -> Option<Vec<crate::runtime::StyledLine>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::ToolCard {
                name: name.into(),
                args,
                output: output.into(),
                is_error,
                presentation,
                reply,
            })
            .ok()?;
        rx.await.ok().flatten()
    }

    pub async fn action_specs(&self) -> Vec<crate::runtime::LuaActionSpec> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::ActionSpecs { reply }).is_err() { return Vec::new(); }
        rx.await.unwrap_or_default()
    }

    pub async fn binding_specs(&self) -> Vec<crate::runtime::LuaBindingSpec> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::BindingSpecs { reply }).is_err() { return Vec::new(); }
        rx.await.unwrap_or_default()
    }

    pub async fn validate_bindings(&self) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::ValidateBindings { reply }).map_err(|_| "Lua host stopped".to_owned())?;
        rx.await.map_err(|_| "Lua host stopped".to_owned())?
    }

    pub fn action_generation(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> { self.generation.clone() }

    pub async fn call_action(&self, name: &str, scope: &str, context: serde_json::Value) -> Result<Vec<crate::runtime::UiActionOperation>, String> {
        self.call_action_at(self.generation.load(std::sync::atomic::Ordering::SeqCst), name, scope, context).await
    }

    pub async fn call_action_at(&self, generation: u64, name: &str, scope: &str, context: serde_json::Value) -> Result<Vec<crate::runtime::UiActionOperation>, String> {
        self.call_action_guarded(generation, name, scope, context, || true).await
    }

    pub async fn call_action_guarded(&self, generation: u64, name: &str, scope: &str, context: serde_json::Value, guard: impl FnOnce() -> bool + Send + 'static) -> Result<Vec<crate::runtime::UiActionOperation>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::Action { guard: Box::new(guard), generation, name: name.into(), scope: scope.into(), context, reply })
            .map_err(|_| "Lua host stopped".to_owned())?;
        rx.await.map_err(|_| "Lua host stopped".to_owned())?
    }

    pub async fn app_specs(&self) -> Vec<LuaAppSpec> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::AppSpecs { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Declared host keymap binds, in order (chord, action|None=unbind).
    pub async fn keymap_binds(&self) -> Vec<(String, Option<String>)> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::KeymapBinds { reply }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn app_view(
        &self,
        name: &str,
        ctx: serde_json::Value,
    ) -> Result<Vec<String>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::AppView { name: name.into(), ctx, reply })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    pub async fn app_key(
        &self,
        name: &str,
        key: &str,
        ctx: serde_json::Value,
    ) -> Result<AppKeyOutcome, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::AppKey { name: name.into(), key: key.into(), ctx, reply })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Replace runtime registrations from `sources`, preserving startup state.
    /// Any load failure leaves the old registrations live.
    pub async fn reload(&self,
        sources: Vec<crate::loader::PluginSource>,
    ) -> Result<Vec<(String, String)>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::Reload { sources, reconcile: None, reply })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?.map_err(|error| error.to_string())
    }

    pub(crate) async fn reload_reconciled(&self, sources: Vec<crate::loader::PluginSource>, reconcile: impl FnOnce(Vec<LuaToolSpec>) + Send + 'static) -> Result<Vec<(String, String)>, ReloadError> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::Reload { sources, reconcile: Some(Box::new(reconcile)), reply }).map_err(|_| ReloadError::Failed("lua vm gone".into()))?;
        rx.await.map_err(|_| ReloadError::Failed("lua vm gone".into()))?
    }

    /// Inject the shared questions broker. Sticky across hot reloads.
    pub async fn install_questions(&self, questions: std::sync::Arc<rness_engine::questions::Questions>) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(Cmd::InstallQuestions { questions, reply }).map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }

    /// Inject `rness.session` (post-mount). Sticky across hot reloads.
    pub async fn install_session(
        &self,
        sessions: std::sync::Arc<rness_engine::service::SessionService>,
        subagents: std::sync::Arc<rness_engine::subagent::SubagentRuntime>,
        registry: std::sync::Arc<rness_engine::tools::ToolRegistry>,
        mcp: crate::api::mcp::McpConnections,
        rt: tokio::runtime::Handle,
        model: String,
    ) -> Result<(), String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Cmd::InstallSession {
                binding: SessionBinding { sessions, subagents, registry, mcp, rt, model },
                reply,
            })
            .map_err(|_| "lua vm gone")?;
        rx.await.map_err(|_| "lua vm gone")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn queued_action_rejects_reloaded_registration_generation() {
        let host = LuaHost::spawn().unwrap();
        let source = "local p = __rness_plugin_context(); p.action('run', {scope='promptbox', description='Run', run=function(ctx) ctx.promptbox.insert('new') end})";
        host.load("review", source).await.unwrap();
        let generation = host.action_generation().load(std::sync::atomic::Ordering::SeqCst);
        host.reload(vec![crate::loader::PluginSource { dependencies: vec![], name: "review".into(), source: source.into() }]).await.unwrap();
        assert!(host.call_action_at(generation, "review.run", "promptbox", json!({})).await.unwrap_err().contains("generation expired"));
        assert_eq!(host.call_action("review.run", "promptbox", json!({})).await.unwrap().len(), 1);
        let generation = host.action_generation().load(std::sync::atomic::Ordering::SeqCst);
        assert!(host.reload(vec![crate::loader::PluginSource { dependencies: vec![], name: "review".into(), source: "error('failed')".into() }]).await.is_err());
        assert!(host.call_action_at(generation, "review.run", "promptbox", json!({})).await.is_ok());
    }

    #[tokio::test]
    async fn reload_does_not_resurrect_session_unloaded_plugins() {
        let host = LuaHost::spawn().unwrap();
        let source = crate::loader::PluginSource {
            dependencies: vec![],
            name: "disabled".into(),
            source: "rness.tool.register{name='owned', run=function() return 'ok' end}".into(),
        };
        host.load(&source.name, &source.source).await.unwrap();
        assert!(host.unload("disabled").await.unwrap());
        assert!(host.reload(vec![source]).await.unwrap().is_empty());
        assert!(host.tool_specs().await.is_empty());
        assert!(host.plugin_names().await.is_empty());
    }

    #[tokio::test]
    async fn unmounted_host_unloads_across_cloned_handles() {
        let host = LuaHost::spawn().unwrap();
        host.load("plugin", "rness.tool.register{name='owned', run=function() return 'ok' end}").await.unwrap();
        assert!(host.clone().unload("plugin").await.unwrap());
        assert!(host.tool_specs().await.is_empty());
        assert!(host.call_tool("owned", json!({})).await.is_err());
        assert!(!host.unload("plugin").await.unwrap());
        host.load("plugin", "rness.tool.register{name='owned', run=function() return 'new' end}").await.unwrap();
        assert_eq!(host.call_tool("owned", json!({})).await.unwrap(), "new");
    }

    #[tokio::test]
    async fn host_loads_and_calls_tools_across_threads() {
        let host = LuaHost::spawn().unwrap();
        host.load(
            "p",
            r#"rness.tool.register{ name = "add", run = function(a) return tostring(a.x + a.y) end }"#,
        )
        .await
        .unwrap();

        let specs = host.tool_specs().await;
        assert_eq!(specs[0].name, "add");
        assert_eq!(host.call_tool("add", json!({"x": 2, "y": 3})).await, Ok("5".into()));

        // Cloned handles talk to the same VM.
        let clone = host.clone();
        assert_eq!(clone.call_tool("add", json!({"x": 1, "y": 1})).await, Ok("2".into()));
    }

    #[tokio::test]
    async fn hooks_fire_without_blocking_and_mutate_vm_state() {
        let host = LuaHost::spawn().unwrap();
        host.load(
            "p",
            r#"
            count = 0
            rness.hook.on("tick", function() count = count + 1 end)
            rness.tool.register{ name = "count", run = function() return tostring(count) end }
            "#,
        )
        .await
        .unwrap();

        host.fire_hook("tick", json!({}));
        host.fire_hook("tick", json!({}));
        // Commands are processed in order: the tool call observes both.
        assert_eq!(host.call_tool("count", json!({})).await, Ok("2".into()));
    }

    #[tokio::test]
    async fn load_error_reports_source_name() {
        let host = LuaHost::spawn().unwrap();
        let err = host.load("broken.lua", "this is not lua").await.unwrap_err();
        assert!(err.contains("broken.lua"), "{err}");
    }

    #[tokio::test]
    async fn questions_lifecycle_through_host() {
        let host = LuaHost::spawn().unwrap();
        let qs = std::sync::Arc::new(rness_engine::questions::Questions::default());
        host.install_questions(qs.clone()).await.unwrap();

        // Plugin enables questions.
        host.load("q-plugin", "rness.questions.enable { height = 25, title = 'Decisions' }").await.unwrap();
        assert!(qs.is_available());
        assert_eq!(qs.owner().as_deref(), Some("q-plugin"));
        assert_eq!(qs.overlay_config().height, 25);
        assert_eq!(qs.overlay_config().title, "Decisions");

        // Unload via unmounted host disables questions.
        assert!(host.unload("q-plugin").await.unwrap());
        assert!(!qs.is_available());
        assert!(qs.owner().is_none());

        // Re-enable works after unload.
        host.load("q-plugin", "rness.questions.enable()").await.unwrap();
        assert!(qs.is_available());

        // Explicit disable from Lua.
        host.load("off", "rness.questions.disable()").await.unwrap();
        assert!(!qs.is_available());
    }
}
