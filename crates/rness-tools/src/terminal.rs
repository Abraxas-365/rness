//! Persistent shell sessions backed by portable-pty.
//!
//! Each terminal is a PTY + shell process that stays alive across tool
//! calls. The model creates sessions with `terminal_open`, sends
//! commands with `terminal_send`, reads output with `terminal_read`,
//! and manages sessions with `terminal_signal`/`terminal_list`/
//! `terminal_close`.
//!
//! Output is collected by a background reader thread into a ring
//! buffer (last MAX_SCROLLBACK_BYTES retained), with escape sequences
//! stripped (see [`sanitize`]). Reads are incremental: `terminal_send`
//! returns output captured since the send; `terminal_read` pages through
//! retained scrollback.
//!
//! By default a terminal runs a *controlled* bash: no startup files, a
//! fixed prompt, pagers off, and a `PROMPT_COMMAND` that emits
//! `OSC 133;D;<exit>` before every prompt. That marker tells
//! `terminal_send` exactly when a command finished and with which exit
//! code. `shell = "login"` opts into the user's own shell instead, where
//! completion is inferred from the terminal's foreground process group.

mod sanitize;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Max bytes retained in the output ring buffer per session.
const MAX_SCROLLBACK_BYTES: usize = 256 * 1024;
/// Default wait time for output after sending a command (ms).
const DEFAULT_WAIT_MS: u64 = 10_000;
/// Max wait time (ms).
const MAX_WAIT_MS: u64 = 60_000;
/// Max bytes returned per read.
const MAX_READ_BYTES: usize = 64 * 1024;
/// A running command silent this long settles the send as "still running;
/// may be waiting for input" (where the platform can't see tty reads).
const QUIET: Duration = Duration::from_secs(5);
/// Silence required before trusting a shell-owned foreground (login
/// shells) or a tty read (Linux) as the end of a send.
const SETTLE_IDLE: Duration = Duration::from_millis(300);
/// How often a send re-checks output and the foreground process group.
const POLL: Duration = Duration::from_millis(25);
/// How long `terminal_open` waits for the shell's first prompt.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// Prompt of the controlled shell.
const CONTROLLED_PROMPT: &str = "rness$ ";
/// Emits the completion marker, then un-exports itself so programs started
/// from the terminal don't inherit it, and restores the prompt in case a
/// command overwrote PS1. `set +H` turns off history expansion, which would
/// otherwise mangle any `!` in a command ("event not found").
const PROMPT_COMMAND: &str =
    "__rness_status=$?; set +H; export -n PROMPT_COMMAND PS1 2>/dev/null; \
     printf '\\033]133;D;%s\\007' \"$__rness_status\"; PS1='rness$ '";
/// How long `terminal_signal` watches for the command to leave the foreground.
const SIGNAL_SETTLE: Duration = Duration::from_secs(1);
/// How long closing a terminal gives its foreground command to exit after
/// SIGHUP before killing it.
const TERMINATE_GRACE: Duration = Duration::from_millis(500);
/// Holds a terminal's job slot between typing a background command and
/// learning its job id.
const JOB_STARTING: &str = "(starting)";

/// Signals `terminal_signal` may deliver. Numbers come from libc because
/// they differ between platforms (SIGSTOP is 19 on Linux, 17 on macOS).
#[cfg(unix)]
const SIGNALS: &[(&str, i32)] = &[
    ("INT", libc::SIGINT),
    ("TERM", libc::SIGTERM),
    ("KILL", libc::SIGKILL),
    ("TSTP", libc::SIGTSTP),
    ("STOP", libc::SIGSTOP),
    ("CONT", libc::SIGCONT),
];

/// Parse `INT`, `sigint`, `SIGINT` or this platform's number for it.
#[cfg(unix)]
fn parse_signal(raw: &str) -> Result<(&'static str, i32), String> {
    let upper = raw.trim().to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    let number = name.parse::<i32>().ok();
    SIGNALS
        .iter()
        .find(|(n, v)| *n == name || Some(*v) == number)
        .copied()
        .ok_or_else(|| {
            let names: Vec<_> = SIGNALS.iter().map(|(n, _)| *n).collect();
            format!(
                "unsupported signal '{raw}'; use one of {}",
                names.join(", ")
            )
        })
}

// ── Output buffer ──────────────────────────────────────────────────

/// Append-only output buffer with a rolling tail retained for reads.
struct OutputBuffer {
    /// All output bytes, truncated from the front when exceeding
    /// `MAX_SCROLLBACK_BYTES`. `base_offset` tracks the logical
    /// position of `data[0]` in the total stream.
    data: Vec<u8>,
    base_offset: usize,
    /// Stream offsets of the controlled shell's prompt markers, with the
    /// exit code each reported (newest last, bounded).
    prompts: Vec<(usize, Option<i32>)>,
    /// Stream offset of the latest switch to the alternate screen.
    alt_screen: Option<usize>,
    /// When text last arrived.
    last_output: Instant,
    /// The PTY reached EOF: the shell exited.
    eof: bool,
}

impl OutputBuffer {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            base_offset: 0,
            prompts: Vec::new(),
            alt_screen: None,
            last_output: Instant::now(),
            eof: false,
        }
    }

    /// Append one sanitized chunk and the events found in it.
    fn record(&mut self, text: &[u8], events: &[sanitize::Event]) {
        let start = self.total_written();
        for event in events {
            match *event {
                sanitize::Event::Prompt { at, exit } => {
                    self.prompts.push((start + at, exit));
                    if self.prompts.len() > 64 {
                        self.prompts.remove(0);
                    }
                }
                sanitize::Event::AltScreen { at } => self.alt_screen = Some(start + at),
            }
        }
        if !text.is_empty() {
            self.append(text);
            self.last_output = Instant::now();
        }
    }

    /// The first prompt marker at or after `from`.
    fn prompt_since(&self, from: usize) -> Option<(usize, Option<i32>)> {
        self.prompts.iter().copied().find(|(at, _)| *at >= from)
    }

    fn append(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > MAX_SCROLLBACK_BYTES {
            let excess = self.data.len() - MAX_SCROLLBACK_BYTES;
            self.data.drain(..excess);
            self.base_offset += excess;
        }
    }

    /// Total bytes written to this buffer since creation.
    fn total_written(&self) -> usize {
        self.base_offset + self.data.len()
    }

    /// Read bytes from logical offset `from` onward, up to `limit`.
    /// If `from` has been evicted, starts from the oldest retained byte.
    fn read_from(&self, from: usize, limit: usize) -> (Vec<u8>, usize) {
        // Clamp to the oldest retained offset if the requested offset was evicted.
        let effective_from = from.max(self.base_offset);
        let start = effective_from - self.base_offset;
        let end = (start + limit).min(self.data.len());
        if start >= self.data.len() {
            return (Vec::new(), self.total_written());
        }
        (self.data[start..end].to_vec(), self.base_offset + end)
    }
}

// ── Terminal session ───────────────────────────────────────────────

struct TerminalSession {
    id: String,
    name: String,
    /// The rness session that created this terminal (ownership scoping).
    owner: String,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Behind its own lock: a PTY write blocks while the terminal's input
    /// queue is full (a command not reading stdin), and that must never
    /// hold the registry lock the UI and statusline also take.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Arc<Mutex<OutputBuffer>>,
    _reader_handle: std::thread::JoinHandle<()>,
    /// Runs the controlled shell, which emits prompt markers.
    controlled: bool,
    /// How the shell was started, for listings ("bash, controlled").
    shell: String,
    /// Shell pid, kept after exit for reporting.
    shell_pid: Option<u32>,
    /// Set once the shell has exited, e.g. "exit code 3".
    exit: Option<String>,
    /// Background job (id) whose command currently owns this terminal.
    job: Option<String>,
    started: Instant,
    /// The latest command typed at the prompt, and when.
    last_command: Option<(String, Instant)>,
}

impl TerminalSession {
    /// Record and describe the shell's exit, if it has exited.
    ///
    /// On Unix the exit is observed without reaping (`WNOWAIT`): the zombie
    /// keeps the shell's pid, which is also its session id, reserved until
    /// [`terminate`] reaps it, so the cleanup sweep can never match an
    /// unrelated session that reused the number.
    fn exited(&mut self) -> Option<String> {
        if self.exit.is_none() {
            self.exit = self.peek_exit();
        }
        self.exit.clone()
    }

    #[cfg(unix)]
    fn peek_exit(&mut self) -> Option<String> {
        let pid = self
            .shell_pid
            .and_then(|pid| libc::id_t::try_from(pid).ok())?;
        // SAFETY: `info` is a writable siginfo_t; WNOWAIT leaves the child
        // waitable, WNOHANG makes this a non-blocking peek.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            // Already reaped (or not our child): fall back to std's view.
            return self
                .child
                .try_wait()
                .ok()
                .flatten()
                .map(|s| describe_exit(&s));
        }
        #[cfg(target_os = "linux")]
        let (exited_pid, status) = unsafe { (info.si_pid(), info.si_status()) };
        #[cfg(not(target_os = "linux"))]
        let (exited_pid, status) = (info.si_pid, info.si_status);
        if exited_pid == 0 {
            return None; // still running
        }
        Some(if info.si_code == libc::CLD_EXITED {
            format!("exit code {status}")
        } else {
            format!("killed by signal {status}")
        })
    }

    #[cfg(not(unix))]
    fn peek_exit(&mut self) -> Option<String> {
        self.child
            .try_wait()
            .ok()
            .flatten()
            .map(|s| describe_exit(&s))
    }

    /// `true` while a command (not the shell) owns the terminal. `None`
    /// when the platform can't tell.
    fn command_running(&self) -> Option<bool> {
        #[cfg(unix)]
        {
            let shell = self.shell_pid? as i32;
            Some(self.master.process_group_leader()? != shell)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    fn state(&mut self) -> TerminalState {
        match (self.exited(), self.command_running()) {
            (Some(exit), _) => TerminalState::Exited(exit),
            (None, Some(true)) => TerminalState::Running,
            (None, Some(false)) => TerminalState::Idle,
            (None, None) => TerminalState::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalState {
    /// The shell is at its prompt.
    Idle,
    /// A command owns the terminal.
    Running,
    /// The shell exited; the description says how.
    Exited(String),
    /// The platform can't report the foreground process.
    Unknown,
}

impl std::fmt::Display for TerminalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => f.write_str("idle at prompt"),
            Self::Running => f.write_str("command running"),
            Self::Exited(how) => write!(f, "exited ({how})"),
            Self::Unknown => f.write_str("open"),
        }
    }
}

/// Why a `terminal_send` stopped waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Settle {
    /// The controlled shell printed its prompt after the command.
    Prompt { at: usize, exit: Option<i32> },
    /// A login shell owns the terminal again (exit code unknown).
    Idle,
    /// The command appears to be reading the terminal (Linux only).
    Input,
    /// The shell is at a continuation prompt: the input is incomplete.
    Incomplete,
    /// A command is running but has been silent for [`QUIET`].
    Quiet,
    /// The wait deadline passed while output was still arriving.
    Timeout(Duration),
    /// A full-screen program switched to the alternate screen.
    FullScreen,
    /// The shell exited.
    Exited(String),
}

// ── Registry ───────────────────────────────────────────────────────

/// Owns all terminal sessions for the composition.
#[derive(Clone)]
pub struct TerminalRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

struct RegistryInner {
    sessions: HashMap<String, TerminalSession>,
    next_id: u64,
    config: TerminalConfig,
}

/// `rness.terminal = { … }` in init.lua. Unknown keys are rejected.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// Shell for `terminal_open` without a `shell` argument:
    /// "controlled" or "login".
    pub shell: String,
    /// More env vars to withhold from terminals, on top of the built-in
    /// secret patterns. `*` at either end matches a prefix or suffix
    /// (`"*_TOKEN"`, `"AWS_*"`); case-insensitive.
    pub env_deny: Vec<String>,
    /// Most terminals open at once per rness session.
    pub max_sessions: usize,
    /// Ask before quitting while a terminal command is running.
    pub confirm_quit: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: "controlled".into(),
            env_deny: Vec::new(),
            max_sessions: 8,
            confirm_quit: true,
        }
    }
}

impl TerminalConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.shell.as_str(), "controlled" | "login") {
            return Err(format!(
                "rness.terminal.shell must be \"controlled\" or \"login\", not {:?}",
                self.shell
            ));
        }
        if self.max_sessions == 0 || self.max_sessions > 64 {
            return Err("rness.terminal.max_sessions must be between 1 and 64".into());
        }
        if let Some(bad) = self.env_deny.iter().find(|p| {
            let core = p.strip_prefix('*').unwrap_or(p);
            let core = core.strip_suffix('*').unwrap_or(core);
            core.is_empty() || core.contains('*') || (p.starts_with('*') && p.ends_with('*'))
        }) {
            return Err(format!(
                "rness.terminal.env_deny entry {bad:?} must be a name, \"PREFIX*\" or \"*SUFFIX\""
            ));
        }
        Ok(())
    }

    fn denies(&self, name: &str) -> bool {
        let name = name.to_ascii_uppercase();
        self.env_deny.iter().any(|pattern| {
            let pattern = pattern.to_ascii_uppercase();
            if let Some(suffix) = pattern.strip_prefix('*') {
                name.ends_with(suffix)
            } else if let Some(prefix) = pattern.strip_suffix('*') {
                name.starts_with(prefix)
            } else {
                name == pattern
            }
        })
    }
}

impl Default for TerminalRegistry {
    fn default() -> Self {
        Self::with_config(TerminalConfig::default())
    }
}

/// Which shell a terminal runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellChoice {
    /// Clean bash with a prompt marker: reliable completion and exit codes.
    Controlled,
    /// The user's `$SHELL` as a login shell, dotfiles and all.
    Login,
    /// Any other program, started as given.
    Program(String),
}

impl ShellChoice {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            None | Some("") | Some("controlled") => Self::Controlled,
            Some("login") => Self::Login,
            Some(program) => Self::Program(program.to_string()),
        }
    }
}

/// What `terminal_send` returns: the model's text and the card's facts.
#[derive(Debug, Clone)]
pub struct Sent {
    pub text: String,
    /// `{kind: "terminal", terminal, sent, outcome, exit_code, elapsed_ms, status}`;
    /// `outcome` is exited | done | input | incomplete | quiet | timeout |
    /// full_screen | terminal_exited | cancelled | background.
    pub presentation: Value,
}

/// What `terminal_open` reports back.
#[derive(Debug, Clone)]
pub struct Opened {
    pub id: String,
    /// "bash, controlled", "zsh, login", ...
    pub shell: String,
    /// Output printed before the first prompt (banners, rc-file noise).
    pub startup: String,
    /// Environment variables withheld from the shell because they look
    /// like secrets.
    pub hidden_env: Vec<String>,
    /// The shell reached its first prompt within [`START_TIMEOUT`].
    pub ready: bool,
}

fn find_bash() -> Option<std::path::PathBuf> {
    [
        "/bin/bash",
        "/usr/bin/bash",
        "/usr/local/bin/bash",
        "/opt/homebrew/bin/bash",
    ]
    .iter()
    .map(std::path::PathBuf::from)
    .find(|path| path.is_file())
}

/// Names that look like credentials. The model can read anything the
/// terminal's environment holds, so these stay out of it.
fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "API_KEY"
        || [
            "_API_KEY",
            "_SECRET",
            "_SECRET_KEY",
            "_ACCESS_KEY",
            "_PASSWORD",
        ]
        .iter()
        .any(|suffix| upper.ends_with(suffix))
}

fn program_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: TerminalConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                sessions: HashMap::new(),
                next_id: 1,
                config,
            })),
        }
    }

    pub fn config(&self) -> TerminalConfig {
        self.inner.lock().expect("registry lock").config.clone()
    }

    /// The shell `terminal_open` uses when none is asked for.
    pub fn default_shell(&self) -> ShellChoice {
        ShellChoice::parse(Some(&self.config().shell))
    }

    /// Open a terminal and wait (bounded) for the shell's first prompt.
    /// Blocks; call from a blocking context.
    pub fn open(
        &self,
        name: Option<String>,
        shell: ShellChoice,
        cwd: std::path::PathBuf,
        owner: String,
    ) -> Result<Opened, String> {
        let config = {
            let inner = self.inner.lock().expect("registry lock");
            check_capacity(&inner, &owner)?;
            inner.config.clone()
        };
        let bash = match shell {
            ShellChoice::Controlled => find_bash(),
            _ => None,
        };
        let (mut cmd, label, controlled) = match (&shell, bash) {
            (ShellChoice::Controlled, Some(bash)) => {
                let mut cmd = CommandBuilder::new(&bash);
                cmd.args(["--noprofile", "--norc", "--noediting", "-i"]);
                for (key, value) in [
                    ("PS1", CONTROLLED_PROMPT),
                    ("PS2", ""),
                    ("PROMPT_COMMAND", PROMPT_COMMAND),
                    ("TERM", "dumb"),
                    ("PAGER", "cat"),
                    ("GIT_PAGER", "cat"),
                    ("MANPAGER", "cat"),
                    ("NO_COLOR", "1"),
                    ("HISTFILE", "/dev/null"),
                    ("BASH_SILENCE_DEPRECATION_WARNING", "1"),
                    ("RNESS_TERMINAL", "1"),
                ] {
                    cmd.env(key, value);
                }
                (cmd, "bash, controlled".to_string(), true)
            }
            (ShellChoice::Controlled, None) | (ShellChoice::Login, _) => {
                let path = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                let mut cmd = CommandBuilder::new(&path);
                cmd.arg("-il");
                cmd.env("RNESS_TERMINAL", "1");
                let why = if shell == ShellChoice::Controlled {
                    "login, bash not found"
                } else {
                    "login"
                };
                (cmd, format!("{}, {why}", program_name(&path)), false)
            }
            (ShellChoice::Program(path), _) => {
                let mut cmd = CommandBuilder::new(path);
                cmd.env("RNESS_TERMINAL", "1");
                (cmd, program_name(path), false)
            }
        };
        let mut hidden_env: Vec<String> = std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| is_secret_name(key) || config.denies(key))
            .collect();
        hidden_env.sort();
        for key in &hidden_env {
            cmd.env_remove(key);
        }
        cmd.cwd(cwd);

        let id = self.spawn(name, label.clone(), cmd, owner.clone(), controlled)?;
        let ready = self.wait_until_ready(&id, controlled);
        let startup = {
            let inner = self.inner.lock().expect("registry lock");
            let session = inner
                .sessions
                .get(&id)
                .ok_or("terminal closed while starting")?;
            let buf = session.output.lock().expect("output lock");
            let end = buf
                .prompt_since(0)
                .map_or(buf.total_written(), |(at, _)| at);
            let (bytes, _) = buf.read_from(0, end.min(MAX_READ_BYTES));
            sanitize::render(&bytes).trim().to_string()
        };
        Ok(Opened {
            id,
            shell: label,
            startup,
            hidden_env,
            ready,
        })
    }

    /// Controlled shells are ready at their first prompt marker; others
    /// once their startup output goes quiet.
    fn wait_until_ready(&self, id: &str, controlled: bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < START_TIMEOUT {
            std::thread::sleep(POLL);
            let inner = self.inner.lock().expect("registry lock");
            let Some(session) = inner.sessions.get(id) else {
                return false;
            };
            let buf = session.output.lock().expect("output lock");
            if buf.eof {
                return false;
            }
            let quiet = buf.last_output.max(started).elapsed() >= SETTLE_IDLE;
            if (controlled && buf.prompt_since(0).is_some())
                || (!controlled && buf.total_written() > 0 && quiet)
            {
                return true;
            }
        }
        false
    }

    fn spawn(
        &self,
        name: Option<String>,
        shell: String,
        cmd: CommandBuilder,
        owner: String,
        controlled: bool,
    ) -> Result<String, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("failed to open PTY: {e}"))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("failed to spawn shell: {e}"))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("failed to get PTY writer: {e}"))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("failed to get PTY reader: {e}"))?;

        let output = Arc::new(Mutex::new(OutputBuffer::new()));

        // Background reader thread: reads PTY output into the buffer.
        let buf_handle = Arc::clone(&output);
        let reader_handle = std::thread::Builder::new()
            .name("terminal-reader".into())
            .spawn(move || {
                reader_loop(reader, buf_handle);
            })
            .map_err(|e| format!("failed to spawn reader thread: {e}"))?;

        let mut inner = self.inner.lock().expect("registry lock");
        let session_name = name.unwrap_or_default();
        let shell_pid = child.process_id();
        let session = TerminalSession {
            id: String::new(),
            name: session_name,
            owner,
            master: pair.master,
            writer: Arc::new(Mutex::new(writer)),
            child,
            output,
            _reader_handle: reader_handle,
            controlled,
            shell,
            shell_pid,
            exit: None,
            job: None,
            started: Instant::now(),
            last_command: None,
        };
        // Checked again under the insert lock: a concurrent open may have
        // taken the last slot while this shell was starting.
        if let Err(full) = check_capacity(&inner, &session.owner) {
            drop(inner);
            terminate(session);
            return Err(full);
        }
        let id = format!("term-{}", inner.next_id);
        inner.next_id += 1;
        let mut session = session;
        session.id = id.clone();
        if session.name.is_empty() {
            session.name = id.clone();
        }
        inner.sessions.insert(id.clone(), session);

        Ok(id)
    }

    /// Run `text` in terminal `id` as a background job: returns at once;
    /// the job streams the terminal's clean output and settles with the
    /// command's exit code when the controlled shell's prompt returns. The
    /// terminal refuses other input until then. `job_kill` interrupts the
    /// command (INT, then TERM, then KILL), never the shell.
    pub fn send_background(
        &self,
        id: &str,
        text: &str,
        owner: &str,
        jobs: &crate::jobs::JobRegistry,
    ) -> Result<String, String> {
        {
            let mut inner = self.inner.lock().expect("registry lock");
            let session = owned(&mut inner, id, owner)?;
            if !session.controlled {
                return Err(format!(
                    "run_in_background needs a controlled terminal to know when the command \
                     ends; {id} runs {}. Open one with terminal_open (no shell argument)",
                    session.shell
                ));
            }
        }
        let (output, _, mark) = self.type_input(id, text, true, owner, true)?;
        let owner_id = owner.to_string();
        let label = format!("{id}: {}", text.lines().next().unwrap_or_default());
        let (job_id, writer) = jobs.start_owned("terminal", label, Some(&owner_id));
        {
            let mut inner = self.inner.lock().expect("registry lock");
            if let Some(session) = inner.sessions.get_mut(id) {
                if session.job.as_deref() == Some(JOB_STARTING) {
                    session.job = Some(job_id.clone());
                }
            }
        }
        let registry = self.clone();
        let thread_id = id.to_string();
        let text = text.to_string();
        let job_writer = writer.clone();
        // Settling may spawn a delivery task, which needs the runtime.
        let runtime = tokio::runtime::Handle::try_current().ok();
        let spawned = std::thread::Builder::new()
            .name("terminal-job".into())
            .spawn(move || {
                let _runtime = runtime.as_ref().map(|handle| handle.enter());
                let id = thread_id;
                let status = registry.drive_job(&id, &text, &output, mark, &owner_id, &writer);
                if let Some(session) = registry
                    .inner
                    .lock()
                    .expect("registry lock")
                    .sessions
                    .get_mut(&id)
                {
                    session.job = None;
                }
                writer.settle(status);
            });
        if let Err(e) = spawned {
            self.clear_job(id, &job_id);
            // The command was already typed; say so rather than leave a job
            // that looks running forever.
            job_writer.append(
                format!("[could not follow the command in {id}: {e}; use terminal_read]\n")
                    .as_bytes(),
            );
            job_writer.settle(crate::jobs::JobStatus::Interrupted);
            return Err(format!("failed to start terminal job thread: {e}"));
        }
        Ok(job_id)
    }

    /// Follow a background command until it settles, streaming its output
    /// into the job. Returns the job's final status.
    fn drive_job(
        &self,
        id: &str,
        text: &str,
        output: &Mutex<OutputBuffer>,
        mark: usize,
        owner: &str,
        writer: &crate::jobs::JobWriter,
    ) -> crate::jobs::JobStatus {
        use crate::jobs::JobStatus;
        let cancel = writer.cancelled();
        let started = Instant::now();
        let mut streamed = mark;
        let mut stopping: Option<Instant> = None;
        let mut escalations = ["TERM", "KILL"].into_iter();
        // Stream whole sanitized lines only: a partial line may still be
        // rewritten by a `\r` (progress bars), so it waits for its `\n`.
        // The first line is the terminal's echo of the command; drop it.
        let mut echo_lines = text.split('\n').count();
        let mut flush = |upto: usize, last: bool| {
            let buf = output.lock().expect("output lock");
            let (bytes, _) = buf.read_from(streamed, upto.saturating_sub(streamed));
            drop(buf);
            let cut = if last {
                bytes.len()
            } else {
                bytes
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |nl| nl + 1)
            };
            if cut == 0 {
                return;
            }
            streamed += cut;
            let rendered = sanitize::render(&bytes[..cut]);
            let mut lines = rendered.split_inclusive('\n');
            while echo_lines > 0 {
                if lines.next().is_none() {
                    break;
                }
                echo_lines -= 1;
            }
            let rest: String = lines.collect();
            if !rest.is_empty() {
                writer.append(rest.as_bytes());
            }
        };
        loop {
            std::thread::sleep(POLL);
            if cancel.is_cancelled() {
                // Timestamps are taken before `signal`, which itself waits
                // up to SIGNAL_SETTLE, so steps are ~1 s apart, not ~2 s.
                let since = *stopping.get_or_insert_with(|| {
                    let at = Instant::now();
                    let _ = self.signal(id, "INT", owner);
                    at
                });
                if since.elapsed() >= SIGNAL_SETTLE {
                    if let Some(next) = escalations.next() {
                        stopping = Some(Instant::now());
                        let _ = self.signal(id, next, owner);
                    }
                }
            }
            let settle = self.poll_settle(id, output, true, mark, started);
            let total = output.lock().expect("output lock").total_written();
            match settle {
                Some(Settle::Prompt { at, exit }) => {
                    flush(at, true);
                    return match (stopping, exit) {
                        (Some(_), _) => JobStatus::Killed,
                        (None, Some(code)) => JobStatus::Exited(Some(code)),
                        (None, None) => JobStatus::Exited(None),
                    };
                }
                Some(Settle::Exited(how)) => {
                    flush(total, true);
                    writer.append(format!("\n[terminal {id} exited ({how})]\n").as_bytes());
                    return JobStatus::Exited(None);
                }
                Some(Settle::Incomplete) if stopping.is_none() => {
                    flush(total, true);
                    writer.append(
                        format!(
                            "\n[the shell in {id} is waiting for more input (unclosed quote or \
                             block?); send the rest with terminal_send, or terminal_signal {id} INT]\n"
                        )
                        .as_bytes(),
                    );
                    return JobStatus::Exited(None);
                }
                // Quiet, input waits and full-screen programs keep running;
                // the job ends when the command does.
                _ => flush(total, false),
            }
        }
    }

    /// Type `text` into terminal `id`; returns its output buffer, whether it
    /// runs the controlled shell, and the stream offset before the input.
    fn type_input(
        &self,
        id: &str,
        text: &str,
        submit: bool,
        owner: &str,
        reserve_job: bool,
    ) -> Result<(Arc<Mutex<OutputBuffer>>, bool, usize), String> {
        let (writer, output, controlled, mark) = {
            let mut inner = self.inner.lock().expect("registry lock");
            let session = owned(&mut inner, id, owner)?;
            if let Some(exit) = session.exited() {
                return Err(format!(
                    "terminal {id} has exited ({exit}); open a new one with terminal_open"
                ));
            }
            if let Some(job) = &session.job {
                return Err(format!(
                    "terminal {id} is running background job {job}; wait for its completion, \
                     stop it with job_kill, or use another terminal"
                ));
            }
            if reserve_job {
                // Claimed under the same lock as the check above, so a
                // concurrent send can't slip in before the job id is known.
                session.job = Some(JOB_STARTING.into());
            }
            let mark = session.output.lock().expect("output lock").total_written();
            // A reply to a running command is input, not a new command.
            if submit && session.command_running() != Some(true) {
                let line = text.lines().next().unwrap_or_default().trim();
                session.last_command = Some((line.to_string(), Instant::now()));
            }
            (
                Arc::clone(&session.writer),
                Arc::clone(&session.output),
                session.controlled,
                mark,
            )
        };
        let mut payload = text.to_string();
        if submit {
            payload.push('\n');
        }
        // Outside the registry lock: this blocks while the PTY input queue
        // is full.
        let written = {
            let mut writer = writer.lock().expect("writer lock");
            writer
                .write_all(payload.as_bytes())
                .and_then(|()| writer.flush())
        };
        if let Err(e) = written {
            if reserve_job {
                self.clear_job(id, JOB_STARTING);
            }
            return Err(format!("write to terminal {id} failed: {e}"));
        }
        Ok((output, controlled, mark))
    }

    /// Release terminal `id`'s job slot if `job` still holds it.
    fn clear_job(&self, id: &str, job: &str) {
        let mut inner = self.inner.lock().expect("registry lock");
        if let Some(session) = inner.sessions.get_mut(id) {
            if session.job.as_deref() == Some(job) {
                session.job = None;
            }
        }
    }

    /// One look at whether the input typed at `mark` has settled. Never
    /// returns [`Settle::Timeout`]; deadlines are the caller's.
    fn poll_settle(
        &self,
        id: &str,
        output: &Mutex<OutputBuffer>,
        controlled: bool,
        mark: usize,
        started: Instant,
    ) -> Option<Settle> {
        let (eof, prompt, alt_screen, total, silent_for) = {
            let buf = output.lock().expect("output lock");
            (
                buf.eof,
                buf.prompt_since(mark),
                buf.alt_screen.filter(|at| *at >= mark),
                buf.total_written(),
                buf.last_output.max(started).elapsed(),
            )
        };
        if let Some((at, exit)) = prompt {
            return Some(Settle::Prompt { at, exit });
        }
        let (state, pgrp) = {
            let mut inner = self.inner.lock().expect("registry lock");
            let Some(session) = inner.sessions.get_mut(id) else {
                return Some(Settle::Exited("closed".into()));
            };
            (session.state(), foreground_group(session))
        };
        if let TerminalState::Exited(how) = state {
            return Some(Settle::Exited(how));
        }
        if eof {
            return Some(Settle::Exited("terminal closed".into()));
        }
        if alt_screen.is_some() {
            return Some(Settle::FullScreen);
        }
        match state {
            // Controlled shell owns the terminal but printed no marker:
            // it is reading more input itself — an unfinished command
            // (open quote, `do` without `done`; PS2 is empty) or a
            // builtin such as `read`.
            TerminalState::Idle if controlled && silent_for >= SETTLE_IDLE => {
                Some(Settle::Incomplete)
            }
            TerminalState::Idle if !controlled && silent_for >= SETTLE_IDLE && total > mark => {
                Some(Settle::Idle)
            }
            TerminalState::Running if silent_for >= SETTLE_IDLE && reads_tty(pgrp) => {
                Some(Settle::Input)
            }
            TerminalState::Running | TerminalState::Unknown if silent_for >= QUIET => {
                Some(Settle::Quiet)
            }
            _ => None,
        }
    }

    /// Send text to a terminal and wait until the command settles: it
    /// finished (controlled shell: prompt marker with exit code), it waits
    /// for input, it went quiet while still running, the deadline passed,
    /// or the shell exited. Returns clean output plus one status line.
    /// Blocks; `cancel` stops the wait (never the command).
    pub fn send(
        &self,
        id: &str,
        text: &str,
        submit: bool,
        wait_ms: Option<u64>,
        owner: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        self.send_presented(id, text, submit, wait_ms, owner, cancel)
            .map(|sent| sent.text)
    }

    /// [`send`](Self::send), plus facts for the UI's tool card.
    pub fn send_presented(
        &self,
        id: &str,
        text: &str,
        submit: bool,
        wait_ms: Option<u64>,
        owner: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Sent, String> {
        let wait = Duration::from_millis(wait_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS));
        let (output, controlled, mark) = self.type_input(id, text, submit, owner, false)?;

        let started = Instant::now();
        let settle = loop {
            if cancel() {
                break None;
            }
            std::thread::sleep(POLL);
            if let Some(settle) = self.poll_settle(id, &output, controlled, mark, started) {
                break Some(settle);
            }
            if started.elapsed() >= wait {
                break Some(Settle::Timeout(wait));
            }
        };

        let buf = output.lock().expect("output lock");
        let end = match settle {
            Some(Settle::Prompt { at, .. }) => at,
            _ => buf.total_written(),
        };
        let (bytes, _) = buf.read_from(mark, end.saturating_sub(mark));
        drop(buf);
        let mut body = sanitize::render(&bytes);
        if submit {
            body = strip_echo(&body, text);
        }
        let (body, truncated) = tail(body.trim_end_matches('\n'), MAX_READ_BYTES);
        let status = match settle {
            None => format!(
                "[cancelled: stopped waiting; the command may still be running in {id} \
                 (terminal_read to check, terminal_signal to stop it)]"
            ),
            Some(ref settle) => status_line(settle, id),
        };
        let mut out = String::new();
        if truncated {
            out.push_str(&format!(
                "[earlier output omitted; terminal_read {id} with offset {mark} pages from the start]\n"
            ));
        }
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
        out.push_str(&status);
        let (outcome, exit) = match &settle {
            None => ("cancelled", None),
            Some(Settle::Prompt { exit, .. }) => ("exited", *exit),
            Some(Settle::Idle) => ("done", None),
            Some(Settle::Input) => ("input", None),
            Some(Settle::Incomplete) => ("incomplete", None),
            Some(Settle::Quiet) => ("quiet", None),
            Some(Settle::Timeout(_)) => ("timeout", None),
            Some(Settle::FullScreen) => ("full_screen", None),
            Some(Settle::Exited(_)) => ("terminal_exited", None),
        };
        let presentation = json!({
            "version": 1,
            "kind": "terminal",
            "terminal": id,
            "sent": text.lines().next().unwrap_or_default(),
            "outcome": outcome,
            "exit_code": exit,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "status": status,
        });
        Ok(Sent {
            text: out,
            presentation,
        })
    }

    /// Read scrollback as clean text. Without `offset`, returns the last
    /// [`MAX_READ_BYTES`].
    pub fn read(
        &self,
        id: &str,
        offset: Option<usize>,
        owner: &str,
    ) -> Result<(String, usize), String> {
        let mut inner = self.inner.lock().expect("registry lock");
        let session = owned(&mut inner, id, owner)?;
        let state = session.state();
        let buf = session.output.lock().expect("output lock");
        let from = offset.unwrap_or_else(|| buf.total_written().saturating_sub(MAX_READ_BYTES));
        let (bytes, next_offset) = buf.read_from(from, MAX_READ_BYTES);
        let mut text = sanitize::render(&bytes);
        if next_offset >= buf.total_written() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("[{id}: {state}]"));
        }
        Ok((text, next_offset))
    }

    /// The shell's process group and the terminal's foreground process
    /// group (`tcgetpgrp` on the PTY). They differ while a command runs.
    #[cfg(unix)]
    fn foreground(&self, id: &str, owner: &str) -> Result<(i32, Option<i32>), String> {
        let mut inner = self.inner.lock().expect("registry lock");
        let session = owned(&mut inner, id, owner)?;
        let pid = session
            .child
            .process_id()
            .ok_or_else(|| format!("terminal session '{id}' has exited"))?;
        let shell = i32::try_from(pid).map_err(|_| format!("PID {pid} out of range"))?;
        Ok((shell, session.master.process_group_leader()))
    }

    /// Signal the command in the terminal's foreground, like pressing a
    /// control key in a real terminal. An interactive shell runs each
    /// command in its own process group, so signalling the shell's group
    /// would never reach it. The shell itself is never signalled.
    ///
    /// Returns a sentence describing what happened, for the model.
    #[cfg(unix)]
    pub fn signal(&self, id: &str, signal: &str, owner: &str) -> Result<String, String> {
        let (name, number) = parse_signal(signal)?;
        let (shell, foreground) = self.foreground(id, owner)?;
        let Some(group) = foreground.filter(|group| *group != shell) else {
            if number == libc::SIGINT {
                // Like Ctrl-C at an idle prompt: discard partial input
                // (for example a stuck continuation line). Wait for the
                // fresh prompt so the next send starts clean.
                let (writer, output) = {
                    let mut inner = self.inner.lock().expect("registry lock");
                    let session = owned(&mut inner, id, owner)?;
                    let mark = session.output.lock().expect("output lock").total_written();
                    (
                        Arc::clone(&session.writer),
                        session
                            .controlled
                            .then(|| (Arc::clone(&session.output), mark)),
                    )
                };
                {
                    let mut writer = writer.lock().expect("writer lock");
                    let _ = writer.write_all(b"\x03");
                    let _ = writer.flush();
                }
                if let Some((output, mark)) = output {
                    let deadline = Instant::now() + SIGNAL_SETTLE;
                    while Instant::now() < deadline
                        && output
                            .lock()
                            .expect("output lock")
                            .prompt_since(mark)
                            .is_none()
                    {
                        std::thread::sleep(POLL);
                    }
                }
                return Ok(format!(
                    "No command is running in {id}; sent Ctrl-C to the shell to clear its input line."
                ));
            }
            return Err(format!(
                "No command is running in {id}, so SIG{name} was not sent (the shell itself is never \
                 signalled). Use terminal_close to end the session."
            ));
        };
        // SAFETY: plain FFI call; a negative pid addresses a process group.
        if unsafe { libc::kill(-group, number) } != 0 {
            return Err(format!(
                "failed to send SIG{name} to process group {group} in {id}: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Report whether the shell got the terminal back: the command
        // exited or stopped.
        let deadline = Instant::now() + SIGNAL_SETTLE;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            match self.foreground(id, owner) {
                Ok((_, Some(now))) if now == group => {}
                Ok(_) => {
                    let resume = if matches!(name, "STOP" | "TSTP") {
                        " Resume it by sending `fg`."
                    } else {
                        ""
                    };
                    return Ok(format!(
                        "Sent SIG{name} to the foreground command in {id}; it has ended or stopped \
                         and the shell is back at its prompt.{resume}"
                    ));
                }
                Err(_) => return Ok(format!("Sent SIG{name}; terminal {id} has exited.")),
            }
        }
        let hint = match name {
            "INT" => " It may ignore SIGINT; try TERM, then KILL.",
            "TERM" => " It may ignore SIGTERM; try KILL.",
            _ => "",
        };
        Ok(format!(
            "Sent SIG{name} to the foreground command in {id}; it is still running after {}s.{hint}",
            SIGNAL_SETTLE.as_secs()
        ))
    }

    #[cfg(not(unix))]
    pub fn signal(&self, _id: &str, _signal: &str, _owner: &str) -> Result<String, String> {
        Err("signals not supported on this platform".into())
    }

    /// List open terminal sessions owned by the given session.
    pub fn list(&self, owner: &str) -> Vec<TerminalInfo> {
        let mut inner = self.inner.lock().expect("registry lock");
        let mut list: Vec<_> = inner
            .sessions
            .values_mut()
            .filter(|s| s.owner == owner)
            .map(TerminalSession::info)
            .collect();
        list.sort_by_key(|info| {
            info.id
                .strip_prefix("term-")
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        list
    }

    /// How many of `owner`'s terminals have a command running.
    pub fn running_count(&self, owner: &str) -> usize {
        self.list(owner).iter().filter(|t| t.running).count()
    }

    /// Every terminal with a command running, across all sessions (for
    /// the quit prompt).
    pub fn running_all(&self) -> Vec<TerminalInfo> {
        let mut inner = self.inner.lock().expect("registry lock");
        let mut list: Vec<_> = inner
            .sessions
            .values_mut()
            .map(TerminalSession::info)
            .filter(|t| t.running)
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// A terminal's snapshot plus its last `lines` lines of clean
    /// scrollback. Reading never moves anything the model sees.
    pub fn inspect(
        &self,
        id: &str,
        owner: &str,
        lines: usize,
    ) -> Result<TerminalInspection, String> {
        let mut inner = self.inner.lock().expect("registry lock");
        let session = owned(&mut inner, id, owner)?;
        let info = session.info();
        let buf = session.output.lock().expect("output lock");
        let from = buf.total_written().saturating_sub(MAX_READ_BYTES);
        let (bytes, _) = buf.read_from(from, MAX_READ_BYTES);
        drop(buf);
        let text = sanitize::render(&bytes);
        let all: Vec<&str> = text.trim_end_matches('\n').lines().collect();
        let tail = all[all.len().saturating_sub(lines)..].join("\n");
        Ok(TerminalInspection {
            terminal: info,
            output: tail,
        })
    }

    /// Interrupt `id`'s foreground command for a user: INT, then TERM and
    /// KILL if it keeps running. Blocks up to about three seconds and
    /// returns what happened.
    #[cfg(unix)]
    pub fn stop(&self, id: &str, owner: &str) -> Result<String, String> {
        let running = {
            let mut inner = self.inner.lock().expect("registry lock");
            owned(&mut inner, id, owner)?.command_running() == Some(true)
        };
        if !running {
            return Ok(format!("{id} has no command running."));
        }
        let mut last = String::new();
        for signal in ["INT", "TERM", "KILL"] {
            last = self.signal(id, signal, owner)?;
            let mut inner = self.inner.lock().expect("registry lock");
            if owned(&mut inner, id, owner)?.command_running() != Some(true) {
                return Ok(last);
            }
        }
        Ok(last)
    }

    #[cfg(not(unix))]
    pub fn stop(&self, _id: &str, _owner: &str) -> Result<String, String> {
        Err("signals not supported on this platform".into())
    }

    /// Close a terminal session.
    pub fn close(&self, id: &str, owner: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock");
        owned(&mut inner, id, owner)?;
        let session = inner.sessions.remove(id).unwrap();
        // Drop the lock before waiting on processes.
        drop(inner);
        terminate(session);
        Ok(())
    }

    /// Close all sessions (cleanup on shutdown).
    pub fn close_all(&self) {
        let sessions: Vec<_> = {
            let mut inner = self.inner.lock().expect("registry lock");
            inner.sessions.drain().map(|(_, session)| session).collect()
        };
        // In parallel: each may spend TERMINATE_GRACE waiting on its
        // processes, and quitting shouldn't pay that once per terminal.
        let mut handles = Vec::new();
        for session in sessions {
            let slot = Arc::new(Mutex::new(Some(session)));
            let theirs = Arc::clone(&slot);
            let spawned = std::thread::Builder::new()
                .name("terminal-close".into())
                .spawn(move || {
                    if let Some(session) = theirs.lock().expect("close slot").take() {
                        terminate(session);
                    }
                });
            match spawned {
                Ok(handle) => handles.push(handle),
                // No thread: close it here rather than skip it.
                Err(_) => {
                    if let Some(session) = slot.lock().expect("close slot").take() {
                        terminate(session);
                    }
                }
            }
        }
        for handle in handles {
            let _ = handle.join();
        }
    }
}

fn describe_exit(status: &portable_pty::ExitStatus) -> String {
    if status.success() {
        "exit code 0".into()
    } else {
        // portable-pty renders signals as "Terminated by ..." and codes as
        // "Exited with code N".
        status.to_string().replace("Exited with code", "exit code")
    }
}

/// End a terminal like closing a real one, but leave nothing behind: hang
/// up every process in the shell's session (the foreground command, `&`
/// jobs, the shell), give them a short grace to exit, then SIGKILL the
/// survivors. This runs even when the shell already exited, since `&` jobs
/// outlive a plain `exit`. The shell is reaped only at the very end, so its
/// pid (the session id) stays reserved throughout and every `getsid`
/// match really is ours. Processes that left the session (`setsid`,
/// daemons) are out of reach. The reader thread is left to finish on its
/// own: such an escapee can hold the PTY open, and close must not wait.
fn terminate(mut session: TerminalSession) {
    #[cfg(unix)]
    if let Some(shell) = session.shell_pid.and_then(|pid| i32::try_from(pid).ok()) {
        let ours = |pid: i32| unsafe { libc::getsid(pid) } == shell;
        let foreground = session.master.process_group_leader();
        let mut members = session_members(shell);
        // SAFETY: plain FFI calls; a negative pid addresses a group. The
        // foreground group was just read from our PTY, so it is ours.
        if let Some(group) = foreground.filter(|g| *g > 0 && *g != shell) {
            unsafe { libc::kill(-group, libc::SIGHUP) };
        }
        for pid in &members {
            unsafe { libc::kill(*pid, libc::SIGHUP) };
        }
        let deadline = Instant::now() + TERMINATE_GRACE;
        loop {
            // Zombies (the shell included, until reaped below) count as gone.
            members.retain(|pid| alive(*pid));
            if members.is_empty() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL);
        }
        // Late joiners (spawned during the grace) are killed too. Each pid
        // is re-checked right before the signal: a member that exited in
        // between may have been reaped and its number reused.
        members.extend(session_members(shell));
        for pid in members {
            if pid != shell && ours(pid) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
    if session.exited().is_none() {
        let _ = session.child.kill();
    }
    let _ = session.child.wait();
}

/// Processes whose session id is `sid`: everything started from a
/// terminal whose shell leads session `sid`.
#[cfg(unix)]
fn session_members(sid: i32) -> Vec<i32> {
    all_pids()
        .into_iter()
        // SAFETY: getsid only reads process metadata.
        .filter(|pid| *pid > 1 && unsafe { libc::getsid(*pid) } == sid)
        .collect()
}

/// A live process, not a zombie awaiting its parent's `wait`.
#[cfg(target_os = "linux")]
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat.rsplit_once(')')?.1.trim_start().chars().next()?;
            Some(state != 'Z' && state != 'X')
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn alive(pid: i32) -> bool {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    written == size && info.pbi_status != libc::SZOMB
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn alive(_pid: i32) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn all_pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .map(|dir| {
            dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn all_pids() -> Vec<i32> {
    // SAFETY: a null buffer asks for the count; the second call fills a
    // buffer sized with headroom for processes started in between.
    unsafe {
        let count = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if count <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0i32; count as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
        let filled = libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes);
        pids.truncate(filled.max(0) as usize);
        pids
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn all_pids() -> Vec<i32> {
    Vec::new()
}

/// `owner` may open another terminal, or the error that says what to do.
fn check_capacity(inner: &RegistryInner, owner: &str) -> Result<(), String> {
    let mut open: Vec<_> = inner
        .sessions
        .values()
        .filter(|s| s.owner == owner)
        .map(|s| s.id.clone())
        .collect();
    if open.len() < inner.config.max_sessions {
        return Ok(());
    }
    open.sort();
    Err(format!(
        "at most {} terminals can be open; close one with terminal_close first (open: {})",
        inner.config.max_sessions,
        open.join(", ")
    ))
}

/// The caller's session `id`, or an error that says what to do instead.
fn owned<'a>(
    inner: &'a mut RegistryInner,
    id: &str,
    owner: &str,
) -> Result<&'a mut TerminalSession, String> {
    match inner.sessions.get(id) {
        Some(session) if session.owner != owner => {
            return Err(format!(
                "terminal session '{id}' belongs to another session"
            ));
        }
        Some(_) => {}
        None => {
            let mut open: Vec<_> = inner
                .sessions
                .values()
                .filter(|s| s.owner == owner)
                .map(|s| {
                    if s.name == s.id {
                        s.id.clone()
                    } else {
                        format!("{} ({})", s.id, s.name)
                    }
                })
                .collect();
            open.sort();
            return Err(if open.is_empty() {
                format!("terminal session '{id}' not found; none are open, start one with terminal_open")
            } else {
                format!(
                    "terminal session '{id}' not found; open: {}",
                    open.join(", ")
                )
            });
        }
    }
    Ok(inner.sessions.get_mut(id).expect("checked above"))
}

/// The terminal's foreground process group, if the platform reports it.
fn foreground_group(session: &TerminalSession) -> Option<i32> {
    #[cfg(unix)]
    {
        session.master.process_group_leader()
    }
    #[cfg(not(unix))]
    {
        let _ = session;
        None
    }
}

/// Whether a process in `group` is blocked reading a terminal. Linux
/// only: `/proc/<pid>/wchan` names the kernel wait. macOS offers no
/// equivalent, so there a silent command reports "still running".
fn reads_tty(group: Option<i32>) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Some(group) = group else { return false };
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        entries.flatten().any(|entry| {
            let dir = entry.path();
            let in_group = std::fs::read_to_string(dir.join("stat"))
                .ok()
                .and_then(|stat| {
                    // Fields after the parenthesised command name:
                    // state ppid pgrp ...
                    let rest = &stat[stat.rfind(')')? + 2..];
                    rest.split(' ').nth(2)?.parse::<i32>().ok()
                })
                == Some(group);
            in_group
                && std::fs::read_to_string(dir.join("wchan"))
                    .is_ok_and(|wchan| matches!(wchan.trim(), "wait_woken" | "n_tty_read"))
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = group;
        false
    }
}

/// Drop the terminal's echo of what was just typed: the model knows what
/// it sent. Only removes an exact echo at the start.
fn strip_echo(body: &str, sent: &str) -> String {
    let mut rest = body;
    for line in sent.split('\n') {
        match rest.strip_prefix(line) {
            Some(after) => rest = after.strip_prefix('\n').unwrap_or(after),
            None => return body.to_string(),
        }
    }
    rest.to_string()
}

/// The last `max` bytes of `text`, cut at a line boundary; `true` if cut.
fn tail(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let start = text[start..].find('\n').map_or(start, |nl| start + nl + 1);
    (&text[start..], true)
}

/// "12s", or "0.5s" below ten seconds.
fn seconds(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 10.0 && duration.subsec_millis() != 0 {
        format!("{secs:.1}s")
    } else {
        format!("{}s", duration.as_secs())
    }
}

/// The one-line outcome that ends every `terminal_send` result.
fn status_line(settle: &Settle, id: &str) -> String {
    match settle {
        Settle::Prompt {
            exit: Some(code), ..
        } => format!("[exit code: {code}]"),
        Settle::Prompt { exit: None, .. } => "[command finished]".into(),
        Settle::Idle => {
            "[back at the prompt; exit code unknown outside the controlled shell]".into()
        }
        Settle::Input => format!(
            "[waiting for input: reply with terminal_send to {id}, or terminal_signal to stop]"
        ),
        Settle::Incomplete => format!(
            "[no prompt yet: the shell wants more input (unclosed quote or block, or a `read`) \
             or is busy running builtins. Send the rest, or terminal_signal {id} INT to discard it]"
        ),
        Settle::Quiet => format!(
            "[still running, no output for {}s; it may be waiting for input. \
             terminal_read to follow, terminal_send to reply, terminal_signal to stop]",
            QUIET.as_secs()
        ),
        Settle::Timeout(after) => format!(
            "[still running after {}. terminal_read to follow, terminal_signal to stop]",
            seconds(*after)
        ),
        Settle::FullScreen => format!(
            "[a full-screen program started; its screen can't be shown as text. Quit it \
             (e.g. terminal_send 'q' or terminal_signal {id} INT) and use a plain command]"
        ),
        Settle::Exited(how) => {
            format!("[terminal exited ({how}); open a new one with terminal_open]")
        }
    }
}

impl Drop for RegistryInner {
    fn drop(&mut self) {
        for (_, session) in self.sessions.drain() {
            terminate(session);
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TerminalInfo {
    pub id: String,
    pub name: String,
    /// "bash, controlled", "zsh, login", ...
    pub shell: String,
    /// "idle at prompt", "command running", "exited (exit code 1)".
    pub state: String,
    /// A command (not the shell) owns the terminal.
    pub running: bool,
    /// The latest command typed at the prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Seconds since that command was typed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_secs: Option<u64>,
    /// Seconds since the terminal opened.
    pub uptime_secs: u64,
    /// The shell's working directory, when the platform can tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Background job running in this terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// Exit code of the last command (controlled shell only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<i32>,
}

/// [`TerminalInfo`] plus recent clean output.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TerminalInspection {
    #[serde(flatten)]
    pub terminal: TerminalInfo,
    pub output: String,
}

impl TerminalSession {
    fn info(&mut self) -> TerminalInfo {
        let state = self.state();
        let running = state == TerminalState::Running;
        let last_exit = self
            .output
            .lock()
            .expect("output lock")
            .prompts
            .last()
            .and_then(|(_, exit)| *exit);
        TerminalInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            shell: self.shell.clone(),
            state: state.to_string(),
            running,
            command: self.last_command.as_ref().map(|(text, _)| text.clone()),
            command_secs: self
                .last_command
                .as_ref()
                .map(|(_, at)| at.elapsed().as_secs()),
            uptime_secs: self.started.elapsed().as_secs(),
            cwd: match state {
                TerminalState::Exited(_) => None,
                _ => self.shell_pid.and_then(process_cwd),
            },
            job_id: self.job.clone(),
            last_exit,
        }
    }
}

/// A process's working directory.
#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // vip_path is a C string laid out as [[c_char; 32]; 32].
    let raw: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .collect();
    (!raw.is_empty()).then(|| String::from_utf8_lossy(&raw).into_owned())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_cwd(_pid: u32) -> Option<String> {
    None
}

/// Background reader: sanitizes PTY output into the buffer until EOF.
fn reader_loop(mut reader: Box<dyn Read + Send>, buf: Arc<Mutex<OutputBuffer>>) {
    let mut chunk = [0u8; 4096];
    let mut sanitizer = sanitize::Sanitizer::new();
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let (text, events) = sanitizer.push(&chunk[..n]);
                buf.lock().expect("output lock").record(&text, &events);
            }
            Err(e) => {
                tracing::debug!(%e, "terminal reader ended");
                break;
            }
        }
    }
    buf.lock().expect("output lock").eof = true;
}

// ── Tool implementations ───────────────────────────────────────────

use async_trait::async_trait;
use rness_engine::tools::Tool;
use rness_protocol::events::SessionId;
use rness_protocol::sandbox::SandboxMode;
use serde_json::{json, Value};

fn sandbox_deny(mode: SandboxMode) -> Result<String, String> {
    Err(format!(
        "terminal tools are disabled in {mode:?} sandbox mode \
         (requires DangerFullAccess)"
    ))
}

// ── terminal_open ─────────────────────────────────────────────────

pub struct TerminalOpenTool {
    registry: TerminalRegistry,
    ws: Arc<crate::Workspace>,
    sandbox: SandboxMode,
}

impl TerminalOpenTool {
    pub fn new(registry: TerminalRegistry, ws: Arc<crate::Workspace>) -> Self {
        Self {
            registry,
            ws,
            sandbox: SandboxMode::DangerFullAccess,
        }
    }
}

#[async_trait]
impl Tool for TerminalOpenTool {
    fn name(&self) -> &str {
        "terminal_open"
    }

    fn description(&self) -> &str {
        "Open a persistent terminal. The shell keeps cwd, env vars and running processes \
         between calls. Default: a clean bash (no dotfiles, pagers off) that reports each \
         command's exit code. Returns an id for terminal_send/read/signal/close."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Short label such as \"server\" or \"repl\""
                },
                "shell": {
                    "type": "string",
                    "description": "\"controlled\" (default): clean bash with exit codes. \"login\": the user's own shell with their dotfiles; exit codes unknown. Or a program path."
                }
            }
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("terminal_open requires session context; use execute_in".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        if self.sandbox != SandboxMode::DangerFullAccess {
            return sandbox_deny(self.sandbox);
        }
        let name = args["name"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let name = name.map(String::from);
        let shell = match args["shell"].as_str() {
            Some(raw) => ShellChoice::parse(Some(raw)),
            None => self.registry.default_shell(),
        };
        let cwd = self.ws.root().to_path_buf();
        let registry = self.registry.clone();
        let owner = session.clone();
        let opened = tokio::task::spawn_blocking(move || registry.open(name, shell, cwd, owner))
            .await
            .map_err(|e| format!("terminal open task: {e}"))??;
        let mut out = format!(
            "Opened terminal {} ({}) in {}.",
            opened.id,
            opened.shell,
            self.ws.root().display()
        );
        if !opened.ready {
            out.push_str(" The shell has not shown a prompt yet; check with terminal_read.");
        }
        if !opened.hidden_env.is_empty() {
            out.push_str(&format!(
                "\nWithheld secret-looking env vars: {}.",
                opened.hidden_env.join(", ")
            ));
        }
        if !opened.startup.is_empty() {
            let (startup, _) = tail(&opened.startup, 4096);
            out.push_str(&format!("\nStartup output:\n{startup}"));
        }
        Ok(out)
    }

    fn for_workspace(
        &self,
        session: &String,
        workspace: &std::path::Path,
    ) -> Option<Arc<dyn Tool>> {
        let _ = session;
        Some(Arc::new(Self {
            registry: self.registry.clone(),
            ws: self.ws.for_session(&String::new(), workspace),
            sandbox: self.sandbox,
        }))
    }

    fn for_workspace_with_policy(
        &self,
        session: &String,
        workspace: &std::path::Path,
        sandbox: SandboxMode,
    ) -> Option<Arc<dyn Tool>> {
        let _ = session;
        Some(Arc::new(Self {
            registry: self.registry.clone(),
            ws: self.ws.for_session(&String::new(), workspace),
            sandbox,
        }))
    }
}

// ── terminal_send ─────────────────────────────────────────────────

pub struct TerminalSendTool {
    registry: TerminalRegistry,
    sandbox: SandboxMode,
    /// Present when background jobs are available (`run_in_background`).
    jobs: Option<crate::jobs::JobRegistry>,
}

impl TerminalSendTool {
    pub fn new(registry: TerminalRegistry) -> Self {
        Self {
            registry,
            sandbox: SandboxMode::DangerFullAccess,
            jobs: None,
        }
    }

    /// Enable `run_in_background`, reporting through `jobs`.
    pub fn with_jobs(mut self, jobs: crate::jobs::JobRegistry) -> Self {
        self.jobs = Some(jobs);
        self
    }

    async fn run(
        &self,
        session: &SessionId,
        _call: &str,
        args: Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Sent, String> {
        if self.sandbox != SandboxMode::DangerFullAccess {
            return sandbox_deny(self.sandbox).map(|text| Sent {
                text,
                presentation: Value::Null,
            });
        }
        let id = crate::required_str(&args, "session_id")?.to_string();
        let text = crate::required_str(&args, "text")?.to_string();
        let submit = args["submit"].as_bool().unwrap_or(true);
        let wait_ms = args["wait_ms"].as_u64();
        let registry = self.registry.clone();
        let owner = session.clone();
        if args["run_in_background"].as_bool().unwrap_or(false) {
            let jobs = self
                .jobs
                .clone()
                .ok_or("run_in_background is unavailable: background jobs are not enabled")?;
            if !submit {
                return Err("run_in_background runs a command, so submit must be true".into());
            }
            let sent = text.lines().next().unwrap_or_default().to_string();
            let terminal = id.clone();
            let job = tokio::task::spawn_blocking(move || {
                registry.send_background(&id, &text, &owner, &jobs)
            })
            .await
            .map_err(|e| format!("terminal send task: {e}"))??;
            return Ok(Sent {
                text: format!(
                    "started background job {job}; completion arrives automatically with the exit code"
                ),
                presentation: json!({
                    "version": 1,
                    "kind": "terminal",
                    "terminal": terminal,
                    "sent": sent,
                    "outcome": "background",
                    "job_id": job,
                }),
            });
        }
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            registry.send_presented(&id, &text, submit, wait_ms, &owner, &|| {
                cancel.is_cancelled()
            })
        })
        .await
        .map_err(|e| format!("terminal send task: {e}"))?
    }
}

#[async_trait]
impl Tool for TerminalSendTool {
    fn name(&self) -> &str {
        "terminal_send"
    }

    fn description(&self) -> &str {
        if self.jobs.is_some() {
            "Type text into a terminal (Enter is pressed unless submit=false) and wait until the \
             command finishes, asks for input, goes quiet, or wait_ms passes. Returns the output \
             (echo stripped) and a final status line such as [exit code: 0] or [still running ...]. \
             For long-running commands (servers, watchers) a short wait_ms returns sooner. \
             run_in_background returns a job id at once; completion arrives with the exit code \
             (job_output to read, job_kill to interrupt) and the terminal stays busy until then."
        } else {
            "Type text into a terminal (Enter is pressed unless submit=false) and wait until the \
             command finishes, asks for input, goes quiet, or wait_ms passes. Returns the output \
             (echo stripped) and a final status line such as [exit code: 0] or [still running ...]. \
             For long-running commands (servers, watchers) a short wait_ms returns sooner."
        }
    }

    fn input_schema(&self) -> Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Terminal id from terminal_open"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type: a command, or a reply to a prompt"
                },
                "submit": {
                    "type": "boolean",
                    "description": "Press Enter after the text (default true)"
                },
                "wait_ms": {
                    "type": "integer",
                    "description": format!("Longest to wait in ms (default {DEFAULT_WAIT_MS}, max {MAX_WAIT_MS})")
                }
            },
            "required": ["session_id", "text"]
        });
        if self.jobs.is_some() {
            schema["properties"]["run_in_background"] = json!({
                "type": "boolean",
                "description": "Run the command as a background job and return its id immediately (controlled terminals only; default false)"
            });
        }
        schema
    }

    fn starts_background_job(&self, args: &Value) -> bool {
        self.jobs.is_some() && args["run_in_background"].as_bool().unwrap_or(false)
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("terminal_send requires session context; use execute_in".into())
    }

    async fn execute_call(
        &self,
        session: &SessionId,
        call: &str,
        args: Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        self.run(session, call, args, cancel)
            .await
            .map(|sent| sent.text)
    }

    async fn execute_presented(
        &self,
        session: &SessionId,
        call: &String,
        args: Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<
        (
            Vec<rness_protocol::events::ToolResultContentPart>,
            Option<rness_protocol::events::TaskSnapshot>,
            bool,
            Option<Value>,
        ),
        String,
    > {
        let sent = self.run(session, call, args, cancel).await?;
        Ok((
            vec![rness_protocol::events::ToolResultContentPart::Text { text: sent.text }],
            None,
            false,
            Some(sent.presentation),
        ))
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        let never = tokio_util::sync::CancellationToken::new();
        self.execute_call(session, "", args, &never).await
    }

    fn for_workspace_with_policy(
        &self,
        _session: &String,
        _workspace: &std::path::Path,
        sandbox: SandboxMode,
    ) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            registry: self.registry.clone(),
            sandbox,
            jobs: self.jobs.clone(),
        }))
    }
}

// ── terminal_read ─────────────────────────────────────────────────

pub struct TerminalReadTool {
    registry: TerminalRegistry,
    sandbox: SandboxMode,
}

impl TerminalReadTool {
    pub fn new(registry: TerminalRegistry) -> Self {
        Self {
            registry,
            sandbox: SandboxMode::DangerFullAccess,
        }
    }
}

#[async_trait]
impl Tool for TerminalReadTool {
    fn name(&self) -> &str {
        "terminal_read"
    }

    fn description(&self) -> &str {
        "Read a terminal's output as clean text, with its current state (idle, command \
         running, exited) at the end. Without offset: the latest output. Pass the returned \
         next_offset later to get only what arrived since."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Terminal id"
                },
                "offset": {
                    "type": "integer",
                    "description": "Position to read from, e.g. a previous next_offset (default: the last 64 KB)"
                }
            },
            "required": ["session_id"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("terminal_read requires session context; use execute_in".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        if self.sandbox != SandboxMode::DangerFullAccess {
            return sandbox_deny(self.sandbox);
        }
        let id = crate::required_str(&args, "session_id")?;
        let offset = args["offset"].as_u64().map(|v| v as usize);
        let (text, next_offset) = self.registry.read(id, offset, session)?;
        Ok(format!("{text}\n[next_offset: {next_offset}]"))
    }

    fn for_workspace_with_policy(
        &self,
        _session: &String,
        _workspace: &std::path::Path,
        sandbox: SandboxMode,
    ) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            registry: self.registry.clone(),
            sandbox,
        }))
    }
}

// ── terminal_signal ───────────────────────────────────────────────

pub struct TerminalSignalTool {
    registry: TerminalRegistry,
    sandbox: SandboxMode,
}

impl TerminalSignalTool {
    pub fn new(registry: TerminalRegistry) -> Self {
        Self {
            registry,
            sandbox: SandboxMode::DangerFullAccess,
        }
    }
}

#[async_trait]
impl Tool for TerminalSignalTool {
    fn name(&self) -> &str {
        "terminal_signal"
    }

    fn description(&self) -> &str {
        "Signal the command running in a terminal, like pressing Ctrl-C there. Targets the \
         terminal's foreground command, never the shell, so the session survives. Reports whether \
         the command ended. Signals: INT (default, Ctrl-C), TERM, KILL, TSTP, STOP, CONT."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Terminal session ID"
                },
                "signal": {
                    "type": ["string", "integer"],
                    "description": "Signal name such as \"INT\", \"TERM\" or \"KILL\" (default \"INT\")"
                }
            },
            "required": ["session_id"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("terminal_signal requires session context; use execute_in".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        if self.sandbox != SandboxMode::DangerFullAccess {
            return sandbox_deny(self.sandbox);
        }
        let id = crate::required_str(&args, "session_id")?.to_string();
        let signal = match &args["signal"] {
            Value::Null => "INT".to_string(),
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            other => return Err(format!("signal must be a name or number, got {other}")),
        };
        let registry = self.registry.clone();
        let owner = session.clone();
        // Waiting for the command to settle blocks briefly.
        tokio::task::spawn_blocking(move || registry.signal(&id, &signal, &owner))
            .await
            .map_err(|e| format!("terminal signal task: {e}"))?
    }

    fn for_workspace_with_policy(
        &self,
        _session: &String,
        _workspace: &std::path::Path,
        sandbox: SandboxMode,
    ) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            registry: self.registry.clone(),
            sandbox,
        }))
    }
}

// ── terminal_list ─────────────────────────────────────────────────

pub struct TerminalListTool {
    registry: TerminalRegistry,
    #[allow(dead_code)] // Carried for for_workspace_with_policy propagation.
    sandbox: SandboxMode,
}

impl TerminalListTool {
    pub fn new(registry: TerminalRegistry) -> Self {
        Self {
            registry,
            sandbox: SandboxMode::DangerFullAccess,
        }
    }
}

#[async_trait]
impl Tool for TerminalListTool {
    fn name(&self) -> &str {
        "terminal_list"
    }

    fn description(&self) -> &str {
        "List this session's terminals with their shell and state (idle, command running, exited)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("terminal_list requires session context; use execute_in".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        let _ = args;
        let sessions = self.registry.list(session);
        if sessions.is_empty() {
            return Ok("No open terminals.".into());
        }
        let mut out = String::new();
        for s in sessions {
            let label = if s.name == s.id {
                String::new()
            } else {
                format!(" \"{}\"", s.name)
            };
            out.push_str(&format!("{}{label} ({}): {}\n", s.id, s.shell, s.state));
        }
        Ok(out)
    }

    fn for_workspace_with_policy(
        &self,
        _session: &String,
        _workspace: &std::path::Path,
        sandbox: SandboxMode,
    ) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            registry: self.registry.clone(),
            sandbox,
        }))
    }
}

// ── terminal_close ────────────────────────────────────────────────

pub struct TerminalCloseTool {
    registry: TerminalRegistry,
    sandbox: SandboxMode,
}

impl TerminalCloseTool {
    pub fn new(registry: TerminalRegistry) -> Self {
        Self {
            registry,
            sandbox: SandboxMode::DangerFullAccess,
        }
    }
}

#[async_trait]
impl Tool for TerminalCloseTool {
    fn name(&self) -> &str {
        "terminal_close"
    }

    fn description(&self) -> &str {
        "Close a persistent terminal session and kill its processes."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Terminal session ID to close"
                }
            },
            "required": ["session_id"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("terminal_close requires session context; use execute_in".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        if self.sandbox != SandboxMode::DangerFullAccess {
            return sandbox_deny(self.sandbox);
        }
        let id = crate::required_str(&args, "session_id")?;
        self.registry.close(id, session)?;
        Ok(format!("Terminal session {id} closed."))
    }

    fn for_workspace_with_policy(
        &self,
        _session: &String,
        _workspace: &std::path::Path,
        sandbox: SandboxMode,
    ) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            registry: self.registry.clone(),
            sandbox,
        }))
    }
}

/// Register all terminal tools with the given registry.
pub fn register_terminal_tools(
    registry: &rness_engine::tools::ToolRegistry,
    terminals: TerminalRegistry,
    ws: Arc<crate::Workspace>,
    jobs: Option<crate::jobs::JobRegistry>,
) {
    let mut send = TerminalSendTool::new(terminals.clone());
    if let Some(jobs) = jobs {
        send = send.with_jobs(jobs);
    }
    registry.register(Arc::new(TerminalOpenTool::new(terminals.clone(), ws)));
    registry.register(Arc::new(send));
    registry.register(Arc::new(TerminalReadTool::new(terminals.clone())));
    registry.register(Arc::new(TerminalSignalTool::new(terminals.clone())));
    registry.register(Arc::new(TerminalListTool::new(terminals.clone())));
    registry.register(Arc::new(TerminalCloseTool::new(terminals)));
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const OWNER: &str = "owner";
    const PATIENCE: Duration = Duration::from_secs(10);

    /// A controlled bash (no dotfiles), as `terminal_open` starts by
    /// default. Markers are computed (`$((1+1))`) so the echoed input
    /// never matches the expected output.
    fn open_bash(registry: &TerminalRegistry) -> String {
        let opened = registry
            .open(
                None,
                ShellChoice::Controlled,
                std::env::temp_dir(),
                OWNER.into(),
            )
            .expect("open bash");
        assert!(opened.ready, "no first prompt: {opened:?}");
        assert_eq!(opened.shell, "bash, controlled");
        opened.id
    }

    fn never() -> bool {
        false
    }

    /// Send and wait for the command to settle.
    fn send(registry: &TerminalRegistry, id: &str, command: &str, wait_ms: u64) -> String {
        registry
            .send(id, command, true, Some(wait_ms), OWNER, &never)
            .expect("send")
    }

    fn scrollback(registry: &TerminalRegistry, id: &str) -> String {
        registry.read(id, Some(0), OWNER).expect("read").0
    }

    fn wait_for_output(registry: &TerminalRegistry, id: &str, needle: &str) {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if scrollback(registry, id).contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "timed out waiting for {needle:?}; got:\n{}",
            scrollback(registry, id)
        );
    }

    /// Wait until a command (not the shell) owns the terminal.
    fn wait_for_foreground_command(registry: &TerminalRegistry, id: &str) -> i32 {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if let Ok((shell, Some(group))) = registry.foreground(id, OWNER) {
                if group != shell {
                    return group;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no foreground command appeared in {id}");
    }

    /// Type a command without waiting for it.
    fn run(registry: &TerminalRegistry, id: &str, command: &str) {
        registry
            .send(id, command, true, Some(0), OWNER, &never)
            .expect("send");
    }

    fn ready(registry: &TerminalRegistry) -> String {
        let id = open_bash(registry);
        run(registry, id.as_str(), "echo \"ready-$((6*7))\"");
        wait_for_output(registry, &id, "ready-42");
        id
    }

    #[test]
    fn parse_signal_accepts_names_prefixes_and_numbers() {
        assert_eq!(parse_signal("INT").unwrap(), ("INT", libc::SIGINT));
        assert_eq!(parse_signal(" sigterm ").unwrap(), ("TERM", libc::SIGTERM));
        assert_eq!(
            parse_signal(&libc::SIGKILL.to_string()).unwrap(),
            ("KILL", libc::SIGKILL)
        );
        let err = parse_signal("HUP").unwrap_err();
        assert!(err.contains("INT, TERM, KILL"), "{err}");
    }

    #[test]
    fn shell_state_persists_between_sends() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        run(&registry, &id, "cd / && export RNESS_KEPT=yes");
        run(&registry, &id, "echo \"state:$PWD:$RNESS_KEPT:$((40+2))\"");
        wait_for_output(&registry, &id, "state:/:yes:42");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn interrupt_reaches_the_foreground_command_and_the_shell_survives() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        run(&registry, &id, "sleep 30; echo \"after-$((1+1))\"");
        let sleeper = wait_for_foreground_command(&registry, &id);

        let started = Instant::now();
        let outcome = registry.signal(&id, "INT", OWNER).unwrap();
        assert!(outcome.contains("has ended or stopped"), "{outcome}");
        assert!(started.elapsed() < Duration::from_secs(3));
        // SAFETY: signal 0 only checks whether the group still exists.
        assert_ne!(
            unsafe { libc::kill(-sleeper, 0) },
            0,
            "sleep survived SIGINT"
        );

        run(&registry, &id, "echo \"alive-$((2+3))\"");
        wait_for_output(&registry, &id, "alive-5");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn ignored_signal_reports_still_running_with_an_escalation_hint() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        run(&registry, &id, "bash -c 'trap \"\" INT; sleep 30'");
        wait_for_foreground_command(&registry, &id);

        let outcome = registry.signal(&id, "INT", OWNER).unwrap();
        assert!(outcome.contains("still running"), "{outcome}");
        assert!(outcome.contains("try TERM"), "{outcome}");

        let outcome = registry.signal(&id, "TERM", OWNER).unwrap();
        assert!(outcome.contains("has ended or stopped"), "{outcome}");
        registry.close(&id, OWNER).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signal_tool_accepts_names_and_numbers() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        let tool = TerminalSignalTool::new(registry.clone());
        let owner: SessionId = OWNER.into();

        run(&registry, &id, "sleep 30");
        wait_for_foreground_command(&registry, &id);
        let outcome = tool
            .execute_in(&owner, json!({ "session_id": id, "signal": libc::SIGTERM }))
            .await
            .unwrap();
        assert!(outcome.contains("SIGTERM"), "{outcome}");
        assert!(outcome.contains("has ended or stopped"), "{outcome}");

        let err = tool
            .execute_in(&owner, json!({ "session_id": id, "signal": "HUP" }))
            .await
            .unwrap_err();
        assert!(err.contains("unsupported signal"), "{err}");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn idle_shell_is_never_signalled() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);

        let outcome = registry.signal(&id, "INT", OWNER).unwrap();
        assert!(outcome.contains("No command is running"), "{outcome}");
        let err = registry.signal(&id, "KILL", OWNER).unwrap_err();
        assert!(err.contains("never signalled"), "{err}");

        run(&registry, &id, "echo \"alive-$((3+4))\"");
        wait_for_output(&registry, &id, "alive-7");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn other_sessions_cannot_touch_a_terminal() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        for err in [
            registry.signal(&id, "INT", "intruder").unwrap_err(),
            registry.read(&id, None, "intruder").unwrap_err(),
            registry
                .send(&id, "true", true, Some(0), "intruder", &never)
                .unwrap_err(),
            registry.close(&id, "intruder").unwrap_err(),
        ] {
            assert!(err.contains("belongs to another session"), "{err}");
        }
        assert!(registry.list("intruder").is_empty());
        assert_eq!(registry.list(OWNER).len(), 1);
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn close_ends_the_shell_and_forgets_the_session() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        let (shell, _) = registry.foreground(&id, OWNER).unwrap();

        registry.close(&id, OWNER).unwrap();
        assert!(registry.list(OWNER).is_empty());
        let err = registry.signal(&id, "INT", OWNER).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("none are open"), "{err}");
        // SAFETY: signal 0 only checks whether the pid exists.
        assert_ne!(
            unsafe { libc::kill(shell, 0) },
            0,
            "shell {shell} still alive"
        );
    }

    // ── phase 4: user controls, limits, cleanup ──

    #[test]
    fn close_kills_background_jobs_the_shell_started() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        // `&` jobs are in their own process group (job control is on in an
        // interactive shell), so hanging up the foreground misses them.
        run(&registry, &id, "sleep 300 & echo \"bgpid=$!\"");
        wait_for_output(&registry, &id, "bgpid=");
        let text = scrollback(&registry, &id);
        let pid: i32 = text
            .rsplit("bgpid=")
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no pid in {text}"));
        assert!(alive(pid), "background sleep {pid} not running");
        registry.close(&id, OWNER).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "background sleep {pid} survived close");
    }

    #[test]
    fn close_after_the_shell_exited_still_kills_its_background_jobs() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        // bash's plain `exit` leaves running `&` jobs alive (no hangup);
        // `trap '' HUP` makes the job survive a hangup too, so only the
        // KILL sweep can end it.
        run(
            &registry,
            &id,
            "(trap '' HUP; exec sleep 300) & echo \"bgpid=$!\"",
        );
        wait_for_output(&registry, &id, "bgpid=");
        let text = scrollback(&registry, &id);
        let pid: i32 = text
            .rsplit("bgpid=")
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no pid in {text}"));
        run(&registry, &id, "exit");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !registry.list(OWNER)[0].state.starts_with("exited") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(registry.list(OWNER)[0].state.starts_with("exited"));
        assert!(alive(pid), "background sleep {pid} should outlive `exit`");
        registry.close(&id, OWNER).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "background sleep {pid} survived close");
    }

    #[test]
    fn exit_is_observed_without_reaping_the_shell() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        let shell = registry.inner.lock().unwrap().sessions[&id]
            .shell_pid
            .unwrap() as i32;
        run(&registry, &id, "exit 3");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !registry.list(OWNER)[0].state.starts_with("exited") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(registry.list(OWNER)[0].state, "exited (exit code 3)");
        // Still a zombie: the pid (and session id) can't be reused yet.
        // SAFETY: signal 0 only checks that the pid exists.
        assert_eq!(unsafe { libc::kill(shell, 0) }, 0, "shell reaped early");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn open_respects_max_sessions_per_owner() {
        let registry = TerminalRegistry::with_config(TerminalConfig {
            max_sessions: 2,
            ..TerminalConfig::default()
        });
        let dir = std::env::temp_dir();
        let a = registry
            .open(None, ShellChoice::Controlled, dir.clone(), OWNER.into())
            .unwrap()
            .id;
        registry
            .open(None, ShellChoice::Controlled, dir.clone(), OWNER.into())
            .unwrap();
        let err = registry
            .open(None, ShellChoice::Controlled, dir.clone(), OWNER.into())
            .unwrap_err();
        assert!(err.contains("at most 2"), "{err}");
        assert!(err.contains("term-1, term-2"), "{err}");
        // Other sessions have their own budget.
        registry
            .open(None, ShellChoice::Controlled, dir.clone(), "other".into())
            .unwrap();
        registry.close(&a, OWNER).unwrap();
        registry
            .open(None, ShellChoice::Controlled, dir, OWNER.into())
            .unwrap();
        registry.close_all();
    }

    #[test]
    fn env_deny_withholds_configured_names() {
        let registry = TerminalRegistry::with_config(TerminalConfig {
            env_deny: vec!["RNESS_TEST_DENY_*".into(), "*_PRIVATE_THING".into()],
            ..TerminalConfig::default()
        });
        let config = registry.config();
        assert!(config.denies("rness_test_deny_x"));
        assert!(config.denies("MY_PRIVATE_THING"));
        assert!(!config.denies("RNESS_TEST_KEEP"));
        assert!(!config.denies("PRIVATE_THING_2"));
        let id = ready(&registry);
        let out = send(
            &registry,
            &id,
            "echo \"deny=${RNESS_TEST_DENY_A:-unset} path=${PATH:+set}\"",
            5000,
        );
        assert!(out.contains("deny=unset path=set"), "{out}");
        registry.close_all();
    }

    #[test]
    fn default_shell_comes_from_config() {
        assert_eq!(
            TerminalRegistry::new().default_shell(),
            ShellChoice::Controlled
        );
        let login = TerminalRegistry::with_config(TerminalConfig {
            shell: "login".into(),
            ..TerminalConfig::default()
        });
        assert_eq!(login.default_shell(), ShellChoice::Login);
    }

    #[test]
    fn info_inspect_and_presentation_describe_the_last_command() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        let sent = registry
            .send_presented(
                &id,
                "echo inspect-me; (exit 3)",
                true,
                Some(5000),
                OWNER,
                &never,
            )
            .unwrap();
        assert_eq!(sent.presentation["kind"], "terminal");
        assert_eq!(sent.presentation["outcome"], "exited");
        assert_eq!(sent.presentation["exit_code"], 3);
        assert_eq!(sent.presentation["sent"], "echo inspect-me; (exit 3)");
        assert!(sent.text.contains("inspect-me"));

        let info = &registry.list(OWNER)[0];
        assert!(!info.running);
        assert_eq!(info.command.as_deref(), Some("echo inspect-me; (exit 3)"));
        assert_eq!(info.last_exit, Some(3));
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let cwd = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let got = std::fs::canonicalize(info.cwd.as_deref().expect("cwd")).unwrap();
            assert_eq!(got, cwd);
        }
        let inspection = registry.inspect(&id, OWNER, 3).unwrap();
        assert!(
            inspection.output.contains("inspect-me"),
            "{}",
            inspection.output
        );
        assert!(inspection.output.lines().count() <= 3);
        assert!(registry.inspect(&id, "intruder", 3).is_err());

        run(&registry, &id, "sleep 30");
        wait_for_foreground_command(&registry, &id);
        assert_eq!(registry.running_count(OWNER), 1);
        assert_eq!(registry.running_all().len(), 1);
        assert_eq!(
            registry.running_all()[0].command.as_deref(),
            Some("sleep 30")
        );
        let stopped = registry.stop(&id, OWNER).unwrap();
        assert!(stopped.contains("SIGINT"), "{stopped}");
        assert_eq!(registry.running_count(OWNER), 0);
        assert_eq!(
            registry.stop(&id, OWNER).unwrap(),
            format!("{id} has no command running.")
        );
        registry.close_all();
    }

    #[test]
    fn stop_escalates_past_ignored_signals() {
        let registry = TerminalRegistry::new();
        let id = ready(&registry);
        run(
            &registry,
            &id,
            "bash -c 'trap \"\" INT TERM; while :; do sleep 0.1; done'",
        );
        wait_for_foreground_command(&registry, &id);
        let stopped = registry.stop(&id, OWNER).unwrap();
        assert!(stopped.contains("SIGKILL"), "{stopped}");
        assert_eq!(registry.running_count(OWNER), 0);
        // The shell survived.
        let out = send(&registry, &id, "echo still-here", 5000);
        assert!(out.contains("still-here"), "{out}");
        registry.close_all();
    }

    #[test]
    fn config_validation() {
        assert!(TerminalConfig::default().validate().is_ok());
        for bad in [
            TerminalConfig {
                shell: "fish".into(),
                ..Default::default()
            },
            TerminalConfig {
                max_sessions: 0,
                ..Default::default()
            },
            TerminalConfig {
                env_deny: vec!["*".into()],
                ..Default::default()
            },
            TerminalConfig {
                env_deny: vec!["A*B".into()],
                ..Default::default()
            },
            TerminalConfig {
                env_deny: vec!["*A*".into()],
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(TerminalConfig {
            env_deny: vec!["A".into(), "*_KEY".into(), "AWS_*".into()],
            ..Default::default()
        }
        .validate()
        .is_ok());
    }

    // ── phase 2: clean output, completion, exit codes ──

    #[test]
    fn send_returns_clean_output_and_the_exit_code() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);

        let out = send(
            &registry,
            &id,
            "printf '\\033[31mred-%s\\033[0m\\n' $((2*3))",
            5000,
        );
        assert_eq!(out, "red-6\n[exit code: 0]");

        let out = send(&registry, &id, "echo \"out-$((1+1))\"; false", 5000);
        assert_eq!(out, "out-2\n[exit code: 1]");

        let out = send(&registry, &id, "(exit 7)", 5000);
        assert_eq!(out, "[exit code: 7]");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn progress_bars_collapse_to_their_final_state() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let out = send(&registry, &id, "printf '10%%\\r50%%\\rdone\\n'", 5000);
        assert_eq!(out, "done\n[exit code: 0]");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn controlled_shell_hides_secrets_and_disables_pagers() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let out = send(
            &registry,
            &id,
            "echo \"[$PAGER:$GIT_PAGER:$TERM:$RNESS_TERMINAL]\"; env | grep -c '^PROMPT_COMMAND=' ",
            5000,
        );
        // PROMPT_COMMAND is not leaked into child environments.
        assert_eq!(out, "[cat:cat:dumb:1]\n0\n[exit code: 1]");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn secret_names_are_recognised() {
        for name in [
            "OPENAI_API_KEY",
            "api_key",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "GH_SECRET",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in [
            "PATH",
            "HOME",
            "KEYBOARD",
            "SECRETARY",
            "PASSWORD_STORE_DIR",
        ] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn a_long_command_reports_still_running_and_output_so_far() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let out = send(&registry, &id, "echo \"started-$((4+4))\"; sleep 30", 500);
        assert!(out.starts_with("started-8\n"), "{out}");
        assert!(
            out.ends_with(
                "[still running after 0.5s. terminal_read to follow, terminal_signal to stop]"
            ),
            "{out}"
        );

        let list = registry.list(OWNER);
        assert_eq!(list[0].state, "command running");

        registry.signal(&id, "INT", OWNER).unwrap();
        let (read, _) = registry.read(&id, None, OWNER).unwrap();
        assert!(read.ends_with(&format!("[{id}: idle at prompt]")), "{read}");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn unfinished_input_is_reported_and_interrupt_discards_it() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let out = send(&registry, &id, "echo \"unclosed", 5000);
        assert!(out.contains("the shell wants more input"), "{out}");

        let outcome = registry.signal(&id, "INT", OWNER).unwrap();
        assert!(outcome.contains("clear its input line"), "{outcome}");
        let out = send(&registry, &id, "echo \"fresh-$((5+5))\"", 5000);
        assert_eq!(out, "fresh-10\n[exit code: 0]");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn a_silent_command_settles_as_quiet_before_the_deadline() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let started = Instant::now();
        let out = send(&registry, &id, "cat", 30_000);
        assert!(started.elapsed() < QUIET + Duration::from_secs(2));
        if cfg!(target_os = "linux") {
            assert!(out.contains("[waiting for input"), "{out}");
        } else {
            assert!(out.contains("it may be waiting for input"), "{out}");
        }
        let out = send(&registry, &id, "hello-cat", 5000);
        assert!(out.starts_with("hello-cat"), "{out}");
        registry.signal(&id, "INT", OWNER).unwrap();
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn shell_exit_is_reported_and_later_sends_explain_it() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let out = send(&registry, &id, "exit 3", 5000);
        assert!(
            out.ends_with("[terminal exited (exit code 3); open a new one with terminal_open]"),
            "{out}"
        );
        assert_eq!(registry.list(OWNER)[0].state, "exited (exit code 3)");

        let err = registry
            .send(&id, "true", true, Some(0), OWNER, &never)
            .unwrap_err();
        assert!(err.contains("has exited (exit code 3)"), "{err}");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn cancel_stops_waiting_but_not_the_command() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let out = registry
            .send(&id, "sleep 30", true, Some(30_000), OWNER, &|| true)
            .unwrap();
        assert!(out.starts_with("[cancelled"), "{out}");
        wait_for_foreground_command(&registry, &id);
        registry.signal(&id, "INT", OWNER).unwrap();
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn full_screen_programs_are_reported() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let out = send(&registry, &id, "printf '\\033[?1049h'; sleep 30", 5000);
        assert!(out.contains("full-screen program"), "{out}");
        registry.signal(&id, "INT", OWNER).unwrap();
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn not_found_lists_the_open_terminals() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let err = registry.read("term-999", None, OWNER).unwrap_err();
        assert!(err.ends_with(&format!("open: {id}")), "{err}");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn prompt_marker_survives_a_ps1_override() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        send(&registry, &id, "PS1='custom> '; PROMPT_COMMAND=", 5000);
        // Clearing PROMPT_COMMAND loses the marker for good; a PS1 change
        // alone does not.
        let out = send(&registry, &id, "true", 3000);
        assert!(!out.contains("[exit code"), "{out}");
        registry.close(&id, OWNER).unwrap();

        let id = open_bash(&registry);
        send(&registry, &id, "PS1='custom> '", 5000);
        let out = send(&registry, &id, "echo \"kept-$((8+1))\"; false", 5000);
        assert_eq!(out, "kept-9\n[exit code: 1]");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn a_timed_out_command_shows_its_result_in_a_later_read() {
        let registry = TerminalRegistry::new();
        let id = open_bash(&registry);
        let out = send(&registry, &id, "sleep 1; echo \"late-$((6+6))\"", 200);
        assert!(out.contains("[still running after 0.2s"), "{out}");
        wait_for_output(&registry, &id, "late-12");
        let deadline = Instant::now() + PATIENCE;
        loop {
            let (read, _) = registry.read(&id, None, OWNER).unwrap();
            if read.ends_with(&format!("[{id}: idle at prompt]")) {
                break;
            }
            assert!(Instant::now() < deadline, "{read}");
            std::thread::sleep(POLL);
        }
        registry.close(&id, OWNER).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_speak_in_model_facing_terms() {
        let registry = TerminalRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let owner: SessionId = OWNER.into();
        let open = TerminalOpenTool::new(registry.clone(), crate::Workspace::new(&root));
        let out = open
            .execute_in(&owner, json!({ "name": "work" }))
            .await
            .unwrap();
        assert!(
            out.starts_with(&format!(
                "Opened terminal term-1 (bash, controlled) in {}.",
                root.display()
            )),
            "{out}"
        );

        let send = TerminalSendTool::new(registry.clone());
        let out = send
            .execute_in(&owner, json!({ "session_id": "term-1", "text": "pwd -P" }))
            .await
            .unwrap();
        assert_eq!(out, format!("{}\n[exit code: 0]", root.display()));

        let list = TerminalListTool::new(registry.clone());
        let out = list.execute_in(&owner, json!({})).await.unwrap();
        assert_eq!(out, "term-1 \"work\" (bash, controlled): idle at prompt\n");

        let read = TerminalReadTool::new(registry.clone());
        let err = read
            .execute_in(&owner, json!({ "session_id": "term-9" }))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "terminal session 'term-9' not found; open: term-1 (work)"
        );
        registry.close("term-1", OWNER).unwrap();
    }

    // ── phase 3: background jobs ──

    fn wait_job(jobs: &crate::jobs::JobRegistry, job: &str) -> crate::jobs::JobInspection {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let inspection = jobs.inspect(OWNER, job).unwrap();
            if !inspection.job.running {
                return inspection;
            }
            assert!(Instant::now() < deadline, "job {job} never settled");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn background_command_reports_output_and_exit_code_as_a_job() {
        let registry = TerminalRegistry::new();
        let jobs = crate::jobs::JobRegistry::new();
        let id = open_bash(&registry);
        let job = registry
            .send_background(
                &id,
                "printf '\\033[1mbg-%s\\033[0m\\n' $((7*6)); sleep 1; false",
                OWNER,
                &jobs,
            )
            .unwrap();

        let busy = registry
            .send(&id, "true", true, Some(0), OWNER, &never)
            .unwrap_err();
        assert!(
            busy.contains(&format!("running background job {job}")),
            "{busy}"
        );
        assert!(jobs
            .list(OWNER)
            .iter()
            .any(|j| j.job_id == job && j.kind == "terminal"));

        let done = wait_job(&jobs, &job);
        assert_eq!(done.output, "bg-42\n");
        assert_eq!(done.job.status, "exited");
        assert_eq!(done.job.exit_code, Some(1));
        assert!(
            done.job.label.starts_with(&format!("{id}: printf")),
            "{}",
            done.job.label
        );

        // The terminal is free again and its state intact.
        let out = send(&registry, &id, "echo \"after-$((2+2))\"", 5000);
        assert_eq!(out, "after-4\n[exit code: 0]");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn background_output_collapses_progress_bars_across_reads() {
        let registry = TerminalRegistry::new();
        let jobs = crate::jobs::JobRegistry::new();
        let id = open_bash(&registry);
        let job = registry
            .send_background(
                &id,
                "printf '10%%'; sleep 0.3; printf '\\r50%%'; sleep 0.3; printf '\\rdone\\n'",
                OWNER,
                &jobs,
            )
            .unwrap();
        let done = wait_job(&jobs, &job);
        assert_eq!(done.output, "done\n");
        assert_eq!(done.job.exit_code, Some(0));
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn job_kill_interrupts_the_command_and_keeps_the_shell() {
        let registry = TerminalRegistry::new();
        let jobs = crate::jobs::JobRegistry::new();
        let id = open_bash(&registry);
        let job = registry
            .send_background(&id, "echo \"serving-$((1+2))\"; sleep 60", OWNER, &jobs)
            .unwrap();
        let deadline = Instant::now() + PATIENCE;
        while !jobs
            .inspect(OWNER, &job)
            .unwrap()
            .output
            .contains("serving-3")
        {
            assert!(Instant::now() < deadline, "no output streamed");
            std::thread::sleep(POLL);
        }
        let started = Instant::now();
        assert!(jobs.stop(OWNER, &job).unwrap());
        let done = wait_job(&jobs, &job);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(done.job.status, "killed");

        let out = send(&registry, &id, "echo \"alive-$((3+3))\"", 5000);
        assert_eq!(out, "alive-6\n[exit code: 0]");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn a_term_ignoring_background_command_is_escalated_on_kill() {
        let registry = TerminalRegistry::new();
        let jobs = crate::jobs::JobRegistry::new();
        let id = open_bash(&registry);
        let job = registry
            .send_background(
                &id,
                "bash -c 'trap \"\" INT TERM; echo \"stubborn-$((4+5))\"; sleep 60'",
                OWNER,
                &jobs,
            )
            .unwrap();
        let deadline = Instant::now() + PATIENCE;
        while !jobs
            .inspect(OWNER, &job)
            .unwrap()
            .output
            .contains("stubborn-9")
        {
            assert!(Instant::now() < deadline, "no output streamed");
            std::thread::sleep(POLL);
        }
        jobs.stop(OWNER, &job).unwrap();
        let done = wait_job(&jobs, &job);
        assert_eq!(done.job.status, "killed");
        let out = send(&registry, &id, "echo \"alive-$((3+4))\"", 5000);
        assert_eq!(out, "alive-7\n[exit code: 0]");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn background_needs_a_controlled_terminal() {
        let registry = TerminalRegistry::new();
        let jobs = crate::jobs::JobRegistry::new();
        let opened = registry
            .open(
                None,
                ShellChoice::Program("/bin/sh".into()),
                std::env::temp_dir(),
                OWNER.into(),
            )
            .unwrap();
        let err = registry
            .send_background(&opened.id, "true", OWNER, &jobs)
            .unwrap_err();
        assert!(err.contains("needs a controlled terminal"), "{err}");
        assert!(jobs.list(OWNER).is_empty());
        registry.close(&opened.id, OWNER).unwrap();
    }

    #[test]
    fn closing_a_terminal_settles_its_job() {
        let registry = TerminalRegistry::new();
        let jobs = crate::jobs::JobRegistry::new();
        let id = open_bash(&registry);
        let job = registry
            .send_background(&id, "sleep 60", OWNER, &jobs)
            .unwrap();
        let group = wait_for_foreground_command(&registry, &id);
        let started = Instant::now();
        registry.close(&id, OWNER).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "close took {:?}",
            started.elapsed()
        );
        let done = wait_job(&jobs, &job);
        // Either the shell reported the hangup (exit 129) before it was
        // killed, or the terminal's exit ended the job.
        assert!(
            done.job.exit_code == Some(128 + libc::SIGHUP) || done.output.contains("exited"),
            "{done:?}"
        );
        // SAFETY: signal 0 only checks whether the group exists.
        assert_ne!(
            unsafe { libc::kill(-group, 0) },
            0,
            "command survived close"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_tool_advertises_background_only_with_jobs() {
        let registry = TerminalRegistry::new();
        let plain = TerminalSendTool::new(registry.clone());
        assert!(plain.input_schema()["properties"]["run_in_background"].is_null());
        assert!(!plain.starts_background_job(&json!({ "run_in_background": true })));

        let jobs = crate::jobs::JobRegistry::new();
        let tool = TerminalSendTool::new(registry.clone()).with_jobs(jobs.clone());
        assert!(tool.input_schema()["properties"]["run_in_background"].is_object());
        assert!(tool.starts_background_job(&json!({ "run_in_background": true })));

        let id = open_bash(&registry);
        let owner: SessionId = OWNER.into();
        let out = tool
            .execute_in(
                &owner,
                json!({ "session_id": id, "text": "echo \"tool-$((5*5))\"", "run_in_background": true }),
            )
            .await
            .unwrap();
        let job = out
            .strip_prefix("started background job ")
            .and_then(|rest| rest.split(';').next())
            .expect(&out)
            .to_string();
        let done = tokio::task::spawn_blocking(move || wait_job(&jobs, &job))
            .await
            .unwrap();
        assert_eq!(done.output, "tool-25\n");
        assert_eq!(done.job.exit_code, Some(0));
        registry.close(&id, OWNER).unwrap();
    }

    /// Without the prompt marker, completion is inferred from the
    /// foreground process group and the exit code is unknown.
    #[test]
    fn a_plain_shell_settles_when_it_owns_the_terminal_again() {
        let registry = TerminalRegistry::new();
        let opened = registry
            .open(
                Some("plain".into()),
                ShellChoice::Program("/bin/sh".into()),
                std::env::temp_dir(),
                OWNER.into(),
            )
            .expect("open sh");
        let id = opened.id;
        let started = Instant::now();
        let out = send(&registry, &id, "sleep 1; echo \"plain-$((3*3))\"", 10_000);
        assert!(out.contains("plain-9"), "{out}");
        assert!(
            out.ends_with("[back at the prompt; exit code unknown outside the controlled shell]"),
            "{out}"
        );
        assert!(started.elapsed() < Duration::from_secs(4));

        let list = registry.list(OWNER);
        assert_eq!(list[0].shell, "sh");
        assert_eq!(list[0].name, "plain");
        registry.close(&id, OWNER).unwrap();
    }

    #[test]
    fn shell_choice_parsing() {
        assert_eq!(ShellChoice::parse(None), ShellChoice::Controlled);
        assert_eq!(ShellChoice::parse(Some(" ")), ShellChoice::Controlled);
        assert_eq!(ShellChoice::parse(Some("login")), ShellChoice::Login);
        assert_eq!(
            ShellChoice::parse(Some("/bin/zsh")),
            ShellChoice::Program("/bin/zsh".into())
        );
    }

    #[test]
    fn echo_and_tail_helpers() {
        assert_eq!(strip_echo("ls\na\nb", "ls"), "a\nb");
        assert_eq!(strip_echo("one\ntwo\nout", "one\ntwo"), "out");
        assert_eq!(strip_echo("other\nout", "ls"), "other\nout");
        assert_eq!(tail("a\nbb\ncc", 4), ("cc", true));
        assert_eq!(tail("short", 64), ("short", false));
        assert_eq!(seconds(Duration::from_millis(500)), "0.5s");
        assert_eq!(seconds(Duration::from_secs(10)), "10s");
    }
}
