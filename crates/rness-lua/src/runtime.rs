//! The single Lua VM and the `rness.*` API surface.
//!
//! One real Lua 5.4 VM (mlua, vendored). It lives on a dedicated actor
//! thread ([`crate::plugin_host`]); this module is the VM-side state:
//! building the `rness` global, collecting registrations, and invoking
//! Lua functions on demand. Nothing here is shared across threads — the
//! actor owns it all.

use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Table, Value as LuaValue};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum LuaError {
    #[error("lua: {0}")]
    Lua(#[from] mlua::Error),
    #[error("unknown lua tool '{0}'")]
    UnknownTool(String),
    #[error("unknown lua app '{0}'")]
    UnknownApp(String),
}

pub use rness_kernel::presentation::{AppKeyOutcome, AppSpec as LuaAppSpec, StyledLine};

/// What a Lua plugin registered via `rness.tool.register{...}` — the
/// engine-facing spec (the callable stays in the VM registry).
#[derive(Debug, Clone)]
pub struct LuaToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub sensitive: bool,
    pub tasks: Option<rness_engine::tasks::TasksConfig>,
}

/// VM-side state: the interpreter plus everything plugins registered.
pub struct LuaRuntime {
    lua: Lua,
    tools: HashMap<String, (LuaToolSpec, RegistryKey)>,
    apps: HashMap<String, (LuaAppSpec, RegistryKey, Option<RegistryKey>)>,
    statusline: Option<RegistryKey>,
    /// tool name (or "*") → card renderer.
    tool_cards: HashMap<String, RegistryKey>,
    /// Host keymap binds, in declaration order: (chord, action|None).
    /// None = unbind. The host recomputes stock+binds on every sync.
    keymap_binds: Vec<(String, Option<String>, Option<String>)>,
    registration_owners: HashMap<(&'static str, String), String>,
    command_completers: HashMap<String, RegistryKey>,
    command_metadata: HashMap<String, (String, Vec<(String, String)>)>,
    commands: HashMap<String, (String, RegistryKey)>,
    plugin_hooks: HashMap<String, Table>,
    /// Shared questions broker, injected post-mount for dynamic enable/disable.
    questions: Option<std::sync::Arc<rness_engine::questions::Questions>>,
}

impl LuaRuntime {
    pub fn startup(&mut self, path: &std::path::Path) -> Result<crate::api::config::StartupConfig, String> {
        let config = crate::api::config::evaluate(&self.lua, path).map_err(|e| e.to_string())?;
        self.drain_registrations(None).map_err(|e| e.to_string())?;
        self.install_config(&config).map_err(|e| e.to_string())?;
        Ok(config)
    }

    pub fn install_config(&self, config: &crate::api::config::StartupConfig) -> Result<(), LuaError> {
        let rness: Table = self.lua.globals().get("rness")?;
        let models: Table = rness.get("models")?;
        let registry = config.models.clone();
        models.set("capabilities", self.lua.create_function(move |lua, (provider, model): (String, String)| {
            lua.to_value(&registry.capabilities(&rness_protocol::events::ModelSelection { route: provider, model }))
        })?)?;
        let profiles = self.lua.create_table()?;
        let registry = config.models.clone();
        profiles.set("resolve", self.lua.create_function(move |lua, name: String| {
            let resolved = registry.resolve_profile(&name).map_err(mlua::Error::runtime)?;
            lua.to_value(&resolved)
        })?)?;
        rness.set("profiles", profiles)?;
        rness.set("default_profile", config.default_profile.clone())?;
        Ok(())
    }

    pub fn new() -> Result<Self, LuaError> {
        let lua = Lua::new();
        install_api(&lua)?;
        Ok(Self {
            lua,
            tools: HashMap::new(),
            apps: HashMap::new(),
            statusline: None,
            tool_cards: HashMap::new(),
            keymap_binds: Vec::new(),
            registration_owners: HashMap::new(),
            command_completers: HashMap::new(),
            command_metadata: HashMap::new(),
            commands: HashMap::new(),
            plugin_hooks: HashMap::new(),
            questions: None,
        })
    }

    /// Inject the shared questions broker so plugins can call
    /// `rness.questions.enable()` / `disable()` at load time.
    pub fn install_questions(&mut self, questions: std::sync::Arc<rness_engine::questions::Questions>) -> Result<(), LuaError> {
        self.questions = Some(questions.clone());
        let rness: Table = self.lua.globals().get("rness")?;
        let q_table = self.lua.create_table()?;
        q_table.set("enable", self.lua.create_function(move |lua, options: Option<Table>| {
            let config: crate::api::config::QuestionOverlayConfig = match options {
                Some(table) => lua.from_value(mlua::Value::Table(table))?,
                None => Default::default(),
            };
            if config.height < 10 || config.title.trim().is_empty() {
                return Err(mlua::Error::runtime("questions height must be >= 10 and title nonempty"));
            }
            require_declaration_phase(lua)?;
            let pending: Table = lua.globals().get("__rness_pending")?;
            pending.set("questions", lua.to_value(&config)?)?;
            Ok(())
        })?)?;
        q_table.set("disable", self.lua.create_function(move |lua, ()| {
            require_declaration_phase(lua)?;
            let pending: Table = lua.globals().get("__rness_pending")?;
            pending.set("questions", false)?;
            Ok(())
        })?)?;
        rness.set("questions", q_table)?;
        Ok(())
    }

    /// Run a plugin chunk. Registrations made during execution are
    /// drained from the VM into runtime state.
    pub fn load(&mut self, name: &str, source: &str) -> Result<(), LuaError> {
        if self.plugin_hooks.contains_key(name) {
            return Err(mlua::Error::runtime(format!("plugin already loaded: {name}; unload it first")).into());
        }
        let declarations: Table = self.lua.globals().get("__rness_declarations")?;
        let mut snapshots = Vec::new();
        for name in ["tools", "apps", "cards", "commands"] {
            let table: Table = declarations.get(name)?;
            let values = table.clone().pairs::<String, bool>().collect::<mlua::Result<Vec<_>>>()?;
            snapshots.push((name, values));
        }
        let hook_cleanup = self.lua.create_table()?;
        let hook_owner = self.lua.create_table()?;
        self.lua.globals().set("__rness_load_owner", hook_owner.clone())?;
        self.lua.globals().set("__rness_loading_hooks", hook_cleanup.clone())?;
        self.lua.globals().set("__rness_loading_plugin", name)?;
        let execution = self.lua.load(source).set_name(format!("@{name}")).exec();
        self.lua.globals().set("__rness_loading_hooks", LuaValue::Nil)?;
        self.lua.globals().set("__rness_load_owner", LuaValue::Nil)?;
        self.lua.globals().set("__rness_loading_plugin", LuaValue::Nil)?;
        if let Err(error) = execution {
            for unsubscribe in hook_cleanup.sequence_values::<Function>() {
                unsubscribe?.call::<bool>(())?;
            }
            hook_owner.clear()?;
            let pending: Table = self.lua.globals().get("__rness_pending")?;
            for category in ["tools", "apps", "tool_cards", "keymaps", "commands"] {
                pending.get::<Table>(category)?.clear()?;
            }
            pending.set("statusline", LuaValue::Nil)?;
            pending.set("questions", LuaValue::Nil)?;
            pending.set("tasks_disabled", LuaValue::Nil)?;
            for (category, values) in snapshots {
                let table: Table = declarations.get(category)?;
                table.clear()?;
                for (key, value) in values { table.set(key, value)?; }
            }
            return Err(error.into());
        }
        self.drain_registrations(Some(name))?;
        let pending: Table = self.lua.globals().get("__rness_pending")?;
        let declaration: LuaValue = pending.get("questions")?;
        if let Some(qs) = &self.questions {
            match declaration {
                LuaValue::Table(table) => {
                    let config: crate::api::config::QuestionOverlayConfig = self.lua.from_value(LuaValue::Table(table))?;
                    qs.set_overlay_config(rness_engine::questions::OverlayConfig { priority: config.priority, height: config.height, title: config.title });
                    qs.set_available(config.enabled);
                    qs.set_owner(Some(name.to_owned()));
                }
                LuaValue::Boolean(false) => { qs.set_available(false); qs.set_owner(None); }
                _ => {}
            }
        }
        if pending.get::<Option<bool>>("tasks_disabled")?.unwrap_or(false) {
            if self.tools.get("TaskWrite").is_some_and(|(spec, _)| spec.tasks.is_some()) {
                if let Some((_, callback)) = self.tools.remove("TaskWrite") { self.lua.remove_registry_value(callback)?; }
                self.registration_owners.remove(&("tools", "TaskWrite".into()));
            }
            pending.set("tasks_disabled", LuaValue::Nil)?;
        }
        pending.set("questions", LuaValue::Nil)?;
        self.plugin_hooks.insert(name.to_owned(), hook_owner);
        Ok(())
    }

    /// Remove a loaded chunk's current VM registrations, without restoring
    /// implementations it replaced. Hosts must reconcile their own caches.
    pub(crate) fn lua(&self) -> &Lua { &self.lua }

    pub fn command_metadata(&self, name: &str) -> (String, Vec<(String, String)>) {
        self.command_metadata.get(name).cloned().unwrap_or_default()
    }

    pub fn command_specs(&self) -> Vec<(String, String)> {
        self.commands.iter().map(|(name, (description, _))| (name.clone(), description.clone())).collect()
    }

    pub fn complete_command(&self, name: &str, context: serde_json::Value) -> Result<Vec<String>, String> {
        if !self.commands.contains_key(name) { return Err(format!("command unavailable: {name}")); }
        let Some(key) = self.command_completers.get(name) else {
            return Ok(self.command_metadata(name).1.into_iter().map(|(value, _)| value).collect());
        };
        let run: Function = self.lua.registry_value(key).map_err(|e| e.to_string())?;
        let context = self.lua.to_value(&context).map_err(|e| e.to_string())?;
        let result: Table = run.call(context).map_err(|e| e.to_string())?;
        result.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>().map_err(|e| e.to_string())
    }

    pub fn call_command(&self, name: &str, context: serde_json::Value) -> Result<rness_engine::interaction::CommandResult, String> {
        let (_, key) = self.commands.get(name).ok_or_else(|| format!("command unavailable: {name}"))?;
        let run: Function = self.lua.registry_value(key).map_err(|e| e.to_string())?;
        let context = self.lua.to_value(&context).map_err(|e| e.to_string())?;
        let result: LuaValue = run.call(context).map_err(|e| e.to_string())?;
        match result {
            LuaValue::Nil => Ok(Default::default()),
            LuaValue::String(text) => Ok(rness_engine::interaction::CommandResult { message: text.to_str().map_err(|e| e.to_string())?.to_owned(), ..Default::default() }),
            value => self.lua.from_value(value).map_err(|e| e.to_string()),
        }
    }

    pub fn plugin_names(&self) -> Vec<String> {
        let mut names = self.plugin_hooks.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    pub fn unload(&mut self, name: &str) -> Result<bool, LuaError> {
        let Some(hooks) = self.plugin_hooks.remove(name) else { return Ok(false) };
        for unsubscribe in hooks.clone().sequence_values::<Function>() {
            unsubscribe?.call::<bool>(())?;
        }
        hooks.clear()?;
        let owned: Vec<_> = self.registration_owners.iter()
            .filter(|(_, owner)| owner.as_str() == name)
            .map(|(key, _)| key.clone()).collect();
        let declarations: Table = self.lua.globals().get("__rness_declarations")?;
        for (category, key) in owned {
            match category {
                "commands" => {
                    if let Some(callback) = self.command_completers.remove(&key) { self.lua.remove_registry_value(callback)?; }
                    self.command_metadata.remove(&key);
                    if let Some((_, callback)) = self.commands.remove(&key) {
                        self.lua.remove_registry_value(callback)?;
                    }
                    declarations.get::<Table>("commands")?.set(key.as_str(), LuaValue::Nil)?;
                }
                "tools" => {
                    if let Some((_, callback)) = self.tools.remove(&key) {
                        self.lua.remove_registry_value(callback)?;
                    }
                }
                "apps" => {
                    if let Some((_, view, on_key)) = self.apps.remove(&key) {
                        self.lua.remove_registry_value(view)?;
                        if let Some(callback) = on_key { self.lua.remove_registry_value(callback)?; }
                    }
                }
                "cards" => {
                    if let Some(callback) = self.tool_cards.remove(&key) {
                        self.lua.remove_registry_value(callback)?;
                    }
                }
                "statusline" => {
                    if let Some(callback) = self.statusline.take() {
                        self.lua.remove_registry_value(callback)?;
                    }
                }
                _ => unreachable!(),
            }
            if category != "statusline" {
                declarations.get::<Table>(category)?.set(key.as_str(), LuaValue::Nil)?;
            }
            self.registration_owners.remove(&(category, key));
        }
        self.keymap_binds.retain(|(_, _, owner)| owner.as_deref() != Some(name));
        if let Some(qs) = &self.questions {
            if qs.owner().as_deref() == Some(name) {
                qs.set_available(false);
                qs.set_owner(None);
            }
        }
        Ok(true)
    }

    pub fn tool_specs(&self) -> Vec<LuaToolSpec> {
        let mut v: Vec<_> = self.tools.values().map(|(spec, _)| spec.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn keymap_binds(&self) -> Vec<(String, Option<String>)> {
        self.keymap_binds.iter().map(|(chord, action, _)| (chord.clone(), action.clone())).collect()
    }

    pub fn app_specs(&self) -> Vec<LuaAppSpec> {
        let mut v: Vec<_> = self.apps.values().map(|(spec, _, _)| spec.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Evaluate an app's `view(ctx)`: an array of strings (its lines).
    pub fn app_view(
        &self,
        name: &str,
        ctx: &serde_json::Value,
    ) -> Result<Vec<String>, LuaError> {
        let Some((_, view, _)) = self.apps.get(name) else {
            return Err(LuaError::UnknownApp(name.to_string()));
        };
        let f: Function = self.lua.registry_value(view)?;
        let lines: Table = f.call(self.lua.to_value(ctx)?)?;
        Ok(lines.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?)
    }

    /// Deliver a key to an app's `on_key(key, ctx)`. The return value
    /// steers the host:
    ///   nil/false        -> Pass (host keymap may act)
    ///   true             -> Consumed (host refreshes the view)
    ///   "close"          -> Close (host hides the app)
    ///   { action=, payload= } -> Action (host applies it, e.g.
    ///                        "session:switch"), then refreshes
    pub fn app_key(
        &self,
        name: &str,
        key: &str,
        ctx: &serde_json::Value,
    ) -> Result<AppKeyOutcome, LuaError> {
        let Some((_, _, on_key)) = self.apps.get(name) else {
            return Err(LuaError::UnknownApp(name.to_string()));
        };
        let Some(on_key) = on_key else { return Ok(AppKeyOutcome::Pass) };
        let f: Function = self.lua.registry_value(on_key)?;
        let out: LuaValue = f.call((key, self.lua.to_value(ctx)?))?;
        Ok(match out {
            LuaValue::Nil | LuaValue::Boolean(false) => AppKeyOutcome::Pass,
            LuaValue::Boolean(true) => AppKeyOutcome::Consumed,
            LuaValue::String(s) if s.to_string_lossy() == "close" => AppKeyOutcome::Close,
            LuaValue::Table(t) => {
                let action: String = t.get("action")?;
                let payload: LuaValue = t.get("payload")?;
                let payload = if payload.is_nil() {
                    serde_json::Value::Null
                } else {
                    self.lua.from_value(payload)?
                };
                AppKeyOutcome::Action { name: action, payload }
            }
            _ => AppKeyOutcome::Consumed,
        })
    }

    /// Invoke a Lua tool. Lua raising an error becomes Err (an is_error
    /// tool result engine-side, never a crash).
    pub fn call_tool(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<Result<String, String>, LuaError> {
        self.call_tool_context(name, args, &serde_json::json!({}))
    }

    pub fn call_tool_context(&self, name: &str, args: &serde_json::Value, context: &serde_json::Value) -> Result<Result<String, String>, LuaError> {
        let Some((_, key)) = self.tools.get(name) else {
            return Err(LuaError::UnknownTool(name.to_string()));
        };
        let f: Function = self.lua.registry_value(key)?;
        let lua_args = self.lua.to_value(args)?;
        match f.call::<LuaValue>((lua_args, self.lua.to_value(context)?)) {
            Ok(LuaValue::Nil) => Ok(Ok(String::new())),
            Ok(v) => Ok(Ok(lua_display(&self.lua, v)?)),
            Err(e) => Ok(Err(user_message(&e))),
        }
    }

    /// Fire an event's hooks in registration order. Hook errors are
    /// reported, not propagated — a broken plugin can't break the host.
    /// Handlers live in the Lua-side `__rness_hooks` table so that
    /// `rness.events.emit` dispatches to exactly the same set.
    pub fn fire_hook(&self, event: &str, payload: &serde_json::Value) -> Vec<String> {
        let mut errors = Vec::new();
        let run = || -> Result<Vec<String>, mlua::Error> {
            let hooks: Table = self.lua.globals().get("__rness_hooks")?;
            let Some(handlers) = hooks.get::<Option<Table>>(event)? else {
                return Ok(Vec::new());
            };
            let payload = self.lua.to_value(payload)?;
            let mut errs = Vec::new();
            for f in handlers.sequence_values::<Function>().collect::<Vec<_>>() {
                if let Err(e) = f.and_then(|f| f.call::<()>(&payload)) {
                    errs.push(user_message(&e));
                }
            }
            Ok(errs)
        };
        match run() {
            Ok(errs) => errors.extend(errs),
            Err(e) => errors.push(user_message(&e)),
        }
        errors
    }

    /// Surface the engine's SessionService as `rness.session`. Called
    /// after mount and again on every fresh VM (hot reload).
    pub fn install_session(
        &self,
        sessions: std::sync::Arc<rness_engine::service::SessionService>,
        subagents: std::sync::Arc<rness_engine::subagent::SubagentRuntime>,
        registry: std::sync::Arc<rness_engine::tools::ToolRegistry>,
        mcp: crate::api::mcp::McpConnections,
        rt: tokio::runtime::Handle,
        model: String,
    ) -> Result<(), LuaError> {
        let rness: Table = self.lua.globals().get("rness")?;
        // rness.model — the active "<provider>/<model>" selection, the
        // key plugins pass to rness.models.get. Composition-root fact.
        rness.set("model", model)?;
        crate::api::session::install(&self.lua, &rness, sessions.clone(), rt.clone())?;
        crate::api::subagents::install(&self.lua, &rness, subagents, sessions, rt.clone())?;
        crate::api::mcp::install(&self.lua, &rness, registry, mcp, rt)?;
        Ok(())
    }

    /// Evaluate the registered statusline provider, if any.
    pub fn statusline(&self) -> Option<String> {
        let key = self.statusline.as_ref()?;
        let f: Function = self.lua.registry_value(key).ok()?;
        match f.call::<Option<String>>(()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("lua statusline failed: {}", user_message(&e));
                None
            }
        }
    }

    /// Render one finished tool call through the registered card
    /// renderer: exact tool-name match first, then the "*" catch-all.
    /// None = no renderer or renderer declined (nil) — caller falls
    /// back to the built-in card. A THROWING renderer also falls back,
    /// loudly in the log.
    pub fn tool_card(
        &self,
        name: &str,
        args: &serde_json::Value,
        output: &str,
        is_error: bool,
    ) -> Option<Vec<StyledLine>> {
        let key = self.tool_cards.get(name).or_else(|| self.tool_cards.get("*"))?;
        let f: Function = self.lua.registry_value(key).ok()?;
        let call = self.lua.create_table().ok()?;
        call.set("name", name).ok()?;
        call.set("args", self.lua.to_value(args).ok()?).ok()?;
        call.set("output", output).ok()?;
        call.set("is_error", is_error).ok()?;
        let lines: LuaValue = match f.call(call) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(tool = name, "lua tool card failed: {}", user_message(&e));
                return None;
            }
        };
        let LuaValue::Table(rows) = lines else { return None }; // nil = declined
        let mut out = Vec::new();
        for row in rows.sequence_values::<LuaValue>() {
            match row.ok()? {
                // "plain text" shorthand
                LuaValue::String(text) => out.push(StyledLine {
                    text: text.to_string_lossy().to_string(),
                    style: String::new(),
                }),
                LuaValue::Table(t) => out.push(StyledLine {
                    text: t.get::<Option<String>>("text").ok()?.unwrap_or_default(),
                    style: t.get::<Option<String>>("style").ok()?.unwrap_or_default(),
                }),
                _ => return None,
            }
        }
        Some(out)
    }

    /// Move registrations parked by the API closures (in Lua globals)
    /// into runtime state.
    fn drain_registrations(&mut self, owner: Option<&str>) -> Result<(), LuaError> {
        let pending: Table = self.lua.globals().get("__rness_pending")?;

        let commands: Table = pending.get("commands")?;
        for entry in commands.sequence_values::<Table>() {
            let entry = entry?;
            let name: String = entry.get("name")?;
            let run: Function = entry.get("run")?;
            let key = self.lua.create_registry_value(run)?;
            let arguments: LuaValue = entry.get("arguments")?;
            self.command_metadata.insert(name.clone(), (entry.get("usage")?, self.lua.from_value(arguments)?));
            if let Some(complete) = entry.get::<Option<Function>>("complete")? {
                self.command_completers.insert(name.clone(), self.lua.create_registry_value(complete)?);
            }
            self.commands.insert(name.clone(), (entry.get("description")?, key));
            if let Some(owner) = owner {
                self.registration_owners.insert(("commands", name), owner.to_owned());
            }
        }
        commands.clear()?;
        let tools: Table = pending.get("tools")?;
        for entry in tools.sequence_values::<Table>() {
            let entry = entry?;
            let name: String = entry.get("name")?;
            let run: Function = entry.get("run")?;
            let schema: LuaValue = entry.get("schema")?;
            let spec = LuaToolSpec {
                name: name.clone(),
                description: entry.get::<Option<String>>("description")?.unwrap_or_default(),
                input_schema: if schema.is_nil() {
                    serde_json::json!({ "type": "object" })
                } else {
                    self.lua.from_value(schema)?
                },
                tasks: entry.get::<Option<Table>>("tasks_config")?.map(|table| self.lua.from_value(LuaValue::Table(table))).transpose()?,
                sensitive: entry.get::<Option<bool>>("sensitive")?.unwrap_or(false),
            };
            let key = self.lua.create_registry_value(run)?;
            if let Some(owner) = owner {
                self.registration_owners.insert(("tools", name.clone()), owner.to_owned());
            }
            if let Some((_, previous)) = self.tools.insert(name, (spec, key)) {
                self.lua.remove_registry_value(previous)?;
            }
        }
        tools.clear()?;

        let apps: Table = pending.get("apps")?;
        for entry in apps.sequence_values::<Table>() {
            let entry = entry?;
            let name: String = entry.get("name")?;
            let view: Function = entry.get("view")?;
            let on_key: Option<Function> = entry.get("on_key")?;
            let spec = LuaAppSpec {
                name: name.clone(),
                slot: entry.get::<Option<String>>("slot")?.unwrap_or_else(|| "overlay".into()),
                title: entry.get::<Option<String>>("title")?.unwrap_or_else(|| name.clone()),
                keymap: entry.get::<Option<String>>("keymap")?,
            };
            let view_key = self.lua.create_registry_value(view)?;
            let key_key = match on_key {
                Some(f) => Some(self.lua.create_registry_value(f)?),
                None => None,
            };
            if let Some(owner) = owner {
                self.registration_owners.insert(("apps", name.clone()), owner.to_owned());
            }
            if let Some((_, previous_view, previous_key)) = self.apps.insert(name, (spec, view_key, key_key)) {
                self.lua.remove_registry_value(previous_view)?;
                if let Some(key) = previous_key { self.lua.remove_registry_value(key)?; }
            }
        }
        apps.clear()?;

        let cards: Table = pending.get("tool_cards")?;
        for entry in cards.sequence_values::<Table>() {
            let entry = entry?;
            let name: String = entry.get("name")?;
            let render: Function = entry.get("render")?;
            let key = self.lua.create_registry_value(render)?;
            if let Some(owner) = owner {
                self.registration_owners.insert(("cards", name.clone()), owner.to_owned());
            }
            if let Some(previous) = self.tool_cards.insert(name, key) {
                self.lua.remove_registry_value(previous)?;
            }
        }
        cards.clear()?;

        let keymaps: Table = pending.get("keymaps")?;
        for entry in keymaps.sequence_values::<Table>() {
            let entry = entry?;
            let chord: String = entry.get("chord")?;
            let action: Option<String> = entry.get("action")?;
            self.keymap_binds.push((chord, action, owner.map(str::to_owned)));
        }
        keymaps.clear()?;

        let statusline: LuaValue = pending.get("statusline")?;
        if let LuaValue::Function(f) = statusline {
            let key = self.lua.create_registry_value(f)?;
            if let Some(previous) = self.statusline.replace(key) {
                self.lua.remove_registry_value(previous)?;
            }
            if let Some(owner) = owner {
                self.registration_owners.insert(("statusline", String::new()), owner.to_owned());
            }
            pending.set("statusline", LuaValue::Nil)?;
        }
        Ok(())
    }
}

/// Capture the registration owner and restore it across nested callbacks,
/// including errors. Declaration APIs remain load-time only.
fn owned_callback(lua: &Lua, callback: Function) -> mlua::Result<Function> {
    let owner: Option<Table> = lua.globals().get("__rness_callback_owner")?;
    let owner = match owner {
        Some(owner) => Some(owner),
        None => lua.globals().get::<Option<Table>>("__rness_load_owner")?,
    };
    lua.create_function(move |lua, args: mlua::MultiValue| {
        let previous: LuaValue = lua.globals().get("__rness_callback_owner")?;
        let depth: Option<u32> = lua.globals().get("__rness_callback_depth")?;
        lua.globals().set("__rness_callback_owner", owner.clone())?;
        lua.globals().set("__rness_callback_depth", depth.unwrap_or(0) + 1)?;
        let result = callback.call::<mlua::MultiValue>(args);
        lua.globals().set("__rness_callback_owner", previous)?;
        lua.globals().set("__rness_callback_depth", depth)?;
        result
    })
}

fn require_declaration_phase(lua: &Lua) -> mlua::Result<()> {
    if lua.globals().get::<Option<u32>>("__rness_callback_depth")?.unwrap_or(0) != 0 {
        return Err(mlua::Error::runtime("registrations are only allowed during startup or plugin loading, not callbacks"));
    }
    Ok(())
}

/// Build the `rness` global. Registration calls park their payloads in
/// `__rness_pending`; the runtime drains them after each chunk load.
fn install_api(lua: &Lua) -> Result<(), LuaError> {
    let pending = lua.create_table()?;
    pending.set("commands", lua.create_table()?)?;
    pending.set("tools", lua.create_table()?)?;
    pending.set("apps", lua.create_table()?)?;
    pending.set("tool_cards", lua.create_table()?)?;
    pending.set("keymaps", lua.create_table()?)?;
    lua.globals().set("__rness_pending", &pending)?;
    // event name → array of handlers. Lua-side so rness.events.emit and
    // host-fired hooks dispatch to the same table.
    lua.globals().set("__rness_hooks", lua.create_table()?)?;

    let rness = lua.create_table()?;

    // rness.tool.register{ name=, description=, schema=, sensitive=, run= }
    let declarations = lua.create_table()?;
    lua.globals().set("__rness_declarations", declarations.clone())?;
    let commands = lua.create_table()?;
    let names = lua.create_table()?;
    declarations.set("commands", names.clone())?;
    commands.set("register", lua.create_function(move |lua, spec: Table| {
        require_declaration_phase(lua)?;
        let name: String = spec.get("name")?;
        if !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
            || ["agent", "help", "skill", "unload", "colorscheme"].contains(&name.as_str()) {
            return Err(mlua::Error::runtime("invalid or reserved command name"));
        }
        if names.contains_key(name.as_str())? { return Err(mlua::Error::runtime("command already registered")); }
        let entry = lua.create_table()?;
        entry.set("name", name.clone())?;
        entry.set("description", spec.get::<Option<String>>("description")?.unwrap_or_default())?;
        entry.set("usage", spec.get::<Option<String>>("usage")?.unwrap_or_default())?;
        let arguments = spec.get::<Option<Table>>("arguments")?.map(|table| table.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()).transpose()?.unwrap_or_default();
        let arguments: Vec<_> = arguments.into_iter().map(|value| (value, String::new())).collect();
        entry.set("arguments", lua.to_value(&arguments)?)?;
        if let Some(complete) = spec.get::<Option<Function>>("complete")? { entry.set("complete", owned_callback(lua, complete)?)?; }
        entry.set("run", owned_callback(lua, spec.get::<Function>("run")?)?)?;
        let pending: Table = lua.globals().get("__rness_pending")?;
        pending.get::<Table>("commands")?.push(entry)?;
        names.set(name, true)?;
        Ok(())
    })?)?;
    rness.set("commands", commands)?;
    let tool = lua.create_table()?;
    let declared = lua.create_table()?;
    declarations.set("tools", declared.clone())?;
    for (method, replacing) in [("register", false), ("replace", true)] {
        let declared = declared.clone();
        tool.set(method, lua.create_function(move |lua, spec: Table| {
            require_declaration_phase(lua)?;
            let name: String = spec.get("name").map_err(|_| mlua::Error::runtime("tool 'name' (string) is required"))?;
            if name == "TaskWrite" { return Err(mlua::Error::runtime("TaskWrite is reserved; use rness.tasks.enable")); }
            if name.trim().is_empty() { return Err(mlua::Error::runtime("tool name must not be empty")); }
            let run: Function = spec.get("run").map_err(|_| mlua::Error::runtime("tool 'run' (function) is required"))?;
            let description: Option<String> = spec.get("description")?;
            let sensitive: Option<bool> = spec.get("sensitive")?;
            let schema: LuaValue = spec.get("schema")?;
            let schema: serde_json::Value = if schema.is_nil() {
                serde_json::json!({ "type": "object" })
            } else {
                lua.from_value(schema)?
            };
            let exists = declared.get::<Option<bool>>(name.as_str())?.unwrap_or(false);
            if replacing != exists {
                return Err(mlua::Error::runtime(if replacing {
                    format!("cannot replace unknown Lua tool: {name}")
                } else { format!("Lua tool already registered: {name}; use rness.tool.replace") }));
            }
            let pending: Table = lua.globals().get("__rness_pending")?;
            let tools: Table = pending.get("tools")?;
            let entry = lua.create_table()?;
            entry.set("name", name.clone())?;
            entry.set("run", owned_callback(lua, run)?)?;
            entry.set("description", description)?;
            entry.set("sensitive", sensitive)?;
            entry.set("schema", lua.to_value(&schema)?)?;
            tools.push(entry)?;
            declared.set(name, true)?;
            Ok(())
        })?)?;
    }
    rness.set("tool", tool)?;

    let tasks = lua.create_table()?;
    tasks.set("enable", lua.create_function(|lua, options: Option<Table>| {
        require_declaration_phase(lua)?;
        let config: rness_engine::tasks::TasksConfig = options.map(|table| lua.from_value(LuaValue::Table(table))).transpose()?.unwrap_or_default();
        let pending: Table = lua.globals().get("__rness_pending")?;
        if pending.get::<Option<bool>>("tasks_disabled")?.unwrap_or(false) { return Err(mlua::Error::runtime("enable or disable tasks once per plugin load")); }
        let declarations: Table = lua.globals().get("__rness_declarations")?;
        let declared: Table = declarations.get("tools")?;
        if declared.contains_key("TaskWrite")? { return Err(mlua::Error::runtime("TaskWrite already registered")); }
        use rness_engine::tools::Tool;
        let native = rness_engine::tasks::TaskWrite(config.clone());
        let entry = lua.create_table()?;
        entry.set("name", native.name())?;
        entry.set("description", native.description())?;
        entry.set("schema", lua.to_value(&native.input_schema())?)?;
        entry.set("tasks_config", lua.to_value(&config)?)?;
        entry.set("run", lua.create_function(|_, ()| Err::<(), _>(mlua::Error::runtime("TaskWrite requires durable agent dispatch")))?)?;
        let pending: Table = lua.globals().get("__rness_pending")?;
        pending.get::<Table>("tools")?.push(entry)?;
        declared.set("TaskWrite", true)?;
        Ok(())
    })?)?;
    tasks.set("disable", lua.create_function(|lua, ()| {
        require_declaration_phase(lua)?;
        let pending: Table = lua.globals().get("__rness_pending")?;
        for entry in pending.get::<Table>("tools")?.sequence_values::<Table>() {
            if entry?.get::<String>("name")? == "TaskWrite" { return Err(mlua::Error::runtime("enable or disable tasks once per plugin load")); }
        }
        let declarations: Table = lua.globals().get("__rness_declarations")?;
        declarations.get::<Table>("tools")?.set("TaskWrite", LuaValue::Nil)?;
        let pending: Table = lua.globals().get("__rness_pending")?;
        pending.set("tasks_disabled", true)?;
        Ok(())
    })?)?;
    rness.set("tasks", tasks)?;

    // rness.hook.on(event, handler)
    let hook = lua.create_table()?;
    hook.set(
        "on",
        lua.create_function(|lua, (event, handler): (String, Function)| {
            let hooks: Table = lua.globals().get("__rness_hooks")?;
            let handlers: Table = match hooks.get::<Option<Table>>(&*event)? {
                Some(t) => t,
                None => {
                    let t = lua.create_table()?;
                    hooks.set(&*event, &t)?;
                    t
                }
            };
            let handler = owned_callback(lua, handler)?;
            let callback = std::sync::Arc::new(std::sync::Mutex::new(Some(handler)));
            let current = callback.clone();
            let wrapper = lua.create_function(move |_, payload: LuaValue| {
                let handler = current.lock().expect("hook lock").clone();
                if let Some(handler) = handler { handler.call::<()>(payload)?; }
                Ok(())
            })?;
            handlers.push(wrapper.clone())?;
            let unsubscribe = lua.create_function(move |_, ()| {
                let removed = callback.lock().expect("hook lock").take().is_some();
                if removed {
                    for index in 1..=handlers.raw_len() {
                        if handlers.raw_get::<Function>(index)? == wrapper {
                            for next in index + 1..=handlers.raw_len() {
                                handlers.raw_set(next - 1, handlers.raw_get::<Function>(next)?)?;
                            }
                            handlers.raw_set(handlers.raw_len(), LuaValue::Nil)?;
                            break;
                        }
                    }
                }
                Ok(removed)
            })?;
            let loading: Option<Table> = lua.globals().get("__rness_loading_hooks")?;
            let owner: Option<Table> = lua.globals().get("__rness_callback_owner")?;
            let owner = owner.or(lua.globals().get::<Option<Table>>("__rness_load_owner")?);
            if let Some(owner) = &owner {
                owner.push(unsubscribe.clone())?;
            }
            if let Some(cleanup) = loading {
                if owner.as_ref() != Some(&cleanup) {
                    cleanup.push(unsubscribe.clone())?;
                }
            }
            Ok(unsubscribe)
        })?,
    )?;
    rness.set("hook", hook)?;

    // rness.events.emit(event, payload) — dispatch to every rness.hook.on
    // listener of `event`, in-VM. Same table the host fires into: Lua
    // plugins talk to each other through the exact seam the engine uses.
    let events = lua.create_table()?;
    events.set(
        "emit",
        lua.create_function(|lua, (event, payload): (String, Option<LuaValue>)| {
            let hooks: Table = lua.globals().get("__rness_hooks")?;
            let Some(handlers) = hooks.get::<Option<Table>>(&*event)? else {
                return Ok(());
            };
            let payload = payload.unwrap_or(LuaValue::Nil);
            for f in handlers.sequence_values::<Function>().collect::<Vec<_>>() {
                if let Err(e) = f.and_then(|f| f.call::<()>(&payload)) {
                    tracing::warn!(target: "lua", "emit '{event}' handler failed: {}", user_message(&e));
                }
            }
            Ok(())
        })?,
    )?;
    rness.set("events", events)?;

    // rness.models — declared model capabilities (dsh: contextWindow is
    // HUMAN-declared adapter config; pi-ai is just someone else's
    // declaration). Pure in-VM state: plugins declare the models THEY
    // use, plugins consume the facts. rness ships ZERO model data — an
    // undeclared model returns nil, callers keep their own fallback.
    //   rness.models.declare("anthropic/claude-x", { context_window = 200000, ... })
    //   rness.models.get("anthropic/claude-x") -> table|nil
    //   rness.models.list() -> { name, ... } sorted
    let models = lua.create_table()?;
    lua.globals().set("__rness_models", lua.create_table()?)?;
    models.set(
        "declare",
        lua.create_function(|lua, (name, caps): (String, Table)| {
            let store: Table = lua.globals().get("__rness_models")?;
            store.set(name, caps)?;
            Ok(())
        })?,
    )?;
    models.set(
        "get",
        lua.create_function(|lua, name: String| {
            let store: Table = lua.globals().get("__rness_models")?;
            store.get::<Option<Table>>(name)
        })?,
    )?;
    models.set(
        "list",
        lua.create_function(|lua, ()| {
            let store: Table = lua.globals().get("__rness_models")?;
            let mut names: Vec<String> = Vec::new();
            for pair in store.pairs::<String, Table>() {
                names.push(pair?.0);
            }
            names.sort();
            let out = lua.create_table()?;
            for n in names {
                out.push(n)?;
            }
            Ok(out)
        })?,
    )?;
    rness.set("models", models)?;

    // rness.ui.statusline(provider) — provider() -> string|nil
    let ui = lua.create_table()?;
    ui.set("wrap", lua.create_function(|_, (text, columns): (String, u16)| {
        if columns == 0 { return Err(mlua::Error::runtime("wrap columns must be positive")); }
        let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        Ok(textwrap::wrap(&text, usize::from(columns)).into_iter().map(|line| line.into_owned()).collect::<Vec<_>>())
    })?)?;
    ui.set(
        "statusline",
        lua.create_function(|lua, provider: Function| {
            require_declaration_phase(lua)?;
            let pending: Table = lua.globals().get("__rness_pending")?;
            pending.set("statusline", owned_callback(lua, provider)?)?;
            Ok(())
        })?,
    )?;
    // rness.ui.tool_card(name, render) — render(call) -> lines|nil.
    // call = { name=, args=, output=, is_error= }; each line is a
    // string or { text=, style= } with a THEME STYLE NAME. name "*"
    // catches every tool; nil return falls back to the built-in card.
    let declared_cards = lua.create_table()?;
    declarations.set("cards", declared_cards.clone())?;
    for (method, replacing) in [("tool_card", false), ("replace_tool_card", true)] {
        let declared = declared_cards.clone();
        ui.set(method, lua.create_function(move |lua, (name, render): (String, Function)| {
            require_declaration_phase(lua)?;
            if name.trim().is_empty() { return Err(mlua::Error::runtime("card name must not be empty")); }
            let exists = declared.get::<Option<bool>>(name.as_str())?.unwrap_or(false);
            if exists != replacing {
                return Err(mlua::Error::runtime(if replacing {
                    format!("unknown tool card: {name}")
                } else { format!("tool card already registered: {name}; use replace_tool_card") }));
            }
            let pending: Table = lua.globals().get("__rness_pending")?;
            let cards: Table = pending.get("tool_cards")?;
            let entry = lua.create_table()?;
            entry.set("name", name.clone())?;
            entry.set("render", owned_callback(lua, render)?)?;
            cards.push(entry)?;
            declared.set(name, true)?;
            Ok(())
        })?)?;
    }
    // rness.ui.app{ name=, slot=, title=, keymap=, view=, on_key= }
    // view(ctx) -> { "line", ... }; on_key(key, ctx) -> see app_key.
    let declared_apps = lua.create_table()?;
    declarations.set("apps", declared_apps.clone())?;
    for (method, replacing) in [("app", false), ("replace_app", true)] {
        let declared = declared_apps.clone();
        ui.set(method, lua.create_function(move |lua, spec: Table| {
            require_declaration_phase(lua)?;
            let name: String = spec.get("name").map_err(|_| mlua::Error::runtime("app 'name' (string) is required"))?;
            if name.trim().is_empty() { return Err(mlua::Error::runtime("app name must not be empty")); }
            let view: Function = spec.get("view").map_err(|_| mlua::Error::runtime("app 'view' (function) is required"))?;
            let on_key: Option<Function> = spec.get("on_key")?;
            let title: Option<String> = spec.get("title")?;
            let keymap: Option<String> = spec.get("keymap")?;
            let slot: Option<String> = spec.get("slot")?;
            if slot.as_deref().is_some_and(|slot| !matches!(slot, "sidebar" | "overlay")) {
                return Err(mlua::Error::runtime("app slot must be sidebar or overlay"));
            }
            let exists = declared.get::<Option<bool>>(name.as_str())?.unwrap_or(false);
            if exists != replacing {
                return Err(mlua::Error::runtime(if replacing {
                    format!("unknown app: {name}")
                } else { format!("app already registered: {name}; use replace_app") }));
            }
            let pending: Table = lua.globals().get("__rness_pending")?;
            let apps: Table = pending.get("apps")?;
            let entry = lua.create_table()?;
            entry.set("name", name.clone())?;
            entry.set("view", owned_callback(lua, view)?)?;
            entry.set("on_key", on_key.map(|f| owned_callback(lua, f)).transpose()?)?;
            entry.set("title", title)?;
            entry.set("keymap", keymap)?;
            entry.set("slot", slot)?;
            apps.push(entry)?;
            declared.set(name, true)?;
            Ok(())
        })?)?;
    }
    rness.set("ui", ui)?;

    // rness.keymaps — rebind the HOST's keys (scroll, quit…). Chords
    // and action names are validated host-side against the live table;
    // bad binds are rejected loudly in the host log. `set(chord, false)`
    // unbinds. Distinct from ui.app keymap= (app toggle keys).
    let keymaps = lua.create_table()?;
    keymaps.set(
        "set",
        lua.create_function(|lua, (chord, action): (String, LuaValue)| {
            require_declaration_phase(lua)?;
            let action = match action {
                LuaValue::String(s) => Some(s.to_string_lossy().to_string()),
                LuaValue::Boolean(false) | LuaValue::Nil => None,
                _ => {
                    return Err(mlua::Error::runtime(
                        "rness.keymaps.set: action must be a string or false",
                    ))
                }
            };
            let pending: Table = lua.globals().get("__rness_pending")?;
            let binds: Table = pending.get("keymaps")?;
            let entry = lua.create_table()?;
            entry.set("chord", chord)?;
            entry.set("action", action)?;
            binds.push(entry)?;
            Ok(())
        })?,
    )?;
    rness.set("keymaps", keymaps)?;

    // rness.log.{info,warn,error}(msg)
    let log = lua.create_table()?;
    for (level, f) in [
        ("info", lua.create_function(|_, msg: String| {
            tracing::info!(target: "lua", "{msg}");
            Ok(())
        })?),
        ("warn", lua.create_function(|_, msg: String| {
            tracing::warn!(target: "lua", "{msg}");
            Ok(())
        })?),
        ("error", lua.create_function(|_, msg: String| {
            tracing::error!(target: "lua", "{msg}");
            Ok(())
        })?),
    ] {
        log.set(level, f)?;
    }
    rness.set("log", log)?;

    // rness.json.{encode,decode}
    let json = lua.create_table()?;
    json.set(
        "encode",
        lua.create_function(|lua, v: LuaValue| {
            let j: serde_json::Value = lua.from_value(v)?;
            Ok(j.to_string())
        })?,
    )?;
    json.set(
        "decode",
        lua.create_function(|lua, s: String| {
            let j: serde_json::Value =
                serde_json::from_str(&s).map_err(mlua::Error::external)?;
            lua.to_value(&j)
        })?,
    )?;
    rness.set("json", json)?;

    crate::api::fs::install(lua, &rness)?;
    crate::api::http::install(lua, &rness)?;
    crate::api::process::install(lua, &rness)?;

    lua.globals().set("rness", rness)?;
    Ok(())
}

/// Tool return value → string output (strings pass through; tables are
/// JSON-encoded; everything else via tostring semantics).
fn lua_display(lua: &Lua, v: LuaValue) -> Result<String, LuaError> {
    Ok(match v {
        LuaValue::String(s) => s.to_string_lossy().to_string(),
        LuaValue::Table(_) => {
            let j: serde_json::Value = lua.from_value(v)?;
            j.to_string()
        }
        other => format!("{other:?}"),
    })
}

/// A Lua error's message without the Rust wrapper noise.
fn user_message(e: &mlua::Error) -> String {
    match e {
        mlua::Error::RuntimeError(m) => m.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_registers_and_executes() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "test",
            r#"
            rness.tool.register{
              name = "greet",
              description = "greets",
              schema = { type = "object", properties = { who = { type = "string" } } },
              run = function(args) return "hola " .. args.who end,
            }
            "#,
        )
        .unwrap();

        let specs = rt.tool_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "greet");
        assert_eq!(specs[0].input_schema["type"], "object");
        assert!(!specs[0].sensitive);

        let out = rt.call_tool("greet", &json!({"who": "mundo"})).unwrap();
        assert_eq!(out, Ok("hola mundo".into()));
    }

    #[test]
    fn tool_error_is_err_result_not_crash() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "test",
            r#"rness.tool.register{ name = "boom", run = function() error("kaput") end }"#,
        )
        .unwrap();
        let out = rt.call_tool("boom", &json!({})).unwrap();
        let err = out.unwrap_err();
        assert!(err.contains("kaput"), "{err}");
    }

    #[test]
    fn tool_table_result_becomes_json() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "test",
            r#"rness.tool.register{ name = "t", run = function() return { ok = true } end }"#,
        )
        .unwrap();
        assert_eq!(rt.call_tool("t", &json!({})).unwrap(), Ok(r#"{"ok":true}"#.into()));
    }

    #[test]
    fn hooks_fire_in_order_and_survive_errors() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "test",
            r#"
            seen = {}
            rness.hook.on("turn_start", function(p) seen[#seen+1] = "a:" .. p.session end)
            rness.hook.on("turn_start", function(p) error("broken hook") end)
            rness.hook.on("turn_start", function(p) seen[#seen+1] = "b" end)
            rness.hook.on("other", function(p) seen[#seen+1] = "never" end)
            "#,
        )
        .unwrap();

        let errors = rt.fire_hook("turn_start", &json!({"session": "s1"}));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("broken hook"));

        rt.load("check", r#"assert(seen[1] == "a:s1"); assert(seen[2] == "b"); assert(#seen == 2)"#)
            .unwrap();
    }

    #[test]
    fn statusline_provider_is_queried() {
        let mut rt = LuaRuntime::new().unwrap();
        assert_eq!(rt.statusline(), None);
        rt.load("test", r#"rness.ui.statusline(function() return "⚡ lua" end)"#).unwrap();
        assert_eq!(rt.statusline(), Some("⚡ lua".into()));
    }

    #[test]
    fn models_registry_declares_gets_and_lists() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "test",
            r#"
            -- Undeclared -> nil (rness ships zero model data).
            assert(rness.models.get("anthropic/claude-x") == nil)
            rness.models.declare("b/two", { context_window = 100 })
            rness.models.declare("a/one", { context_window = 200000, reasoning = { "high" } })
            local caps = rness.models.get("a/one")
            assert(caps.context_window == 200000)
            assert(caps.reasoning[1] == "high")
            -- Redeclaration replaces (hot reload rebuilds the registry).
            rness.models.declare("b/two", { context_window = 999 })
            assert(rness.models.get("b/two").context_window == 999)
            local names = rness.models.list()
            assert(#names == 2 and names[1] == "a/one" and names[2] == "b/two")
            "#,
        )
        .unwrap();
    }

    #[test]
    fn tool_replacement_is_explicit_and_unknown_names_fail() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("first", r#"
            rness.tool.register { name='x', run=function() return 'old' end }
            assert(not pcall(rness.tool.register, {name='x', run=function() return 'bad' end}))
            assert(not pcall(rness.tool.replace, {name='absent', run=function() end}))
        "#).unwrap();
        rt.load("second", r#"
            rness.tool.replace { name='x', run=function() return 'new' end }
        "#).unwrap();
        assert_eq!(rt.call_tool("x", &serde_json::json!({})).unwrap().unwrap(), "new");
        assert_eq!(rt.tool_specs().len(), 1);
    }

    #[test]
    fn hook_disposal_is_scoped_and_safe_during_dispatch() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("hooks", r#"
            seen = ''
            local off_first, off_second
            off_first = rness.hook.on('test', function()
                seen = seen .. 'a'
                assert(off_first())
                assert(not off_first())
                assert(off_second())
                rness.hook.on('test', function() seen = seen .. 'd' end)
            end)
            off_second = rness.hook.on('test', function() seen = seen .. 'b' end)
            rness.hook.on('test', function() seen = seen .. 'c' end)
            rness.events.emit('test', {})
            assert(seen == 'ac', seen)
            rness.events.emit('test', {})
            assert(seen == 'accd', seen)
        "#).unwrap();
        assert!(rt.fire_hook("test", &serde_json::json!({})).is_empty());
        rt.load("check", "assert(seen == 'accdcd')").unwrap();
    }

    #[test]
    fn ui_replacement_is_explicit_and_validated() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("base", r#"
            rness.ui.app {name='panel', view=function() return {'old'} end, on_key=function() return true end}
            rness.ui.tool_card('*', function() return {'old'} end)
            assert(not pcall(rness.ui.app, {name='panel', view=function() return {} end}))
            assert(not pcall(rness.ui.tool_card, '*', function() return {} end))
            assert(not pcall(rness.ui.replace_app, {name='missing', view=function() return {} end}))
            assert(not pcall(rness.ui.replace_tool_card, 'missing', function() return {} end))
            assert(not pcall(rness.ui.replace_app, {name='panel', slot='invalid', view=function() return {} end}))
        "#).unwrap();
        assert_eq!(rt.app_view("panel", &serde_json::json!({})).unwrap(), vec!["old"]);
        rt.load("override", r#"
            rness.ui.replace_app {name='panel', title='New', view=function() return {'new'} end}
            rness.ui.replace_tool_card('*', function() return {'new'} end)
        "#).unwrap();
        assert_eq!(rt.app_view("panel", &serde_json::json!({})).unwrap(), vec!["new"]);
        assert_eq!(rt.app_key("panel", "x", &serde_json::json!({})).unwrap(), AppKeyOutcome::Pass);
        assert_eq!(rt.tool_card("any", &serde_json::json!({}), "", false).unwrap()[0].text, "new");
        assert_eq!(rt.app_specs().len(), 1);
    }

    #[test]
    fn unload_preserves_later_owners_and_releases_names() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("first", r#"
            seen = ''
            rness.tool.register{name='shared', run=function() return 'first' end}
            rness.ui.app{name='panel', view=function() return {'first'} end}
            rness.ui.tool_card('*', function() return {'first'} end)
            rness.ui.statusline(function() return 'first' end)
            rness.keymaps.set('ctrl+x', 'first')
            rness.hook.on('tick', function() seen = seen .. 'a' end)
        "#).unwrap();
        rt.load("second", r#"
            rness.tool.replace{name='shared', run=function() return 'second' end}
            rness.ui.replace_app{name='panel', view=function() return {'second'} end}
            rness.ui.replace_tool_card('*', function() return {'second'} end)
            rness.ui.statusline(function() return 'second' end)
            rness.keymaps.set('ctrl+x', 'second')
            rness.hook.on('tick', function() seen = seen .. 'b' end)
        "#).unwrap();
        assert!(rt.load("second", "error('must not execute')").unwrap_err().to_string().contains("already loaded"));
        assert!(rt.unload("first").unwrap());
        assert!(!rt.unload("first").unwrap());
        assert_eq!(rt.call_tool("shared", &serde_json::json!({})).unwrap().unwrap(), "second");
        assert_eq!(rt.app_view("panel", &serde_json::json!({})).unwrap(), vec!["second"]);
        assert_eq!(rt.tool_card("anything", &serde_json::json!({}), "", false).unwrap()[0].text, "second");
        assert_eq!(rt.statusline().as_deref(), Some("second"));
        assert_eq!(rt.keymap_binds(), vec![("ctrl+x".into(), Some("second".into()))]);
        assert!(rt.fire_hook("tick", &serde_json::json!({})).is_empty());
        rt.lua.load("assert(seen == 'b')").exec().unwrap();
        assert!(rt.unload("second").unwrap());
        assert!(rt.tool_specs().is_empty());
        assert!(rt.app_specs().is_empty());
        assert!(rt.statusline().is_none());
        assert!(rt.tool_card("anything", &serde_json::json!({}), "", false).is_none());
        assert!(rt.keymap_binds().is_empty());
        rt.fire_hook("tick", &serde_json::json!({}));
        rt.lua.load("assert(seen == 'b')").exec().unwrap();
        rt.load("first", r#"
            rness.tool.register{name='shared', run=function() return 'new' end}
            rness.ui.app{name='panel', view=function() return {} end}
            rness.ui.tool_card('*', function() return {} end)
        "#).unwrap();
    }

    #[test]
    fn unloading_replacement_does_not_restore_previous_implementation() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("original", r#"
            rness.tool.register{name='shared', run=function() return 'original' end}
            rness.keymaps.set('ctrl+x', 'original')
        "#).unwrap();
        rt.load("replacement", r#"
            rness.tool.replace{name='shared', run=function() return 'replacement' end}
            rness.keymaps.set('ctrl+x', 'replacement')
        "#).unwrap();
        assert!(rt.unload("replacement").unwrap());
        assert!(rt.tool_specs().is_empty());
        assert_eq!(rt.keymap_binds(), vec![("ctrl+x".into(), Some("original".into()))]);
        rt.load("new", "rness.tool.register{name='shared', run=function() return 'new' end}").unwrap();
        assert!(rt.unload("original").unwrap());
        assert_eq!(rt.call_tool("shared", &serde_json::json!({})).unwrap().unwrap(), "new");
        assert!(!rt.unload("missing").unwrap());
    }

    #[test]
    fn callbacks_keep_their_owner_and_reject_declarations() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("owner", r#"
            seen = 0
            rness.hook.on('parent', function()
                rness.hook.on('child', function() seen = seen + 1 end)
                assert(not pcall(rness.tool.register, {name='late', run=function() end}))
                assert(not pcall(rness.ui.statusline, function() return 'late' end))
                assert(not pcall(rness.keymaps.set, 'ctrl+x', 'late'))
                error('expected callback failure')
            end)
            rness.tool.register{name='subscribe', run=function()
                rness.hook.on('child', function() seen = seen + 10 end)
                return 'ok'
            end}
        "#).unwrap();
        assert_eq!(rt.fire_hook("parent", &serde_json::json!({})).len(), 1);
        rt.call_tool("subscribe", &serde_json::json!({})).unwrap().unwrap();
        rt.load("other", "rness.events.emit('parent'); rness.tool.register{name='other', run=function() end}").unwrap();
        rt.unload("other").unwrap();
        rt.fire_hook("child", &serde_json::json!({}));
        rt.lua.load("assert(seen == 12)").exec().unwrap();
        rt.unload("owner").unwrap();
        rt.fire_hook("child", &serde_json::json!({}));
        rt.lua.load("assert(seen == 12)").exec().unwrap();
        assert!(rt.tool_specs().is_empty());
    }

    #[test]
    fn declarations_snapshot_validated_values() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("snapshot", r#"
            local schema = {type='object', properties={value={type='string'}}}
            local tool = {name='snapshot', schema=schema, description='original',
                run=function() return 'original' end}
            rness.tool.register(tool)
            tool.name = 'mutated'
            tool.run = false
            tool.description = {}
            schema.properties.value.type = 'number'
            schema.properties.loop = schema
            local app = {name='panel', slot='sidebar', title='Original', keymap='ctrl+p',
                view=function() return {'original'} end, on_key=function() return true end}
            rness.ui.app(app)
            app.name = 'mutated'
            app.slot = 'invalid'
            app.view = false
            app.on_key = false
            app.title = {}
            app.keymap = {}
        "#).unwrap();
        let specs = rt.tool_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "snapshot");
        assert_eq!(specs[0].description, "original");
        assert_eq!(specs[0].input_schema["properties"]["value"]["type"], "string");
        assert!(specs[0].input_schema["properties"].get("loop").is_none());
        assert_eq!(rt.call_tool("snapshot", &serde_json::json!({})).unwrap().unwrap(), "original");
        let apps = rt.app_specs();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].slot, "sidebar");
        assert_eq!(apps[0].title, "Original");
        assert_eq!(apps[0].keymap.as_deref(), Some("ctrl+p"));
        assert_eq!(rt.app_view("panel", &serde_json::json!({})).unwrap(), vec!["original"]);
        assert_eq!(rt.app_key("panel", "x", &serde_json::json!({})).unwrap(), AppKeyOutcome::Consumed);
    }

    #[test]
    fn commands_snapshot_results_failure_cleanup_and_unload() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("commands", r#"
            local spec = {name='hello', description='Greeting', run=function(ctx)
                return {message='Hello' .. ctx.raw_input, data={session=ctx.session}}
            end}
            rness.commands.register(spec)
            spec.run = function() error('mutated') end
        "#).unwrap();
        let result = rt.call_command("hello", serde_json::json!({"session":"s", "raw_input":"  world"})).unwrap();
        assert_eq!(result.message, "Hello  world");
        assert_eq!(result.data["session"], "s");
        assert!(rt.load("duplicate", "rness.commands.register{name='hello', run=function() end}").is_err());
        assert!(rt.load("broken", "rness.commands.register{name='retry', run=function() end}; error('fail')").is_err());
        rt.load("next", "rness.commands.register{name='retry', run=function() return 'ok' end}").unwrap();
        assert!(rt.unload("commands").unwrap());
        assert!(rt.call_command("hello", serde_json::json!({})).is_err());
        assert_eq!(rt.call_command("retry", serde_json::json!({})).unwrap().message, "ok");
    }

    #[test]
    fn failed_chunk_does_not_leak_declarations_into_next_plugin() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("base", "rness.tool.register{name='base', run=function() return 'old' end}").unwrap();
        assert!(rt.load("broken", r#"
            rness.tool.replace{name='base', run=function() return 'bad' end}
            rness.tool.register{name='retry', run=function() return 'bad' end}
            rness.ui.app{name='panel', view=function() return {'bad'} end}
            rness.ui.tool_card('*', function() return {'bad'} end)
            rness.ui.statusline(function() return 'bad' end)
            error('failure after registrations')
        "#).is_err());
        rt.load("next", r#"
            rness.tool.register{name='retry', run=function() return 'good' end}
            rness.ui.app{name='panel', view=function() return {'good'} end}
            rness.ui.tool_card('*', function() return {'good'} end)
        "#).unwrap();
        assert_eq!(rt.call_tool("base", &serde_json::json!({})).unwrap().unwrap(), "old");
        assert_eq!(rt.call_tool("retry", &serde_json::json!({})).unwrap().unwrap(), "good");
        assert_eq!(rt.app_view("panel", &serde_json::json!({})).unwrap(), vec!["good"]);
        assert_eq!(rt.statusline(), None);
    }

    #[test]
    fn failed_load_cleans_only_new_hook_subscriptions() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load("base", "seen = ''; rness.hook.on('event', function() seen = seen .. 'a' end)").unwrap();
        assert!(rt.load("broken", r#"
            stale_unsubscribe = rness.hook.on('event', function() seen = seen .. 'b' end)
            local off = rness.hook.on('event', function() seen = seen .. 'c' end)
            off()
            rness.hook.on('new-event', function() error('leaked callback') end)
            error('load failed')
        "#).is_err());
        assert!(rt.fire_hook("event", &serde_json::json!({})).is_empty());
        assert!(rt.fire_hook("new-event", &serde_json::json!({})).is_empty());
        rt.load("check", "assert(seen == 'a'); assert(not stale_unsubscribe()); rness.events.emit('event'); assert(seen == 'aa')").unwrap();
    }

    #[test]
    fn missing_name_or_run_is_a_load_error() {
        let mut rt = LuaRuntime::new().unwrap();
        let e = rt.load("bad", r#"rness.tool.register{ run = function() end }"#).unwrap_err();
        assert!(e.to_string().contains("'name'"), "{e}");
        let e = rt.load("bad2", r#"rness.tool.register{ name = "x" }"#).unwrap_err();
        assert!(e.to_string().contains("'run'"), "{e}");
    }

    #[test]
    fn ui_app_registers_views_and_handles_keys() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "app",
            r#"
            local cursor = 1
            rness.ui.app{
              name = "picker",
              slot = "overlay",
              title = "pick",
              keymap = "ctrl+p",
              view = function(ctx)
                return { "session: " .. ctx.session, "cursor: " .. cursor }
              end,
              on_key = function(key, ctx)
                if key == "j" then cursor = cursor + 1; return true end
                if key == "enter" then
                  return { action = "session:switch", payload = { session = "s2" } }
                end
                if key == "q" then return "close" end
                return false
              end,
            }
            "#,
        )
        .unwrap();

        let specs = rt.app_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "picker");
        assert_eq!(specs[0].slot, "overlay");
        assert_eq!(specs[0].keymap.as_deref(), Some("ctrl+p"));

        let ctx = json!({"session": "s1"});
        assert_eq!(
            rt.app_view("picker", &ctx).unwrap(),
            vec!["session: s1", "cursor: 1"]
        );

        // Stateful: j advances the cursor, view reflects it.
        assert_eq!(rt.app_key("picker", "j", &ctx).unwrap(), AppKeyOutcome::Consumed);
        assert_eq!(rt.app_view("picker", &ctx).unwrap()[1], "cursor: 2");

        assert_eq!(
            rt.app_key("picker", "enter", &ctx).unwrap(),
            AppKeyOutcome::Action {
                name: "session:switch".into(),
                payload: json!({"session": "s2"})
            }
        );
        assert_eq!(rt.app_key("picker", "q", &ctx).unwrap(), AppKeyOutcome::Close);
        assert_eq!(rt.app_key("picker", "x", &ctx).unwrap(), AppKeyOutcome::Pass);
    }

    #[test]
    fn ui_app_requires_name_and_view() {
        let mut rt = LuaRuntime::new().unwrap();
        let e = rt.load("bad", r#"rness.ui.app{ view = function() return {} end }"#).unwrap_err();
        assert!(e.to_string().contains("'name'"), "{e}");
        let e = rt.load("bad2", r#"rness.ui.app{ name = "x" }"#).unwrap_err();
        assert!(e.to_string().contains("'view'"), "{e}");
    }

    #[test]
    fn events_emit_reaches_hook_listeners_in_vm() {
        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "a",
            r#"
            got = nil
            rness.hook.on("custom:ping", function(p) got = p.msg end)
            "#,
        )
        .unwrap();
        // Another plugin emits — cross-plugin messaging via the same table.
        rt.load("b", r#"rness.events.emit("custom:ping", { msg = "hola" })"#).unwrap();
        rt.load("check", r#"assert(got == "hola")"#).unwrap();

        // Host-fired hooks land on the same listeners.
        let errs = rt.fire_hook("custom:ping", &json!({"msg": "otra"}));
        assert!(errs.is_empty());
        rt.load("check2", r#"assert(got == "otra")"#).unwrap();
    }

    #[test]
    fn fs_namespace_reads_writes_lists() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().display().to_string();
        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "test",
            &format!(
                r#"
                local base = "{base}"
                rness.fs.write(base .. "/sub/x.txt", "contenido")
                assert(rness.fs.exists(base .. "/sub/x.txt"))
                assert(rness.fs.read(base .. "/sub/x.txt") == "contenido")
                local names = rness.fs.list(base)
                assert(names[1] == "sub")
                assert(not rness.fs.exists(base .. "/nope"))
                "#
            ),
        )
        .unwrap();
    }

    #[test]
    fn fs_read_error_includes_path() {
        let mut rt = LuaRuntime::new().unwrap();
        let e = rt.load("t", r#"rness.fs.read("/nonexistent/rness/file")"#).unwrap_err();
        assert!(e.to_string().contains("/nonexistent/rness/file"), "{e}");
    }

    #[test]
    fn json_roundtrip_helpers() {        let mut rt = LuaRuntime::new().unwrap();
        rt.load(
            "test",
            r#"
            local t = rness.json.decode('{"a": [1, 2]}')
            assert(t.a[2] == 2)
            encoded = rness.json.encode({ b = "x" })
            "#,
        )
        .unwrap();
        rt.load("check", r#"assert(encoded == '{"b":"x"}')"#).unwrap();
    }

    #[test]
    fn questions_enable_disable_and_unload_ownership() {
        use rness_engine::questions::Questions;
        use std::sync::Arc;

        let mut rt = LuaRuntime::new().unwrap();
        let qs = Arc::new(Questions::default());
        rt.install_questions(qs.clone()).unwrap();

        // Not available by default.
        assert!(!qs.is_available());

        // Plugin enables questions — becomes the owner.
        rt.load("asker", "rness.questions.enable()").unwrap();
        assert!(qs.is_available());
        assert_eq!(qs.owner().as_deref(), Some("asker"));

        // Config is applied.
        rt.load("custom", "rness.questions.enable { height = 30, title = 'Custom' }").unwrap();
        assert!(qs.is_available());
        let config = qs.overlay_config();
        assert_eq!(config.height, 30);
        assert_eq!(config.title, "Custom");
        assert_eq!(qs.owner().as_deref(), Some("custom"));

        // Unloading a non-owner does NOT disable questions.
        assert!(rt.unload("asker").unwrap());
        assert!(qs.is_available());

        // Unloading the owner DOES disable questions.
        assert!(rt.unload("custom").unwrap());
        assert!(!qs.is_available());
        assert!(qs.owner().is_none());

        // Explicit disable from Lua.
        rt.load("re-enable", "rness.questions.enable()").unwrap();
        assert!(qs.is_available());
        rt.load("off", "rness.questions.disable()").unwrap();
        assert!(!qs.is_available());
        assert!(qs.owner().is_none());

        // Validation: bad config.
        assert!(rt.load("bad", "rness.questions.enable { height = 5 }").is_err());
        assert!(rt.load("bad2", "rness.questions.enable { title = '' }").is_err());
    }
}
