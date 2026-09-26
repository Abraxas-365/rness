//! Claude Code / Codex command-hook compatibility (dsh `hooks-claude-code` +
//! `hooks-codex` parity, plus the gaps dsh leaves as TODOs).
//!
//! Existing `hooks.json` / `settings.json` hook configs run unmodified:
//!
//! | Event            | rness seam                 | Effect                                        |
//! |------------------|----------------------------|-----------------------------------------------|
//! | SessionStart     | `session/start` bus event  | context delivered on the session's next step  |
//! | UserPromptSubmit | `pre_step` (new prompt)    | block → reject the step; context → injected   |
//! | PreToolUse       | `pre_tool`                 | deny / ask / allow (no objection)             |
//! | PostToolUse      | `post_tool`                | block → corrective feedback; context          |
//! | Stop             | `turn_stopping`            | block → continue the turn with the reason     |
//! | SubagentStart    | `subagent/start` (CC only) | context delivered to the child's first step   |
//! | SubagentStop     | `subagent/stop` (CC only)  | observe                                       |
//!
//! Protocol (both dialects): JSON payload on stdin, exit 2 = block with
//! stderr as the reason, exit 0 + JSON object stdout = structured output
//! (`continue`, `stopReason`, `decision`, `reason`, `systemMessage`,
//! `hookSpecificOutput.{hookEventName, permissionDecision,
//! permissionDecisionReason, additionalContext}`), exit 0 + plain stdout =
//! context for SessionStart/UserPromptSubmit. Several matching hooks merge
//! deny > ask > allow; every context is kept.
//!
//! Beyond dsh: SessionStart/SubagentStart context is gated so it lands on
//! the first step (dsh may miss it), `stop_hook_active` is real and forced
//! continuations are capped, `continue: false` halts the turn, the payload
//! carries a real `transcript_path`, `file_path` is aliased for
//! Read/Edit/Write so CC hooks that inspect it work, and hook commands run
//! off the Lua VM (no 30 s ceiling; default timeout 600 s like CC).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rness_engine::service::SessionService;
use rness_engine::tools::hooks::{
    ExecuteNext, ExecuteOutcome, HookMessage, PostToolDecision, PreToolDecision, ToolHookEvent,
    ToolHooks,
};
use rness_engine::turn::hooks::{
    LoopEvent, LoopHooks, PreStepDecision, RequestError, RequestErrorAction, TurnStoppingAction,
};
use rness_protocol::events::{
    ContentPart, MessageSource, SessionEvent, ToolResult, ToolResultContentPart, UserIntent,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

/// CC's default per-hook timeout (seconds) when a hook sets none.
const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// Consecutive Stop-hook continuations allowed within one turn.
const DEFAULT_MAX_STOP_CONTINUATIONS: u32 = 8;
/// Bounded stderr kept in audit events and reasons.
const STDERR_SUMMARY_MAX: usize = 2000;
/// Tag on hook-injected messages (`user.sources.hook.tags.hooks`).
const MESSAGE_TAG: &str = "hooks";

// ── configuration ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    ClaudeCode,
    Codex,
}

impl Dialect {
    fn as_str(self) -> &'static str {
        match self {
            Dialect::ClaudeCode => "claude-code",
            Dialect::Codex => "codex",
        }
    }
}

fn default_dialect() -> Dialect {
    Dialect::ClaudeCode
}

/// `rness.hooks = { … }` in init.lua.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    /// Load `~/.claude/settings.json`, `<project>/.claude/settings.json`
    /// and `<project>/.claude/settings.local.json`.
    #[serde(default)]
    pub claude_code: bool,
    /// Load `~/.codex/hooks.json` and `<project>/.codex/hooks.json`.
    #[serde(default)]
    pub codex: bool,
    /// Extra config files: `{ path = "…", dialect = "claude_code" | "codex" }`.
    #[serde(default)]
    pub files: Vec<HookFile>,
    /// Default per-hook timeout in seconds (600).
    pub timeout: Option<u64>,
    /// Cap on consecutive Stop-hook continuations per turn (8).
    pub max_stop_continuations: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookFile {
    pub path: PathBuf,
    #[serde(default = "default_dialect")]
    pub dialect: Dialect,
}

impl HooksConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.timeout == Some(0) {
            return Err("rness.hooks.timeout must be a positive number of seconds".into());
        }
        Ok(())
    }
}

/// Config files to load, in order: rness's own `hooks.json` files (CC
/// event names in them use the CC dialect), then the opted-in CC/Codex
/// locations, then explicit `files`.
pub fn discover(config: &HooksConfig, cwd: &Path, home: Option<&Path>) -> Vec<HookFile> {
    let root = rness_tools::skills::project_root(cwd);
    let mut files = Vec::new();
    let mut push = |path: PathBuf, dialect| files.push(HookFile { path, dialect });
    push(cwd.join(".rness/hooks.json"), Dialect::ClaudeCode);
    if let Some(home) = home {
        push(home.join(".rness/hooks.json"), Dialect::ClaudeCode);
    }
    if config.claude_code {
        if let Some(home) = home {
            push(home.join(".claude/settings.json"), Dialect::ClaudeCode);
        }
        push(root.join(".claude/settings.json"), Dialect::ClaudeCode);
        push(
            root.join(".claude/settings.local.json"),
            Dialect::ClaudeCode,
        );
    }
    if config.codex {
        if let Some(home) = home {
            push(home.join(".codex/hooks.json"), Dialect::Codex);
        }
        push(root.join(".codex/hooks.json"), Dialect::Codex);
    }
    for file in &config.files {
        let path = if file.path.is_absolute() {
            file.path.clone()
        } else {
            cwd.join(&file.path)
        };
        push(path, file.dialect);
    }
    // One file may be reachable twice (e.g. cwd is the home directory).
    let mut seen = HashSet::new();
    files.retain(|f| seen.insert(f.path.canonicalize().unwrap_or_else(|_| f.path.clone())));
    files
}

// ── parsed config ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    SubagentStart,
    SubagentStop,
}

impl Event {
    pub fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::PreToolUse => "PreToolUse",
            Event::PostToolUse => "PostToolUse",
            Event::Stop => "Stop",
            Event::SubagentStart => "SubagentStart",
            Event::SubagentStop => "SubagentStop",
        }
    }

    fn parse(name: &str, dialect: Dialect) -> Option<Self> {
        let event = match name {
            "SessionStart" => Event::SessionStart,
            "UserPromptSubmit" => Event::UserPromptSubmit,
            "PreToolUse" => Event::PreToolUse,
            "PostToolUse" => Event::PostToolUse,
            "Stop" => Event::Stop,
            "SubagentStart" => Event::SubagentStart,
            "SubagentStop" => Event::SubagentStop,
            _ => return None,
        };
        // Codex has no subagent events.
        if dialect == Dialect::Codex && matches!(event, Event::SubagentStart | Event::SubagentStop)
        {
            return None;
        }
        Some(event)
    }

    /// Events without a matcher subject (matchers are ignored, like CC).
    fn subjectless(self) -> bool {
        matches!(self, Event::UserPromptSubmit | Event::Stop)
    }
}

/// Whether `name` is a CC/Codex-style event key (PascalCase), as opposed
/// to a native rness hook point — used to split a mixed `hooks.json`.
pub fn is_foreign_event(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

#[derive(Debug)]
enum Matcher {
    All,
    Literal(Vec<String>),
    Regex(regex::Regex),
}

impl Matcher {
    fn compile(pattern: Option<&str>, dialect: Dialect) -> Result<Self, String> {
        let Some(pattern) = pattern.filter(|p| !p.is_empty() && *p != "*") else {
            return Ok(Matcher::All);
        };
        // CC: a plain identifier list is an exact `|` alternation; anything
        // else is a regex. Codex: always a regex.
        let literal = pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|');
        if dialect == Dialect::ClaudeCode && literal {
            return Ok(Matcher::Literal(
                pattern.split('|').map(str::to_owned).collect(),
            ));
        }
        regex::Regex::new(pattern).map(Matcher::Regex).map_err(|e| {
            format!(
                "invalid {} regex matcher {pattern:?}: {e}",
                dialect.as_str()
            )
        })
    }

    fn matches(&self, subject: &str) -> bool {
        match self {
            Matcher::All => true,
            Matcher::Literal(names) => names.iter().any(|n| n == subject),
            Matcher::Regex(re) => re.is_match(subject),
        }
    }
}

#[derive(Debug)]
struct Command {
    command: String,
    timeout: Option<u64>,
}

#[derive(Debug)]
struct Group {
    matcher_text: Option<String>,
    matcher: Matcher,
    hooks: Vec<Command>,
}

#[derive(Debug)]
struct Source {
    dialect: Dialect,
    events: HashMap<Event, Vec<Group>>,
}

/// Parse one config document. Returns the source plus warnings for keys
/// that were skipped. An invalid regex rejects the whole file (dsh).
fn parse_source(
    value: &serde_json::Value,
    dialect: Dialect,
    settings_file: bool,
) -> Result<(Source, Vec<String>), String> {
    let root = value
        .as_object()
        .ok_or("hook config must be a JSON object")?;
    let map = match root.get("hooks") {
        Some(serde_json::Value::Object(map)) => map,
        Some(_) => return Err("\"hooks\" must be an object".into()),
        // A settings file without a hooks key simply has no hooks.
        None if settings_file => {
            return Ok((
                Source {
                    dialect,
                    events: HashMap::new(),
                },
                Vec::new(),
            ))
        }
        None => root,
    };
    let mut events: HashMap<Event, Vec<Group>> = HashMap::new();
    let mut warnings = Vec::new();
    for (key, raw_groups) in map {
        if !is_foreign_event(key) {
            continue; // native rness point: handled by the Lua bridge
        }
        let Some(event) = Event::parse(key, dialect) else {
            warnings.push(format!(
                "unsupported {} hook event {key:?} ignored",
                dialect.as_str()
            ));
            continue;
        };
        let Some(raw_groups) = raw_groups.as_array() else {
            return Err(format!("{key}: expected an array of matcher groups"));
        };
        for raw_group in raw_groups {
            let Some(group) = raw_group.as_object() else {
                continue;
            };
            let Some(raw_hooks) = group.get("hooks").and_then(|h| h.as_array()) else {
                continue;
            };
            let mut hooks = Vec::new();
            for raw_hook in raw_hooks {
                let Some(hook) = raw_hook.as_object() else {
                    continue;
                };
                let kind = hook
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("command");
                if kind != "command" {
                    warnings.push(format!("{key}: unsupported {kind:?} hook skipped"));
                    continue;
                }
                let Some(command) = hook.get("command").and_then(|c| c.as_str()) else {
                    continue;
                };
                let timeout = hook
                    .get("timeout")
                    .or_else(|| hook.get("timeoutSec"))
                    .and_then(|t| t.as_f64())
                    .filter(|t| *t > 0.0)
                    .map(|t| t.ceil() as u64);
                hooks.push(Command {
                    command: command.to_owned(),
                    timeout,
                });
            }
            if hooks.is_empty() {
                continue;
            }
            let matcher_text = if event.subjectless() {
                None
            } else {
                group
                    .get("matcher")
                    .and_then(|m| m.as_str())
                    .map(str::to_owned)
            };
            let matcher = Matcher::compile(matcher_text.as_deref(), dialect)?;
            events.entry(event).or_default().push(Group {
                matcher_text,
                matcher,
                hooks,
            });
        }
    }
    Ok((Source { dialect, events }, warnings))
}

// ── hook output protocol ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Decision {
    None,
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Default)]
struct Output {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    decision: Option<Decision>,
    reason: Option<String>,
    cont: Option<bool>,
    stop_reason: Option<String>,
    additional_context: Option<String>,
    system_message: Option<String>,
}

/// Port of dsh `parseHookOutput` (both dialects share it).
fn parse_output(exit_code: Option<i32>, stdout: &str, stderr: &str, event: Event) -> Output {
    let mut out = Output {
        exit_code,
        stdout: stdout.trim().to_owned(),
        stderr: stderr.trim().to_owned(),
        ..Output::default()
    };
    if exit_code == Some(2) {
        out.decision = Some(Decision::Deny);
        if !out.stderr.is_empty() {
            out.reason = Some(out.stderr.clone());
        }
    }
    if exit_code != Some(0) || !out.stdout.starts_with('{') {
        return out;
    }
    let Ok(serde_json::Value::Object(parsed)) =
        serde_json::from_str::<serde_json::Value>(&out.stdout)
    else {
        return out;
    };
    let s = |map: &serde_json::Map<String, serde_json::Value>, k: &str| {
        map.get(k).and_then(|v| v.as_str()).map(str::to_owned)
    };
    out.cont = parsed.get("continue").and_then(|v| v.as_bool());
    out.stop_reason = s(&parsed, "stopReason");
    out.system_message = s(&parsed, "systemMessage");
    match s(&parsed, "decision").as_deref() {
        Some("approve") => out.decision = Some(Decision::Allow),
        Some("block") => out.decision = Some(Decision::Deny),
        _ => {}
    }
    if let Some(reason) = s(&parsed, "reason") {
        out.reason = Some(reason);
    }
    if let Some(serde_json::Value::Object(hso)) = parsed.get("hookSpecificOutput") {
        // A block keyed to another event cannot affect this one.
        if s(hso, "hookEventName").as_deref() != Some(event.name()) {
            return out;
        }
        match s(hso, "permissionDecision").as_deref() {
            Some("allow") => out.decision = Some(Decision::Allow),
            Some("ask") => out.decision = Some(Decision::Ask),
            Some("deny") => out.decision = Some(Decision::Deny),
            _ => {}
        }
        if let Some(reason) = s(hso, "permissionDecisionReason") {
            out.reason = Some(reason);
        }
        out.additional_context = s(hso, "additionalContext");
        if hso.get("updatedInput").is_some() {
            tracing::warn!(
                event = event.name(),
                "hook requested updatedInput; rness freezes tool arguments (ignored)"
            );
        }
    }
    out
}

#[derive(Debug, Default)]
struct Merged {
    decision: Option<Decision>,
    reason: Option<String>,
    stop: bool,
    stop_reason: Option<String>,
    contexts: Vec<String>,
}

impl Merged {
    fn denied(&self) -> bool {
        self.decision == Some(Decision::Deny)
    }
}

/// Port of dsh `mergeHookOutputs`: deny > ask > allow; only the reasons
/// explaining the winning objection surface.
fn merge(outputs: Vec<Output>) -> Merged {
    let mut best = Decision::None;
    let mut reasons: HashMap<Decision, Vec<String>> = HashMap::new();
    let mut merged = Merged::default();
    for out in outputs {
        let decision = out.decision.unwrap_or(Decision::None);
        best = best.max(decision);
        if matches!(decision, Decision::Deny | Decision::Ask) {
            if let Some(reason) = out.reason.filter(|r| !r.is_empty()) {
                reasons.entry(decision).or_default().push(reason);
            }
        }
        if out.cont == Some(false) && !merged.stop {
            merged.stop = true;
            merged.stop_reason = out.stop_reason;
        }
        if let Some(ctx) = out.additional_context.filter(|c| !c.trim().is_empty()) {
            merged.contexts.push(ctx);
        }
        if let Some(msg) = out.system_message.filter(|m| !m.is_empty()) {
            tracing::info!("hook systemMessage: {msg}");
        }
    }
    merged.decision = (best != Decision::None).then_some(best);
    merged.reason = reasons.remove(&best).map(|r| r.join("\n\n"));
    merged
}

// ── process runner ───────────────────────────────────────────────────────

struct Run {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run_command(
    command: &str,
    stdin: String,
    cwd: Option<&Path>,
    env: &[(&str, &str)],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Run {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    if let Some(cwd) = cwd.filter(|d| d.is_dir()) {
        cmd.current_dir(cwd);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Run {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("spawn failed: {e}"),
            }
        }
    };
    let pid = child.id();
    if let Some(mut pipe) = child.stdin.take() {
        // A hook that never reads stdin must not wedge the writer.
        tokio::spawn(async move {
            let _ = pipe.write_all(stdin.as_bytes()).await;
        });
    }
    let kill_group = || {
        #[cfg(unix)]
        if let Some(pid) = pid {
            // SAFETY: signalling our own child's process group.
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            kill_group();
            Run { exit_code: None, stdout: String::new(), stderr: "hook cancelled".into() }
        }
        _ = tokio::time::sleep(timeout) => {
            kill_group();
            Run { exit_code: None, stdout: String::new(), stderr: format!("hook timed out after {}s", timeout.as_secs()) }
        }
        output = child.wait_with_output() => match output {
            Ok(o) => {
                // The shell exited; reap anything it left in the group.
                kill_group();
                Run {
                    exit_code: o.status.code(),
                    stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                }
            }
            Err(e) => Run { exit_code: None, stdout: String::new(), stderr: format!("wait failed: {e}") },
        },
    }
}

fn bounded(text: &str) -> String {
    if text.len() <= STDERR_SUMMARY_MAX {
        return text.to_owned();
    }
    format!("{}…", &text[..text.floor_char_boundary(STDERR_SUMMARY_MAX)])
}

// ── the layer ────────────────────────────────────────────────────────────

#[derive(Default)]
struct State {
    /// `continue: false` from a tool hook: the next step is rejected.
    halt: HashSet<String>,
    /// session → (turn, consecutive Stop continuations in that turn).
    stop_forced: HashMap<String, (u32, u32)>,
    /// Context from SessionStart/SubagentStart awaiting the next step.
    pending: HashMap<String, Vec<HookMessage>>,
    /// In-flight SessionStart/SubagentStart runs the next step waits for.
    gates: HashMap<String, Vec<Arc<tokio::sync::Mutex<()>>>>,
    /// Sessions whose SessionStart already ran in this process. rness
    /// emits `session/start` on every idle→running burst; CC fires it once
    /// per startup/resume, and again only after a compaction.
    started: HashSet<String>,
}

/// Compiled CC/Codex hooks plus the per-session state they need.
pub struct CommandHooks {
    sources: Vec<Source>,
    default_timeout: Duration,
    max_stop_continuations: u32,
    sessions: Arc<SessionService>,
    state: Mutex<State>,
    fallback_cwd: PathBuf,
}

/// Facts shared by every payload of one invocation.
struct Ctx<'a> {
    session: &'a str,
    turn: u32,
    cancel: &'a CancellationToken,
}

impl CommandHooks {
    /// Load every discovered file. Missing files are skipped; a file that
    /// fails to parse is reported and skipped (the rest still load).
    pub fn load(
        files: &[HookFile],
        config: &HooksConfig,
        sessions: Arc<SessionService>,
        cwd: PathBuf,
    ) -> (Option<Arc<Self>>, Vec<String>) {
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file.path) else {
                continue;
            };
            let name = file.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let settings = name.starts_with("settings");
            let parsed = serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| e.to_string())
                .and_then(|v| parse_source(&v, file.dialect, settings));
            match parsed {
                Ok((source, file_warnings)) => {
                    for w in file_warnings {
                        warnings.push(format!("{}: {w}", file.path.display()));
                    }
                    if !source.events.is_empty() {
                        tracing::info!(path = %file.path.display(), dialect = file.dialect.as_str(), "loaded command hooks");
                        sources.push(source);
                    }
                }
                Err(e) => warnings.push(format!("{}: {e}", file.path.display())),
            }
        }
        if sources.is_empty() {
            return (None, warnings);
        }
        let hooks = Self {
            sources,
            default_timeout: Duration::from_secs(config.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS)),
            max_stop_continuations: config
                .max_stop_continuations
                .unwrap_or(DEFAULT_MAX_STOP_CONTINUATIONS),
            sessions,
            state: Mutex::new(State::default()),
            fallback_cwd: cwd,
        };
        (Some(Arc::new(hooks)), warnings)
    }

    fn has(&self, event: Event) -> bool {
        self.sources.iter().any(|s| s.events.contains_key(&event))
    }

    fn workspace(&self, session: &str) -> PathBuf {
        self.sessions
            .store()
            .workspace(&session.to_owned())
            .ok()
            .flatten()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.fallback_cwd.clone())
    }

    fn base_payload(
        &self,
        dialect: Dialect,
        event: Event,
        ctx: &Ctx,
        cwd: &Path,
    ) -> serde_json::Value {
        let transcript = self.sessions.store().root().join(ctx.session).join(format!(
            "session.v{}.jsonl",
            rness_protocol::events::FORMAT_VERSION
        ));
        let mut payload = serde_json::json!({
            "session_id": ctx.session,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": event.name(),
        });
        if dialect == Dialect::Codex {
            let model = self
                .sessions
                .config(&ctx.session.to_owned())
                .ok()
                .and_then(|c| c.selection)
                .map(|s| s.model)
                .unwrap_or_default();
            payload["model"] = model.into();
            payload["permission_mode"] = "default".into();
            if !matches!(event, Event::SessionStart) {
                payload["turn_id"] = ctx.turn.to_string().into();
            }
        }
        payload
    }

    /// Run every matching hook of `event` (all sources, in order) and merge.
    async fn run_point(
        &self,
        event: Event,
        subject: &str,
        ctx: &Ctx<'_>,
        extra: impl Fn(Dialect) -> serde_json::Value,
    ) -> Merged {
        use rness_engine::service::{HookAuditEv, HookAuditEvent, HookAuditNotice};
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let cwd = self.workspace(ctx.session);
        let cwd_text = cwd.to_string_lossy().into_owned();
        let bus = self.sessions.bus();
        let mut outputs = Vec::new();
        for source in &self.sources {
            let Some(groups) = source.events.get(&event) else {
                continue;
            };
            for group in groups.iter().filter(|g| g.matcher.matches(subject)) {
                for hook in &group.hooks {
                    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let handler_id = format!("{}-{}-{seq}", source.dialect.as_str(), event.name());
                    bus.emit::<HookAuditEv>(&HookAuditNotice {
                        session: ctx.session.to_owned(),
                        event: HookAuditEvent::Invoked(rness_protocol::events::HookInvoked {
                            turn: ctx.turn,
                            point: event.name().into(),
                            source: source.dialect.as_str().into(),
                            matcher: group.matcher_text.clone(),
                            handler_id: handler_id.clone(),
                        }),
                    });
                    let mut payload = self.base_payload(source.dialect, event, ctx, &cwd);
                    if let (Some(obj), serde_json::Value::Object(more)) =
                        (payload.as_object_mut(), extra(source.dialect))
                    {
                        obj.extend(more);
                    }
                    let mut stdin = payload.to_string();
                    let env: &[(&str, &str)] = match source.dialect {
                        // CC writes a trailing newline and exports the project dir.
                        Dialect::ClaudeCode => {
                            stdin.push('\n');
                            &[("CLAUDE_PROJECT_DIR", cwd_text.as_str())]
                        }
                        Dialect::Codex => &[],
                    };
                    let timeout = hook
                        .timeout
                        .map(Duration::from_secs)
                        .unwrap_or(self.default_timeout);
                    let started = Instant::now();
                    let run =
                        run_command(&hook.command, stdin, Some(&cwd), env, timeout, ctx.cancel)
                            .await;
                    let mut out = parse_output(run.exit_code, &run.stdout, &run.stderr, event);
                    // Plain stdout on a clean exit is context for these events
                    // (CC and Codex both document it).
                    if matches!(event, Event::SessionStart | Event::UserPromptSubmit)
                        && out.exit_code == Some(0)
                        && out.additional_context.is_none()
                        && !out.stdout.is_empty()
                        && !out.stdout.starts_with('{')
                    {
                        out.additional_context = Some(out.stdout.clone());
                    }
                    if !matches!(out.exit_code, Some(0) | Some(2)) {
                        tracing::warn!(command = %hook.command, exit = ?out.exit_code, "{} hook failed (non-blocking): {}", event.name(), bounded(&out.stderr));
                    }
                    let decision = match out.decision {
                        Some(Decision::Deny) => "deny",
                        Some(Decision::Ask) => "ask",
                        Some(Decision::Allow) => "allow",
                        _ if out.exit_code == Some(0) => "ok",
                        _ => "error",
                    };
                    bus.emit::<HookAuditEv>(&HookAuditNotice {
                        session: ctx.session.to_owned(),
                        event: HookAuditEvent::Result(rness_protocol::events::HookResult {
                            turn: ctx.turn,
                            point: event.name().into(),
                            handler_id,
                            decision: decision.into(),
                            exit_code: out.exit_code,
                            stderr_summary: (!out.stderr.is_empty()).then(|| bounded(&out.stderr)),
                            duration_ms: started.elapsed().as_millis() as u64,
                        }),
                    });
                    outputs.push(out);
                }
            }
        }
        merge(outputs)
    }

    fn messages(contexts: Vec<String>) -> Vec<HookMessage> {
        contexts
            .into_iter()
            .map(|text| HookMessage {
                text,
                tag: Some(MESSAGE_TAG.into()),
            })
            .collect()
    }

    /// Text of the prompt(s) submitted since the model last spoke, or
    /// `None` when this step carries no new human input.
    fn new_prompt(&self, session: &str) -> Option<String> {
        let history = self.sessions.store().history(&session.to_owned()).ok()?;
        let mut texts = Vec::new();
        for envelope in history.iter().rev() {
            match &envelope.event {
                SessionEvent::AssistantMessage(_) => break,
                SessionEvent::UserMessage(m)
                    if m.intent != UserIntent::Inject
                        && matches!(
                            m.source,
                            None | Some(MessageSource::ExternalPrompt { .. })
                        ) =>
                {
                    let text = m
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    texts.push(text);
                }
                _ => {}
            }
        }
        if texts.is_empty() {
            return None;
        }
        texts.reverse();
        Some(texts.join("\n"))
    }

    /// Start a detached SessionStart/SubagentStart run whose context lands
    /// on `target`'s next step; that step waits for it (dsh cannot).
    fn spawn_start(
        self: &Arc<Self>,
        event: Event,
        target: String,
        subject: String,
        extra: serde_json::Value,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let Ok(guard) = gate.clone().try_lock_owned() else {
            return;
        };
        self.state
            .lock()
            .unwrap()
            .gates
            .entry(target.clone())
            .or_default()
            .push(gate);
        let this = Arc::clone(self);
        handle.spawn(async move {
            let cancel = CancellationToken::new();
            let ctx = Ctx {
                session: &target,
                turn: 0,
                cancel: &cancel,
            };
            let merged = this
                .run_point(event, &subject, &ctx, |_| extra.clone())
                .await;
            if !merged.contexts.is_empty() {
                this.state
                    .lock()
                    .unwrap()
                    .pending
                    .entry(target.clone())
                    .or_default()
                    .extend(Self::messages(merged.contexts));
            }
            drop(guard);
        });
    }

    /// Subscribe SessionStart / SubagentStart / SubagentStop to the bus.
    pub fn subscribe(
        self: &Arc<Self>,
        bus: &rness_kernel::EventBus,
    ) -> Vec<rness_kernel::Disposer> {
        use rness_engine::service::{SessionStartEv, SubagentStartEv, SubagentStopEv};
        let mut subs = Vec::new();
        if self.has(Event::SessionStart) {
            let this = Arc::clone(self);
            subs.push(bus.on::<SessionStartEv>(move |n| {
                let first = this.state.lock().unwrap().started.insert(n.session.clone());
                if !first && n.source != "compact" {
                    return;
                }
                let extra = serde_json::json!({ "source": n.source });
                this.spawn_start(
                    Event::SessionStart,
                    n.session.clone(),
                    n.source.clone(),
                    extra,
                );
            }));
        }
        if self.has(Event::SubagentStart) {
            let this = Arc::clone(self);
            subs.push(bus.on::<SubagentStartEv>(move |n| {
                let agent_type = n.agent.clone().unwrap_or_else(|| "general-purpose".into());
                let extra = serde_json::json!({ "agent_id": n.child, "agent_type": agent_type });
                this.spawn_start(Event::SubagentStart, n.child.clone(), agent_type, extra);
            }));
        }
        if self.has(Event::SubagentStop) {
            let this = Arc::clone(self);
            subs.push(bus.on::<SubagentStopEv>(move |n| {
                let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
                let this = Arc::clone(&this);
                let child = n.child.clone();
                handle.spawn(async move {
                    let cancel = CancellationToken::new();
                    let ctx = Ctx { session: &child, turn: 0, cancel: &cancel };
                    let extra = serde_json::json!({ "agent_id": child, "agent_type": "general-purpose", "stop_hook_active": false });
                    this.run_point(Event::SubagentStop, "general-purpose", &ctx, |_| extra.clone()).await;
                });
            }));
        }
        subs
    }

    /// Wait for in-flight start hooks, then drain their context.
    async fn take_pending(&self, session: &str, cancel: &CancellationToken) -> Vec<HookMessage> {
        let gates = self
            .state
            .lock()
            .unwrap()
            .gates
            .remove(session)
            .unwrap_or_default();
        for gate in gates {
            tokio::select! {
                _ = gate.lock() => {}
                _ = cancel.cancelled() => break,
            }
        }
        self.state
            .lock()
            .unwrap()
            .pending
            .remove(session)
            .unwrap_or_default()
    }
}

/// Tool + loop hooks: CC/Codex command hooks layered over the Lua host.
pub struct Layered {
    pub cmd: Arc<CommandHooks>,
    pub tools: Arc<dyn ToolHooks>,
    pub loops: Arc<dyn LoopHooks>,
}

fn tool_input(event: &ToolHookEvent, dialect: Dialect) -> serde_json::Value {
    let mut input = event.args.clone();
    // CC hooks read `tool_input.file_path` for its file tools.
    if dialect == Dialect::ClaudeCode && matches!(event.tool.as_str(), "Read" | "Edit" | "Write") {
        if let Some(obj) = input.as_object_mut() {
            if let (Some(path), false) = (obj.get("path").cloned(), obj.contains_key("file_path")) {
                obj.insert("file_path".into(), path);
            }
        }
    }
    input
}

#[async_trait::async_trait]
impl ToolHooks for Layered {
    async fn pre_tool(
        &self,
        event: &ToolHookEvent,
        cancel: &CancellationToken,
    ) -> Result<PreToolDecision, String> {
        let mut ask = None;
        if self.cmd.has(Event::PreToolUse) {
            let ctx = Ctx {
                session: &event.session,
                turn: event.turn,
                cancel,
            };
            let merged = self
                .cmd
                .run_point(Event::PreToolUse, &event.tool, &ctx, |d| {
                    serde_json::json!({ "tool_name": event.tool, "tool_input": tool_input(event, d), "tool_use_id": event.call })
                })
                .await;
            if merged.stop {
                self.cmd
                    .state
                    .lock()
                    .unwrap()
                    .halt
                    .insert(event.session.clone());
                let reason = merged
                    .stop_reason
                    .or(merged.reason)
                    .unwrap_or_else(|| "stopped by PreToolUse hook".into());
                return Ok(PreToolDecision::Deny { reason });
            }
            match merged.decision {
                Some(Decision::Deny) => {
                    return Ok(PreToolDecision::Deny {
                        reason: merged
                            .reason
                            .unwrap_or_else(|| "blocked by PreToolUse hook".into()),
                    })
                }
                Some(Decision::Ask) => ask = Some(merged.reason),
                _ => {}
            }
        }
        let inner = self.tools.pre_tool(event, cancel).await?;
        Ok(match (inner, ask) {
            (PreToolDecision::Deny { reason }, _) => PreToolDecision::Deny { reason },
            (PreToolDecision::Ask { reason }, Some(ours)) => PreToolDecision::Ask {
                reason: ours.or(reason),
            },
            (_, Some(reason)) => PreToolDecision::Ask { reason },
            (inner, None) => inner,
        })
    }

    async fn guard(
        &self,
        event: &ToolHookEvent,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, String> {
        self.tools.guard(event, cancel).await
    }

    async fn tool_execute(
        &self,
        event: &ToolHookEvent,
        next: &ExecuteNext,
        cancel: &CancellationToken,
    ) -> Result<ExecuteOutcome, String> {
        self.tools.tool_execute(event, next, cancel).await
    }

    async fn post_tool(
        &self,
        event: &ToolHookEvent,
        result: &ToolResult,
        cancel: &CancellationToken,
    ) -> Result<PostToolDecision, String> {
        let mut ours = Vec::new();
        if self.cmd.has(Event::PostToolUse) {
            let ctx = Ctx {
                session: &event.session,
                turn: event.turn,
                cancel,
            };
            let merged = self
                .cmd
                .run_point(Event::PostToolUse, &event.tool, &ctx, |d| {
                    serde_json::json!({
                        "tool_name": event.tool,
                        "tool_input": tool_input(event, d),
                        "tool_use_id": event.call,
                        "tool_response": result.output,
                    })
                })
                .await;
            if merged.stop {
                self.cmd
                    .state
                    .lock()
                    .unwrap()
                    .halt
                    .insert(event.session.clone());
            }
            let blocked = merged.denied() || merged.stop;
            ours = CommandHooks::messages(merged.contexts);
            if blocked {
                let text = merged
                    .reason
                    .or(merged.stop_reason)
                    .unwrap_or_else(|| "blocked by PostToolUse hook".into());
                return Ok(PostToolDecision::Block {
                    feedback: vec![ToolResultContentPart::Text { text }],
                    additional_contexts: ours,
                });
            }
        }
        let mut inner = self.tools.post_tool(event, result, cancel).await?;
        if !ours.is_empty() {
            let (PostToolDecision::Accept {
                additional_contexts,
                ..
            }
            | PostToolDecision::Block {
                additional_contexts,
                ..
            }) = &mut inner;
            ours.append(additional_contexts);
            *additional_contexts = ours;
        }
        Ok(inner)
    }

    fn tool_result(&self, event: &ToolHookEvent, result: &ToolResult) {
        self.tools.tool_result(event, result)
    }
}

#[async_trait::async_trait]
impl LoopHooks for Layered {
    async fn pre_step(
        &self,
        event: &LoopEvent,
        cancel: &CancellationToken,
    ) -> Result<PreStepDecision, String> {
        if self.cmd.state.lock().unwrap().halt.remove(&event.session) {
            return Ok(PreStepDecision::Reject);
        }
        let mut messages = self.cmd.take_pending(&event.session, cancel).await;
        if self.cmd.has(Event::UserPromptSubmit) {
            if let Some(prompt) = self.cmd.new_prompt(&event.session) {
                let ctx = Ctx {
                    session: &event.session,
                    turn: event.turn,
                    cancel,
                };
                let merged = self
                    .cmd
                    .run_point(
                        Event::UserPromptSubmit,
                        "",
                        &ctx,
                        |_| serde_json::json!({ "prompt": prompt }),
                    )
                    .await;
                if merged.denied() || merged.stop {
                    tracing::warn!(session = %event.session, "prompt blocked by UserPromptSubmit hook: {}", merged.reason.or(merged.stop_reason).unwrap_or_default());
                    return Ok(PreStepDecision::Reject);
                }
                messages.extend(CommandHooks::messages(merged.contexts));
            }
        }
        match self.loops.pre_step(event, cancel).await {
            Ok(PreStepDecision::Reject) => Ok(PreStepDecision::Reject),
            Ok(PreStepDecision::EnterWithMessages { messages: theirs }) => {
                messages.extend(theirs);
                Ok(PreStepDecision::EnterWithMessages { messages })
            }
            Ok(PreStepDecision::Enter) | Err(_) if !messages.is_empty() => {
                Ok(PreStepDecision::EnterWithMessages { messages })
            }
            other => other,
        }
    }

    async fn request(&self, event: &LoopEvent, cancel: &CancellationToken) -> Result<(), String> {
        self.loops.request(event, cancel).await
    }

    async fn request_error(
        &self,
        event: &LoopEvent,
        error: &RequestError,
        cancel: &CancellationToken,
    ) -> Result<RequestErrorAction, String> {
        self.loops.request_error(event, error, cancel).await
    }

    async fn turn_stopping(
        &self,
        event: &LoopEvent,
        cancel: &CancellationToken,
    ) -> Result<TurnStoppingAction, String> {
        let inner = self.loops.turn_stopping(event, cancel).await;
        if matches!(inner, Ok(TurnStoppingAction::Continue { .. })) || !self.cmd.has(Event::Stop) {
            return inner;
        }
        let forced = {
            let state = self.cmd.state.lock().unwrap();
            match state.stop_forced.get(&event.session) {
                Some((turn, n)) if *turn == event.turn => *n,
                _ => 0,
            }
        };
        let ctx = Ctx {
            session: &event.session,
            turn: event.turn,
            cancel,
        };
        let merged = self
            .cmd
            .run_point(Event::Stop, "", &ctx, |d| match d {
                Dialect::ClaudeCode => serde_json::json!({ "stop_hook_active": forced > 0 }),
                Dialect::Codex => serde_json::json!({ "stop_hook_active": forced > 0, "last_assistant_message": serde_json::Value::Null }),
            })
            .await;
        let mut state = self.cmd.state.lock().unwrap();
        if merged.denied() && !merged.stop {
            if forced >= self.cmd.max_stop_continuations {
                tracing::warn!(session = %event.session, forced, "Stop hook continuation cap reached; closing the turn");
            } else {
                state
                    .stop_forced
                    .insert(event.session.clone(), (event.turn, forced + 1));
                let text = merged
                    .reason
                    .unwrap_or_else(|| "continue: blocked by Stop hook".into());
                return Ok(TurnStoppingAction::Continue {
                    messages: vec![HookMessage {
                        text,
                        tag: Some(MESSAGE_TAG.into()),
                    }],
                });
            }
        }
        state.stop_forced.remove(&event.session);
        inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source(value: serde_json::Value, dialect: Dialect) -> Source {
        parse_source(&value, dialect, false).unwrap().0
    }

    #[test]
    fn parses_settings_and_bare_maps_and_skips_native_points() {
        let s = source(
            json!({"hooks": {
                "PreToolUse": [{"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "a", "timeout": 5}]}],
                "Stop": [{"matcher": "ignored", "hooks": [{"command": "b"}]}],
                "pre_tool": [{"hooks": [{"type": "command", "command": "lua-side"}]}],
            }}),
            Dialect::ClaudeCode,
        );
        assert_eq!(s.events.len(), 2);
        let pre = &s.events[&Event::PreToolUse][0];
        assert!(pre.matcher.matches("Write") && !pre.matcher.matches("WriteX"));
        assert_eq!(pre.hooks[0].timeout, Some(5));
        assert!(s.events[&Event::Stop][0].matcher_text.is_none());
        let bare = source(
            json!({"PostToolUse": [{"hooks": [{"command": "c"}]}]}),
            Dialect::Codex,
        );
        assert!(bare.events.contains_key(&Event::PostToolUse));
        let (empty, _) =
            parse_source(&json!({"permissions": {}}), Dialect::ClaudeCode, true).unwrap();
        assert!(empty.events.is_empty());
    }

    #[test]
    fn matcher_dialects_and_invalid_regex() {
        let cc = Matcher::compile(Some("mcp__.*"), Dialect::ClaudeCode).unwrap();
        assert!(cc.matches("mcp__github__search"));
        // Codex treats identifiers as (unanchored) regex.
        let codex = Matcher::compile(Some("Bash"), Dialect::Codex).unwrap();
        assert!(codex.matches("Bash") && codex.matches("MyBash"));
        assert!(Matcher::compile(Some("*"), Dialect::Codex)
            .unwrap()
            .matches("x"));
        assert!(parse_source(
            &json!({"PreToolUse": [{"matcher": "(", "hooks": [{"command": "x"}]}]}),
            Dialect::ClaudeCode,
            false
        )
        .is_err());
        let (codex_src, warnings) = parse_source(
            &json!({"SubagentStart": [{"hooks": [{"command": "x"}]}], "Notification": []}),
            Dialect::Codex,
            false,
        )
        .unwrap();
        assert!(codex_src.events.is_empty());
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn output_protocol() {
        let o = parse_output(Some(2), "", " nope \n", Event::PreToolUse);
        assert_eq!(
            (o.decision, o.reason.as_deref()),
            (Some(Decision::Deny), Some("nope"))
        );
        let o = parse_output(
            Some(0),
            r#"{"decision":"approve","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"why"}}"#,
            "",
            Event::PreToolUse,
        );
        assert_eq!(
            (o.decision, o.reason.as_deref()),
            (Some(Decision::Ask), Some("why"))
        );
        // Mismatched hookEventName is ignored.
        let o = parse_output(
            Some(0),
            r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","permissionDecision":"deny"}}"#,
            "",
            Event::PreToolUse,
        );
        assert_eq!(o.decision, None);
        let o = parse_output(
            Some(0),
            r#"{"continue":false,"stopReason":"done"}"#,
            "",
            Event::Stop,
        );
        assert_eq!(
            (o.cont, o.stop_reason.as_deref()),
            (Some(false), Some("done"))
        );
        // Structured stdout only counts on exit 0.
        assert_eq!(
            parse_output(Some(1), r#"{"decision":"block"}"#, "", Event::Stop).decision,
            None
        );
    }

    #[test]
    fn merge_ranks_and_collects() {
        let out = |d, r: &str, ctx: Option<&str>| Output {
            decision: d,
            reason: Some(r.into()),
            additional_context: ctx.map(str::to_owned),
            ..Output::default()
        };
        let m = merge(vec![
            out(Some(Decision::Allow), "a", Some("c1")),
            out(Some(Decision::Ask), "b", None),
            out(Some(Decision::Deny), "c", Some("c2")),
            out(Some(Decision::Deny), "d", None),
        ]);
        assert_eq!(m.decision, Some(Decision::Deny));
        assert_eq!(m.reason.as_deref(), Some("c\n\nd"));
        assert_eq!(m.contexts, vec!["c1", "c2"]);
        assert_eq!(merge(vec![]).decision, None);
    }

    #[tokio::test]
    async fn runner_passes_stdin_env_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let run = run_command(
            r#"read line; printf '%s|%s|%s' "$line" "$CLAUDE_PROJECT_DIR" "$(pwd -P)"; echo err >&2; exit 2"#,
            "{\"a\":1}\n".into(),
            Some(dir.path()),
            &[("CLAUDE_PROJECT_DIR", "/proj")],
            Duration::from_secs(10),
            &cancel,
        )
        .await;
        let real = dir.path().canonicalize().unwrap();
        assert_eq!(run.exit_code, Some(2));
        assert_eq!(run.stdout, format!("{{\"a\":1}}|/proj|{}", real.display()));
        assert_eq!(run.stderr.trim(), "err");
        let started = Instant::now();
        let run = run_command(
            "sleep 30",
            String::new(),
            None,
            &[],
            Duration::from_millis(200),
            &cancel,
        )
        .await;
        assert!(run.exit_code.is_none() && run.stderr.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn discovery_respects_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let none = discover(&HooksConfig::default(), dir.path(), Some(home.path()));
        assert_eq!(none.len(), 2); // .rness/hooks.json (project + user)
        let all = discover(
            &HooksConfig {
                claude_code: true,
                codex: true,
                files: vec![HookFile {
                    path: "x.json".into(),
                    dialect: Dialect::Codex,
                }],
                ..HooksConfig::default()
            },
            dir.path(),
            Some(home.path()),
        );
        assert_eq!(all.len(), 8);
        assert_eq!(all.last().unwrap().path, dir.path().join("x.json"));
        assert!(all
            .iter()
            .any(|f| f.path.ends_with(".codex/hooks.json") && f.dialect == Dialect::Codex));
    }

    struct UnusedProvider;
    #[async_trait::async_trait]
    impl rness_engine::turn::provider::Provider for UnusedProvider {
        fn model(&self) -> &str {
            "unused"
        }
        async fn step(
            &self,
            _: rness_engine::turn::provider::StepRequest<'_>,
            _: &CancellationToken,
        ) -> rness_engine::turn::provider::StepOutcome {
            panic!("no turn runs in this test")
        }
    }

    /// A Lua host + session service + CC hooks from `config` (JSON).
    async fn harness(
        config: serde_json::Value,
        dialect: Dialect,
    ) -> (
        tempfile::TempDir,
        Arc<SessionService>,
        Layered,
        crate::plugin_host::LuaHost,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, config.to_string()).unwrap();
        let store = rness_engine::session::branch::SessionStore::new(dir.path().join("sessions"));
        let session = store
            .create(Some(dir.path().to_string_lossy().into_owned()))
            .unwrap()
            .session()
            .clone();
        let sessions = Arc::new(SessionService::new(
            store,
            Arc::new(UnusedProvider),
            Arc::new(rness_engine::tools::ToolRegistry::default()),
            Default::default(),
            Arc::new(rness_kernel::EventBus::default()),
        ));
        let (cmd, warnings) = CommandHooks::load(
            &[HookFile { path, dialect }],
            &HooksConfig {
                max_stop_continuations: Some(2),
                ..HooksConfig::default()
            },
            sessions.clone(),
            dir.path().into(),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let host = crate::plugin_host::LuaHost::spawn().unwrap();
        let layered = Layered {
            cmd: cmd.unwrap(),
            tools: Arc::new(host.clone()),
            loops: Arc::new(host.clone()),
        };
        (dir, sessions, layered, host, session)
    }

    #[tokio::test]
    async fn claude_code_tool_hooks_drive_dispatch_alongside_lua() {
        use rness_engine::tools::{ToolCall, ToolRegistry};
        let (dir, _sessions, layered, host, session) = harness(
            json!({"hooks": {
                "PreToolUse": [
                    {"matcher": "echo", "hooks": [{"type": "command", "command":
                        "input=$(cat); case \"$input\" in *secret*) echo 'no secrets' >&2; exit 2;; esac; case \"$input\" in *ask*) echo '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"json deny\"}}';; esac"}]}
                ],
                "PostToolUse": [
                    {"matcher": "ec.*", "hooks": [{"command":
                        "cat > \"$CLAUDE_PROJECT_DIR/post.json\"; echo '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"additionalContext\":\"cc saw it\"}}'"}]}
                ]
            }}),
            Dialect::ClaudeCode,
        )
        .await;
        host.load(
            "t",
            r#"
            rness.tool.register{ name = "echo", run = function(args) return args.say end }
            rness.hook.on("post_tool", function(ev, next)
              next()
              return {kind = "accept", additional_contexts = {"lua saw it"}}
            end)
            "#,
        )
        .await
        .unwrap();
        let registry = ToolRegistry::default();
        crate::api::tools::sync_lua_tools(&registry, &host, &[]).await;
        let layered = Arc::new(layered);
        registry.set_hooks(Some(layered.clone()));
        let call = |id: &str, say: &str| ToolCall {
            call: id.into(),
            name: "echo".into(),
            args: json!({"say": say}),
        };
        let results = registry
            .dispatch(
                &session,
                &[call("a", "hi"), call("b", "secret"), call("c", "ask")],
                1,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(
            (results[0].output.as_str(), results[0].is_error),
            ("hi", false)
        );
        assert_eq!(
            (results[1].output.as_str(), results[1].is_error),
            ("no secrets", true)
        );
        assert_eq!(
            (results[2].output.as_str(), results[2].is_error),
            ("json deny", true)
        );
        let texts: Vec<_> = registry
            .take_hook_contexts(&session)
            .into_iter()
            .filter(|c| c.call == "a")
            .map(|c| c.text)
            .collect();
        assert_eq!(texts, ["cc saw it", "lua saw it"]);
        let post: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("post.json")).unwrap())
                .unwrap();
        assert_eq!(post["hook_event_name"], "PostToolUse");
        assert_eq!(post["tool_name"], "echo");
        assert_eq!(post["session_id"], session.as_str());
        assert!(post["transcript_path"]
            .as_str()
            .unwrap()
            .ends_with(".jsonl"));
    }

    #[tokio::test]
    async fn stop_hook_forces_continuation_with_cap_and_active_flag() {
        let (dir, _sessions, layered, _host, session) = harness(
            json!({"Stop": [{"hooks": [{"command":
                "cat >> \"$CLAUDE_PROJECT_DIR/stop.log\"; echo '{\"decision\":\"block\",\"reason\":\"run the tests\"}'"}]}]}),
            Dialect::ClaudeCode,
        )
        .await;
        let cancel = CancellationToken::new();
        let ev = LoopEvent {
            session: session.clone(),
            turn: 1,
            step: 1,
        };
        for _ in 0..2 {
            match layered.turn_stopping(&ev, &cancel).await.unwrap() {
                TurnStoppingAction::Continue { messages } => {
                    assert_eq!(messages[0].text, "run the tests")
                }
                other => panic!("expected continue, got {other:?}"),
            }
        }
        // Cap (2) reached: the turn closes and the counter resets.
        assert_eq!(
            layered.turn_stopping(&ev, &cancel).await.unwrap(),
            TurnStoppingAction::Stop
        );
        let log = std::fs::read_to_string(dir.path().join("stop.log")).unwrap();
        let flags: Vec<bool> = log
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["stop_hook_active"]
                    .as_bool()
                    .unwrap()
            })
            .collect();
        assert_eq!(flags, [false, true, true]);
    }

    #[tokio::test]
    async fn prompt_submit_blocks_or_adds_context_and_session_start_is_gated() {
        let (_dir, sessions, layered, _host, session) = harness(
            json!({
                "UserPromptSubmit": [{"hooks": [{"command":
                    "input=$(cat); case \"$input\" in *forbidden*) echo nope >&2; exit 2;; esac; echo 'plain context'"}]}],
                "SessionStart": [{"matcher": "startup", "hooks": [{"command": "sleep 0.3; echo 'start context'"}]}]
            }),
            Dialect::Codex,
        )
        .await;
        let cmd = layered.cmd.clone();
        cmd.spawn_start(
            Event::SessionStart,
            session.clone(),
            "startup".into(),
            json!({"source": "startup"}),
        );
        sessions
            .store()
            .open(&session)
            .unwrap()
            .append(&SessionEvent::UserMessage(
                rness_protocol::events::UserMessage {
                    intent: UserIntent::Followup,
                    content: vec![ContentPart::Text {
                        text: "hello".into(),
                    }],
                    source: None,
                },
            ))
            .unwrap();
        let cancel = CancellationToken::new();
        let ev = LoopEvent {
            session: session.clone(),
            turn: 1,
            step: 1,
        };
        match layered.pre_step(&ev, &cancel).await.unwrap() {
            PreStepDecision::EnterWithMessages { messages } => {
                let texts: Vec<_> = messages.iter().map(|m| m.text.as_str()).collect();
                assert_eq!(texts, ["start context", "plain context"]);
            }
            other => panic!("expected messages, got {other:?}"),
        }
        sessions
            .store()
            .open(&session)
            .unwrap()
            .append(&SessionEvent::UserMessage(
                rness_protocol::events::UserMessage {
                    intent: UserIntent::Followup,
                    content: vec![ContentPart::Text {
                        text: "something forbidden".into(),
                    }],
                    source: None,
                },
            ))
            .unwrap();
        assert_eq!(
            layered.pre_step(&ev, &cancel).await.unwrap(),
            PreStepDecision::Reject
        );
    }
}
