//! Startup-only declarations. Evaluated before any provider is constructed.
use mlua::{Lua, LuaSerdeExt, Table};
use rness_engine::config::{ModelDeclaration, ModelRegistry, Profile};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuestionOverlayConfig {
    pub enabled: bool,
    pub priority: i32,
    pub height: u16,
    pub title: String,
}
impl Default for QuestionOverlayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 99,
            height: 20,
            title: "AskUser".into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserMapping {
    pub scope: String,
    pub key: String,
    pub action: String,
}

#[derive(Clone, Default)]
pub struct StartupConfig {
    pub lsp: Option<rness_tools::lsp::Config>,
    pub web: Option<serde_json::Value>,
    pub tool_exposure: rness_engine::tools::exposure::Exposure,
    pub compaction: BTreeMap<String, rness_engine::turn::compaction::Policy>,
    pub mappings: Vec<UserMapping>,
    pub images: rness_engine::images::ImagePolicy,
    pub promptbox: serde_json::Value,
    pub messagebox: serde_json::Value,
    pub permissions: BTreeMap<String, rness_engine::approval::ToolPolicy>,
    pub plugins: Vec<String>,
    pub plugin_specs: Vec<crate::loader::PluginSpec>,
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
    fn web_configuration_registers_selected_backends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        for provider in ["exa", "perplexity", "deepseek"] {
            std::fs::write(&path, format!("rness.web = {{ fetch = {{timeout_ms=30000}}, search = {{provider='{provider}'}} }}")).unwrap();
            let startup = super::load(&path).unwrap();
            let config = serde_json::from_value(startup.web.unwrap()).unwrap();
            let registry = rness_engine::tools::ToolRegistry::default();
            rness_tools::web::register(&registry, config).unwrap();
            assert!(registry.get("web_search").is_some());
            assert!(registry.get("web_fetch").is_some());
        }
        std::fs::write(&path, "rness.web = {search={provider='brave'}}").unwrap();
        let startup = super::load(&path).unwrap();
        assert!(serde_json::from_value::<rness_tools::web::Config>(startup.web.unwrap()).is_err());
    }
    #[test]
    fn boundary_compaction_requires_explicit_valid_route_budgets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        let valid = "system_prompt='Summarize the conversation.', prompt='Preserve essential context.', threshold_tokens=1000, retain_tokens=160, summary_tokens=100, max_overflow_retries=1, max_compactions=2, prune_threshold=8192, prune_head=4096, prune_tail=1024";
        std::fs::write(
            &path,
            format!("rness.compaction = {{['test/model']={{{valid}}}}}"),
        )
        .unwrap();
        assert_eq!(
            super::load(&path).unwrap().compaction["test/model"].retain_tokens,
            160
        );
        for bad in [
            valid.replace("retain_tokens=160", "retain_tokens=1000"),
            valid.replace("summary_tokens=100", "summary_tokens=0"),
            format!("{valid}, typo=1"),
        ] {
            std::fs::write(
                &path,
                format!("rness.compaction = {{['test/model']={{{bad}}}}}"),
            )
            .unwrap();
            assert!(super::load(&path).is_err());
        }
    }

    #[test]
    fn stream_timeouts_are_opt_in_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        assert!(super::load(&path).unwrap().stream_idle_timeouts.is_empty());
        for source in [
            "rness.providers.set_stream_idle_timeout('anthropic', -1)",
            "rness.providers.set_stream_idle_timeout('anthropic', 2147483648)",
            "rness.providers.set_stream_idle_timeout('', 500)",
        ] {
            std::fs::write(&path, source).unwrap();
            assert!(super::load(&path).is_err());
        }
        std::fs::write(
            &path,
            "rness.providers.set_stream_idle_timeout('anthropic', 500)",
        )
        .unwrap();
        assert_eq!(
            super::load(&path).unwrap().stream_idle_timeouts["anthropic"],
            500
        );
        std::fs::write(
            &path,
            "rness.providers.set_stream_idle_timeout('anthropic', 0)",
        )
        .unwrap();
        assert_eq!(
            super::load(&path).unwrap().stream_idle_timeouts["anthropic"],
            0
        );
    }

    #[test]
    fn messagebox_all_sections_accept_supported_options() {
        use serde_json::json;
        let style = json!({"fg":"#abcdef","bg":"#123456","bold":true,"italic":false,"underline":true,"reverse":false});
        let padding = json!({"left":1,"right":2,"top":1,"bottom":2});
        let border = json!({"kind":"rounded","style":style});
        let message = json!({"visible":true,"display":"preview","preview_lines":2,"style":style,"padding":padding,"border":border,"marker":{"text":">","mode":"bar","style":"dim"},"label":{"text":"Label","style":"title"},"markdown":{"heading":style,"link":style,"quote":style,"inline_code":style,"code_block":{"style":style,"padding":padding,"show_language":true,"syntax_highlight":false}}});
        let tool = json!({"visible":false,"display":"expanded","preview_lines":3,"style":style,"padding":padding,"border":border,"spacing":1,"label":false,"header":{"visible":true,"show_name":true,"show_status":true,"show_duration":true,"style":"tool_name"},"arguments":{"visible":true,"wrap":false,"style":"dim"},"output":{"visible":true,"wrap":true,"style":"tool_output"}});
        let mut config = json!({"style":style,"padding":padding,"border":border,"spacing":1,"tool":tool,"compaction":tool,"tools":{"Read":tool},"keys":{"next_tool":false,"previous_tool":false,"toggle_tool":false,"toggle_thinking":false,"next_thinking":false,"previous_thinking":false}});
        for role in [
            "message",
            "user",
            "assistant",
            "thinking",
            "notice",
            "error",
        ] {
            config[role] = message.clone();
        }
        for state in ["running", "success", "error", "cancelled"] {
            config["tool"]["states"][state] = tool.clone();
        }
        config["keys"]["select_message"] = json!("alt+m");
        for action in [
            "selection_previous",
            "selection_next",
            "selection_scroll_up",
            "selection_scroll_down",
            "selection_copy",
            "selection_editor",
            "selection_toggle",
            "selection_stop",
            "selection_close",
        ] {
            for value in [json!("ctrl+f"), json!(false)] {
                config["keys"][action] = value;
                super::validate_messagebox(&config, "messagebox", "root").unwrap();
            }
            config["keys"][action] = json!(42);
            assert!(super::validate_messagebox(&config, "messagebox", "root").is_err());
            config["keys"][action] = json!(false);
        }
        config["selection"] = json!({"style":{"bg":"#504945"},"marker":{"text":"▎","style":{"fg":"#83a598","bold":true}}});
        super::validate_messagebox(&config, "messagebox", "root").unwrap();
        config["selection"]["marker"]["text"] = json!("\n");
        assert!(super::validate_messagebox(&config, "messagebox", "root").is_err());
        config["selection"]["marker"] = json!(false);
        config["editor"] = json!(["nvim", "-n"]);
        super::validate_messagebox(&config, "messagebox", "root").unwrap();
        for budget in [0, 8 * 1024 * 1024, 1024 * 1024 * 1024] {
            config["cache_bytes"] = json!(budget);
            super::validate_messagebox(&config, "messagebox", "root").unwrap();
        }
        for invalid in [json!(-1), json!(1.5), json!("1024"), json!(1073741825u64)] {
            let mut bad = config.clone();
            bad["cache_bytes"] = invalid;
            assert!(super::validate_messagebox(&bad, "messagebox", "root").is_err());
        }
        for section in ["tool", "message", "user"] {
            let mut bad = config.clone();
            bad[section]["visible"] = json!("false");
            assert!(super::validate_messagebox(&bad, "messagebox", "root").is_err());
        }
    }

    #[test]
    fn messagebox_configuration_separates_functions_and_validates_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        std::fs::write(
            &path,
            r##"
            rness.ui.messagebox = {
                padding={left=1}, user={style={fg='#ebdbb2',bg='#3c3836'},marker=false},
                tool={display='preview',preview_lines=8},
                tools={Read={render=function() return nil end,style='tool_output'}},
            }
        "##,
        )
        .unwrap();
        let config = super::load(&path).unwrap();
        assert_eq!(config.messagebox["user"]["style"]["bg"], "#3c3836");
        assert!(config.messagebox["tools"]["Read"].get("render").is_none());
        for source in [
            "rness.ui.messagebox={padding={left=-1}}",
            "rness.ui.messagebox={tool={collapsed=true}}",
            "rness.ui.messagebox={compaction={collapsed=true}}",
            "rness.ui.messagebox={tools={Read={render=3}}}",
            "rness.ui.messagebox={user={style={unknown=true}}}",
            "rness.ui.messagebox={keys={surprise='ctrl+x'}}",
        ] {
            std::fs::write(&path, source).unwrap();
            assert!(super::load(&path).is_err(), "{source}");
        }
    }

    #[test]
    fn rules_are_explicit_validated_and_replace_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        std::fs::write(
            &path,
            "rness.permissions.set { Read = 'ask', Bash = 'deny' }",
        )
        .unwrap();
        let config = super::load(&path).unwrap();
        assert_eq!(
            config.permissions["Read"],
            rness_engine::approval::ToolPolicy::Ask
        );
        assert_eq!(
            config.permissions["Bash"],
            rness_engine::approval::ToolPolicy::Deny
        );
        for source in [
            "rness.permissions.set { Read = 'alow' }",
            "rness.permissions.set { ['*'] = 'deny' }",
        ] {
            std::fs::write(&path, source).unwrap();
            assert!(super::load(&path).is_err());
        }
        std::fs::write(
            &path,
            "rness.permissions.set { Bash = 'deny' }; rness.permissions.set {}",
        )
        .unwrap();
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

fn validate_messagebox(value: &serde_json::Value, path: &str, section: &str) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{path} must be a table"))?;
    for (key, value) in object {
        let field = format!("{path}.{key}");
        let child = match (section, key.as_str()) {
            ("root", "message" | "user" | "assistant" | "thinking" | "notice" | "error") => {
                Some("message")
            }
            ("root", "tool" | "compaction") => Some("tool"),
            ("root", "tools") => Some("tools"),
            ("root", "selection") => Some("selection"),
            ("selection", "marker") if value != false => Some("selection_marker"),
            ("root", "keys") => Some("keys"),
            ("tools", _) => Some("tool"),
            ("root" | "message" | "tool" | "code", "padding") => Some("padding"),
            ("root" | "message" | "tool", "border") => Some("border"),
            ("message", "marker" | "label") | ("tool", "label") if value != false => {
                Some(if key == "marker" { "marker" } else { "label" })
            }
            ("message", "markdown") => Some("markdown"),
            ("markdown", "code_block") => Some("code"),
            ("tool", "header") => Some("header"),
            ("tool", "arguments" | "output") => Some("output"),
            ("tool", "states") => Some("states"),
            ("states", "running" | "success" | "error" | "cancelled") => Some("tool"),
            _ => None,
        };
        if let Some(child) = child {
            validate_messagebox(value, &field, child)?;
            continue;
        }
        let valid = match (section, key.as_str()) {
            (
                "root" | "selection" | "selection_marker" | "message" | "tool" | "border"
                | "marker" | "label" | "header" | "output" | "code",
                "style",
            )
            | ("markdown", "heading" | "link" | "quote" | "inline_code") => {
                if let Some(name) = value.as_str() {
                    !name.is_empty()
                } else if let Some(style) = value.as_object() {
                    style.iter().all(|(k, v)| match k.as_str() {
                        "fg" | "bg" => v.as_str().is_some_and(|s| !s.is_empty()),
                        "bold" | "italic" | "underline" | "reverse" => v.is_boolean(),
                        _ => false,
                    })
                } else {
                    false
                }
            }
            ("padding", "left" | "right" | "top" | "bottom") | ("root" | "tool", "spacing") => {
                value.as_u64().is_some_and(|n| n <= 64)
            }
            ("root", "cache_bytes") => value.as_u64().is_some_and(|n| n <= 1024 * 1024 * 1024),
            ("message" | "tool", "preview_lines") => {
                value.as_u64().is_some_and(|n| (1..=10000).contains(&n))
            }
            ("message" | "tool", "display") => value
                .as_str()
                .is_some_and(|s| matches!(s, "collapsed" | "preview" | "expanded")),
            ("border", "kind") => value
                .as_str()
                .is_some_and(|s| matches!(s, "none" | "plain" | "rounded" | "double")),
            ("marker", "mode") => value
                .as_str()
                .is_some_and(|s| matches!(s, "first_line" | "bar")),
            ("selection", "marker") => value == false,
            ("selection_marker", "text") => value.as_str().is_some_and(|s| {
                !s.is_empty() && s.len() <= 16 && !s.chars().any(char::is_control)
            }),
            ("marker" | "label", "text") => value
                .as_str()
                .is_some_and(|s| s.len() <= 256 && !s.chars().any(char::is_control)),
            ("message", "marker" | "label") | ("tool", "label") => value == false,
            ("message" | "tool" | "header" | "output", "visible")
            | ("header", "show_name" | "show_status" | "show_duration")
            | ("output", "wrap")
            | ("code", "show_language" | "syntax_highlight") => value.is_boolean(),
            ("root", "editor") => value.as_array().is_some_and(|args| {
                !args.is_empty()
                    && args
                        .iter()
                        .all(|arg| arg.as_str().is_some_and(|s| !s.is_empty()))
            }),
            (
                "keys",
                "selection_previous"
                | "selection_next"
                | "selection_scroll_up"
                | "selection_scroll_down"
                | "selection_copy"
                | "selection_editor"
                | "selection_toggle"
                | "selection_stop"
                | "selection_close"
                | "select_message"
                | "next_tool"
                | "previous_tool"
                | "toggle_tool"
                | "toggle_thinking"
                | "next_thinking"
                | "previous_thinking",
            ) => value == false || value.as_str().is_some_and(|s| !s.is_empty()),
            _ => false,
        };
        if !valid {
            return Err(format!("invalid messagebox option: {field}"));
        }
    }
    Ok(())
}

pub fn load(
    path: &std::path::Path,
) -> Result<StartupConfig, Box<dyn std::error::Error + Send + Sync>> {
    if !path.is_file() {
        return Ok(StartupConfig::default());
    }
    let lua = Lua::new();
    lua.globals().set("rness", lua.create_table()?)?;
    evaluate(&lua, path)
}

pub fn evaluate(
    lua: &Lua,
    path: &std::path::Path,
) -> Result<StartupConfig, Box<dyn std::error::Error + Send + Sync>> {
    let state = Arc::new(Mutex::new(StartupConfig::default()));
    let ui = match lua
        .globals()
        .get::<Table>("rness")?
        .get::<Option<Table>>("ui")?
    {
        Some(ui) => ui,
        None => lua.create_table()?,
    };
    let schemes = lua.create_table()?;
    let registered = state.clone();
    schemes.set(
        "register",
        lua.create_function(move |lua, (name, spec): (String, Table)| {
            crate::loader::validate_name(&name).map_err(mlua::Error::runtime)?;
            let value = lua.from_value(mlua::Value::Table(spec))?;
            let mut config = registered.lock().unwrap();
            if name == "default" || config.colorschemes.contains_key(&name) {
                return Err(mlua::Error::runtime("duplicate colorscheme"));
            }
            config.colorschemes.insert(name, value);
            Ok(())
        })?,
    )?;
    let selected = state.clone();
    schemes.set(
        "set",
        lua.create_function(move |_, name: String| {
            let mut config = selected.lock().unwrap();
            if name != "default" && !config.colorschemes.contains_key(&name) {
                return Err(mlua::Error::runtime("unknown colorscheme"));
            }
            config.colorscheme = Some(name);
            Ok(())
        })?,
    )?;
    ui.set("colorscheme", schemes)?;
    lua.globals().get::<Table>("rness")?.set("ui", ui)?;
    let rness: Table = lua.globals().get("rness")?;
    let questions = lua.create_table()?;
    let s = state.clone();
    questions.set(
        "enable",
        lua.create_function(move |lua, options: Option<Table>| {
            let config: QuestionOverlayConfig = match options {
                Some(table) => lua.from_value(mlua::Value::Table(table))?,
                None => Default::default(),
            };
            if config.height < 10 || config.title.trim().is_empty() {
                return Err(mlua::Error::runtime(
                    "questions height must be >= 10 and title nonempty",
                ));
            }
            let mut state = s.lock().unwrap();
            state.ask_user = true;
            state.question_overlay = config;
            Ok(())
        })?,
    )?;
    rness.set("questions", questions)?;
    let permissions = lua.create_table()?;
    let s = state.clone();
    permissions.set(
        "set",
        lua.create_function(move |lua, value: Table| {
            let rules: BTreeMap<String, rness_engine::approval::ToolPolicy> =
                lua.from_value(mlua::Value::Table(value))?;
            if rules
                .keys()
                .any(|name| name.is_empty() || name.trim() != name || name.contains('*'))
            {
                return Err(mlua::Error::runtime(
                    "permissions require exact, nonempty tool names (no wildcards)",
                ));
            }
            s.lock().unwrap().permissions = rules;
            Ok(())
        })?,
    )?;
    rness.set("permissions", permissions)?;
    let plugins = lua.create_table()?;
    let s = state.clone();
    let callbacks = lua.create_table()?;
    lua.globals()
        .set("__rness_plugin_callbacks", callbacks.clone())?;
    let configured = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setup_configured = configured.clone();
    plugins.set(
        "setup",
        lua.create_function(move |lua, specs: Table| {
            use crate::loader::{PluginLocation, PluginSpec};
            if setup_configured.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(mlua::Error::runtime(
                    "plugins.setup may only be called once",
                ));
            }
            let mut selected = Vec::new();
            let mut functions = Vec::new();
            let mut names = std::collections::HashSet::new();
            let count = specs.raw_len();
            for pair in specs.clone().pairs::<mlua::Value, mlua::Value>() {
                let (key, _) = pair?;
                if !matches!(key, mlua::Value::Integer(i) if i > 0 && (i as usize) <= count) {
                    return Err(mlua::Error::runtime("plugins.setup expects a dense list"));
                }
            }
            for index in 1..=count {
                let spec: Table = specs.raw_get(index)?;
                for pair in spec.clone().pairs::<String, mlua::Value>() {
                    let (key, _) = pair?;
                    if !matches!(
                        key.as_str(),
                        "name"
                            | "file"
                            | "package"
                            | "config"
                            | "enabled"
                            | "watch"
                            | "opts"
                            | "keys"
                    ) {
                        return Err(mlua::Error::runtime(format!("unknown plugin field: {key}")));
                    }
                }
                let file: Option<String> = spec.get("file")?;
                let package: Option<String> = spec.get("package")?;
                let callback: Option<mlua::Function> = spec.get("config")?;
                if usize::from(file.is_some())
                    + usize::from(package.is_some())
                    + usize::from(callback.is_some())
                    != 1
                {
                    return Err(mlua::Error::runtime(
                        "plugin requires exactly one of file, package, config",
                    ));
                }
                let explicit_name: Option<String> = spec.get("name")?;
                if package.is_some() && explicit_name.is_some() {
                    return Err(mlua::Error::runtime(
                        "package plugins use their manifest name; omit name",
                    ));
                }
                let name = package
                    .clone()
                    .or(explicit_name)
                    .ok_or_else(|| mlua::Error::runtime("file and inline plugins require name"))?;
                crate::loader::validate_name(&name).map_err(mlua::Error::runtime)?;
                if !names.insert(name.clone()) {
                    return Err(mlua::Error::runtime(format!("duplicate plugin: {name}")));
                }
                let enabled = spec.get::<Option<bool>>("enabled")?.unwrap_or(true);
                let watch = spec.get::<Option<bool>>("watch")?.unwrap_or(false);
                if callback.is_some() && watch {
                    return Err(mlua::Error::runtime(
                        "inline plugins cannot be watched; restart after editing init.lua",
                    ));
                }
                let opts = match spec.get::<mlua::Value>("opts")? {
                    mlua::Value::Nil => serde_json::json!({}),
                    mlua::Value::Table(table) => lua.from_value(mlua::Value::Table(table))?,
                    _ => return Err(mlua::Error::runtime("plugin opts must be a table")),
                };
                let keys = match spec.get::<mlua::Value>("keys")? {
                    mlua::Value::Nil => serde_json::json!({}),
                    mlua::Value::Boolean(false) => serde_json::Value::Bool(false),
                    mlua::Value::Table(table) => lua.from_value(mlua::Value::Table(table))?,
                    _ => return Err(mlua::Error::runtime("plugin keys must be a table or false")),
                };
                let source = if let Some(file) = file {
                    if file.is_empty() || file.starts_with('~') {
                        return Err(mlua::Error::runtime(
                            "plugin file must be an explicit nonempty path; expand HOME yourself",
                        ));
                    }
                    PluginLocation::File(file.into())
                } else if let Some(package) = package {
                    PluginLocation::Package(package)
                } else {
                    PluginLocation::Inline
                };
                if let Some(callback) = callback {
                    functions.push((name.clone(), callback));
                }
                selected.push(PluginSpec {
                    name,
                    source,
                    enabled,
                    watch,
                    opts,
                    keys,
                });
            }
            let mut state = s.lock().unwrap();
            if !state.plugins.is_empty() {
                return Err(mlua::Error::runtime(
                    "do not mix plugins.setup and plugins.load",
                ));
            }
            for (name, callback) in functions {
                callbacks.set(name, callback)?;
            }
            state.plugin_specs = selected;
            setup_configured.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    let s = state.clone();
    plugins.set(
        "load",
        lua.create_function(move |_, name: String| {
            if configured.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(mlua::Error::runtime(
                    "do not mix plugins.setup and plugins.load",
                ));
            }
            crate::loader::validate_name(&name).map_err(mlua::Error::runtime)?;
            let mut state = s.lock().unwrap();
            if state.plugins.contains(&name) {
                return Err(mlua::Error::runtime(format!("duplicate plugin: {name}")));
            }
            state.plugins.push(name);
            Ok(())
        })?,
    )?;
    rness.set("plugins", plugins)?;
    let models = lua.create_table()?;
    let s = state.clone();
    models.set(
        "declare",
        lua.create_function(move |lua, value: Table| {
            let declaration: ModelDeclaration = lua.from_value(mlua::Value::Table(value))?;
            s.lock()
                .unwrap()
                .models
                .declare_model(declaration)
                .map_err(mlua::Error::runtime)
        })?,
    )?;
    rness.set("models", models)?;
    let profiles = lua.create_table()?;
    let s = state.clone();
    profiles.set(
        "declare",
        lua.create_function(move |lua, (name, value): (String, Table)| {
            let profile: Profile = lua.from_value(mlua::Value::Table(value))?;
            s.lock()
                .unwrap()
                .models
                .declare_profile(name, profile)
                .map_err(mlua::Error::runtime)
        })?,
    )?;
    rness.set("profiles", profiles)?;
    let providers = lua.create_table()?;
    let s = state.clone();
    providers.set(
        "set_stream_idle_timeout",
        lua.create_function(move |_, (name, milliseconds): (String, u64)| {
            if name.trim().is_empty() || milliseconds > 2_147_483_647 {
                return Err(mlua::Error::runtime(
                    "provider name must be nonempty and timeout must be 0..2147483647 milliseconds",
                ));
            }
            s.lock()
                .unwrap()
                .stream_idle_timeouts
                .insert(name, milliseconds);
            Ok(())
        })?,
    )?;
    let s = state.clone();
    providers.set(
        "register",
        lua.create_function(move |lua, (name, value): (String, Table)| {
            let declaration: ProviderDeclaration = lua.from_value(mlua::Value::Table(value))?;
            if name.is_empty() || name.contains('/') {
                return Err(mlua::Error::runtime("invalid provider name"));
            }
            if matches!(declaration.auth, ProviderAuth::Disabled(true)) {
                return Err(mlua::Error::runtime(
                    "auth must be false or {credential = name}",
                ));
            }
            let mut s = s.lock().unwrap();
            if s.providers.contains_key(&name) {
                return Err(mlua::Error::runtime("duplicate provider"));
            }
            s.providers.insert(name, declaration);
            Ok(())
        })?,
    )?;
    let agents = lua.create_table()?;
    let s = state.clone();
    agents.set(
        "declare",
        lua.create_function(move |lua, (name, value): (String, Table)| {
            let definition: rness_engine::config::AgentDefinition =
                lua.from_value(mlua::Value::Table(value))?;
            if name.trim().is_empty()
                || definition.description.trim().is_empty()
                || definition.instructions.trim().is_empty()
            {
                return Err(mlua::Error::runtime(
                    "agent name, description and instructions must not be empty",
                ));
            }
            let mut state = s.lock().unwrap();
            if state.agents.contains_key(&name) {
                return Err(mlua::Error::runtime("duplicate agent"));
            }
            state.agents.insert(name, definition);
            Ok(())
        })?,
    )?;
    let keymap = lua.create_table()?;
    let mapping_state = state.clone();
    let configured = std::sync::atomic::AtomicBool::new(false);
    keymap.set(
        "setup",
        lua.create_function(move |lua, entries: Table| {
            if configured.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(mlua::Error::runtime("keymap.setup may only be called once"));
            }
            let mut mappings = Vec::new();
            for entry in entries.clone().sequence_values::<Table>() {
                let mut mapping: UserMapping = lua.from_value(mlua::Value::Table(entry?))?;
                if !matches!(
                    mapping.scope.as_str(),
                    "global" | "promptbox" | "messagebox"
                ) && !mapping
                    .scope
                    .strip_prefix("app:")
                    .is_some_and(|name| !name.is_empty())
                {
                    return Err(mlua::Error::runtime("invalid mapping scope"));
                }
                if mapping.action.is_empty() {
                    return Err(mlua::Error::runtime("mapping action is required"));
                }
                mapping.key = rness_kernel::presentation::canonical_chord(&mapping.key)
                    .map_err(mlua::Error::runtime)?;
                mappings.push(mapping);
            }
            if entries.pairs::<mlua::Value, mlua::Value>().count() != mappings.len() {
                return Err(mlua::Error::runtime("keymap.setup expects a dense list"));
            }
            mapping_state.lock().unwrap().mappings = mappings;
            configured.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    rness.set("keymap", keymap.clone())?;
    rness.set("agents", agents)?;
    rness.set("providers", providers)?;
    lua.globals().set("rness", rness.clone())?;
    if let Some(root) = path.parent() {
        let package: Table = lua.globals().get("package")?;
        let existing: String = package.get("path")?;
        package.set(
            "path",
            format!(
                "{}/lua/?.lua;{}/lua/?/init.lua;{existing}",
                root.display(),
                root.display()
            ),
        )?;
    }
    if path.is_file() {
        lua.load(std::fs::read_to_string(path)?)
            .set_name(path.to_string_lossy())
            .exec()?;
    }
    keymap.set(
        "setup",
        lua.create_function(|_, _: mlua::Value| -> mlua::Result<()> {
            Err(mlua::Error::runtime(
                "keymap.setup is startup-only; restart to change mappings",
            ))
        })?,
    )?;
    if let Some(lsp) = rness.get::<Option<Table>>("lsp")? {
        state.lock().unwrap().lsp = Some(lua.from_value(mlua::Value::Table(lsp))?);
    }
    if let Some(web) = rness.get::<Option<Table>>("web")? {
        let data = lua.create_table()?;
        for entry in web.pairs::<String, mlua::Value>() {
            let (key, value) = entry?;
            if matches!(key.as_str(), "search" | "fetch") {
                let mlua::Value::Table(section) = value else {
                    return Err("web sections must be tables".into());
                };
                let cleaned = lua.create_table()?;
                for entry in section.pairs::<String, mlua::Value>() {
                    let (field, value) = entry?;
                    if matches!(field.as_str(), "before" | "after") {
                        if !matches!(value, mlua::Value::Function(_)) {
                            return Err("web hooks must be functions".into());
                        }
                    } else {
                        cleaned.set(field, value)?;
                    }
                }
                data.set(key, cleaned)?;
            } else {
                data.set(key, value)?;
            }
        }
        state.lock().unwrap().web = Some(lua.from_value(mlua::Value::Table(data))?);
    }
    if let Some(exposure) = rness.get::<Option<Table>>("tool_exposure")? {
        state.lock().unwrap().tool_exposure = lua.from_value(mlua::Value::Table(exposure))?;
    }
    if let Some(policies) = rness.get::<Option<Table>>("compaction")? {
        let policies: BTreeMap<String, rness_engine::turn::compaction::Policy> =
            lua.from_value(mlua::Value::Table(policies))?;
        for (route, policy) in &policies {
            if route != "default"
                && !route
                    .split_once('/')
                    .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
            {
                return Err("compaction keys must be provider/model or default".into());
            }
            policy.validate().map_err(std::io::Error::other)?;
        }
        state.lock().unwrap().compaction = policies;
    }
    if let Some(images) = rness.get::<Option<Table>>("images")? {
        let policy: rness_engine::images::ImagePolicy =
            lua.from_value(mlua::Value::Table(images))?;
        policy.validate().map_err(std::io::Error::other)?;
        state.lock().unwrap().images = policy;
    }
    if let Some(messagebox) = rness
        .get::<Table>("ui")?
        .get::<Option<Table>>("messagebox")?
    {
        let data = lua.create_table()?;
        for entry in messagebox.pairs::<String, mlua::Value>() {
            let (key, value) = entry?;
            if key == "tools" {
                let mlua::Value::Table(tools) = value else {
                    return Err("ui.messagebox.tools must be a table".into());
                };
                let declarations = lua.create_table()?;
                for entry in tools.pairs::<String, mlua::Value>() {
                    let (name, spec) = entry?;
                    if name.trim().is_empty() {
                        return Err("ui.messagebox tool name must not be empty".into());
                    }
                    let options = lua.create_table()?;
                    match spec {
                        mlua::Value::Function(_) => {}
                        mlua::Value::Table(spec) => {
                            for entry in spec.pairs::<String, mlua::Value>() {
                                let (field, value) = entry?;
                                if field == "render" {
                                    if !matches!(value, mlua::Value::Function(_)) {
                                        return Err(
                                            "ui.messagebox tool render must be a function".into()
                                        );
                                    }
                                } else {
                                    options.set(field, value)?;
                                }
                            }
                        }
                        _ => return Err("ui.messagebox tools must be functions or tables".into()),
                    }
                    declarations.set(name, options)?;
                }
                data.set(key, declarations)?;
            } else {
                data.set(key, value)?;
            }
        }
        let value: serde_json::Value = lua.from_value(mlua::Value::Table(data))?;
        validate_messagebox(&value, "ui.messagebox", "root")?;
        state.lock().unwrap().messagebox = value;
    }
    if let Some(promptbox) = rness
        .get::<Table>("ui")?
        .get::<Option<Table>>("promptbox")?
    {
        let value: serde_json::Value = lua.from_value(mlua::Value::Table(promptbox))?;
        let valid_keys = |field: &serde_json::Value, names: &[&str]| {
            field.as_object().is_some_and(|keys| {
                keys.iter().all(|(key, chord)| {
                    names.contains(&key.as_str())
                        && (chord == &serde_json::Value::Bool(false)
                            || chord.as_str().is_some_and(|s| !s.is_empty()))
                })
            })
        };
        for (key, field) in value.as_object().ok_or("ui.promptbox must be a table")? {
            match key.as_str() {
                "editor"
                    if field.as_array().is_some_and(|a| {
                        !a.is_empty() && a.iter().all(|s| s.as_str().is_some_and(|s| !s.is_empty()))
                    }) => {}
                "images" => {
                    for (key, field) in field
                        .as_object()
                        .ok_or("ui.promptbox.images must be a table")?
                    {
                        if key != "keys"
                            || !field.as_object().is_some_and(|keys| {
                                keys.iter().all(|(name, chord)| {
                                    matches!(
                                        name.as_str(),
                                        "remove" | "next" | "preview" | "close" | "history"
                                    ) && (chord == &serde_json::Value::Bool(false)
                                        || chord.as_str().is_some_and(|s| !s.is_empty()))
                                })
                            })
                        {
                            return Err(format!("invalid ui.promptbox.images option: {key}").into());
                        }
                    }
                }
                "keys" if valid_keys(field, &["edit", "paste"]) => {}
                "paste" => {
                    for (key, field) in field
                        .as_object()
                        .ok_or("ui.promptbox.paste must be a table")?
                    {
                        match key.as_str() {
                            "lines" | "chars" if field.as_u64().is_some() => {}
                            "keys" if valid_keys(field, &["preview"]) => {}
                            _ => {
                                return Err(
                                    format!("invalid ui.promptbox.paste option: {key}").into()
                                );
                            }
                        }
                    }
                }
                _ => return Err(format!("invalid ui.promptbox option: {key}").into()),
            }
        }
        state.lock().unwrap().promptbox = value;
    }
    for (table, method) in [
        ("plugins", "setup"),
        ("providers", "set_stream_idle_timeout"),
        ("plugins", "load"),
        ("agents", "declare"),
        ("providers", "register"),
        ("profiles", "declare"),
        ("models", "declare"),
    ] {
        let table: Table = rness.get(table)?;
        table.set(
            method,
            lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
                Err(mlua::Error::runtime(
                    "startup declarations are closed; edit init.lua and restart",
                ))
            })?,
        )?;
    }
    let mut config = state.lock().unwrap().clone();
    config.default_agent = rness.get("default_agent")?;
    if config
        .default_agent
        .as_ref()
        .is_some_and(|name| !config.agents.contains_key(name))
    {
        return Err(std::io::Error::other("unknown default_agent").into());
    }
    for agent in config.agents.values() {
        if let Some(profile) = &agent.profile {
            config
                .models
                .validate_profile(profile)
                .map_err(std::io::Error::other)?;
        }
    }
    config.default_profile = rness.get("default_profile")?;
    if let Some(name) = &config.default_profile {
        config
            .models
            .resolve_profile(name)
            .map_err(std::io::Error::other)?;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn central_mappings_validate_targets_conflicts_and_unload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        std::fs::write(&path, r#"
            rness.keymap.setup({{scope='promptbox', key='<F8>', action='review.insert'}})
            assert(not pcall(rness.keymap.setup, {}))
            rness.plugins.setup({{name='review', config=function(opts, plugin)
                plugin.action('insert', {scope='promptbox', description='Insert', run=function() end})
                plugin.keys({insert={action='insert', key='<F6>'}})
            end}})
        "#).unwrap();
        let (host, config) = crate::plugin_host::LuaHost::spawn_from_init(path).unwrap();
        assert!(host.validate_bindings().await.is_err());
        let sources = crate::loader::discover_specs(dir.path(), &config.plugin_specs).unwrap();
        assert!(crate::loader::load_all(&host, &sources).await.is_empty());
        host.validate_bindings().await.unwrap();
        assert!(
            host.binding_specs()
                .await
                .iter()
                .any(|binding| binding.user && binding.keys == ["f8"])
        );
        host.load("check", "assert(not pcall(rness.keymap.setup, {}))")
            .await
            .unwrap();
        assert!(host.unload("review").await.unwrap());
        assert!(host.binding_specs().await.is_empty());
    }

    #[tokio::test]
    async fn explicit_specs_preserve_inline_closures_and_file_options() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("custom.lua"), "return function(opts) assert(opts.message == 'configured'); order = order .. 'file' end").unwrap();
        std::fs::write(dir.path().join("init.lua"), r#"
            local captured = 'inline'
            rness.plugins.setup({
                {name='inline', config=function(opts, plugin)
                    order = captured
                    assert(plugin.name == 'inline')
                    plugin.action('invoke', {scope='global', description='Invoke', run=function(ctx) invoked = ctx.value end})
                end},
                {name='custom', file='./custom.lua', opts={message='configured'}},
                {name='missing', file='./missing.lua', enabled=false},
            })
            assert(order == nil)
            assert(not pcall(rness.plugins.setup, {}))
        "#).unwrap();
        let (host, config) =
            crate::plugin_host::LuaHost::spawn_from_init(dir.path().join("init.lua")).unwrap();
        assert_eq!(config.plugin_specs.len(), 3);
        let sources = crate::loader::discover_specs(dir.path(), &config.plugin_specs).unwrap();
        assert_eq!(sources.len(), 2);
        assert!(crate::loader::load_all(&host, &sources).await.is_empty());
        assert_eq!(host.action_specs().await[0].name, "inline.invoke");
        host.call_action(
            "inline.invoke",
            "global",
            serde_json::json!({"value":"called"}),
        )
        .await
        .unwrap();
        host.load("verify", "assert(invoked == 'called'); assert(order == 'inlinefile'); assert(not pcall(rness.plugins.setup, {}))").await.unwrap();
    }

    #[test]
    fn explicit_specs_reject_ambiguous_or_invalid_declarations() {
        for declaration in [
            "{{name='x', file='x.lua', package='x'}}",
            "{{file='x.lua'}}",
            "{{name='x', file='x.lua'}, {name='x', file='y.lua'}}",
            "{{name='x', config=function() end, watch=true}}",
            "{{name='x', file='x.lua', typo=true}}",
            "{{name='x', file='x.lua', opts=false}}",
            "{[2]={name='x', file='x.lua'}}",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("init.lua");
            std::fs::write(&path, format!("rness.plugins.setup({declaration})")).unwrap();
            assert!(load(&path).is_err(), "accepted {declaration}");
        }
    }

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
        assert!(
            host.reload(vec![crate::loader::PluginSource {
                name: "check".into(),
                source: check.into(),
            }])
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn shipped_startup_example_loads_without_credentials() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/init.lua");
        let config = load(&path).unwrap();
        assert_eq!(config.providers.len(), 4);
        assert_eq!(
            config
                .models
                .resolve_profile("local-qwen")
                .unwrap()
                .selection
                .unwrap()
                .route,
            "ollama"
        );
        assert!(config.models.resolve_profile("router-sonnet").is_ok());
        assert!(config.default_profile.is_none());
    }

    #[test]
    fn image_policy_is_validated_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        std::fs::write(
            &path,
            "rness.images = { lossless=false, quality=75, max_pixels=1000000 }",
        )
        .unwrap();
        let config = load(&path).unwrap();
        assert!(!config.images.lossless);
        assert_eq!(config.images.quality, 75);
        for source in [
            "rness.images = {quality=0}",
            "rness.images = {max_pixels=0}",
            "rness.images = {typo=1}",
        ] {
            std::fs::write(&path, source).unwrap();
            assert!(load(&path).is_err());
        }
    }

    #[test]
    fn paste_configuration_is_validated_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.lua");
        std::fs::write(&path, "rness.ui.promptbox = { paste={lines=5, chars=500, keys={preview=false}}, keys={edit='ctrl+x'}, editor={'nvim','--clean'} }").unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.promptbox["editor"][0], "nvim");
        for source in [
            "rness.ui.promptbox = { paste={lines=-1} }",
            "rness.ui.promptbox = { editor='nvim' }",
            "rness.ui.promptbox = { keys={preview='ctrl+g'} }",
            "rness.ui.promptbox = { paste={keys={edit='ctrl+e'}} }",
        ] {
            std::fs::write(&path, source).unwrap();
            assert!(load(&path).is_err());
        }
    }

    #[test]
    fn providers_module_loads_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("lua")).unwrap();
        std::fs::write(
            dir.path().join("lua/providers.lua"),
            include_str!("../../../../examples/lua/providers.lua"),
        )
        .unwrap();
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
        std::fs::write(
            dir.path().join("lua/preferences.lua"),
            "rness.profiles.declare('local', {provider='ollama', model='qwen'})",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("init.lua"),
            r#"
            require('preferences')
            executions = (executions or 0) + 1
            rness.hook.on('ready', function() ready = true end)
            rness.ui.statusline(function() return ready and 'ready' or 'booting' end)
        "#,
        )
        .unwrap();
        let (host, config) =
            crate::plugin_host::LuaHost::spawn_from_init(dir.path().join("init.lua")).unwrap();
        assert!(config.models.resolve_profile("local").is_ok());
        assert_eq!(host.statusline().await.as_deref(), Some("booting"));
        host.fire_hook("ready", serde_json::json!({}));
        assert_eq!(host.statusline().await.as_deref(), Some("ready"));
        assert!(host.reload(vec![]).await.unwrap().is_empty());
        assert_eq!(host.statusline().await.as_deref(), Some("ready"));
        host.load("check", "assert(executions == 1); assert(ready)")
            .await
            .unwrap();
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
        std::fs::write(
            dir.path().join("plugins/b.lua"),
            "assert(mounted); order = 'b'",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("plugins/a.lua"),
            "assert(order == 'b'); order = order .. 'a'",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("plugins/unselected.lua"),
            "error('must not run')",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("init.lua"),
            "rness.plugins.load('b'); rness.plugins.load('a'); assert(order == nil)",
        )
        .unwrap();
        let (host, config) =
            crate::plugin_host::LuaHost::spawn_from_init(dir.path().join("init.lua")).unwrap();
        assert_eq!(config.plugins, vec!["b", "a"]);
        host.load(
            "mount-check",
            "assert(order == nil); mounted = true; assert(not pcall(rness.plugins.load, 'late'))",
        )
        .await
        .unwrap();
        let sources = crate::loader::discover(dir.path(), &config.plugins).unwrap();
        assert!(crate::loader::load_all(&host, &sources).await.is_empty());
        host.load("check", "assert(order == 'ba')").await.unwrap();
        for script in [
            "rness.plugins.load('../bad')",
            "rness.plugins.load('a'); rness.plugins.load('a')",
        ] {
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
