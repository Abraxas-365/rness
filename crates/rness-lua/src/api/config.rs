//! Startup-only declarations. Evaluated before any provider is constructed.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use mlua::{Lua, LuaSerdeExt, Table};
use rness_engine::config::{ModelDeclaration, ModelRegistry, Profile};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuestionOverlayConfig { pub enabled: bool, pub priority: i32, pub height: u16, pub title: String }
impl Default for QuestionOverlayConfig { fn default() -> Self { Self { enabled: true, priority: 99, height: 20, title: "AskUser".into() } } }

#[derive(Clone, Default)]
pub struct StartupConfig {
    pub promptbox: serde_json::Value,
    pub permissions: BTreeMap<String, rness_engine::approval::ToolPolicy>,
    pub plugins: Vec<String>,
    pub colorschemes: BTreeMap<String, serde_json::Value>,
    pub colorscheme: Option<String>,
    pub models: ModelRegistry,
    pub providers: BTreeMap<String, ProviderDeclaration>,
    pub stream_idle_timeouts: BTreeMap<String, u64>,
    pub default_profile: Option<String>,
    pub agents: BTreeMap<String, rness_engine::config::AgentDefinition>,
    pub question_overlay: QuestionOverlayConfig,
    pub ask_user: bool,
    pub default_agent: Option<String>,
}

#[cfg(test)]
mod permission_tests {
    #[test]
    fn stream_timeouts_are_opt_in_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        assert!(super::load(&path).unwrap().stream_idle_timeouts.is_empty());
        for source in ["rness.providers.set_stream_idle_timeout('anthropic', -1)", "rness.providers.set_stream_idle_timeout('anthropic', 2147483648)", "rness.providers.set_stream_idle_timeout('', 500)"] {
            std::fs::write(&path, source).unwrap();
            assert!(super::load(&path).is_err());
        }
        std::fs::write(&path, "rness.providers.set_stream_idle_timeout('anthropic', 500)").unwrap();
        assert_eq!(super::load(&path).unwrap().stream_idle_timeouts["anthropic"], 500);
        std::fs::write(&path, "rness.providers.set_stream_idle_timeout('anthropic', 0)").unwrap();
        assert_eq!(super::load(&path).unwrap().stream_idle_timeouts["anthropic"], 0);
    }

    #[test]
    fn rules_are_explicit_validated_and_replace_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        std::fs::write(&path, "rness.permissions.set { Read = 'ask', Bash = 'deny' }").unwrap();
        let config = super::load(&path).unwrap();
        assert_eq!(config.permissions["Read"], rness_engine::approval::ToolPolicy::Ask);
        assert_eq!(config.permissions["Bash"], rness_engine::approval::ToolPolicy::Deny);
        for source in ["rness.permissions.set { Read = 'alow' }", "rness.permissions.set { ['*'] = 'deny' }"] {
            std::fs::write(&path, source).unwrap();
            assert!(super::load(&path).is_err());
        }
        std::fs::write(&path, "rness.permissions.set { Bash = 'deny' }; rness.permissions.set {}").unwrap();
        assert!(super::load(&path).unwrap().permissions.is_empty());
        std::fs::write(&path, "").unwrap();
        assert!(super::load(&path).unwrap().permissions.is_empty());
    }
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDeclaration {
    pub protocol: String,
    pub base_url: String,
    pub auth: ProviderAuth,
}

#[derive(Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum ProviderAuth {
    Disabled(bool),
    Store { credential: String },
    Env { env: String },
    OAuth { oauth: String },
}

pub fn load(path: &std::path::Path) -> Result<StartupConfig, Box<dyn std::error::Error + Send + Sync>> {
    if !path.is_file() { return Ok(StartupConfig::default()); }
    let lua = Lua::new();
    lua.globals().set("rness", lua.create_table()?)?;
    evaluate(&lua, path)
}

pub fn evaluate(lua: &Lua, path: &std::path::Path) -> Result<StartupConfig, Box<dyn std::error::Error + Send + Sync>> {
    let state = Arc::new(Mutex::new(StartupConfig::default()));
    let ui = match lua.globals().get::<Table>("rness")?.get::<Option<Table>>("ui")? { Some(ui) => ui, None => lua.create_table()? };
    let schemes = lua.create_table()?;
    let registered = state.clone();
    schemes.set("register", lua.create_function(move |lua, (name, spec): (String, Table)| {
        crate::loader::validate_name(&name).map_err(mlua::Error::runtime)?;
        let value = lua.from_value(mlua::Value::Table(spec))?;
        let mut config = registered.lock().unwrap();
        if name == "default" || config.colorschemes.contains_key(&name) { return Err(mlua::Error::runtime("duplicate colorscheme")); }
        config.colorschemes.insert(name, value);
        Ok(())
    })?)?;
    let selected = state.clone();
    schemes.set("set", lua.create_function(move |_, name: String| {
        let mut config = selected.lock().unwrap();
        if name != "default" && !config.colorschemes.contains_key(&name) { return Err(mlua::Error::runtime("unknown colorscheme")); }
        config.colorscheme = Some(name);
        Ok(())
    })?)?;
    ui.set("colorscheme", schemes)?;
    lua.globals().get::<Table>("rness")?.set("ui", ui)?;
    let rness: Table = lua.globals().get("rness")?;
    let questions = lua.create_table()?;
    let s = state.clone();
    questions.set("enable", lua.create_function(move |lua, options: Option<Table>| {
        let config: QuestionOverlayConfig = match options { Some(table) => lua.from_value(mlua::Value::Table(table))?, None => Default::default() };
        if config.height < 10 || config.title.trim().is_empty() { return Err(mlua::Error::runtime("questions height must be >= 10 and title nonempty")); }
        let mut state = s.lock().unwrap(); state.ask_user = true; state.question_overlay = config; Ok(())
    })?)?;
    rness.set("questions", questions)?;
    let permissions = lua.create_table()?;
    let s = state.clone();
    permissions.set("set", lua.create_function(move |lua, value: Table| {
        let rules: BTreeMap<String, rness_engine::approval::ToolPolicy> = lua.from_value(mlua::Value::Table(value))?;
        if rules.keys().any(|name| name.is_empty() || name.trim() != name || name.contains('*')) {
            return Err(mlua::Error::runtime("permissions require exact, nonempty tool names (no wildcards)"));
        }
        s.lock().unwrap().permissions = rules;
        Ok(())
    })?)?;
    rness.set("permissions", permissions)?;
    let plugins = lua.create_table()?;
    let s = state.clone();
    plugins.set("load", lua.create_function(move |_, name: String| {
        crate::loader::validate_name(&name).map_err(mlua::Error::runtime)?;
        let mut state = s.lock().unwrap();
        if state.plugins.contains(&name) {
            return Err(mlua::Error::runtime(format!("duplicate plugin: {name}")));
        }
        state.plugins.push(name);
        Ok(())
    })?)?;
    rness.set("plugins", plugins)?;
    let models = lua.create_table()?;
    let s = state.clone();
    models.set("declare", lua.create_function(move |lua, value: Table| {
        let declaration: ModelDeclaration = lua.from_value(mlua::Value::Table(value))?;
        s.lock().unwrap().models.declare_model(declaration).map_err(mlua::Error::runtime)
    })?)?;
    rness.set("models", models)?;
    let profiles = lua.create_table()?;
    let s = state.clone();
    profiles.set("declare", lua.create_function(move |lua, (name, value): (String, Table)| {
        let profile: Profile = lua.from_value(mlua::Value::Table(value))?;
        s.lock().unwrap().models.declare_profile(name, profile).map_err(mlua::Error::runtime)
    })?)?;
    rness.set("profiles", profiles)?;
    let providers = lua.create_table()?;
    let s = state.clone();
    providers.set("set_stream_idle_timeout", lua.create_function(move |_, (name, milliseconds): (String, u64)| {
        if name.trim().is_empty() || milliseconds > 2_147_483_647 {
            return Err(mlua::Error::runtime("provider name must be nonempty and timeout must be 0..2147483647 milliseconds"));
        }
        s.lock().unwrap().stream_idle_timeouts.insert(name, milliseconds);
        Ok(())
    })?)?;
    let s = state.clone();
    providers.set("register", lua.create_function(move |lua, (name, value): (String, Table)| {
        let declaration: ProviderDeclaration = lua.from_value(mlua::Value::Table(value))?;
        if name.is_empty() || name.contains('/') { return Err(mlua::Error::runtime("invalid provider name")); }
        if matches!(declaration.auth, ProviderAuth::Disabled(true)) {
            return Err(mlua::Error::runtime("auth must be false or {credential = name}"));
        }
        let mut s = s.lock().unwrap();
        if s.providers.contains_key(&name) { return Err(mlua::Error::runtime("duplicate provider")); }
        s.providers.insert(name, declaration);
        Ok(())
    })?)?;
    let agents = lua.create_table()?;
    let s = state.clone();
    agents.set("declare", lua.create_function(move |lua, (name, value): (String, Table)| {
        let definition: rness_engine::config::AgentDefinition = lua.from_value(mlua::Value::Table(value))?;
        if name.trim().is_empty() || definition.description.trim().is_empty() || definition.instructions.trim().is_empty() {
            return Err(mlua::Error::runtime("agent name, description and instructions must not be empty"));
        }
        let mut state = s.lock().unwrap();
        if state.agents.contains_key(&name) { return Err(mlua::Error::runtime("duplicate agent")); }
        state.agents.insert(name, definition);
        Ok(())
    })?)?;
    rness.set("agents", agents)?;
    rness.set("providers", providers)?;
    lua.globals().set("rness", rness.clone())?;
    if let Some(root) = path.parent() {
        let package: Table = lua.globals().get("package")?;
        let existing: String = package.get("path")?;
        package.set("path", format!("{}/lua/?.lua;{}/lua/?/init.lua;{existing}", root.display(), root.display()))?;
    }
    if path.is_file() {
        lua.load(std::fs::read_to_string(path)?).set_name(path.to_string_lossy()).exec()?;
    }
    if let Some(promptbox) = rness.get::<Table>("ui")?.get::<Option<Table>>("promptbox")? {
        let value: serde_json::Value = lua.from_value(mlua::Value::Table(promptbox))?;
        let valid_keys = |field: &serde_json::Value, name: &str| field.as_object().is_some_and(|keys| keys.iter().all(|(key, chord)| key == name && (chord == &serde_json::Value::Bool(false) || chord.as_str().is_some_and(|s| !s.is_empty()))));
        for (key, field) in value.as_object().ok_or("ui.promptbox must be a table")? {
            match key.as_str() {
                "editor" if field.as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|s| s.as_str().is_some_and(|s| !s.is_empty()))) => {},
                "keys" if valid_keys(field, "edit") => {},
                "paste" => {
                    for (key, field) in field.as_object().ok_or("ui.promptbox.paste must be a table")? {
                        match key.as_str() {
                            "lines" | "chars" if field.as_u64().is_some() => {},
                            "keys" if valid_keys(field, "preview") => {},
                            _ => return Err(format!("invalid ui.promptbox.paste option: {key}").into()),
                        }
                    }
                }
                _ => return Err(format!("invalid ui.promptbox option: {key}").into()),
            }
        }
        state.lock().unwrap().promptbox = value;
    }
    for (table, method) in [("providers", "set_stream_idle_timeout"), ("plugins", "load"), ("agents", "declare"), ("providers", "register"), ("profiles", "declare"), ("models", "declare")] {
        let table: Table = rness.get(table)?;
        table.set(method, lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
            Err(mlua::Error::runtime("startup declarations are closed; edit init.lua and restart"))
        })?)?;
    }
    let mut config = state.lock().unwrap().clone();
    config.default_agent = rness.get("default_agent")?;
    if config.default_agent.as_ref().is_some_and(|name| !config.agents.contains_key(name)) {
        return Err(std::io::Error::other("unknown default_agent").into());
    }
    for agent in config.agents.values() {
        if let Some(profile) = &agent.profile {
            config.models.resolve_profile(profile).map_err(std::io::Error::other)?;
        }
    }
    config.default_profile = rness.get("default_profile")?;
    if let Some(name) = &config.default_profile { config.models.resolve_profile(name).map_err(std::io::Error::other)?; }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn startup_registry_reaches_plugins_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.lua");
        std::fs::write(&path, r#"
            rness.providers.register('local', {
                protocol = 'openai-chat', base_url = 'http://localhost:11434/v1', auth = false,
            })
            rness.models.declare {provider='local', model='qwen', capabilities={max_output_tokens=8000}}
            rness.profiles.declare('work', {provider='local', model='qwen', options={max_output_tokens=4000}})
            rness.default_profile = 'work'
        "#).unwrap();
        let config = load(&path).unwrap();
        let host = crate::plugin_host::LuaHost::spawn_with_config(config).unwrap();
        let check = r#"
            assert(rness.default_profile == 'work')
            local c = rness.models.capabilities('local', 'qwen')
            assert(c.max_output_tokens == 8000)
            c.max_output_tokens = 1
            assert(rness.models.capabilities('local', 'qwen').max_output_tokens == 8000)
            assert(rness.profiles.resolve('work').max_output_tokens == 4000)
        "#;
        host.load("check", check).await.unwrap();
        assert!(host.reload(vec![crate::loader::PluginSource {
            name: "check".into(), source: check.into(),
        }]).await.unwrap().is_empty());
    }

    #[test]
    fn shipped_startup_example_loads_without_credentials() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/init.lua");
        let config = load(&path).unwrap();
        assert_eq!(config.providers.len(), 4);
        assert_eq!(config.models.resolve_profile("local-qwen").unwrap()
            .selection.unwrap().route, "ollama");
        assert!(config.models.resolve_profile("router-sonnet").is_ok());
        assert!(config.default_profile.is_none());
    }

    #[test]
    fn paste_configuration_is_validated_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        std::fs::write(&path, "rness.ui.promptbox = { paste={lines=5, chars=500, keys={preview=false}}, keys={edit='ctrl+x'}, editor={'nvim','--clean'} }").unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.promptbox["editor"][0], "nvim");
        for source in ["rness.ui.promptbox = { paste={lines=-1} }", "rness.ui.promptbox = { editor='nvim' }", "rness.ui.promptbox = { keys={preview='ctrl+g'} }", "rness.ui.promptbox = { paste={keys={edit='ctrl+e'}} }"] {
            std::fs::write(&path, source).unwrap();
            assert!(load(&path).is_err());
        }
    }

    #[test]
    fn providers_module_loads_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("lua")).unwrap();
        std::fs::write(dir.path().join("lua/providers.lua"),
            include_str!("../../../../examples/lua/providers.lua")).unwrap();
        let init = dir.path().join("init.lua");
        std::fs::write(&init, "require('providers')").unwrap();
        let config = load(&init).unwrap();
        assert_eq!(config.providers.len(), 9);
        assert_eq!(config.providers["chatgpt"].protocol, "chatgpt-responses");
        assert!(matches!(&config.providers["chatgpt"].auth,
            ProviderAuth::OAuth { oauth } if oauth == "openai-chatgpt"));
        assert!(config.default_profile.is_none());
    }

    #[tokio::test]
    async fn init_runs_once_preserves_callbacks_and_loads_modules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("lua")).unwrap();
        std::fs::write(dir.path().join("lua/preferences.lua"), "rness.profiles.declare('local', {provider='ollama', model='qwen'})").unwrap();
        std::fs::write(dir.path().join("init.lua"), r#"
            require('preferences')
            executions = (executions or 0) + 1
            rness.hook.on('ready', function() ready = true end)
            rness.ui.statusline(function() return ready and 'ready' or 'booting' end)
        "#).unwrap();
        let (host, config) = crate::plugin_host::LuaHost::spawn_from_init(dir.path().join("init.lua")).unwrap();
        assert!(config.models.resolve_profile("local").is_ok());
        assert_eq!(host.statusline().await.as_deref(), Some("booting"));
        host.fire_hook("ready", serde_json::json!({}));
        assert_eq!(host.statusline().await.as_deref(), Some("ready"));
        assert!(host.reload(vec![]).await.unwrap().is_empty());
        assert_eq!(host.statusline().await.as_deref(), Some("ready"));
        host.load("check", "assert(executions == 1); assert(ready)").await.unwrap();
    }

    #[test]
    fn agent_delegation_is_explicit_and_profile_optional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        std::fs::write(&path, r#"
            rness.agents.declare('principal', {description='Main', instructions='Plan'})
            rness.agents.declare('worker', {description='Work', instructions='Implement', subagent=true})
            rness.default_agent = 'principal'
        "#).unwrap();
        let config = load(&path).unwrap();
        assert!(!config.agents["principal"].subagent);
        assert!(config.agents["worker"].subagent);
        assert!(config.agents["worker"].profile.is_none());
        assert_eq!(config.default_agent.as_deref(), Some("principal"));
    }

    #[tokio::test]
    async fn plugins_are_explicit_deferred_ordered_and_frozen() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("plugins")).unwrap();
        std::fs::write(dir.path().join("plugins/b.lua"), "assert(mounted); order = 'b'").unwrap();
        std::fs::write(dir.path().join("plugins/a.lua"), "assert(order == 'b'); order = order .. 'a'").unwrap();
        std::fs::write(dir.path().join("plugins/unselected.lua"), "error('must not run')").unwrap();
        std::fs::write(dir.path().join("init.lua"), "rness.plugins.load('b'); rness.plugins.load('a'); assert(order == nil)").unwrap();
        let (host, config) = crate::plugin_host::LuaHost::spawn_from_init(dir.path().join("init.lua")).unwrap();
        assert_eq!(config.plugins, vec!["b", "a"]);
        host.load("mount-check", "assert(order == nil); mounted = true; assert(not pcall(rness.plugins.load, 'late'))").await.unwrap();
        let sources = crate::loader::discover(dir.path(), &config.plugins).unwrap();
        assert!(crate::loader::load_all(&host, &sources).await.is_empty());
        host.load("check", "assert(order == 'ba')").await.unwrap();
        for script in ["rness.plugins.load('../bad')", "rness.plugins.load('a'); rness.plugins.load('a')"] {
            std::fs::write(dir.path().join("init.lua"), script).unwrap();
            assert!(load(&dir.path().join("init.lua")).is_err());
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let config = load(std::path::Path::new("/nonexistent/rness/config.lua")).unwrap();
        assert!(config.default_profile.is_none());
        assert!(config.providers.is_empty());
    }
}
