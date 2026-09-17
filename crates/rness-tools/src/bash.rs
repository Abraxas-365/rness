//! Bash: run a shell command in the workspace, combined stdout/stderr,
//! with a timeout. Each invocation is a fresh non-interactive shell.
//!
//! dsh-informed semantics: non-zero exits are REPORTED (`[exit code: N]`),
//! not failed — the model decides how to react. Long output keeps its
//! tail. `run_in_background: true` registers the command as a job and
//! returns immediately; collect with `job_output`, stop with `job_kill`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rness_engine::tools::Tool;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::jobs::{JobRegistry, JobStatus};
use crate::sandbox::{self, Policy as SandboxPolicy};
use crate::{Workspace, required_str};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub struct BashTool {
    ws: Arc<Workspace>,
    jobs: JobRegistry,
    sandbox: rness_protocol::sandbox::SandboxMode,
    sandbox_policy: SandboxPolicy,
    process: rness_engine::sandbox::ProcessConfig,
}

impl BashTool {
    pub fn new(ws: Arc<Workspace>, jobs: JobRegistry) -> Self {
        Self::with_sandbox(ws, jobs, rness_protocol::sandbox::SandboxMode::DangerFullAccess)
    }

    fn with_sandbox(
        ws: Arc<Workspace>,
        jobs: JobRegistry,
        sandbox: rness_protocol::sandbox::SandboxMode,
    ) -> Self {
        let sandbox_policy = SandboxPolicy::new(sandbox, ws.root());
        Self { ws, jobs, sandbox, sandbox_policy, process: Default::default() }
    }

    pub fn with_process_config(mut self, process: rness_engine::sandbox::ProcessConfig) -> Self {
        self.process = process;
        self
    }
}

/// Keep the TAIL of oversized output — the newest bytes are the ones the
/// model needs (test summaries, final errors); the cut is marked up front.
fn tail_truncate(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let cut = text.len() - MAX_OUTPUT_BYTES;
    let mut start = cut;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text = text.split_off(start);
    format!("… {cut} bytes truncated …\n{text}")
}

fn publish_stream(pending: &mut Vec<u8>, bytes: &[u8], eof: bool, stream: &Option<(JobRegistry, String, String)>) {
    let Some((jobs, session, call)) = stream else { return };
    pending.extend_from_slice(bytes);
    let complete = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(error) if error.error_len().is_none() && !eof => error.valid_up_to(),
        Err(_) => pending.len(),
    };
    if complete > 0 {
        jobs.stream_output(session, call, String::from_utf8_lossy(&pending[..complete]).into_owned());
        pending.drain(..complete);
    }
}

async fn drain_stream(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    stream: &Option<(JobRegistry, String, String)>,
    artifact: &crate::jobs::JobWriter,
) -> std::io::Result<(Vec<u8>, usize)> {
    let mut total = 0;
    let mut output = Vec::new();
    let mut pending = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buffer).await?;
        total += n;
        crate::jobs::retain_tail(&mut output, &buffer[..n], MAX_OUTPUT_BYTES);
        artifact.append(&buffer[..n]);
        if artifact.cancelled().is_cancelled() { return Err(std::io::Error::other("output persistence failed")); }
        publish_stream(&mut pending, &buffer[..n], n == 0, stream);
        if n == 0 { return Ok((output, total)); }
    }
}

struct ShellGroup {
    #[cfg(unix)]
    pid: Option<i32>,
}

impl ShellGroup {
    fn disarm(&mut self) {
        #[cfg(unix)]
        { self.pid = None; }
    }

    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take() {
            // The child starts its own session, so this targets only its process group.
            unsafe { libc::kill(-pid, libc::SIGKILL); }
        }
    }
}

impl Drop for ShellGroup {
    fn drop(&mut self) { self.kill(); }
}

fn spawn_shell(
    command: &str,
    workdir: &std::path::Path,
    sandbox_policy: &SandboxPolicy,
    config: &rness_engine::sandbox::ProcessConfig,
) -> Result<(tokio::process::Child, ShellGroup, sandbox::Lease), String> {
    let (mut process, lease) = sandbox::prepare_configured(command, workdir, sandbox_policy, config)?;
    sandbox::standard_io(&mut process, workdir);
    #[cfg(unix)]
    // Closing stdin alone does not prevent sudo/getpass from opening /dev/tty.
    // setsid detaches the controlling terminal and creates a cleanup process group.
    unsafe {
        process.pre_exec(|| {
            if libc::setsid() == -1 { return Err(std::io::Error::last_os_error()); }
            Ok(())
        });
    }
    let child = process.spawn().map_err(|e| format!("spawn: {e}"))?;
    let group = ShellGroup {
        #[cfg(unix)]
        pid: child.id().map(|id| id as i32),
    };
    Ok((child, group, lease))
}

#[async_trait]
impl Tool for BashTool {
    fn for_workspace_with_policy(
        &self,
        session: &String,
        workspace: &std::path::Path,
        sandbox: rness_protocol::sandbox::SandboxMode,
    ) -> Option<Arc<dyn Tool>> {
        // Session binding supplies the durable canonical root. Re-resolving it
        // here would silently grant a replacement symlink's target on each turn.
        Some(Arc::new(Self {
            ws: self.ws.for_session(session, workspace),
            jobs: self.jobs.clone(),
            sandbox,
            process: self.process.clone(),
            sandbox_policy: SandboxPolicy { mode: sandbox, workspace: workspace.to_path_buf() },
        }))
    }
    fn name(&self) -> &str {
        "Bash"
    }

    fn starts_background_job(&self, args: &Value) -> bool {
        args["run_in_background"].as_bool().unwrap_or(false)
    }

    fn sensitive(&self) -> bool {
        true // arbitrary shell execution
    }

    fn description(&self) -> &str {
        "Run a shell command in the working directory. Returns combined \
         stdout/stderr; check the [exit code: N] marker on every result. \
         Each call is a fresh non-interactive shell with closed stdin and no \
         controlling terminal on Unix; password/input prompts cannot be answered. \
         No state persists; pass workdir \
         instead of using cd. Set run_in_background for long-running \
         commands: the call returns a job id immediately; read output \
         with job_output, stop with job_kill. Do not repeatedly poll running jobs. \
         If no independent work remains, tell the user you are waiting and end \
         your turn; the job continues and completion resumes the owning session. \
         Do not claim the task is finished until you have checked the result."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run" },
                "description": { "type": "string", "description": "Clear, concise description of what this command does in active voice, 5-10 words (shown in the UI)" },
                "workdir": { "type": "string", "description": "Working directory (default: workspace root)" },
                "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (default 120000, max 600000); ignored for background jobs" },
                "run_in_background": { "type": "boolean", "description": "Run as a background job and return its id immediately (default false)" },
            },
            "required": ["command", "description"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        self.run_presented(args, None, None).await.map(|(output, _)| output)
    }

    async fn execute_presented(
        &self,
        _session: &String,
        _call: &String,
        args: Value,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<
        (
            Vec<rness_protocol::events::ToolResultContentPart>,
            Option<rness_protocol::events::TaskSnapshot>,
            bool,
            Option<Value>,
        ),
        String,
    > {
        // Foreground commands must die with the turn: racing the cancel
        // token and DROPPING the run future kills the shell's process
        // group (ShellGroup::drop) and settles the capture as
        // Interrupted (CaptureGuard). Background jobs are owned by the
        // job registry, not the turn, so they are not raced here.
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let run = self.run_presented(args, Some(_session), Some(_call));
        let (output, mut presentation) = if background {
            run.await?
        } else {
            tokio::pin!(run);
            tokio::select! {
                biased;
                _ = _cancel.cancelled() => {
                    return Err("command cancelled; its process group was killed".into());
                }
                result = &mut run => result?,
            }
        };
        presentation["sandbox"] = json!(match self.sandbox {
            rness_protocol::sandbox::SandboxMode::ReadOnly => "read-only",
            rness_protocol::sandbox::SandboxMode::WorkspaceWrite => "workspace-write",
            rness_protocol::sandbox::SandboxMode::DangerFullAccess => "danger-full-access",
        });
        Ok((
            vec![rness_protocol::events::ToolResultContentPart::Text { text: output }],
            None,
            false,
            Some(presentation),
        ))
    }
}

impl BashTool {
    async fn run_presented(&self, args: Value, owner: Option<&String>, call: Option<&String>) -> Result<(String, Value), String> {
        let command = required_str(&args, "command")?;
        required_str(&args, "description")?;
        let workdir = self.ws.resolve(args["workdir"].as_str().unwrap_or("."));
        if !workdir.is_dir() {
            return Err(format!("workdir {} is not a directory", workdir.display()));
        }
        let workdir = std::fs::canonicalize(&workdir)
            .map_err(|e| format!("canonicalize workdir {}: {e}", workdir.display()))?;
        let sandbox_policy = &self.sandbox_policy;

        let stream = owner.zip(call).map(|(owner, call)| (self.jobs.clone(), owner.clone(), call.clone()));
        if args["run_in_background"].as_bool().unwrap_or(false) {
            let (mut child, mut group, lease) = spawn_shell(command, &workdir, sandbox_policy, &self.process)?;
            let (id, writer) = self.jobs.start_owned("bash", command.to_string(), owner);
            let mut stdout_pipe = child.stdout.take().expect("piped stdout");
            let mut stderr_pipe = child.stderr.take().expect("piped stderr");
            // Drain task: stream both pipes into the job, then settle.
            tokio::spawn(async move {
                let _lease = lease;
                let cancel = writer.cancelled();
                let mut out_buf = [0u8; 8192];
                let mut err_buf = [0u8; 8192];
                let mut out_stream = Vec::new();
                let mut err_stream = Vec::new();
                let mut out_open = true;
                let mut err_open = true;
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            group.kill();
                            let _ = child.kill().await;
                            writer.settle(JobStatus::Killed);
                            return;
                        }
                        n = stdout_pipe.read(&mut out_buf), if out_open => match n {
                            Ok(0) | Err(_) => { out_open = false; publish_stream(&mut out_stream, &[], true, &stream); },
                            Ok(n) => {
                                writer.append(&out_buf[..n]);
                                publish_stream(&mut out_stream, &out_buf[..n], false, &stream);
                            },
                        },
                        n = stderr_pipe.read(&mut err_buf), if err_open => match n {
                            Ok(0) | Err(_) => { err_open = false; publish_stream(&mut err_stream, &[], true, &stream); },
                            Ok(n) => {
                                writer.append(&err_buf[..n]);
                                publish_stream(&mut err_stream, &err_buf[..n], false, &stream);
                            },
                        },
                        status = child.wait(), if !out_open && !err_open => {
                            let code = status.ok().and_then(|s| s.code());
                            group.disarm();
                            writer.settle(JobStatus::Exited(code));
                            return;
                        }
                    }
                }
            });
            return Ok((format!("started background job {id}"), json!({
                "version":1,"kind":"bash","command":command,"cwd":workdir,
                "job_id":id,"status":"running",
            })));
        }

        let timeout = Duration::from_millis(
            args["timeout_ms"]
                .as_u64()
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        let (mut child, mut group, _lease) = spawn_shell(command, &workdir, sandbox_policy, &self.process)?;
        let (artifact_id, artifact) = self.jobs.capture(command.into(), owner);
        struct CaptureGuard(crate::jobs::JobWriter);
        impl Drop for CaptureGuard {
            fn drop(&mut self) { self.0.settle(JobStatus::Interrupted); }
        }
        let _capture_guard = CaptureGuard(artifact.clone());

        // Interleave-tolerant capture: drain both pipes concurrently, then
        // append stderr after stdout.
        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let run = async {
            let (out, err, status) = tokio::try_join!(
                drain_stream(&mut stdout_pipe, &stream, &artifact),
                drain_stream(&mut stderr_pipe, &stream, &artifact),
                child.wait(),
            ).map_err(|e| format!("capture command output: {e}"))?;
            Ok::<_, String>((out, err, status))
        };

        let artifact_cancel = artifact.cancelled();
        let outcome = tokio::select! {
            biased;
            _ = artifact_cancel.cancelled() => Err(artifact.output_error().unwrap_or_else(|| "output capture cancelled or persistence failed".to_owned())),
            result = tokio::time::timeout(timeout, run) => match result {
                Ok(result) => result,
                Err(_) => Err(format!("command timed out after {}ms", timeout.as_millis())),
            },
        };
        let ((out, stdout_bytes), (err, stderr_bytes), status) = match outcome {
            Ok(result) => result,
            Err(error) => {
                group.kill();
                let _ = child.kill().await;
                return Err(format!("{error}; retained output: job_output(job_id=\"{artifact_id}\", offset=0)"));
            }
        };

        artifact.settle(JobStatus::Exited(status.code()));
        group.disarm();
        let mut text = String::from_utf8_lossy(&out).into_owned();
        if !err.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&String::from_utf8_lossy(&err));
        }
        let presentation = json!({
            "version":1,"kind":"bash","command":command,"cwd":workdir,
            "status":"finished","exit_code":status.code(),
            "killed_by_signal":status.code().is_none(),
            "stdout_bytes":stdout_bytes,"stderr_bytes":stderr_bytes,
            "output_artifact":artifact_id,
            "truncated":stdout_bytes + stderr_bytes > MAX_OUTPUT_BYTES,
            "output_order":"stdout_then_stderr",
        });
        let mut text = tail_truncate(text);
        if stdout_bytes + stderr_bytes > MAX_OUTPUT_BYTES {
            text = format!("… output truncated; full output: job_output(job_id=\"{artifact_id}\", offset=0), page using returned byte offsets …\n{text}");
        }

        // Exit status is a RESULT the model reads, not a tool error: a
        // failing test run is a successful observation of that failure.
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let marker = match status.code() {
            Some(code) => format!("[exit code: {code}]"),
            None => "[killed by signal]".to_string(),
        };
        Ok((if text.is_empty() { marker } else { format!("{text}{marker}") }, presentation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rness_engine::tools::Tool;
    use rness_protocol::sandbox::SandboxMode;

    fn args(command: &str, workdir: &std::path::Path) -> Value {
        json!({"command": command, "description": "exercise sandbox behavior", "workdir": workdir})
    }

    async fn run(
        mode: SandboxMode,
        workspace: &std::path::Path,
        command: &str,
    ) -> Result<String, String> {
        let tool = BashTool::with_sandbox(Workspace::new(workspace), JobRegistry::new(), mode);
        tool.execute(args(command, workspace)).await
    }

    #[tokio::test]
    async fn unrestricted_mode_preserves_current_shell_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let output = run(
            SandboxMode::DangerFullAccess,
            dir.path(),
            "printf unrestricted",
        )
        .await
        .unwrap();
        assert!(output.contains("unrestricted"));
    }

    #[tokio::test]
    async fn default_mode_allows_workdir_and_writes_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let tool = BashTool::new(Workspace::new(workspace.path()), JobRegistry::new());
        let output = tool.execute(args("printf allowed > outside.txt", outside.path())).await.unwrap();
        assert!(output.contains("[exit code: 0]"), "{output}");
        assert_eq!(std::fs::read_to_string(outside.path().join("outside.txt")).unwrap(), "allowed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enforced_modes_reject_symlink_and_parent_workdir_escapes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::os::unix::fs::symlink(root.path(), workspace.join("escape")).unwrap();
        for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
            let tool = BashTool::with_sandbox(Workspace::new(&workspace), JobRegistry::new(), mode);
            for workdir in [workspace.join("escape"), workspace.join("..")] {
                let error = tool.execute(args("touch should-not-run", &workdir)).await.unwrap_err();
                assert!(error.contains("outside session workspace"), "{error}");
            }
        }
        assert!(!root.path().join("should-not-run").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bound_policy_rejects_retargeted_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let base = BashTool::new(Workspace::new(&workspace), JobRegistry::new());
        let tool = base.for_workspace_with_policy(
            &"session".to_string(), &workspace, SandboxMode::WorkspaceWrite,
        ).unwrap();
        std::fs::rename(&workspace, root.path().join("original")).unwrap();
        std::os::unix::fs::symlink(&outside, &workspace).unwrap();
        let error = tool.execute(args("touch should-not-run", &workspace)).await.unwrap_err();
        assert!(error.contains("no longer a canonical directory"), "{error}");
        let rebound = base.for_workspace_with_policy(
            &"session".to_string(), &workspace, SandboxMode::WorkspaceWrite,
        ).unwrap();
        let error = rebound.execute(args("touch should-not-run", &workspace)).await.unwrap_err();
        assert!(error.contains("no longer a canonical directory"), "{error}");
        assert!(!outside.join("should-not-run").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_interrupts_foreground_command_and_kills_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let tool = BashTool::new(Workspace::new(dir.path()), JobRegistry::new());
        let cancel = tokio_util::sync::CancellationToken::new();
        let command = format!("sleep 30 && touch {}", marker.display());
        let session = "session".to_string();
        let call = "call".to_string();
        let run = tool.execute_presented(
            &session,
            &call,
            args(&command, dir.path()),
            &cancel,
        );
        tokio::pin!(run);
        // Let the shell start, then cancel mid-flight.
        tokio::select! {
            _ = &mut run => panic!("command finished before cancellation"),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("cancellation must settle the call promptly");
        let error = result.unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        // The killed process group cannot resurrect and touch the marker.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!marker.exists());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[tokio::test]
    async fn unsupported_modes_never_execute_command() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
            let error = run(mode, dir.path(), "touch should-not-run").await.unwrap_err();
            assert!(error.contains("no backend has been implemented") || error.contains("windows_container_image"), "{error}");
            assert!(error.contains("refusing to run"), "{error}");
        }
        assert!(!dir.path().join("should-not-run").exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn read_only_mode_denies_workspace_writes() {
        let dir = tempfile::tempdir().unwrap();
        let output = run(
            SandboxMode::ReadOnly,
            dir.path(),
            "printf blocked > blocked.txt",
        )
        .await
        .unwrap();
        assert!(!dir.path().join("blocked.txt").exists());
        assert!(!output.contains("[exit code: 0]"), "{output}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn bound_read_only_policy_allows_reads_and_null_but_denies_outside_writes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(root.path().join("outside"), "readable").unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let base = BashTool::new(Workspace::new(root.path()), JobRegistry::new());
        let tool = base.for_workspace_with_policy(
            &"readonly-session".to_string(), &workspace, SandboxMode::ReadOnly,
        ).unwrap();
        let output = tool.execute(args("cat ../outside && printf discarded > /dev/null", &workspace)).await.unwrap();
        assert!(output.contains("readable"), "{output}");
        assert!(output.contains("[exit code: 0]"), "{output}");
        let output = tool.execute(args("printf blocked > ../outside", &workspace)).await.unwrap();
        assert!(!output.contains("[exit code: 0]"), "{output}");
        assert_eq!(std::fs::read_to_string(root.path().join("outside")).unwrap(), "readable");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn workspace_write_mode_allows_workspace_and_denies_parent() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let output = run(
            SandboxMode::WorkspaceWrite,
            &workspace,
            "printf allowed > inside.txt",
        )
        .await
        .unwrap();
        assert!(output.contains("[exit code: 0]"), "{output}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("inside.txt")).unwrap(),
            "allowed"
        );

        let output = run(
            SandboxMode::WorkspaceWrite,
            &workspace,
            "printf blocked > ../outside.txt",
        )
        .await
        .unwrap();
        assert!(!root.path().join("outside.txt").exists());
        assert!(!output.contains("[exit code: 0]"), "{output}");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn workspace_write_mode_blocks_link_escapes_and_outside_mutations() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("file-link")).unwrap();
        std::os::unix::fs::symlink(root.path(), workspace.join("dir-link")).unwrap();
        for command in [
            "printf escaped > file-link",
            "printf escaped > dir-link/outside",
            "printf escaped > dir-link/new-file",
            "ln ../outside hard-link && printf escaped > hard-link",
            "mv ../outside stolen",
            "rm ../outside",
            "chmod 777 ../outside",
        ] {
            let output = run(SandboxMode::WorkspaceWrite, &workspace, command).await.unwrap();
            assert!(!output.contains("[exit code: 0]"), "{command}: {output}");
            assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged", "{command}");
            assert!(!root.path().join("new-file").exists());
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn private_temp_writes_follow_mode_and_are_cleaned() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
            let output = run(mode, dir.path(),
                "printf '%s\\n' \"$TMPDIR\"; test \"$TMP\" = \"$TMPDIR\" && test \"$TEMP\" = \"$TMPDIR\" && printf data > \"$TMPDIR/payload\" && cat \"$TMPDIR/payload\"",
            ).await.unwrap();
            let path = std::path::Path::new(output.lines().next().unwrap());
            assert!(path.is_absolute(), "{output}");
            assert!(path.file_name().unwrap().to_string_lossy().starts_with("rness-sandbox-"));
            assert!(!path.exists(), "temporary directory leaked: {output}");
            assert_eq!(output.contains("[exit code: 0]"), mode == SandboxMode::WorkspaceWrite, "{output}");
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn profile_parameters_handle_special_workspace_paths() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("quote\"back\\slash\nλ");
        std::fs::create_dir(&workspace).unwrap();
        let tool = BashTool::with_sandbox(Workspace::new(&workspace), JobRegistry::new(), SandboxMode::WorkspaceWrite);
        let output = tool.execute(json!({"command": "printf allowed > inside", "description": "test literal profile path"})).await.unwrap();
        assert!(output.contains("[exit code: 0]"), "{output}");
        assert_eq!(std::fs::read_to_string(workspace.join("inside")).unwrap(), "allowed");
        let output = tool.execute(json!({"command": "printf blocked > ../outside", "description": "test literal profile confinement"})).await.unwrap();
        assert!(!output.contains("[exit code: 0]"), "{output}");
        assert!(!root.path().join("outside").exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn workspace_write_mode_confines_child_processes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let output = run(
            SandboxMode::WorkspaceWrite,
            &workspace,
            "sh -c 'printf blocked > ../child-outside.txt'",
        )
        .await
        .unwrap();
        assert!(!root.path().join("child-outside.txt").exists());
        assert!(!output.contains("[exit code: 0]"), "{output}");
    }
}
