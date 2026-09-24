//! The `rness` binary — the ONLY composition root.
//!
//! Like dsh profiles: assembles the plugin tree (built-in Rust plugins +
//! user's Lua plugins from ~/.rness — nothing embedded, zero magic;
//! examples/ in the repo is copyable ones), applies config layers
//! (defaults -> user -> project -> CLI overlay), then launches TUI,
//! server, or headless mode.
//!
//! Currently: TUI (default) or headless one-shot with -p.

mod activity_recovery;
mod agent_monitor;
#[cfg(feature = "experimental-control")]
mod control_journal;
#[cfg(feature = "experimental-control")]
mod control_socket;
#[cfg(all(feature = "experimental-control", windows))]
mod control_windows;

use std::sync::Arc;

use anyhow::{bail, Context as _};
use clap::Parser;
use rness_engine::service::{FrameEv, SessionService, SessionsPlugin};
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::TurnConfig;
use rness_kernel::Kernel;
use rness_protocol::api::{ClientRequest, History};
use rness_protocol::events::{
    CallConfig, ContentPart, ModelSelection, Reasoning, SessionId, UserIntent,
};
use rness_providers::auth::{login_as, CredentialStore, LoginPrompt, OAuthConfig};
use rness_providers::routes;

#[derive(Parser)]
#[command(name = "rness", version, about = "rness — hackable coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Prompt to run headless (one turn, print the transcript).
    #[arg(short, long)]
    prompt: Option<String>,

    /// Continue an existing session instead of creating one.
    #[arg(short, long)]
    session: Option<String>,

    /// Model selection: <provider>/<model> (e.g. anthropic/claude-sonnet-4-5,
    /// openai-chatgpt/gpt-5.5, ollama/qwen3). Both parts required — rness
    /// has no default provider and no default model.
    #[arg(short, long)]
    model: Option<String>,

    /// Saved selection and request preferences from ~/.rness/init.lua.
    #[arg(long, conflicts_with = "model")]
    profile: Option<String>,

    /// Agent definition from init.lua (role, tools and optional profile).
    #[arg(long)]
    agent: Option<String>,

    /// Connection name; when supplied, --model is a literal model ID.
    #[arg(long, requires = "model", conflicts_with = "profile")]
    provider: Option<String>,

    /// Override the provider's endpoint (e.g. an OpenAI-compatible gateway,
    /// include the /v1 prefix).
    #[arg(long)]
    base_url: Option<String>,

    /// Session root directory (default: ~/.rness/sessions).
    #[arg(long)]
    root: Option<std::path::PathBuf>,

    /// Tool approval policy: allow (default, no questions), ask
    /// (sensitive tools pause for y/n), never (sensitive tools rejected).
    #[arg(long, default_value = "allow")]
    approval: String,

    /// List sessions whose saved workspace is the current directory and exit.
    #[arg(long)]
    list: bool,

    /// Serve the engine over HTTP/SSE instead of the TUI (e.g.
    /// 127.0.0.1:7777). Same wire shapes the TUI uses in-process.
    #[arg(long, value_name = "ADDR")]
    serve: Option<String>,

    /// Experimental native-TUI submission endpoint: private Unix socket or local Windows pipe.
    #[cfg(feature = "experimental-control")]
    #[arg(long, value_name = "PATH", conflicts_with_all = ["serve", "prompt", "list"])]
    control_socket: Option<std::path::PathBuf>,

    /// Workspace instruction files, comma-separated in precedence order,
    /// discovered from the project root (.git) down to the cwd and
    /// injected as durable context (re-injected after compaction folds
    /// them). "none" disables.
    #[arg(long, default_value = "AGENTS.md,CLAUDE.md", value_name = "NAMES")]
    instructions: String,

    /// Byte budget for the rendered instruction baseline; broader files
    /// are dropped whole before the most-specific file is truncated.
    #[arg(long, default_value_t = 65536, value_name = "BYTES")]
    instructions_bytes: usize,

    /// Reasoning effort convenience selector (e.g. low, medium, high).
    /// Equivalent to `--effort`; cannot be combined with `--budget-tokens`.
    #[arg(long, value_name = "LEVEL", conflicts_with_all = ["effort", "budget_tokens"])]
    reasoning: Option<String>,

    /// Named reasoning effort for Anthropic adaptive thinking and OpenAI APIs.
    #[arg(long, value_name = "LEVEL", conflicts_with_all = ["reasoning", "budget_tokens"])]
    effort: Option<String>,

    /// Anthropic manual thinking budget in tokens (minimum 1024).
    #[arg(long, value_name = "TOKENS", conflicts_with_all = ["reasoning", "effort"])]
    budget_tokens: Option<u32>,

    /// Maximum output tokens for each provider request.
    #[arg(long, value_name = "TOKENS")]
    max_output_tokens: Option<u32>,

    /// Sampling temperature for each provider request.
    #[arg(long, value_name = "VALUE")]
    temperature: Option<f64>,

    /// Declare an OpenAI-compatible provider route (repeatable):
    /// <name>=<url>[,<credential>|,none]. The url includes the /v1
    /// prefix. Credential defaults to the route name (<NAME>_API_KEY
    /// env or `rness auth set-key`); `none` = unauthenticated (local
    /// servers). Then select with -m <name>/<model>. E.g.
    /// --route openrouter=https://openrouter.ai/api/v1
    /// --route lan=http://192.168.1.50:8000/v1,none
    #[arg(long, value_name = "SPEC")]
    route: Vec<String>,

    /// Named credential account for the selected provider (e.g.
    /// `--account work`). Multiple keys per provider are stored
    /// with `rness auth set-key --account <name>`.
    #[arg(long)]
    account: Option<String>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Experimental: submit chat text to a running native TUI, without keyboard injection.
    #[cfg(feature = "experimental-control")]
    Send(control_socket::SendArgs),
    /// Install Lua packages without activating or executing them.
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Manage provider credentials (OAuth login for Claude Pro/Max, API keys)..
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(clap::Subcommand)]
enum PluginAction {
    Install {
        repository: String,
        #[arg(long)]
        rev: String,
    },
    Link {
        directory: std::path::PathBuf,
    },
    List,
    Update {
        name: String,
        #[arg(long)]
        rev: String,
    },
    /// Remove a package and its managed checkout; stop affected sessions first.
    Remove {
        name: String,
        #[arg(long)]
        confirm: bool,
    },
}

fn run_plugin(action: PluginAction) -> anyhow::Result<()> {
    use rness_lua::packages;
    let root = dirs::home_dir()
        .context("no home directory")?
        .join(".rness");
    let name = match action {
        PluginAction::List => {
            for (name, p) in packages::inventory(&root)? {
                println!(
                    "{name}\t{}\t{}",
                    p.revision.as_deref().unwrap_or("local link (mutable)"),
                    p.source
                );
            }
            return Ok(());
        }
        PluginAction::Install { repository, rev } => {
            packages::change(&root, Some((&repository, &rev)), None, None)?
        }
        PluginAction::Link { directory } => packages::change(&root, None, Some(&directory), None)?,
        PluginAction::Update { name, rev } => {
            let inventory = packages::inventory(&root)?;
            let package = inventory.get(&name).context("package is not installed")?;
            if package.revision.is_none() {
                bail!("local links update directly from their directory");
            }
            packages::change(&root, Some((&package.source, &rev)), None, Some(&name))?
        }
        PluginAction::Remove { name, confirm } => {
            if !confirm {
                bail!("stop affected sessions and remove its setup declaration from init.lua first, then pass --confirm; init.lua will not be executed to inspect activation");
            }
            packages::change(&root, None, None, Some(&name))?;
            println!("Unregistered {name}. Managed checkout removed; linked source directories are retained. Restart affected sessions.");
            return Ok(());
        }
    };
    println!("Installed {name}. Review before adding this entry to rness.plugins.setup in init.lua:\n  {{ package = \"{name}\" }}\nRestart to load it. No plugin code was executed. Stop affected sessions before updating or removing managed packages.");
    Ok(())
}

#[derive(clap::Subcommand)]
enum AuthAction {
    /// Log in via browser OAuth (Claude Pro/Max, or ChatGPT Plus/Pro).
    Login {
        /// Which provider: anthropic or openai-chatgpt.
        #[arg(long, default_value = "anthropic")]
        provider: String,
        /// Named account (e.g. "work"). Default account when omitted.
        #[arg(long)]
        account: Option<String>,
    },
    /// Store an API key for a provider.
    SetKey {
        /// The API key. Reads stdin if omitted.
        key: Option<String>,
        /// Which provider the key is for.
        #[arg(long, default_value = "anthropic")]
        provider: String,
        /// Named account (e.g. "work"). Default account when omitted.
        #[arg(long)]
        account: Option<String>,
    },
    /// Show credential status for every provider.
    Status,
    /// Delete a provider's stored credentials.
    Logout {
        /// Which provider to log out of.
        #[arg(long, default_value = "anthropic")]
        provider: String,
        /// Named account to remove. Default account when omitted.
        #[arg(long)]
        account: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    raise_fd_limit();
    let cli = Cli::parse();
    #[cfg(feature = "experimental-control")]
    if cli.control_socket.is_some() && cli.command.is_some() {
        bail!("--control-socket is only supported in native TUI mode");
    }
    #[cfg(feature = "experimental-control")]
    if let Some(Command::Send(args)) = cli.command {
        return control_socket::send(args).await;
    }
    #[cfg(feature = "experimental-control")]
    let control_path = cli.control_socket.clone();
    #[cfg(not(feature = "experimental-control"))]
    let control_path = None;

    // Interactive mode owns the terminal: a stray stderr line would be
    // painted over the TUI. Log to ~/.rness/log instead; headless and
    // server modes keep stderr.
    let interactive =
        cli.command.is_none() && cli.prompt.is_none() && cli.serve.is_none() && !cli.list;
    let env_filter =
        || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    if interactive {
        let log_path = dirs::home_dir()
            .context("no home directory")?
            .join(".rness")
            .join("log");
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open log file {}", log_path.display()))?;
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_writer(std::io::stderr)
            .init();
    }

    if let Some(command) = cli.command {
        return match command {
            #[cfg(feature = "experimental-control")]
            Command::Send(_) => unreachable!("handled before provider/config initialization"),
            Command::Auth { action } => run_auth(action).await,
            Command::Plugin { action } => run_plugin(action),
        };
    }

    let root = match cli.root {
        Some(r) => r,
        None => dirs::home_dir()
            .context("no home directory")?
            .join(".rness")
            .join("sessions"),
    };

    // Routes and credentials remain exclusively at this composition root.
    // The resolver receives only the durable public route/model selection.
    let (mut lua, startup) = rness_lua::plugin_host::LuaHost::spawn_from_init(
        dirs::home_dir()
            .context("no home directory")?
            .join(".rness/init.lua"),
    )
    .map_err(|e| anyhow::anyhow!("startup configuration: {e}"))?;
    let mut route_table = std::collections::HashMap::new();
    let mut auth_env = std::collections::HashMap::<String, String>::new();
    let mut auth_oauth = std::collections::HashMap::<String, String>::new();
    let mut auth_store = std::collections::HashMap::<String, String>::new();
    for (name, declaration) in &startup.providers {
        let kind = match declaration.protocol.as_str() {
            "openai-chat" => routes::Kind::OpenAiCompatible,
            "anthropic" => routes::Kind::Anthropic,
            "chatgpt-responses" => routes::Kind::ChatGptResponses,
            _ => anyhow::bail!("provider {name}: unknown protocol"),
        };
        let credential = match &declaration.auth {
            rness_lua::api::config::ProviderAuth::Disabled(false)
                if matches!(kind, routes::Kind::OpenAiCompatible) =>
            {
                None
            }
            rness_lua::api::config::ProviderAuth::OAuth { oauth }
                if !oauth.is_empty() && !matches!(kind, routes::Kind::OpenAiCompatible) =>
            {
                auth_oauth.insert(name.clone(), oauth.clone());
                None
            }
            rness_lua::api::config::ProviderAuth::Store { credential }
                if !credential.is_empty() && !matches!(kind, routes::Kind::ChatGptResponses) =>
            {
                auth_store.insert(name.clone(), credential.clone());
                Some(credential.clone())
            }
            rness_lua::api::config::ProviderAuth::Env { env }
                if !env.is_empty()
                    && !env.contains(['=', '\0'])
                    && !matches!(kind, routes::Kind::ChatGptResponses) =>
            {
                auth_env.insert(name.clone(), env.clone());
                None
            }
            _ => anyhow::bail!("provider {name}: invalid credential declaration"),
        };
        route_table.insert(
            name.clone(),
            routes::Route {
                kind,
                base_url: Some(declaration.base_url.clone()),
                credential,
                stream_idle_timeout: Some(std::time::Duration::from_secs(300)),
                prompt_caching: matches!(kind, routes::Kind::Anthropic),
                cache_ttl: routes::CacheTtl::Auto,
                headers: rness_providers::headers::ProviderHeaders::new(&declaration.headers)
                    .map_err(|e| anyhow::anyhow!("provider {name}: {e}"))?,
            },
        );
    }
    for spec in &cli.route {
        let (name, route) = routes::parse_route_spec(spec).map_err(|e| anyhow::anyhow!("{e}"))?;
        auth_env.remove(&name);
        auth_oauth.remove(&name);
        auth_store.remove(&name);
        route_table.insert(name, route);
    }
    for (name, milliseconds) in &startup.stream_idle_timeouts {
        let route = route_table
            .get_mut(name)
            .with_context(|| format!("timeout configured for unknown provider: {name}"))?;
        route.stream_idle_timeout =
            (*milliseconds != 0).then(|| std::time::Duration::from_millis(*milliseconds));
    }
    for (name, ttl) in &startup.cache_ttls {
        let route = route_table
            .get_mut(name)
            .with_context(|| format!("cache_ttl configured for unknown provider: {name}"))?;
        route.cache_ttl = match ttl.as_str() {
            "1h" => routes::CacheTtl::OneHour,
            "5m" => routes::CacheTtl::FiveMinutes,
            _ => routes::CacheTtl::Auto,
        };
    }

    // -- account selection: --account flag overrides per-provider default_account.
    // Qualify credential keys in auth_store / auth_oauth so the resolver reads
    // the right slot from credentials.json (e.g. "anthropic/work" instead of "anthropic").
    for (provider_name, declaration) in &startup.providers {
        let effective_account = cli.account.as_deref().or(declaration.default_account.as_deref());
        if let Some(acct) = effective_account {
            if let Some(base) = auth_store.get(provider_name) {
                let qualified = CredentialStore::credential_key(base, Some(acct));
                auth_store.insert(provider_name.clone(), qualified);
            }
            if let Some(base) = auth_oauth.get(provider_name) {
                let qualified = CredentialStore::credential_key(base, Some(acct));
                auth_oauth.insert(provider_name.clone(), qualified);
            }
        }
    }

    let cli_selection = match cli.model.as_deref() {
        Some(model) => Some(if let Some(provider) = &cli.provider {
            if !route_table.contains_key(provider) {
                anyhow::bail!("unknown provider: {provider}");
            }
            routes::Selection {
                route: provider.clone(),
                model: model.into(),
            }
        } else {
            routes::Selection::parse(&route_table, Some(model))
                .map_err(|e| anyhow::anyhow!("{e}"))?
        }),
        None => None,
    };
    if let (Some(url), Some(selection)) = (cli.base_url.as_ref(), cli_selection.as_ref()) {
        route_table
            .get_mut(&selection.route)
            .expect("parsed selection references a known route")
            .base_url = Some(url.clone());
    }
    let mut creation_seed = if let Some(session) = &cli.session {
        let store = rness_engine::session::branch::SessionStore::new(root.clone());
        rness_engine::session::replay::replay(&store, session)?
            .context
            .config
    } else {
        CallConfig::default()
    };
    let inherited_ceiling = creation_seed.tool_ceiling.clone();
    let mut sandbox = creation_seed.sandbox;
    if cli.session.is_none()
        && startup.sandbox.default != rness_protocol::sandbox::SandboxMode::DangerFullAccess
    {
        sandbox = Some(startup.sandbox.default);
    }
    let chosen_agent = cli.agent.as_ref().or_else(|| {
        if cli.session.is_none() {
            startup.default_agent.as_ref()
        } else {
            None
        }
    });
    if let Some(name) = chosen_agent {
        let agent = startup
            .agents
            .get(name)
            .with_context(|| format!("unknown agent: {name}"))?;
        if let Some(profile) = &agent.profile {
            creation_seed = startup
                .models
                .resolve_profile(profile)
                .map_err(|e| anyhow::anyhow!(e))?;
        }
        creation_seed.agent = Some(rness_protocol::events::AgentSnapshot {
            name: name.clone(),
            instructions: agent.instructions.clone(),
            tools: agent.tools.clone(),
        });
        if let Some(mode) = agent.sandbox {
            sandbox = Some(sandbox.unwrap_or_default().min(mode));
        }
    }
    if let Some(profile) = cli.profile.as_ref().or_else(|| {
        if cli.session.is_none()
            && cli.model.is_none()
            && chosen_agent
                .and_then(|name| startup.agents.get(name))
                .and_then(|agent| agent.profile.as_ref())
                .is_none()
        {
            startup.default_profile.as_ref()
        } else {
            None
        }
    }) {
        let agent = creation_seed.agent.clone();
        creation_seed = startup
            .models
            .resolve_profile(profile)
            .map_err(|e| anyhow::anyhow!(e))?;
        creation_seed.agent = agent;
    }
    if let Some(selected) = cli_selection {
        let selected = ModelSelection {
            route: selected.route,
            model: selected.model,
        };
        if creation_seed.selection.as_ref() != Some(&selected) {
            creation_seed = CallConfig {
                selection: Some(selected),
                profile: None,
                agent: creation_seed.agent.clone(),
                ..Default::default()
            };
        }
    }
    creation_seed.tool_ceiling = inherited_ceiling;
    creation_seed.sandbox = sandbox;
    let selected = creation_seed.selection.as_ref().context(
        "no model selected: pass -m <route>/<model>; older sessions need an explicit selection",
    )?;
    let selection = routes::Selection {
        route: selected.route.clone(),
        model: selected.model.clone(),
    };
    if let Some(url) = &cli.base_url {
        route_table
            .get_mut(&selection.route)
            .context("saved route is not configured; declare it with --route")?
            .base_url = Some(url.clone());
    }
    if let Some(effort) = cli.reasoning.as_ref().or(cli.effort.as_ref()) {
        creation_seed.reasoning = Some(Reasoning::Effort {
            effort: effort.clone(),
        });
    }
    if let Some(tokens) = cli.budget_tokens {
        creation_seed.reasoning = Some(Reasoning::BudgetTokens { tokens });
    }
    if let Some(tokens) = cli.max_output_tokens {
        creation_seed.max_output_tokens = Some(tokens);
    }
    if let Some(temperature) = cli.temperature {
        creation_seed.temperature = Some(temperature);
    }
    startup
        .models
        .validate(&creation_seed)
        .map_err(|e| anyhow::anyhow!(e))?;
    for (name, route) in &route_table {
        if let Some(url) = &route.base_url {
            routes::validate_base_url(url).map_err(|e| anyhow::anyhow!("provider {name}: {e}"))?;
        }
    }
    let credential_store = CredentialStore::new(CredentialStore::default_path());
    let resolver_routes = route_table.clone();
    let resolver_store = credential_store.clone();
    let image_policy = startup.images.clone();
    let image_root = CredentialStore::default_path()
        .parent()
        .context("credential store has no parent")?
        .join("images");
    let image_store = Arc::new(
        rness_engine::images::ImageStore::new(image_root, image_policy.clone())
            .map_err(anyhow::Error::msg)?,
    );
    let resolver_images = image_store.clone();
    let provider_resolver: Arc<rness_engine::service::ProviderResolver> =
        Arc::new(move |selection| {
            let mut provider = (|| {
                if let Some(credential) = auth_oauth.get(&selection.route) {
                    // If the exact credential key has no tokens, try the first
                    // available account for the base provider.
                    let effective = resolver_store
                        .tokens(credential)
                        .ok()
                        .flatten()
                        .map(|_| credential.clone())
                        .or_else(|| {
                            let base = credential.split('/').next().unwrap_or(credential);
                            first_available_credential(&resolver_store, base)
                        })
                        .unwrap_or_else(|| credential.clone());
                    let route = resolver_routes
                        .get(&selection.route)
                        .ok_or("unknown provider")?;
                    return routes::build_with_oauth(
                        route,
                        &selection.model,
                        resolver_store.clone(),
                        effective,
                    );
                }
                if let Some(credential) = auth_store.get(&selection.route) {
                    // Try the exact key first; fall back to any account that
                    // has a stored API key for the same base provider.
                    let key = resolver_store
                        .api_key(credential)
                        .map_err(|e| e.to_string())?
                        .filter(|key| !key.is_empty())
                        .or_else(|| {
                            let base = credential.split('/').next().unwrap_or(credential);
                            first_available_api_key(&resolver_store, base)
                        })
                        .ok_or_else(|| {
                            let base = credential.split('/').next().unwrap_or(credential);
                            let accounts = resolver_store.accounts(base).unwrap_or_default();
                            if accounts.is_empty() {
                                format!(
                                    "no API key for {base}: run `rness auth set-key --provider {base}`"
                                )
                            } else {
                                format!("no API key for credential {credential}")
                            }
                        })?;
                    let route = resolver_routes
                        .get(&selection.route)
                        .ok_or("unknown provider")?;
                    return routes::build_with_api_key(route, &selection.model, key);
                }
                if let Some(env) = auth_env.get(&selection.route) {
                    let key = std::env::var(env)
                        .ok()
                        .filter(|key| !key.is_empty())
                        .ok_or_else(|| format!("missing API key environment variable: {env}"))?;
                    let route = resolver_routes
                        .get(&selection.route)
                        .ok_or("unknown provider")?;
                    return routes::build_with_api_key(route, &selection.model, key);
                }
                routes::build(
                    &resolver_routes,
                    &routes::Selection {
                        route: selection.route.clone(),
                        model: selection.model.clone(),
                    },
                    resolver_store.clone(),
                )
                .map_err(|e| e.to_string())
            })()?;
            Arc::get_mut(&mut provider)
                .ok_or("new provider unexpectedly shared")?
                .configure_images(resolver_images.clone(), image_policy.clone());
            Ok(provider)
        });

    let provider = provider_resolver(creation_seed.selection.as_ref().unwrap())
        .map_err(|e| anyhow::anyhow!(e))?;

    // Built-in tools, rooted at the current directory. Arc'd up front:
    // the hot-reload watcher re-syncs Lua tools into the same registry.
    let cwd = std::env::current_dir().context("no working directory")?;
    let tools = Arc::new(ToolRegistry::default());
    let jobs = rness_tools::register_all_configured(
        &tools,
        rness_tools::Workspace::new(&cwd),
        startup.sandbox.process.clone(),
    );
    jobs.configure_retention(startup.job_retention.clone())
        .map_err(anyhow::Error::msg)?;
    jobs.enable_persistence(&root.join("jobs"))
        .map_err(|e| anyhow::anyhow!("job recovery: {e}"))?;
    if let Some(lsp) = startup.lsp.clone() {
        rness_tools::lsp::register(&tools, lsp)
            .map_err(|e| anyhow::anyhow!("invalid rness.lsp: {e}"))?;
    }
    if let Some(web) = startup.web.clone() {
        let web = serde_json::from_value(web).context("invalid rness.web configuration")?;
        rness_tools::web::register_with_hooks(&tools, web, Arc::new(lua.clone()))
            .map_err(|e| anyhow::anyhow!("invalid rness.web: {e}"))?;
    }
    // Tool-pipeline hooks (pre_tool / guard / post_tool / tool_result).
    // Shared by every derived registry, so they also govern subagents; with
    // no handler registered each phase short-circuits without the VM.
    tools.set_hooks(Some(Arc::new(lua.clone())));

    // Lua host: the VM boots now, but plugins load AFTER the engine
    // mounts — rness.session/subagents/mcp must exist when a plugin's
    // top level runs (same order a hot reload gets). The registry is
    // interior-mutable, so late tool registration is fine.

    let lua_root = dirs::home_dir()
        .context("no home directory")?
        .join(".rness");

    let policy: rness_engine::approval::Policy = cli
        .approval
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    let questions = Arc::new(rness_engine::questions::Questions::default());
    questions.bind_registry(&tools);
    if startup.ask_user {
        questions.set_available(startup.question_overlay.enabled);
        questions.set_overlay_config(rness_engine::questions::OverlayConfig {
            priority: startup.question_overlay.priority,
            height: startup.question_overlay.height,
            title: startup.question_overlay.title.clone(),
            ui: startup.question_overlay.ui.clone(),
        });
    }
    tools.approvals().set_policy(policy);
    tools.approvals().set_rules(startup.permissions.clone());

    // Under `ask`, the TUI overlay is the answerer; approvals channel
    // bridges engine → UI. (Headless ask fails closed by design.)
    let (approval_tx, approval_rx) = tokio::sync::mpsc::unbounded_channel();
    if policy == rness_engine::approval::Policy::Ask
        || startup
            .permissions
            .values()
            .any(|rule| *rule == rness_engine::approval::ToolPolicy::Ask)
    {
        tools
            .approvals()
            .set_answerer(Arc::new(TuiAnswerer { tx: approval_tx }));
    }

    // Composition root: mount the engine on the kernel.
    let mut kernel = Kernel::new();
    kernel
        .mount(SessionsPlugin {
            root,
            provider,
            resolver: Some(provider_resolver),
            creation_seed: creation_seed.clone(),
            agents: startup.agents.clone(),
            sandbox: startup.sandbox.clone(),
            models: startup.models.clone(),
            tools: Arc::clone(&tools),
            config: TurnConfig {
                compaction: startup.compaction.clone(),
                tool_exposure: startup.tool_exposure.clone(),
                system: "You are rness, a coding agent. Be concise.".into(),
                ..Default::default()
            },
        })
        .map_err(|e| anyhow::anyhow!("mount sessions: {e}"))?;
    let sessions = kernel
        .services()
        .get::<SessionService>("sessions")
        .context("sessions service missing")?;
    sessions.set_images(image_store)?;
    lua.set_bus(Arc::clone(kernel.bus()));
    sessions.set_loop_hooks(Some(Arc::new(lua.clone())));
    // Re-set tool hooks so the clone carries the bus reference for audit events.
    tools.set_hooks(Some(Arc::new(lua.clone())));
    sessions.set_default_workspace(cwd.to_string_lossy().into_owned())?;
    let legacy_skill_roots = rness_tools::skills::default_roots(&cwd);
    sessions.set_input_resolver(Arc::new(move |workspace, content| {
        let roots = workspace.map(rness_tools::skills::default_roots);
        rness_tools::skills::resolve_input(roots.as_deref().unwrap_or(&legacy_skill_roots), content)
    }));

    // Workspace instructions (dsh agent-instructions model): explicit
    // policy from flags — candidates + budget — mechanism in the engine.
    if cli.instructions != "none" {
        sessions.set_instructions(rness_engine::instructions::InstructionsConfig {
            cwd: std::env::current_dir()?,
            candidates: cli
                .instructions
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            max_bytes: cli.instructions_bytes,
        });
    }

    // Subagents: providers registered here at the composition root —
    // spawn (fresh child) and fork (inherits completed history). The
    // model gets the delegation tool; late registration is exactly why
    // the tool registry is interior-mutable.
    let subagents = Arc::new(
        rness_engine::subagent::SubagentRuntime::new(
            Arc::clone(&sessions),
            3, // max delegation depth: parent -> child -> grandchild, then stop
        )
        .with_allow_generic(startup.allow_generic_subagents),
    );
    subagents.register(Arc::new(rness_engine::subagent::SpawnProvider));
    subagents.register(Arc::new(rness_engine::subagent::ForkProvider));
    // Attach job delivery only after startup configuration is committed below.
    // Recovered completion notices can start turns immediately on attachment.
    lua.install_jobs(jobs.clone())
        .await
        .map_err(|e| anyhow::anyhow!("lua jobs bridge: {e}"))?;
    rness_tools::register_subagent(&tools, Arc::clone(&subagents), jobs.clone());
    rness_tools::subagent_control::register_subagent_control(&tools, Arc::clone(&subagents));

    // Skills: filesystem catalogs, project shadowing user on name
    // conflicts. The catalog lives in the tool's description.
    rness_tools::skills::register_skills(
        &tools,
        rness_tools::skills::default_roots(&std::env::current_dir()?),
    );

    // Surface the engine to plugins: rness.session + rness.subagents
    // work from here on (sticky across hot reloads).
    let mcp_connections: rness_lua::api::mcp::McpConnections = Default::default();
    lua.install_session(
        Arc::clone(&sessions),
        Arc::clone(&subagents),
        Arc::clone(&tools),
        Arc::clone(&mcp_connections),
        tokio::runtime::Handle::current(),
        format!("{}/{}", selection.route, selection.model),
    )
    .await
    .map_err(|e| anyhow::anyhow!("lua session bridge: {e}"))?;
    lua.install_questions(questions.clone())
        .await
        .map_err(|e| anyhow::anyhow!("lua questions bridge: {e}"))?;

    // NOW load the user's plugins: every rness.* namespace is live.
    // ONLY ~/.rness loads — nothing is embedded, nothing is implicit.
    // examples/ in the repo is copyable examples, not shipped defaults.
    let plugins = if startup.plugin_specs.is_empty() {
        rness_lua::loader::discover(&lua_root, &startup.plugins)?
    } else {
        rness_lua::loader::discover_specs(&lua_root, &startup.plugin_specs)?
    };
    for (name, err) in rness_lua::loader::load_all(&lua, &plugins).await {
        eprintln!("warning: lua plugin '{name}' failed: {err}");
    }

    // Load hooks.json (project .rness/hooks.json, then user ~/.rness/hooks.json).
    {
        let project_hooks = cwd.join(".rness/hooks.json");
        let user_hooks = dirs::home_dir()
            .map(|h| h.join(".rness/hooks.json"))
            .unwrap_or_default();
        for path in [&project_hooks, &user_hooks] {
            match rness_lua::hooks_json::load(path) {
                Ok(Some(config)) => {
                    let code = rness_lua::hooks_json::generate_lua(&config);
                    if !code.is_empty() {
                        if let Err(e) = lua.load("hooks.json", &code).await {
                            eprintln!(
                                "warning: hooks.json at {}: {e}",
                                path.display()
                            );
                        } else {
                            tracing::info!(path = %path.display(), "loaded hooks.json");
                        }
                    }
                }
                Ok(None) => {} // file doesn't exist, skip
                Err(e) => eprintln!("warning: {}: {e}", path.display()),
            }
        }
    }

    lua.validate_bindings().await.map_err(anyhow::Error::msg)?;
    let installed_lua_tools = rness_lua::api::tools::sync_lua_tools(&tools, &lua, &[]).await;
    lua.fire_hook("ready", serde_json::json!({}));

    // Fan bus events out to Lua hooks (fire-and-forget: a slow hook
    // can't block the engine).
    let _lua_hook_subs = {
        use rness_engine::service::{
            SessionStartEv, SubagentStartEv, SubagentStopEv, TurnEndedEv, TurnStartedEv,
        };
        let l1: Arc<dyn rness_kernel::presentation::HookSink> = Arc::new(lua.clone());
        let s1 = kernel.bus().on::<TurnStartedEv>(move |n| {
            l1.fire_hook(
                "turn_start",
                serde_json::json!({"session": n.session, "turn": n.turn}),
            );
        });
        let l2: Arc<dyn rness_kernel::presentation::HookSink> = Arc::new(lua.clone());
        let s2 = kernel.bus().on::<TurnEndedEv>(move |n| {
            l2.fire_hook(
                "turn_end",
                serde_json::json!({
                    "session": n.session,
                    "turn": n.turn,
                    "outcome": format!("{:?}", n.outcome),
                }),
            );
        });
        // Live frames, protocol wire shape (tagged "type": step_started,
        // delta, tool_started, ...). Same ephemeral channel the TUI and
        // SSE clients consume — Lua gets the identical seam.
        let l3: Arc<dyn rness_kernel::presentation::HookSink> = Arc::new(lua.clone());
        let s3 = kernel.bus().on::<FrameEv>(move |frame| {
            if let Ok(payload) = serde_json::to_value(frame) {
                l3.fire_hook("frame", payload);
            }
        });
        // Session lifecycle: first burst entry.
        let l4: Arc<dyn rness_kernel::presentation::HookSink> = Arc::new(lua.clone());
        let s4 = kernel.bus().on::<SessionStartEv>(move |n| {
            l4.fire_hook(
                "session_start",
                serde_json::json!({
                    "session": n.session,
                    "workspace": n.workspace,
                    "source": n.source,
                    "delegation": n.delegation.as_ref().map(|d| serde_json::json!({
                        "parent": d.parent,
                        "depth": d.depth,
                        "mode": format!("{:?}", d.mode),
                    })),
                }),
            );
        });
        // Subagent lifecycle.
        let l5: Arc<dyn rness_kernel::presentation::HookSink> = Arc::new(lua.clone());
        let s5 = kernel.bus().on::<SubagentStartEv>(move |n| {
            l5.fire_hook(
                "subagent_start",
                serde_json::json!({
                    "session": n.parent,
                    "parent": n.parent,
                    "child": n.child,
                    "agent": n.agent,
                    "mode": format!("{:?}", n.mode),
                    "depth": n.depth,
                }),
            );
        });
        let l6: Arc<dyn rness_kernel::presentation::HookSink> = Arc::new(lua.clone());
        let s6 = kernel.bus().on::<SubagentStopEv>(move |n| {
            l6.fire_hook(
                "subagent_stop",
                serde_json::json!({
                    "session": n.parent,
                    "parent": n.parent,
                    "child": n.child,
                    "outcome": n.outcome,
                }),
            );
        });
        (s1, s2, s3, s4, s5, s6)
    };

    if cli.list {
        for id in sessions_in_directory(sessions.store(), &cwd)? {
            println!("{id}");
        }
        return Ok(());
    }

    // Server mode: same engine, same plugins, HTTP/SSE instead of TUI.
    // Hot reload stays on — a server deployment edits plugins too.
    if let Some(addr) = cli.serve {
        let _reload = if lua_root.is_dir() {
            let lua_tools = installed_lua_tools.clone();
            rness_lua::reload::watch_startup(
                lua_root,
                &startup,
                lua.clone(),
                Arc::clone(&tools),
                lua_tools,
                |_| {},
            )
            .ok()
        } else {
            None
        };
        let mut state = rness_server::ServerState::new(Arc::clone(&sessions));
        state.questions = questions.clone();
        let _frame_sub = kernel
            .bus()
            .on::<rness_engine::service::FrameEv>(state.frame_sink());
        // Under `ask`, remote clients are the approvers: replace the TUI
        // answerer (there is no TUI here) with the HTTP/SSE one. Pending
        // questions surface as `approval_requested` frames and resolve
        // via POST /api/approvals/:call.
        if policy == rness_engine::approval::Policy::Ask
            || startup
                .permissions
                .values()
                .any(|rule| *rule == rness_engine::approval::ToolPolicy::Ask)
        {
            tools
                .approvals()
                .set_answerer(Arc::clone(&state.approvals) as _);
        }
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        jobs.attach_sessions(&sessions);
        eprintln!(
            "rness serving on http://{addr} (model: {})",
            selection.model
        );
        let auth = rness_server::auth::Auth::for_bind(
            listener.local_addr()?,
            std::env::var("RNESS_SERVER_TOKEN").ok(),
        )
        .map_err(anyhow::Error::msg)?;
        let router = rness_server::router(state).layer(axum::middleware::from_fn_with_state(
            auth,
            rness_server::auth::authorize,
        ));
        axum::serve(listener, router).await?;
        return Ok(());
    }

    let session = match cli.session {
        Some(s) => s,
        None => sessions.create(Some(cwd.display().to_string()))?,
    };

    sessions
        .set_config(&session, creation_seed)
        .with_context(|| {
            format!("apply startup model/reasoning configuration to session {session}")
        })?;
    jobs.attach_sessions(&sessions);

    let Some(prompt) = cli.prompt else {
        // Interactive: hot-reload Lua plugins while the TUI runs. The
        // watcher owns re-sync; dropping it (end of scope) stops it.
        let _reload = if lua_root.is_dir() {
            let lua_tools = installed_lua_tools.clone();
            match rness_lua::reload::watch_startup(
                lua_root,
                &startup,
                lua.clone(),
                Arc::clone(&tools),
                lua_tools,
                |report| {
                    use rness_lua::reload::ReloadReport;
                    match report {
                        ReloadReport::Reloaded {
                            plugins,
                            tools,
                            errors,
                        } => {
                            tracing::info!(target: "lua", "reloaded {plugins} plugin(s), tools: {tools:?}");
                            for (name, err) in errors {
                                tracing::warn!(target: "lua", "plugin '{name}' failed: {err}");
                            }
                        }
                        ReloadReport::Failed(e) => {
                            tracing::warn!(target: "lua", "hot reload failed: {e}");
                        }
                    }
                },
            ) {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("warning: lua watcher: {e}");
                    None
                }
            }
        } else {
            None
        };
        // Run the TUI over protocol frames.
        return run_tui(
            kernel,
            sessions,
            subagents,
            session,
            selection.model.clone(),
            questions,
            startup.question_overlay.priority,
            approval_rx,
            lua,
            installed_lua_tools,
            startup.colorschemes.clone(),
            startup.colorscheme.clone(),
            startup.promptbox.clone(),
            startup.messagebox.clone(),
            control_path,
        )
        .await;
    };

    questions.set_available(false);
    sessions.send(
        &session,
        UserIntent::Followup,
        vec![ContentPart::Text { text: prompt }],
    )?;
    sessions.join(&session).await;

    // Print the turn's transcript.
    let transcript = sessions.transcript(&session)?;
    for item in &transcript.items {
        use rness_engine::session::projection::TranscriptItem;
        match item {
            TranscriptItem::User { content, .. } => print_parts("you", content),
            TranscriptItem::Assistant { content, .. } => print_parts("rness", content),
            TranscriptItem::Attempt { attempt, .. } => {
                eprintln!("[attempt: {:?}]", attempt.outcome)
            }
            TranscriptItem::Tool { result, .. } => {
                eprintln!("[tool {} → {} bytes]", result.name, result.output.len())
            }
            TranscriptItem::Compaction {
                summary, shadowed, ..
            } => {
                eprintln!("[compacted {shadowed} events]\nsummary: {summary}")
            }
        }
    }
    eprintln!("\nsession: {session}");
    Ok(())
}

fn print_parts(who: &str, parts: &[ContentPart]) {
    for part in parts {
        match part {
            ContentPart::Image { attachment } => println!(
                "{who}: [Image {} × {} · {} bytes]",
                attachment.width, attachment.height, attachment.bytes
            ),
            ContentPart::Text { text } => println!("{who}: {text}"),
            ContentPart::Thinking { .. } => {}
            ContentPart::ToolUse { name, .. } => eprintln!("[{who} calls {name}]"),
        }
    }
}

/// In-process protocol backend: the TUI sees ClientRequest/History only.
mod questions;

struct LocalBackend {
    completion_lock: Arc<tokio::sync::Mutex<()>>,
    completion_generation: Arc<std::sync::atomic::AtomicU64>,
    reference_cancel: std::sync::Mutex<tokio_util::sync::CancellationToken>,
    results: tokio::sync::mpsc::UnboundedSender<rness_tui::app::Action>,
    sessions: Arc<SessionService>,
}

/// Bridges engine approval checks to the TUI overlay: sends the pending
/// question over a channel, awaits the one-shot answer. A dropped
/// receiver or responder resolves as Cancelled (fail closed).
#[cfg(test)]
mod permission_integration {
    use super::*;
    use rness_engine::approval::ToolPolicy;
    use rness_engine::tools::{Tool, ToolCall, ToolRegistry};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Count(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl Tool for Count {
        fn name(&self) -> &str {
            "Read"
        }
        async fn execute(&self, _: serde_json::Value) -> Result<String, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("executed".into())
        }
    }

    #[tokio::test]
    async fn per_tool_permissions_cross_dispatch_and_real_tui_answerer() {
        for mode in ["allow", "deny", "yes", "no", "drop", "disconnect", "cancel"] {
            let registry = Arc::new(ToolRegistry::default());
            let count = Arc::new(AtomicUsize::new(0));
            registry.register(Arc::new(Count(count.clone())));
            if mode != "allow" {
                registry.approvals().set_rules(
                    [(
                        "Read".into(),
                        if mode == "deny" {
                            ToolPolicy::Deny
                        } else {
                            ToolPolicy::Ask
                        },
                    )]
                    .into(),
                );
            }
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            registry
                .approvals()
                .set_answerer(Arc::new(TuiAnswerer { tx }));
            if mode == "disconnect" {
                rx.close();
            }
            let cancel = tokio_util::sync::CancellationToken::new();
            let token = cancel.clone();
            let task = tokio::spawn(async move {
                registry
                    .dispatch(
                        &"s".into(),
                        &[ToolCall {
                            call: "c".into(),
                            name: "Read".into(),
                            args: serde_json::json!({}),
                        }],
                        1,
                        &token,
                    )
                    .await
            });
            if ["yes", "no", "drop", "cancel"].contains(&mode) {
                let pending = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(pending.request.tool, "Read");
                match mode {
                    "yes" | "no" => {
                        use rness_tui::component::Component;
                        let model = rness_tui::app::Model::new("s".into(), "m".into());
                        let theme = rness_tui::theme::Theme::default();
                        let ctx = rness_tui::component::Ctx {
                            model: &model,
                            theme: &theme,
                        };
                        let mut overlay = rness_tui::modules::approval::ApprovalOverlay;
                        let outcome = overlay.on_key(
                            &ctx,
                            crossterm::event::KeyEvent::new(
                                crossterm::event::KeyCode::Char(if mode == "yes" {
                                    'y'
                                } else {
                                    'n'
                                }),
                                crossterm::event::KeyModifiers::NONE,
                            ),
                        );
                        let rness_tui::app::Action::ResolveApproval(decision) =
                            outcome.actions.into_iter().next().unwrap()
                        else {
                            panic!("approval action")
                        };
                        pending.respond.send(decision).unwrap();
                    }
                    "cancel" => {
                        cancel.cancel();
                        drop(pending);
                    }
                    _ => drop(pending),
                }
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            let allowed = mode == "allow" || mode == "yes";
            assert_eq!(!result[0].is_error, allowed, "{mode}");
            assert_eq!(count.load(Ordering::SeqCst), usize::from(allowed), "{mode}");
        }
    }
}

struct TuiAnswerer {
    tx: tokio::sync::mpsc::UnboundedSender<rness_tui::modules::approval::PendingApproval>,
}

#[async_trait::async_trait]
impl rness_engine::approval::Answerer for TuiAnswerer {
    async fn answer(
        &self,
        request: &rness_protocol::api::ApprovalRequest,
    ) -> rness_engine::approval::Decision {
        use rness_engine::approval::Decision;
        let (respond, answered) = tokio::sync::oneshot::channel();
        let pending = rness_tui::modules::approval::PendingApproval {
            request: request.clone(),
            respond,
        };
        if self.tx.send(pending).is_err() {
            return Decision::Unavailable; // no UI attached
        }
        match answered.await {
            Ok(rness_protocol::api::ApprovalDecision::Allowed) => Decision::Allowed,
            Ok(rness_protocol::api::ApprovalDecision::Rejected) => Decision::Rejected,
            Err(_) => Decision::Cancelled, // overlay dropped unanswered
        }
    }
}

impl rness_tui::app::Backend for LocalBackend {
    fn read_image(&self, session: &SessionId, id: &str) -> Result<Vec<u8>, String> {
        self.sessions
            .read_image(session, id)
            .map(|(_, bytes)| bytes)
            .map_err(|e| e.to_string())
    }
    fn admit_image(
        &self,
        session: &SessionId,
        data: &[u8],
        media_type: &str,
    ) -> Result<rness_protocol::events::ImageRef, String> {
        self.sessions
            .admit_image(session, data, media_type)
            .map_err(|e| e.to_string())
    }
    fn complete(&self, session: &SessionId, text: String) {
        if let Some(start) = text.rfind('@').filter(|_| !text.starts_with('/')) {
            let cancel = tokio_util::sync::CancellationToken::new();
            self.reference_cancel.lock().unwrap().cancel();
            *self.reference_cancel.lock().unwrap() = cancel.clone();
            let query = text[start + 1..].trim_start_matches('"').to_owned();
            let sessions = self.sessions.clone();
            let session = session.clone();
            let tx = self.results.clone();
            let generation = sessions.reference_service().generation();
            let service = sessions.reference_service().clone();
            tokio::spawn(async move {
                let target = session.clone();
                let worker_cancel = cancel.clone();
                let values = tokio::task::spawn_blocking(move || {
                    sessions.file_references(
                        &target,
                        &rness_engine::file_references::Query { query, limit: 50 },
                        &worker_cancel,
                    )
                })
                .await;
                if cancel.is_cancelled() || service.generation() != generation {
                    return;
                }
                if let Ok(Ok(paths)) = values {
                    let _ = tx.send(rness_tui::app::Action::CompletionResult(
                        session,
                        text,
                        paths.into_iter().map(|p| p.path).collect(),
                    ));
                }
            });
            return;
        }
        let generation = self
            .completion_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let current_generation = self.completion_generation.clone();
        let lock = self.completion_lock.clone();
        let tx = self.results.clone();
        let session = session.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let _guard = lock.lock().await;
            if current_generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
                return;
            }
            let prepared = sessions.prepare_command(&session, &text);
            let result = match prepared {
                Ok(Some(command)) => {
                    tokio::task::spawn_blocking(move || command.complete_items(&sessions))
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r.map_err(|e| e.to_string()))
                }
                Ok(None) => Ok(Vec::new()),
                Err(rness_engine::service::ServiceError::Busy) => return,
                Err(error) => Err(error.to_string()),
            };
            if current_generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
                return;
            }
            match result {
                Ok(values) => {
                    let _ = tx.send(rness_tui::app::Action::CommandCompletionResult(
                        session, text, values,
                    ));
                }
                Err(error) => {
                    let _ = tx.send(rness_tui::app::Action::CommandResult(
                        session,
                        format!("Completion failed: {error}"),
                    ));
                }
            }
        });
    }

    fn command_running(&self, session: &SessionId) -> bool {
        self.sessions.command_running(session)
    }

    fn submit(&self, request: ClientRequest) -> Result<Option<String>, String> {
        match request {
            ClientRequest::Send {
                session,
                intent,
                content,
            } => {
                let prepared = match content.as_slice() {
                    [ContentPart::Text { text }] => self
                        .sessions
                        .prepare_command(&session, text)
                        .map_err(|e| e.to_string())?,
                    _ => None,
                };
                if let Some(command) = prepared {
                    let sessions = self.sessions.clone();
                    let tx = self.results.clone();
                    tokio::spawn(async move {
                        let result =
                            tokio::task::spawn_blocking(move || command.execute(&sessions)).await;
                        let message = match result {
                            Ok(Ok(rness_engine::inbox::Disposition::Command(result))) => {
                                if let Some(action) = command_app_action(&result.data, &session) {
                                    let _ = tx.send(action);
                                }
                                result.message
                            }
                            Ok(Ok(_)) => {
                                unreachable!("prepared command always returns a command result")
                            }
                            Ok(Err(error)) => error.to_string(),
                            Err(error) => format!("Command failed: {error}"),
                        };
                        let _ = tx.send(rness_tui::app::Action::CommandResult(session, message));
                    });
                    return Ok(Some("Command running".into()));
                }
                match self
                    .sessions
                    .send(&session, intent, content)
                    .map_err(|e| e.to_string())?
                {
                    rness_engine::inbox::Disposition::Command(result) => {
                        if let Some(action) = command_app_action(&result.data, &session) {
                            let _ = self.results.send(action);
                        }
                        Ok(Some(if result.message.is_empty() {
                            "Command completed".into()
                        } else {
                            result.message
                        }))
                    }
                    _ => Ok(None),
                }
            }
            ClientRequest::Retry { session } => self
                .sessions
                .retry(&session)
                .map(|_| None)
                .map_err(|e| e.to_string()),
            ClientRequest::Cancel { session } => {
                self.sessions.cancel(&session);
                Ok(Some("Cancelled".into()))
            }
        }
    }

    fn request(&self, request: ClientRequest) {
        match request {
            ClientRequest::Send {
                session,
                intent,
                content,
            } => {
                if let Err(e) = self.sessions.send(&session, intent, content) {
                    tracing::error!(error = %e, "send failed");
                }
            }
            ClientRequest::Retry { session } => {
                if let Err(e) = self.sessions.retry(&session) {
                    tracing::error!(error = %e, "retry failed");
                }
            }
            ClientRequest::Cancel { session } => self.sessions.cancel(&session),
        }
    }

    fn history_after(
        &self,
        session: &SessionId,
        after: &str,
    ) -> Option<Vec<rness_protocol::events::Envelope>> {
        self.sessions
            .store()
            .history_after(session, after)
            .ok()
            .flatten()
    }

    fn history(&self, session: &SessionId) -> History {
        let envelopes = self.sessions.store().history(session).unwrap_or_default();
        History {
            session: session.clone(),
            envelopes,
        }
    }
}

async fn run_tui(
    kernel: Kernel,
    sessions: Arc<SessionService>,
    subagents: Arc<rness_engine::subagent::SubagentRuntime>,
    session: SessionId,
    model_name: String,
    questions: Arc<rness_engine::questions::Questions>,
    overlay_priority: i32,
    approval_rx: tokio::sync::mpsc::UnboundedReceiver<
        rness_tui::modules::approval::PendingApproval,
    >,
    lua: rness_lua::plugin_host::LuaHost,
    installed_lua_tools: rness_lua::api::tools::InstalledTools,
    schemes: std::collections::BTreeMap<String, serde_json::Value>,
    selected_scheme: Option<String>,
    promptbox_config: serde_json::Value,
    messagebox_config: serde_json::Value,
    control_path: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    use rness_tui::app::{App, Model};
    use rness_tui::modules::{approval, chat, ext_apps, ext_statusline, input, statusline};
    use rness_tui::slots::Slots;

    // Keep background streams flowing so switching sessions cannot lose deltas.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let watched = Arc::new(std::sync::RwLock::new(session.clone()));
    let _frame_sub = kernel.bus().on::<FrameEv>(move |frame| {
        let _ = tx.send(frame.clone());
    });

    // Built-in modules self-install through the same slot seam Lua
    // components will use (priority + wants() selection).
    let mut slots = Slots::default();
    let card_cache = chat::install(&mut slots);
    let agent_monitor = rness_tui::modules::agents::install(&mut slots, card_cache.clone());
    let monitor_frames = agent_monitor.clone();
    let _monitor_sub = kernel
        .bus()
        .on::<FrameEv>(move |frame| monitor_frames.observe(frame));
    let monitor_task = agent_monitor::spawn(
        sessions.clone(),
        subagents.clone(),
        lua.clone(),
        agent_monitor.clone(),
    );
    let mut candidates = vec![
        ("unload".into(), "Command: unload a plugin".into()),
        ("agent".into(), "Command: choose an agent".into()),
        ("skill".into(), "Command: invoke a skill".into()),
    ];
    for (name, definition) in sessions.agents() {
        candidates.push((format!("agent {name}"), definition.description.clone()));
    }
    let workspace = sessions
        .store()
        .workspace(&session)?
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    for skill in rness_tools::skills::discover(&rness_tools::skills::default_roots(&workspace)) {
        candidates.push((format!("skill {}", skill.name), skill.description.clone()));
        if !["agent", "skill", "unload"].contains(&skill.name.as_str()) {
            candidates.push((skill.name, format!("Skill: {}", skill.description)));
        }
    }
    let mut colorschemes = std::collections::BTreeMap::from([(
        "default".to_string(),
        rness_tui::theme::Theme::default(),
    )]);
    for (name, spec) in schemes {
        colorschemes.insert(
            name.clone(),
            rness_tui::theme::Theme::from_overrides(&spec)
                .map_err(|e| anyhow::anyhow!("colorscheme {name}: {e}"))?,
        );
    }
    candidates.push((
        "retry".into(),
        "Retry failed turn from saved context".into(),
    ));
    candidates.push(("colorscheme".into(), "Command: change colorscheme".into()));
    for name in colorschemes.keys() {
        candidates.push((format!("colorscheme {name}"), "Colorscheme".into()));
    }
    slots.mount(
        rness_tui::slots::INPUT_FOOTER,
        0,
        Box::new(input::Input::with_candidates(candidates)),
    );
    statusline::install(&mut slots);
    approval::install(&mut slots);

    // External (Lua) apps: mount the renderer early so the status poll
    // below can re-sync the roster after hot reloads.
    let apps_state = ext_apps::install(&mut slots);
    apps_state.set_session(session.clone());
    let mut question_overlay = questions::QuestionOverlay::new(questions.clone());
    question_overlay.apps = Some(apps_state.clone());
    slots.mount(
        rness_tui::slots::OVERLAY,
        overlay_priority,
        Box::new(question_overlay),
    );
    let apps_for_status = apps_state.clone();

    // Lua statusline: a background task polls the VM's provider and
    // publishes into the shared cell; the ext component shadows the
    // built-in only while text exists. TUI stays Lua-agnostic.
    let status_text = ext_statusline::install(&mut slots);
    // Lua has loaded all declarations before entering `run_tui`; publish its
    // first view before terminal rendering can show the built-in fallback.
    status_text
        .prime(
            &lua,
            serde_json::json!({
                "session": session,
                "model": model_name,
                "profile": serde_json::Value::Null,
                "agent": serde_json::Value::Null,
                "busy": false,
                "activity": "idle",
            }),
        )
        .await;
    // Host keymap: stock bindings + Lua rebinds (rness.keymaps.set).
    // Recomputed on the same poll as the statusline/roster so hot
    // reloads revert bindings from removed plugins.
    let keymap = rness_tui::keymaps::KeymapState::stock();

    let plugin_keymap = Arc::new(std::sync::RwLock::new(
        rness_tui::keymaps::ScopedKeymap::default(),
    ));
    let presentation_epoch = Arc::new(std::sync::Mutex::new(0u64));
    let status_task = {
        let epoch = presentation_epoch.clone();
        let lua = lua.clone();
        let cell = status_text.clone();
        let apps = apps_for_status.clone();
        let plugin_keymap = plugin_keymap.clone();
        let keymap = keymap.clone();
        tokio::spawn(async move {
            let mut previous_roster = None;
            loop {
                let generation = *epoch.lock().unwrap();
                cell.refresh(&lua).await;
                // Roster re-sync piggybacks on the same poll: after a hot
                // reload the fresh VM's apps replace the old set.
                let action_generation = lua
                    .action_generation()
                    .load(std::sync::atomic::Ordering::SeqCst);
                let specs = lua.app_specs().await;
                let key_help = specs
                    .iter()
                    .map(|spec| (spec.name.clone(), spec.key_help.clone()))
                    .collect();
                let roster: Vec<rness_tui::modules::ext_apps::AppInfo> =
                    specs.into_iter().map(app_info).collect();
                let binds = lua.keymap_binds().await;
                let mut declarations = Vec::new();
                for binding in lua.binding_specs().await {
                    use rness_tui::keymaps::{BindingLayer, Scope, ScopedBinding};
                    let scope = match binding.scope.as_str() {
                        "promptbox" => Scope::Promptbox,
                        "messagebox" => Scope::Messagebox,
                        scope if scope.starts_with("app:") => Scope::App(scope[4..].into()),
                        "global" => Scope::Global,
                        _ => continue,
                    };
                    for key in binding.keys {
                        if let Some(chord) = rness_tui::keys::Chord::parse(&key) {
                            declarations.push(ScopedBinding {
                                owner: binding.owner.clone(),
                                scope: scope.clone(),
                                chord,
                                action: binding.action.clone(),
                                layer: if binding.user {
                                    BindingLayer::User
                                } else {
                                    BindingLayer::PluginDefault
                                },
                            });
                        }
                    }
                }
                let resolved = rness_tui::keymaps::ScopedKeymap::resolve(&declarations);
                {
                    let current = epoch.lock().unwrap();
                    if *current == generation
                        && lua
                            .action_generation()
                            .load(std::sync::atomic::Ordering::SeqCst)
                            == action_generation
                    {
                        match resolved {
                            Ok(mut resolved) => {
                                resolved.generation = action_generation;
                                *plugin_keymap.write().unwrap() = resolved;
                            }
                            Err(errors) => {
                                *plugin_keymap.write().unwrap() = Default::default();
                                for error in errors {
                                    tracing::warn!(target: "lua", "{error}");
                                }
                            }
                        }
                        apps.set_key_help(key_help);
                        // set_apps requests a visible view even for identical metadata.
                        // Polling alone must not refresh event-driven apps; a changed
                        // runtime generation still refreshes same-metadata reloads.
                        let signature = (action_generation, roster);
                        if previous_roster.as_ref() != Some(&signature) {
                            apps.set_runtime_generation(signature.0);
                            apps.set_apps(signature.1.clone());
                            previous_roster = Some(signature);
                        }
                        for err in keymap.rebuild(&binds) {
                            tracing::warn!(target: "lua", "{err}");
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        })
    };

    // Lua tool cards: when a step commits, render any NEW tool results
    // through the Lua card seam off the render path. Miss (no renderer,
    // nil, error) = no cache entry = built-in card. Cursor deltas avoid
    // reassembling prior history until the session or plugin generation changes.
    let (card_tx, mut card_rx) = tokio::sync::mpsc::unbounded_channel::<SessionId>();
    let card_task = {
        let lua = lua.clone();
        let cache = card_cache.clone();
        let sessions = Arc::clone(&sessions);
        tokio::spawn(async move {
            let mut cursor: Option<(SessionId, String, u64)> = None;
            let mut calls: std::collections::HashMap<String, (String, serde_json::Value)> =
                Default::default();
            while let Some(sid) = card_rx.recv().await {
                let generation = cache.generation();
                let delta = cursor
                    .as_ref()
                    .filter(|(session, _, epoch)| session == &sid && *epoch == generation)
                    .and_then(|(_, after, _)| {
                        sessions.store().history_after(&sid, after).ok().flatten()
                    });
                let history = if let Some(delta) = delta {
                    delta
                } else {
                    calls.clear();
                    let Ok(history) = sessions.store().history(&sid) else {
                        continue;
                    };
                    history
                };
                if let Some(last) = history.last() {
                    cursor = Some((sid.clone(), last.id.clone(), generation));
                }
                for env in &history {
                    match &env.event {
                        rness_protocol::events::SessionEvent::AssistantMessage(m) => {
                            for part in &m.content {
                                if let ContentPart::ToolUse { call, name, args } = part {
                                    calls.insert(call.clone(), (name.clone(), args.clone()));
                                }
                            }
                        }
                        rness_protocol::events::SessionEvent::ToolResult(r) => {
                            if cache.contains(&r.call) {
                                continue;
                            }
                            let Some((_name, args)) = calls.get(&r.call) else {
                                continue;
                            };
                            let generation = cache.generation();
                            if let Some(lines) =
                                rness_engine::presentation::ToolCards::tool_card_result(
                                    &lua,
                                    args.clone(),
                                    r,
                                )
                                .await
                            {
                                cache.insert_if_current(generation, r.call.clone(), lines);
                            }
                        }
                        _ => {}
                    }
                }
            }
        })
    };
    // Tool results commit after the assistant step; refresh without waiting for another step.
    let card_feed = Arc::clone(&watched);
    let card_tx_sub = card_tx.clone();
    let _card_sub = kernel.bus().on::<FrameEv>(move |frame| {
        if let rness_protocol::frames::Frame::StepCommitted { session, .. }
        | rness_protocol::frames::Frame::HistoryChanged { session } = frame
        {
            if *session == *card_feed.read().unwrap() {
                let _ = card_tx_sub.send(session.clone());
            }
        }
    });

    let activity_runtime = subagents.clone();
    let _activity_sub = kernel
        .bus()
        .on::<FrameEv>(move |frame| activity_runtime.activity.observe(frame));
    let stream_runtime = subagents.clone();
    let _stream_sub =
        kernel
            .bus()
            .on::<rness_engine::subagent::ToolStreamEv>(move |(session, call, text)| {
                stream_runtime.activity.stream(session, call, text);
            });
    let activity_task = {
        let lua = lua.clone();
        let cache = card_cache.clone();
        let sessions = sessions.clone();
        let subagents = subagents.clone();
        let watched = watched.clone();
        tokio::spawn(async move {
            let mut recovery = activity_recovery::ActivityRecovery::default();
            let mut published = std::collections::HashMap::new();
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let session = watched.read().unwrap().clone();
                recovery.recover(&subagents.activity, &sessions, &session);
                let generation = cache.generation();
                for (call, args, mut presentation) in
                    subagents.activity.card_snapshots(&sessions, &session)
                {
                    let ms = presentation["elapsed_ms"].as_u64().unwrap_or(0);
                    presentation["elapsed_ms"] = serde_json::json!(ms / 1000 * 1000);
                    let key = (session.clone(), call.clone(), generation);
                    if published.get(&key) == Some(&presentation) {
                        continue;
                    }
                    let snapshot = presentation.clone();
                    let error = presentation["status"] == "error";
                    if let Some(lines) = rness_engine::presentation::ToolCards::tool_card_presented(
                        &lua,
                        "subagent",
                        args,
                        "",
                        error,
                        Some(presentation),
                    )
                    .await
                    {
                        if *watched.read().unwrap() == session {
                            if cache.insert_if_current(generation, call, lines) {
                                published.insert(key, snapshot);
                            }
                        }
                    }
                }
            }
        })
    };

    // Initial hydration: continued sessions may already hold tool results.
    let _ = card_tx.send(session.clone());

    let (host_tx, host_rx) = tokio::sync::mpsc::unbounded_channel();
    let (plugin_action_tx, mut plugin_action_rx) =
        tokio::sync::mpsc::unbounded_channel::<rness_tui::app::PluginActionRequest>();
    let input_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    apps_state.track_input_epoch(input_epoch.clone());
    let plugin_action_task = {
        let input_epoch = input_epoch.clone();
        let apps = apps_state.clone();
        let lua = lua.clone();
        let host_tx = host_tx.clone();
        tokio::spawn(async move {
            while let Some(request) = plugin_action_rx.recv().await {
                let Some(spec) = lua
                    .action_specs()
                    .await
                    .into_iter()
                    .find(|spec| spec.name == request.name)
                else {
                    continue;
                };
                if !matches!(spec.scope.as_str(), "promptbox" | "global" | "messagebox")
                    && !spec.scope.starts_with("app:")
                {
                    continue;
                }
                if let Some((app, activation)) = &request.app {
                    if apps.activation() != (Some(app.clone()), *activation)
                        || spec.scope != format!("app:{app}")
                    {
                        continue;
                    }
                } else if spec.scope.starts_with("app:") {
                    continue;
                }
                let guard_apps = apps.clone();
                let expected_app = request.app.clone();
                let guard_epoch = input_epoch.clone();
                match lua
                    .call_action_guarded(
                        request.generation,
                        &request.name,
                        &spec.scope,
                        serde_json::json!({"session":request.session}),
                        move || {
                            guard_epoch.load(std::sync::atomic::Ordering::SeqCst)
                                == request.input_epoch
                                && expected_app.is_none_or(|(name, generation)| {
                                    guard_apps.activation() == (Some(name), generation)
                                })
                        },
                    )
                    .await
                {
                    Ok(operations) => {
                        let operations = operations
                            .into_iter()
                            .map(|operation| match operation {
                                rness_lua::runtime::UiActionOperation::CloseApp(name) => {
                                    rness_tui::app::PluginOperation::CloseApp(name)
                                }
                                rness_lua::runtime::UiActionOperation::QueuePrompt => {
                                    rness_tui::app::PluginOperation::QueuePrompt
                                }
                                rness_lua::runtime::UiActionOperation::SteerPrompt => {
                                    rness_tui::app::PluginOperation::SteerPrompt
                                }
                                rness_lua::runtime::UiActionOperation::InsertPrompt(text) => {
                                    rness_tui::app::PluginOperation::InsertPrompt(text)
                                }
                            })
                            .collect();
                        let _ = host_tx.send(rness_tui::app::Action::PluginBatch {
                            request,
                            operations,
                        });
                    }
                    Err(error) => {
                        let _ = host_tx.send(rness_tui::app::Action::Notice(format!(
                            "Plugin action failed: {error}"
                        )));
                    }
                }
            }
        })
    };
    let backend = Arc::new(LocalBackend {
        completion_lock: Default::default(),
        completion_generation: Default::default(),
        reference_cancel: Default::default(),
        sessions: Arc::clone(&sessions),
        results: host_tx.clone(),
    });

    // Drive view/key round-trips for the mounted apps from a host task.
    // The TUI stays Lua-agnostic — it renders published lines and
    // forwards keys.
    let (app_ev_tx, mut app_ev_rx) = tokio::sync::mpsc::unbounded_channel();
    apps_state.connect(app_ev_tx);
    let plugin_catalog_task = {
        let lua = lua.clone();
        let tx = host_tx.clone();
        let sessions = sessions.clone();
        let watched = watched.clone();
        let workspace_fallback = std::env::current_dir()?;
        tokio::spawn(async move {
            let mut previous = None;
            let mut previous_references = None;
            let mut previous_commands = None;
            let mut previous_session = None;
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                tick.tick().await;
                let session = watched.read().unwrap().clone();
                if previous_session.as_ref() != Some(&session) {
                    if let Ok(workspace) = sessions.store().workspace(&session) {
                        let root = workspace
                            .map(std::path::PathBuf::from)
                            .unwrap_or_else(|| workspace_fallback.clone());
                        let skills: Vec<_> = rness_tools::skills::discover(
                            &rness_tools::skills::default_roots(&root),
                        )
                        .into_iter()
                        .map(|s| (s.name, s.description))
                        .collect();
                        if tx
                            .send(rness_tui::app::Action::Custom(
                                "input:skills".into(),
                                serde_json::json!(skills),
                            ))
                            .is_err()
                        {
                            break;
                        }
                    }
                    previous_session = Some(session);
                }
                let generation = sessions.reference_service().generation();
                if previous_references != Some(generation) {
                    let _ = tx.send(rness_tui::app::Action::Custom(
                        "input:references".into(),
                        serde_json::json!(generation),
                    ));
                    previous_references = Some(generation);
                }
                let commands = sessions.commands().completions();
                if previous_commands.as_ref() != Some(&commands) {
                    if tx
                        .send(rness_tui::app::Action::Custom(
                            "input:commands".into(),
                            serde_json::json!(commands),
                        ))
                        .is_err()
                    {
                        break;
                    }
                    previous_commands = Some(commands);
                }
                let names = lua.plugin_names().await;
                if previous.as_ref() != Some(&names) {
                    if tx
                        .send(rness_tui::app::Action::Custom(
                            "input:plugins".into(),
                            serde_json::json!(names),
                        ))
                        .is_err()
                    {
                        break;
                    }
                    previous = Some(names);
                }
            }
        })
    };
    apps_state.set_runtime_generation(
        lua.action_generation()
            .load(std::sync::atomic::Ordering::SeqCst),
    );
    let roster: Vec<ext_apps::AppInfo> = lua.app_specs().await.into_iter().map(app_info).collect();
    apps_state.set_apps(roster);
    let apps_task = {
        let epoch = presentation_epoch.clone();
        let status = status_text.clone();
        let cards = card_cache.clone();
        let monitor = agent_monitor.clone();
        let keys = keymap.clone();
        let lua = lua.clone();
        let state = apps_state.clone();
        let host_tx = host_tx.clone();
        let card_tx = card_tx.clone();
        let watched = Arc::clone(&watched);
        tokio::spawn(async move {
            // One serial worker: a busy VM cannot accumulate overlapping refreshes.
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_refresh = tokio::time::Instant::now();
            loop {
                let ev = tokio::select! {
                    ev = app_ev_rx.recv() => match ev { Some(ev) => ev, None => break },
                    _ = tick.tick() => {
                        if !app_refresh_due(state.refresh_ms(), last_refresh.elapsed()) { continue; }
                        let Some(app) = state.active() else { continue; };
                        ext_apps::AppEvent::Shown(app)
                    }
                };
                match ev {
                    ext_apps::AppEvent::Unload(name) => {
                        let epoch = epoch.clone();
                        let status = status.clone();
                        let cards = cards.clone();
                        let monitor = monitor.clone();
                        let keys = keys.clone();
                        let apps = state.clone();
                        let result = lua
                            .unload_coordinated(
                                &name,
                                installed_lua_tools.clone(),
                                move |snapshot| {
                                    let mut generation = epoch.lock().unwrap();
                                    *generation = generation
                                        .checked_add(1)
                                        .expect("presentation generation exhausted");
                                    status.invalidate();
                                    cards.invalidate();
                                    monitor.invalidate_cards();
                                    apps.invalidate();
                                    apps.set_apps(
                                        snapshot.apps.into_iter().map(app_info).collect(),
                                    );
                                    for err in keys.rebuild(&snapshot.keymap_binds) {
                                        tracing::warn!(target: "lua", "{err}");
                                    }
                                },
                            )
                            .await;
                        let notice = match result {
                            Ok(true) => {
                                let _ = card_tx.send(watched.read().unwrap().clone());
                                format!("Unloaded plugin: {name}")
                            }
                            Ok(false) => format!("Plugin is not loaded: {name}"),
                            Err(e) => format!("Could not unload {name}: {e}"),
                        };
                        let _ = host_tx.send(rness_tui::app::Action::Notice(notice));
                    }
                    ext_apps::AppEvent::Shown(app) => {
                        refresh_lua_app(&lua, &state, &app).await;
                        last_refresh = tokio::time::Instant::now();
                    }
                    ext_apps::AppEvent::Key(app, key, generation) => {
                        use rness_lua::runtime::AppKeyOutcome;
                        let runtime_generation = lua
                            .action_generation()
                            .load(std::sync::atomic::Ordering::SeqCst);
                        let Some(ctx) = state.context_if_current(generation, &app) else {
                            continue;
                        };
                        let guard = app_request_guard(
                            &lua,
                            &state,
                            &app,
                            generation,
                            runtime_generation,
                            &ctx,
                        );
                        let outcome = lua
                            .app_key_guarded(&app, &key, ctx.clone(), guard.clone())
                            .await;
                        if !guard() {
                            continue;
                        }
                        let mut refresh_view = false;
                        let mut unload = None;
                        let mut error = None;
                        state.apply_key_if_current(generation, &app, || {
                            match outcome {
                                Ok(AppKeyOutcome::Pass) => {
                                    if key == "esc" {
                                        return true;
                                    }
                                }
                                Ok(AppKeyOutcome::Consumed) => refresh_view = true,
                                Ok(AppKeyOutcome::Close) => return true,
                                Ok(AppKeyOutcome::Action { name, payload }) => {
                                    if name == "plugin:unload" {
                                        match payload
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .filter(|s| !s.is_empty())
                                        {
                                            Some(name) => unload = Some(name.to_owned()),
                                            None => {
                                                error = Some(
                                                    "plugin:unload requires payload.name".into(),
                                                )
                                            }
                                        }
                                    } else if name == "session:switch" {
                                        if let Some(id) =
                                            payload.get("session").and_then(|v| v.as_str())
                                        {
                                            *watched.write().unwrap() = id.to_string();
                                            let _ = card_tx.send(id.to_string());
                                            let _ = host_tx.send(
                                                rness_tui::app::Action::SwitchSession(
                                                    id.to_string(),
                                                ),
                                            );
                                            return true;
                                        }
                                    } else {
                                        let _ = host_tx
                                            .send(rness_tui::app::Action::Custom(name, payload));
                                        refresh_view = true;
                                    }
                                }
                                Err(e)
                                    if e.contains("runtime is busy")
                                        || e == "stale app request" => {}
                                Err(e) => error = Some(e),
                            }
                            false
                        });
                        if let Some(name) = unload {
                            state.request_unload(name);
                        }
                        if let Some(e) = error {
                            state.publish_if_current(generation, &app, vec![format!("error: {e}")]);
                        }
                        if refresh_view {
                            refresh_lua_app(&lua, &state, &app).await;
                            last_refresh = tokio::time::Instant::now();
                        }
                    }
                }
            }
        })
    };

    let mut app = App::new(Model::new(session.clone(), model_name), slots, backend);
    let displayed = Arc::new(std::sync::RwLock::new(session.clone()));
    app.displayed_session = Some(displayed.clone());
    app.apply(rness_tui::app::Action::Custom(
        "input:promptbox-config".into(),
        promptbox_config,
    ));
    app.theme
        .validate_messagebox(&messagebox_config)
        .map_err(anyhow::Error::msg)?;
    app.apply(rness_tui::app::Action::Custom(
        "chat:messagebox-config".into(),
        messagebox_config,
    ));
    app.colorschemes = colorschemes;
    if let Some(name) = selected_scheme {
        app.theme = app
            .colorschemes
            .get(&name)
            .context("unknown initial colorscheme")?
            .clone();
    }
    app.apps = Some(apps_state);
    app.input_epoch = input_epoch;
    app.plugin_generation = lua.action_generation();
    app.plugin_keymap = plugin_keymap;
    app.plugin_actions = Some(plugin_action_tx);
    app.keymap = keymap;
    #[cfg(feature = "experimental-control")]
    let control = control_path
        .as_deref()
        .map(|path| control_socket::start(path, sessions.clone(), displayed, host_tx.clone()))
        .transpose()?;
    #[cfg(not(feature = "experimental-control"))]
    let _ = control_path;
    let result = rness_tui::app::run(app, rx, approval_rx, host_rx).await;
    #[cfg(feature = "experimental-control")]
    if let Some(control) = control {
        control.shutdown().await;
    }
    result?;
    plugin_action_task.abort();
    plugin_catalog_task.abort();
    status_task.abort();
    card_task.abort();
    activity_task.abort();
    monitor_task.abort();
    apps_task.abort();

    // Suppress child-result wakes before letting the in-flight turn settle.
    sessions.begin_teardown(&session);
    sessions.join(&session).await;
    eprintln!("session: {session}");
    Ok(())
}

/// Raise the open-file-descriptor limit to the OS-allowed maximum.
/// macOS defaults to 256 which is far too low for a session store with
/// hundreds of sessions + subagent children + MCP servers + jobs.
/// This mirrors what Node.js (used by dsh) does automatically at startup.
fn raise_fd_limit() {
    #[cfg(unix)]
    {
        use std::io;
        // getrlimit / setrlimit via libc
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: rlim is a valid stack-allocated struct.
        let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
        if rc != 0 {
            tracing::debug!(
                error = %io::Error::last_os_error(),
                "getrlimit(NOFILE) failed; keeping inherited fd limit"
            );
            return;
        }
        let target = rlim.rlim_max.min(10240);
        if rlim.rlim_cur >= target {
            return;
        }
        rlim.rlim_cur = target;
        // SAFETY: rlim values are within the hard limit.
        let rc = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) };
        if rc != 0 {
            tracing::debug!(
                error = %io::Error::last_os_error(),
                target,
                "setrlimit(NOFILE) failed; keeping inherited fd limit"
            );
        }
    }
}

/// Match the saved workspace, not the repository root or a path prefix.
/// Canonicalization also recognizes alternate symlink spellings of the directory.
/// Excludes delegated (subagent) children — those are internal sessions.
fn sessions_in_directory(
    store: &rness_engine::session::branch::SessionStore,
    cwd: &std::path::Path,
) -> anyhow::Result<Vec<SessionId>> {
    let cwd = cwd.canonicalize()?;
    let mut matches = Vec::new();
    for id in store.list_roots()? {
        let Some(workspace) = store.workspace(&id)? else {
            continue;
        };
        let workspace = std::path::Path::new(&workspace);
        if workspace == cwd || workspace.canonicalize().is_ok_and(|path| path == cwd) {
            matches.push(id);
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod session_list_tests {
    use super::*;
    use rness_engine::session::branch::SessionStore;

    #[test]
    fn lists_only_sessions_saved_in_the_exact_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("expenses");
        let nested = project.join("nested");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(&other).unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let local = store.create(Some(project.display().to_string())).unwrap();
        store.create(Some(nested.display().to_string())).unwrap();
        store.create(Some(other.display().to_string())).unwrap();
        store
            .create(Some(dir.path().join("removed").display().to_string()))
            .unwrap();
        store.create(None).unwrap();

        assert_eq!(
            sessions_in_directory(&store, &project).unwrap(),
            vec![local.session().clone()]
        );
        assert!(sessions_in_directory(&store, dir.path())
            .unwrap()
            .is_empty());
        assert_eq!(store.list().unwrap().len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn recognizes_symlinked_workspace_paths() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("expenses");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&project).unwrap();
        std::os::unix::fs::symlink(&project, &alias).unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let session = store.create(Some(alias.display().to_string())).unwrap();

        assert_eq!(
            sessions_in_directory(&store, &project).unwrap(),
            vec![session.session().clone()]
        );
        assert_eq!(
            sessions_in_directory(&store, &alias).unwrap(),
            vec![session.session().clone()]
        );
    }
}

/// Command results can request only these session-scoped presentation routes.
fn command_app_action(
    data: &serde_json::Value,
    session: &SessionId,
) -> Option<rness_tui::app::Action> {
    let name = data.get("action")?.as_str()?;
    if !matches!(name, "app:open" | "agents:open") {
        return None;
    }
    let mut payload = data.clone();
    payload["session"] = serde_json::json!(session);
    Some(rness_tui::app::Action::Custom(name.into(), payload))
}

fn app_info(spec: rness_kernel::presentation::AppSpec) -> rness_tui::modules::ext_apps::AppInfo {
    rness_tui::modules::ext_apps::AppInfo {
        name: spec.name,
        slot: spec.slot,
        title: spec.title,
        keymap: spec.keymap,
        refresh_ms: spec.refresh_ms,
        capture_escape: spec.capture_escape,
        config: spec.config,
    }
}

fn app_refresh_due(refresh_ms: Option<u64>, elapsed: std::time::Duration) -> bool {
    refresh_ms.is_some_and(|ms| elapsed >= std::time::Duration::from_millis(ms.max(50)))
}

fn app_request_guard(
    lua: &rness_lua::plugin_host::LuaHost,
    state: &rness_tui::modules::ext_apps::AppsState,
    app: &str,
    generation: u64,
    runtime_generation: u64,
    ctx: &serde_json::Value,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    let runtime = lua.action_generation();
    let state = state.clone();
    let app = app.to_owned();
    let ctx = ctx.clone();
    Arc::new(move || {
        runtime.load(std::sync::atomic::Ordering::SeqCst) == runtime_generation
            && state.runtime_generation() == runtime_generation
            && state.context_if_current(generation, &app).as_ref() == Some(&ctx)
    })
}

async fn refresh_lua_app(
    lua: &rness_lua::plugin_host::LuaHost,
    state: &rness_tui::modules::ext_apps::AppsState,
    app: &str,
) -> bool {
    let runtime_generation = lua
        .action_generation()
        .load(std::sync::atomic::Ordering::SeqCst);
    refresh_app(state, app, |generation, ctx| async move {
        let guard = app_request_guard(lua, state, app, generation, runtime_generation, &ctx);
        let result = lua.app_view_guarded(app, ctx, guard.clone()).await;
        if guard() {
            result
        } else {
            Err("stale app request".into())
        }
    })
    .await
}

async fn refresh_app<F, Fut>(
    state: &rness_tui::modules::ext_apps::AppsState,
    app: &str,
    view: F,
) -> bool
where
    F: FnOnce(u64, serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, String>>,
{
    let generation = state.generation();
    let Some(ctx) = state.context_if_current(generation, app) else {
        return false;
    };
    let result = view(generation, ctx.clone()).await;
    if state.context_if_current(generation, app).as_ref() != Some(&ctx) {
        return false;
    }
    let lines = match result {
        Ok(lines) => lines,
        // Retain the last good view; opted-in ticks retry without a backlog.
        Err(error) if error.contains("runtime is busy") || error == "stale app request" => {
            return false
        }
        Err(error) => vec![format!("error: {error}")],
    };
    state.publish_if_current(generation, app, lines)
}

#[cfg(test)]
mod app_host_tests {
    use super::*;
    use rness_kernel::presentation::{AppKeyOutcome, AppSpec, Applications};
    use rness_tui::modules::ext_apps::AppsState;

    fn spec() -> AppSpec {
        AppSpec {
            name: "probe".into(),
            slot: "overlay".into(),
            title: "Probe".into(),
            keymap: None,
            key_help: vec![],
            refresh_ms: Some(125),
            capture_escape: true,
            config: serde_json::json!({"layout":{"width":72},"styles":{"border":"dim"}}),
        }
    }

    #[test]
    fn command_routes_are_allowlisted_and_bound_to_invoker() {
        for name in ["app:open", "agents:open"] {
            let data = serde_json::json!({"action":name,"session":"spoofed","name":"probe"});
            let Some(rness_tui::app::Action::Custom(route, payload)) =
                command_app_action(&data, &"invoker".into())
            else {
                panic!("missing route")
            };
            assert_eq!(route, name);
            assert_eq!(payload["session"], "invoker");
            assert_eq!(payload["name"], "probe");
        }
        for data in [
            serde_json::Value::Null,
            serde_json::json!([]),
            serde_json::json!({"action":"session:switch"}),
            serde_json::json!({"action":"plugin:unload"}),
        ] {
            assert!(command_app_action(&data, &"s".into()).is_none());
        }
    }

    #[test]
    fn metadata_and_refresh_opt_in_are_preserved() {
        let info = app_info(spec());
        assert_eq!(info.refresh_ms, Some(125));
        assert!(info.capture_escape);
        assert_eq!(info.config, spec().config);
        let ms = std::time::Duration::from_millis;
        assert!(!app_refresh_due(None, ms(u64::MAX)));
        for interval in [0, 1, 49, 50] {
            assert!(!app_refresh_due(Some(interval), ms(49)));
            assert!(app_refresh_due(Some(interval), ms(50)));
        }
        assert!(!app_refresh_due(Some(125), ms(124)));
        assert!(app_refresh_due(Some(125), ms(125)));
    }

    #[tokio::test]
    async fn request_guard_rejects_unsynchronized_runtime_and_same_metadata_reload() {
        let lua = rness_lua::plugin_host::LuaHost::spawn().unwrap();
        let state = AppsState::default();
        state.set_session("s".into());
        let runtime = lua
            .action_generation()
            .load(std::sync::atomic::Ordering::SeqCst);
        state.set_runtime_generation(runtime);
        state.set_apps(vec![app_info(spec())]);
        state.open("probe");
        let generation = state.generation();
        let ctx = state.context_if_current(generation, "probe").unwrap();
        let guard = app_request_guard(&lua, &state, "probe", generation, runtime, &ctx);
        assert!(guard());
        lua.load("other", "").await.unwrap();
        assert!(!guard());
        let fresh = lua
            .action_generation()
            .load(std::sync::atomic::Ordering::SeqCst);
        assert!(!app_request_guard(
            &lua, &state, "probe", generation, fresh, &ctx
        )());
        state.set_runtime_generation(fresh);
        state.set_apps(vec![app_info(spec())]);
        assert_eq!(state.active().as_deref(), Some("probe"));
        assert!(state.context_if_current(generation, "probe").is_none());
    }

    struct ViewProbe {
        state: AppsState,
        change: &'static str,
    }
    #[async_trait::async_trait]
    impl Applications for ViewProbe {
        async fn app_specs(&self) -> Vec<AppSpec> {
            vec![spec()]
        }
        async fn app_key(
            &self,
            _: &str,
            _: &str,
            _: serde_json::Value,
        ) -> Result<AppKeyOutcome, String> {
            unreachable!()
        }
        async fn app_view(&self, _: &str, ctx: serde_json::Value) -> Result<Vec<String>, String> {
            assert_eq!(ctx["session"], "s");
            assert!(ctx.get("rows").is_none());
            match self.change {
                "closed" => panic!("closed app must not call Lua"),
                "close" => self.state.close(),
                "reopen" => {
                    self.state.close();
                    self.state.open("probe");
                }
                "session" => self.state.set_session("other".into()),
                "roundtrip" => {
                    self.state.set_session("other".into());
                    self.state.set_session("s".into());
                }
                "reload" => self.state.set_runtime_generation(2),
                "busy" => return Err("extension runtime is busy".into()),
                _ => {}
            }
            Ok(vec!["fresh".into()])
        }
    }

    #[tokio::test]
    async fn refresh_rejects_closed_reopened_switched_and_busy_results() {
        for change in [
            "closed",
            "close",
            "reopen",
            "session",
            "roundtrip",
            "reload",
            "busy",
            "none",
        ] {
            let state = AppsState::default();
            state.set_session("s".into());
            state.set_apps(vec![app_info(spec())]);
            if change != "closed" {
                assert!(state.open("probe"));
            }
            let probe = ViewProbe {
                state: state.clone(),
                change,
            };
            assert_eq!(
                refresh_app(&state, "probe", |_, ctx| probe.app_view("probe", ctx)).await,
                change == "none",
                "{change}"
            );
        }
    }
}

/// Find the first available API key for `base_provider`, checking the bare key
/// first then any `base_provider/<account>` keys in sorted order.
fn first_available_api_key(store: &CredentialStore, base_provider: &str) -> Option<String> {
    let accounts = store.accounts(base_provider).ok()?;
    // Try bare key first (None account), then sorted named accounts.
    for acct in &accounts {
        let key = CredentialStore::credential_key(base_provider, acct.as_deref());
        if let Ok(Some(k)) = store.api_key(&key) {
            return Some(k);
        }
    }
    None
}

/// Find the first credential key that has OAuth tokens for `base_provider`.
fn first_available_credential(store: &CredentialStore, base_provider: &str) -> Option<String> {
    let accounts = store.accounts(base_provider).ok()?;
    for acct in &accounts {
        let key = CredentialStore::credential_key(base_provider, acct.as_deref());
        if store
            .tokens(&key)
            .ok()
            .flatten()
            .is_some_and(|t| !t.access_token.is_empty())
        {
            return Some(key);
        }
    }
    None
}

async fn run_auth(action: AuthAction) -> anyhow::Result<()> {
    let store = CredentialStore::new(CredentialStore::default_path());
    match action {
        AuthAction::Login { provider, account } => {
            let cred_key = CredentialStore::credential_key(&provider, account.as_deref());
            let label = if let Some(ref a) = account {
                format!("{provider} (account: {a})")
            } else {
                provider.clone()
            };
            let prompt = LoginPrompt {
                on_url: Box::new(|url| {
                    println!("If the browser didn't open, visit:\n\n  {url}\n");
                    println!("Waiting for authentication…");
                }),
            };
            match provider.as_str() {
                "anthropic" => {
                    println!("Opening browser to log in with your Claude account ({label})…");
                    let tokens =
                        login_as(OAuthConfig::default(), &store, &prompt, &cred_key).await?;
                    print!("Login successful");
                    if !tokens.subscription_type.is_empty() {
                        print!(" ({} subscription)", tokens.subscription_type);
                    }
                    println!("!");
                }
                "openai-chatgpt" | "openai" | "chatgpt" => {
                    println!("Opening browser to sign in with ChatGPT ({label})…");
                    let tokens =
                        rness_providers::auth::openai::login_as(&store, &prompt, &cred_key).await?;
                    print!("Login successful");
                    if let Some(email) = tokens.extra.get("email").and_then(|v| v.as_str()) {
                        print!(" ({email})");
                    }
                    println!("!");
                }
                other => bail!("no OAuth login for '{other}' (anthropic or openai-chatgpt)"),
            }
        }
        AuthAction::SetKey {
            key,
            provider,
            account,
        } => {
            let cred_key = CredentialStore::credential_key(&provider, account.as_deref());
            let label = if let Some(ref a) = account {
                format!("{provider} (account: {a})")
            } else {
                provider.clone()
            };
            let key = match key {
                Some(k) => k,
                None => {
                    use std::io::BufRead;
                    println!("Paste your {label} API key:");
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line)?;
                    line.trim().to_string()
                }
            };
            if key.is_empty() {
                bail!("empty API key");
            }
            store.save_api_key(&cred_key, &key)?;
            println!("{label} API key saved to {}", store.path().display());
        }
        AuthAction::Status => {
            let config = rness_lua::api::config::load(
                &dirs::home_dir()
                    .context("no home directory")?
                    .join(".rness/init.lua"),
            )
            .map_err(|e| anyhow::anyhow!("startup configuration: {e}"))?;
            let mut names = store.list()?;
            for (name, declaration) in &config.providers {
                match &declaration.auth {
                    rness_lua::api::config::ProviderAuth::Store { credential } => {
                        names.push(credential.clone());
                        // Also collect accounts for this credential.
                        for acct in store.accounts(credential)? {
                            if let Some(a) = acct {
                                names.push(CredentialStore::credential_key(credential, Some(&a)));
                            }
                        }
                    }
                    rness_lua::api::config::ProviderAuth::OAuth { oauth } => {
                        names.push(oauth.clone());
                        for acct in store.accounts(oauth)? {
                            if let Some(a) = acct {
                                names.push(CredentialStore::credential_key(oauth, Some(&a)));
                            }
                        }
                    }
                    rness_lua::api::config::ProviderAuth::Env { env } => {
                        let state = if std::env::var(env).is_ok_and(|v| !v.is_empty()) {
                            "set"
                        } else {
                            "not set"
                        };
                        println!("{name:<16}env {env} ({state})");
                        if let Some(ref default_acct) = declaration.default_account {
                            println!("{:<16}default account: {default_acct}", "");
                        }
                    }
                    _ => {}
                }
            }
            names.sort();
            names.dedup();

            for name in names {
                let mut states = Vec::new();
                if store.api_key(&name)?.is_some() {
                    states.push("api key stored".into());
                }
                if let Some(t) = store.tokens(&name)? {
                    if !t.access_token.is_empty() {
                        let mut s = if t.is_expired(0) {
                            "oauth expired (will auto-refresh)".to_string()
                        } else {
                            "oauth valid".to_string()
                        };
                        if !t.subscription_type.is_empty() {
                            s.push_str(&format!(" — {} subscription", t.subscription_type));
                        }
                        states.push(s);
                    }
                }
                if states.is_empty() {
                    states.push("none".into());
                }
                // Show account-qualified names with a nicer format.
                let display = if let Some((base, acct)) = name.split_once('/') {
                    format!("{base:<12} [{acct}]")
                } else {
                    name.clone()
                };
                println!("{display:<16}{}", states.join(", "));
            }
            println!("\nfile: {}", store.path().display());
        }
        AuthAction::Logout { provider, account } => {
            let cred_key = CredentialStore::credential_key(&provider, account.as_deref());
            let label = if let Some(ref a) = account {
                format!("{provider} (account: {a})")
            } else {
                provider.clone()
            };
            store.delete(&cred_key)?;
            println!("{label} credentials removed.");
        }
    }
    Ok(())
}
