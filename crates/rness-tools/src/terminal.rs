//! Persistent shell sessions backed by portable-pty.
//!
//! Each terminal is a PTY + shell process that stays alive across tool
//! calls. The model creates sessions with `terminal_open`, sends
//! commands with `terminal_send`, reads output with `terminal_read`,
//! and manages sessions with `terminal_signal`/`terminal_list`/
//! `terminal_close`.
//!
//! Output is collected by a background reader thread into a ring
//! buffer (last MAX_SCROLLBACK_BYTES retained). Reads are
//! incremental: `terminal_send` returns output captured since the
//! send; `terminal_read` pages through retained scrollback.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Max bytes retained in the output ring buffer per session.
const MAX_SCROLLBACK_BYTES: usize = 256 * 1024;
/// Default wait time for output after sending a command (ms).
const DEFAULT_WAIT_MS: u64 = 3000;
/// Max wait time (ms).
const MAX_WAIT_MS: u64 = 30_000;
/// Max bytes returned per read.
const MAX_READ_BYTES: usize = 64 * 1024;

// ── Output buffer ──────────────────────────────────────────────────

/// Append-only output buffer with a rolling tail retained for reads.
struct OutputBuffer {
    /// All output bytes, truncated from the front when exceeding
    /// `MAX_SCROLLBACK_BYTES`. `base_offset` tracks the logical
    /// position of `data[0]` in the total stream.
    data: Vec<u8>,
    base_offset: usize,
}

impl OutputBuffer {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            base_offset: 0,
        }
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
    /// Notified when new output arrives.
    notify: Arc<std::sync::Condvar>,
    _reader_handle: std::thread::JoinHandle<()>,
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

impl TerminalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a new PTY session with the given shell.
    pub fn open(
        &self,
        name: Option<String>,
        shell: Option<String>,
        cwd: Option<std::path::PathBuf>,
        owner: String,
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

        let shell_cmd = shell.unwrap_or_else(|| {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
        });
        let mut cmd = CommandBuilder::new(&shell_cmd);
        cmd.cwd(cwd.unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        }));

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
        let notify = Arc::new(std::sync::Condvar::new());

        // Background reader thread: reads PTY output into the buffer.
        let buf_handle = Arc::clone(&output);
        let notify_handle = Arc::clone(&notify);
        let reader_handle = std::thread::Builder::new()
            .name("terminal-reader".into())
            .spawn(move || {
                reader_loop(reader, buf_handle, notify_handle);
            })
            .map_err(|e| format!("failed to spawn reader thread: {e}"))?;

        let mut inner = self.inner.lock().expect("registry lock");
        let id = format!("term-{}", inner.next_id);
        inner.next_id += 1;
        let session_name = name.unwrap_or_else(|| format!("{shell_cmd} ({id})"));

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
                notify,
                _reader_handle: reader_handle,
            },
        );

        Ok(id)
    }

    /// Send text to a terminal and wait for output.
    pub fn send(
        &self,
        id: &str,
        text: &str,
        submit: bool,
        wait_ms: Option<u64>,
        owner: &str,
    ) -> Result<String, String> {
        let wait = Duration::from_millis(
            wait_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS),
        );

        let (output_buf, _notify, mark) = {
            let mut inner = self.inner.lock().expect("registry lock");
            let session = inner
                .sessions
                .get_mut(id)
                .ok_or_else(|| format!("terminal session '{id}' not found"))?;
            Self::check_owner(session, owner)?;

            // Record position before sending.
            let mark = session.output.lock().expect("output lock").total_written();

            // Send the text.
            let mut payload = text.to_string();
            if submit {
                payload.push('\n');
            }
            session
                .writer
                .write_all(payload.as_bytes())
                .map_err(|e| format!("write to terminal failed: {e}"))?;
            session
                .writer
                .flush()
                .map_err(|e| format!("flush terminal failed: {e}"))?;

            (
                Arc::clone(&session.output),
                Arc::clone(&session.notify),
                mark,
            )
        };

        // Wait for output with idle detection: if no new output arrives for
        // 200ms after the last chunk, assume the command has finished.
        let deadline = Instant::now() + wait;
        let idle_threshold = Duration::from_millis(200);
        let mut last_output_at = Instant::now();
        let mut prev_total = mark;

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let current = output_buf.lock().expect("output lock").total_written();
            if current != prev_total {
                // New output arrived since last check.
                last_output_at = now;
                prev_total = current;
            }
            if current > mark && now.duration_since(last_output_at) >= idle_threshold {
                // Output received and then silence — command likely done.
                break;
            }
            // Sleep briefly to allow output to arrive.
            let sleep = idle_threshold.min(deadline - now);
            std::thread::sleep(sleep);
        }

        // Read everything from mark onward.
        let buf = output_buf.lock().expect("output lock");
        let (bytes, _) = buf.read_from(mark, MAX_READ_BYTES);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(text)
    }

    /// Read scrollback from a terminal session.
    pub fn read(&self, id: &str, offset: Option<usize>, owner: &str) -> Result<(String, usize), String> {
        let inner = self.inner.lock().expect("registry lock");
        let session = inner
            .sessions
            .get(id)
            .ok_or_else(|| format!("terminal session '{id}' not found"))?;
        Self::check_owner(session, owner)?;
        let buf = session.output.lock().expect("output lock");
        let from = offset.unwrap_or_else(|| buf.total_written().saturating_sub(MAX_READ_BYTES));
        let (bytes, next_offset) = buf.read_from(from, MAX_READ_BYTES);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok((text, next_offset))
    }

    /// Send a signal to the terminal's shell process group.
    #[cfg(unix)]
    pub fn signal(&self, id: &str, signal: i32, owner: &str) -> Result<(), String> {
        // Only allow common signals.
        const ALLOWED: &[i32] = &[2, 15, 9, 18, 19, 20]; // INT, TERM, KILL, CONT, STOP, TSTP
        if !ALLOWED.contains(&signal) {
            return Err(format!(
                "signal {signal} not allowed; permitted: {}",
                ALLOWED.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", "),
            ));
        }
        let inner = self.inner.lock().expect("registry lock");
        let session = inner
            .sessions
            .get(id)
            .ok_or_else(|| format!("terminal session '{id}' not found"))?;
        Self::check_owner(session, owner)?;
        let pid = session
            .child
            .process_id()
            .ok_or("cannot get child PID")?;
        let pid_i32 = i32::try_from(pid)
            .map_err(|_| format!("PID {pid} out of range for signal"))?;
        // Signal the process group (negative PID). The PTY child is a
        // session leader so its PID == its PGID.
        unsafe {
            if libc::kill(-pid_i32, signal) != 0 {
                return Err(format!(
                    "kill({}, {}) failed: {}",
                    pid,
                    signal,
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn signal(&self, _id: &str, _signal: i32, _owner: &str) -> Result<(), String> {
        Err("signals not supported on this platform".into())
    }

    /// List open terminal sessions owned by the given session.
    pub fn list(&self, owner: &str) -> Vec<TerminalInfo> {
        let inner = self.inner.lock().expect("registry lock");
        inner
            .sessions
            .values()
            .filter(|s| s.owner == owner)
            .map(|s| TerminalInfo {
                id: s.id.clone(),
                name: s.name.clone(),
            })
            .collect()
    }

    /// Close a terminal session.
    pub fn close(&self, id: &str, owner: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock");
        // Check ownership before removing.
        {
            let session = inner
                .sessions
                .get(id)
                .ok_or_else(|| format!("terminal session '{id}' not found"))?;
            Self::check_owner(session, owner)?;
        }
        let mut session = inner.sessions.remove(id).unwrap();
        // Drop the lock before blocking on join.
        drop(inner);

        // Kill the child process.
        session
            .child
            .kill()
            .map_err(|e| format!("failed to kill terminal process: {e}"))?;
        let _ = session.child.wait();
        // Drop the master to close the PTY, which will cause the reader
        // thread to exit on EOF.
        drop(session.writer);
        drop(session.master);
        // Wait for the reader thread to finish (bounded — it exits on EOF).
        let _ = session._reader_handle.join();
        Ok(())
    }

    fn check_owner(session: &TerminalSession, owner: &str) -> Result<(), String> {
        if session.owner != owner {
            return Err(format!(
                "terminal session '{}' belongs to another session",
                session.id,
            ));
        }
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
}

/// Background reader: reads PTY output into the buffer until EOF.
fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    buf: Arc<Mutex<OutputBuffer>>,
    notify: Arc<std::sync::Condvar>,
) {
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut b = buf.lock().expect("output lock");
                b.append(&chunk[..n]);
                notify.notify_all();
            }
            Err(e) => {
                tracing::debug!(%e, "terminal reader ended");
                break;
            }
        }
    }
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
        "Open a persistent terminal (PTY) session. The shell stays alive \
         between calls, preserving cwd, env vars, and running processes. \
         Returns a session ID for use with terminal_send/read/signal/close."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Human-readable name for this terminal session"
                },
                "shell": {
                    "type": "string",
                    "description": "Shell to spawn (default: $SHELL or /bin/bash)"
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
        let name = args["name"].as_str().map(String::from);
        let shell = args["shell"].as_str().map(String::from);
        let cwd = Some(self.ws.root().to_path_buf());
        let id = self.registry.open(name, shell, cwd, session.clone())?;
        // Give the shell a moment to start and print its banner.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (output, _) = self.registry.read(&id, Some(0), session)?;
        Ok(format!("Terminal session opened: {id}\n{output}"))
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
        "Send text to a persistent terminal session and wait for output. \
         Set submit=true (default) to press Enter after the text."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Terminal session ID from terminal_open"
                },
                "text": {
                    "type": "string",
                    "description": "Text to send to the terminal"
                },
                "submit": {
                    "type": "boolean",
                    "description": "Press Enter after the text (default true)"
                },
                "wait_ms": {
                    "type": "integer",
                    "description": "Max milliseconds to wait for output (default 3000, max 30000)"
                }
            },
            "required": ["session_id", "text"]
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("terminal_send requires session context; use execute_in".into())
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
        let text = crate::required_str(&args, "text")?;
        let submit = args["submit"].as_bool().unwrap_or(true);
        let wait_ms = args["wait_ms"].as_u64();
        // Run the blocking send on a dedicated thread.
        let registry = self.registry.clone();
        let id = id.to_string();
        let text = text.to_string();
        let owner = session.clone();
        tokio::task::spawn_blocking(move || registry.send(&id, &text, submit, wait_ms, &owner))
            .await
            .map_err(|e| format!("terminal send task: {e}"))?
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
        "Read output from a persistent terminal session. Returns the latest \
         output and a next_offset for paging."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Terminal session ID"
                },
                "offset": {
                    "type": "integer",
                    "description": "Byte offset to read from (default: last 64KB)"
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
        if text.is_empty() {
            Ok(format!("(no output; next_offset: {next_offset})"))
        } else {
            Ok(format!("{text}\n[next_offset: {next_offset}]"))
        }
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
        "Send a signal to a terminal session's foreground process group. \
         Common signals: 2 (SIGINT/Ctrl-C), 15 (SIGTERM), 9 (SIGKILL)."
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
                    "type": "integer",
                    "description": "Signal number (default: 2 = SIGINT)"
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
        let id = crate::required_str(&args, "session_id")?;
        let signal = args["signal"].as_i64().unwrap_or(2) as i32;
        self.registry.signal(id, signal, session)?;
        Ok(format!("Signal {signal} sent to {id}"))
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
        "List open persistent terminal sessions for this session."
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
            return Ok("No open terminal sessions.".into());
        }
        let mut out = String::new();
        for s in sessions {
            out.push_str(&format!("{}: {}\n", s.id, s.name));
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
