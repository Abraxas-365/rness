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
/// command overwrote PS1.
const PROMPT_COMMAND: &str = "__rness_status=$?; export -n PROMPT_COMMAND PS1 2>/dev/null; \
     printf '\\033]133;D;%s\\007' \"$__rness_status\"; PS1='rness$ '";
/// How long `terminal_signal` watches for the command to leave the foreground.
const SIGNAL_SETTLE: Duration = Duration::from_secs(1);

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
        (
            self.data[start..end].to_vec(),
            self.base_offset + end,
        )
    }
}

// ── Terminal session ───────────────────────────────────────────────

struct TerminalSession {
    id: String,
    name: String,
    /// The rness session that created this terminal (ownership scoping).
    owner: String,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
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
}

impl TerminalSession {
    /// Record and describe the shell's exit, if it has exited.
    fn exited(&mut self) -> Option<String> {
        if self.exit.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit = Some(if status.success() {
                    "exit code 0".into()
                } else {
                    // portable-pty renders signals as "Terminated by ..."
                    // and codes as "Exited with code N".
                    status.to_string().replace("Exited with code", "exit code")
                });
            }
        }
        self.exit.clone()
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
}

impl Default for TerminalRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                sessions: HashMap::new(),
                next_id: 1,
            })),
        }
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

    /// Open a terminal and wait (bounded) for the shell's first prompt.
    /// Blocks; call from a blocking context.
    pub fn open(
        &self,
        name: Option<String>,
        shell: ShellChoice,
        cwd: std::path::PathBuf,
        owner: String,
    ) -> Result<Opened, String> {
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
            .filter(|key| is_secret_name(key))
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
        let id = format!("term-{}", inner.next_id);
        inner.next_id += 1;
        let session_name = name.unwrap_or_else(|| id.clone());
        let shell_pid = child.process_id();

        inner.sessions.insert(
            id.clone(),
            TerminalSession {
                id: id.clone(),
                name: session_name,
                owner,
                master: pair.master,
                writer,
                child,
                output,
                _reader_handle: reader_handle,
                controlled,
                shell,
                shell_pid,
                exit: None,
            },
        );

        Ok(id)
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
        let wait = Duration::from_millis(wait_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS));
        let (output, controlled, mark) = {
            let mut inner = self.inner.lock().expect("registry lock");
            let session = owned(&mut inner, id, owner)?;
            if let Some(exit) = session.exited() {
                return Err(format!(
                    "terminal {id} has exited ({exit}); open a new one with terminal_open"
                ));
            }
            let mark = session.output.lock().expect("output lock").total_written();
            let mut payload = text.to_string();
            if submit {
                payload.push('\n');
            }
            session
                .writer
                .write_all(payload.as_bytes())
                .and_then(|()| session.writer.flush())
                .map_err(|e| format!("write to terminal {id} failed: {e}"))?;
            (Arc::clone(&session.output), session.controlled, mark)
        };

        let started = Instant::now();
        let settle = loop {
            if cancel() {
                break None;
            }
            std::thread::sleep(POLL);
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
                break Some(Settle::Prompt { at, exit });
            }
            let (state, pgrp) = {
                let mut inner = self.inner.lock().expect("registry lock");
                let Some(session) = inner.sessions.get_mut(id) else {
                    break Some(Settle::Exited("closed".into()));
                };
                (session.state(), foreground_group(session))
            };
            if let TerminalState::Exited(how) = state {
                break Some(Settle::Exited(how));
            }
            if eof {
                break Some(Settle::Exited("terminal closed".into()));
            }
            if alt_screen.is_some() {
                break Some(Settle::FullScreen);
            }
            match state {
                // Controlled shell owns the terminal but printed no marker:
                // it is reading more input itself — an unfinished command
                // (open quote, `do` without `done`; PS2 is empty) or a
                // builtin such as `read`.
                TerminalState::Idle if controlled && silent_for >= SETTLE_IDLE => {
                    break Some(Settle::Incomplete);
                }
                TerminalState::Idle if !controlled && silent_for >= SETTLE_IDLE && total > mark => {
                    break Some(Settle::Idle);
                }
                TerminalState::Running if silent_for >= SETTLE_IDLE && reads_tty(pgrp) => {
                    break Some(Settle::Input);
                }
                TerminalState::Running | TerminalState::Unknown if silent_for >= QUIET => {
                    break Some(Settle::Quiet);
                }
                _ => {}
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
        Ok(out)
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
                let output = {
                    let mut inner = self.inner.lock().expect("registry lock");
                    let session = owned(&mut inner, id, owner)?;
                    let mark = session.output.lock().expect("output lock").total_written();
                    let _ = session.writer.write_all(b"\x03");
                    let _ = session.writer.flush();
                    session
                        .controlled
                        .then(|| (Arc::clone(&session.output), mark))
                };
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
            .map(|s| TerminalInfo {
                id: s.id.clone(),
                name: s.name.clone(),
                shell: s.shell.clone(),
                state: s.state().to_string(),
            })
            .collect();
        list.sort_by_key(|info| {
            info.id
                .strip_prefix("term-")
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        list
    }

    /// Close a terminal session.
    pub fn close(&self, id: &str, owner: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock");
        owned(&mut inner, id, owner)?;
        let mut session = inner.sessions.remove(id).unwrap();
        // Drop the lock before blocking on join.
        drop(inner);

        // End the shell unless it already exited on its own.
        if session.child.try_wait().ok().flatten().is_none() {
            session
                .child
                .kill()
                .map_err(|e| format!("failed to kill terminal process: {e}"))?;
        }
        let _ = session.child.wait();
        // Drop the master to close the PTY, which will cause the reader
        // thread to exit on EOF.
        drop(session.writer);
        drop(session.master);
        // Wait for the reader thread to finish (bounded — it exits on EOF).
        let _ = session._reader_handle.join();
        Ok(())
    }

    /// Close all sessions (cleanup on shutdown).
    pub fn close_all(&self) {
        let mut inner = self.inner.lock().expect("registry lock");
        for (_, mut session) in inner.sessions.drain() {
            let _ = session.child.kill();
            let _ = session.child.wait();
        }
    }
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
        for (_, mut session) in self.sessions.drain() {
            let _ = session.child.kill();
            let _ = session.child.wait();
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

    async fn execute_in(
        &self,
        session: &SessionId,
        args: Value,
    ) -> Result<String, String> {
        if self.sandbox != SandboxMode::DangerFullAccess {
            return sandbox_deny(self.sandbox);
        }
        let name = args["name"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let name = name.map(String::from);
        let shell = ShellChoice::parse(args["shell"].as_str());
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
}

impl TerminalSendTool {
    pub fn new(registry: TerminalRegistry) -> Self {
        Self {
            registry,
            sandbox: SandboxMode::DangerFullAccess,
        }
    }
}

#[async_trait]
impl Tool for TerminalSendTool {
    fn name(&self) -> &str {
        "terminal_send"
    }

    fn description(&self) -> &str {
        "Type text into a terminal (Enter is pressed unless submit=false) and wait until the \
         command finishes, asks for input, goes quiet, or wait_ms passes. Returns the output \
         (echo stripped) and a final status line such as [exit code: 0] or [still running ...]. \
         For long-running commands (servers, watchers) a short wait_ms returns sooner."
    }

    fn input_schema(&self) -> Value {
        json!({
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
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("terminal_send requires session context; use execute_in".into())
    }

    async fn execute_call(
        &self,
        session: &SessionId,
        _call: &str,
        args: Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        if self.sandbox != SandboxMode::DangerFullAccess {
            return sandbox_deny(self.sandbox);
        }
        let id = crate::required_str(&args, "session_id")?.to_string();
        let text = crate::required_str(&args, "text")?.to_string();
        let submit = args["submit"].as_bool().unwrap_or(true);
        let wait_ms = args["wait_ms"].as_u64();
        let registry = self.registry.clone();
        let owner = session.clone();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            registry.send(&id, &text, submit, wait_ms, &owner, &|| {
                cancel.is_cancelled()
            })
        })
        .await
        .map_err(|e| format!("terminal send task: {e}"))?
    }

    async fn execute_in(
        &self,
        session: &SessionId,
        args: Value,
    ) -> Result<String, String> {
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

    async fn execute_in(
        &self,
        session: &SessionId,
        args: Value,
    ) -> Result<String, String> {
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

    async fn execute_in(
        &self,
        session: &SessionId,
        args: Value,
    ) -> Result<String, String> {
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

    async fn execute_in(
        &self,
        session: &SessionId,
        args: Value,
    ) -> Result<String, String> {
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

    async fn execute_in(
        &self,
        session: &SessionId,
        args: Value,
    ) -> Result<String, String> {
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
) {
    registry.register(Arc::new(TerminalOpenTool::new(terminals.clone(), ws)));
    registry.register(Arc::new(TerminalSendTool::new(terminals.clone())));
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
