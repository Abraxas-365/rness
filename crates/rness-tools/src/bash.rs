//! Bash: run a shell command in the workspace, combined stdout/stderr,
//! with a timeout. Each invocation is a fresh non-interactive shell.
//!
//! dsh-informed semantics: non-zero exits are REPORTED (`[exit code: N]`),
//! not failed — the model decides how to react. Long output keeps its
//! tail. `run_in_background: true` registers the command as a job and
//! returns immediately; collect with `job_output`, stop with `job_kill`.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rness_engine::tools::Tool;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::jobs::{JobRegistry, JobStatus};
use crate::{required_str, Workspace};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub struct BashTool {
    ws: Arc<Workspace>,
    jobs: JobRegistry,
}

impl BashTool {
    pub fn new(ws: Arc<Workspace>, jobs: JobRegistry) -> Self {
        Self { ws, jobs }
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
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut pending = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buffer).await?;
        output.extend_from_slice(&buffer[..n]);
        publish_stream(&mut pending, &buffer[..n], n == 0, stream);
        if n == 0 { return Ok(output); }
    }
}

fn spawn_shell(command: &str, workdir: &std::path::Path) -> std::io::Result<tokio::process::Child> {
    tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

#[async_trait]
impl Tool for BashTool {
    fn for_workspace(&self, session: &String, workspace: &std::path::Path) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::new(self.ws.for_session(session, workspace), self.jobs.clone())))
    }
    fn name(&self) -> &str {
        "Bash"
    }

    fn sensitive(&self) -> bool {
        true // arbitrary shell execution
    }

    fn description(&self) -> &str {
        "Run a shell command in the working directory. Returns combined \
         stdout/stderr; check the [exit code: N] marker on every result. \
         Each call is a fresh shell — no state persists; pass workdir \
         instead of using cd. Set run_in_background for long-running \
         commands: the call returns a job id immediately; read output \
         with job_output, stop with job_kill."
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
    ) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, presentation) = self.run_presented(args, Some(_session), Some(_call)).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text { text: output }], None, false, Some(presentation)))
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

        let stream = owner.zip(call).map(|(owner, call)| (self.jobs.clone(), owner.clone(), call.clone()));
        if args["run_in_background"].as_bool().unwrap_or(false) {
            let mut child =
                spawn_shell(command, &workdir).map_err(|e| format!("spawn: {e}"))?;
            let (id, writer) = self.jobs.start_owned("bash", command.to_string(), owner);
            let mut stdout_pipe = child.stdout.take().expect("piped stdout");
            let mut stderr_pipe = child.stderr.take().expect("piped stderr");
            // Drain task: stream both pipes into the job, then settle.
            tokio::spawn(async move {
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

        let mut child = spawn_shell(command, &workdir).map_err(|e| format!("spawn: {e}"))?;

        // Interleave-tolerant capture: drain both pipes concurrently, then
        // append stderr after stdout.
        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let run = async {
            let (out, err, status) = tokio::join!(
                drain_stream(&mut stdout_pipe, &stream),
                drain_stream(&mut stderr_pipe, &stream),
                child.wait(),
            );
            let out = out.map_err(|e| format!("read stdout: {e}"))?;
            let err = err.map_err(|e| format!("read stderr: {e}"))?;
            let status = status.map_err(|e| format!("wait: {e}"))?;
            Ok::<_, String>((out, err, status))
        };

        let (out, err, status) = match tokio::time::timeout(timeout, run).await {
            Ok(r) => r?,
            Err(_) => {
                let _ = child.kill().await;
                return Err(format!("command timed out after {}ms", timeout.as_millis()));
            }
        };

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
            "stdout_bytes":out.len(),"stderr_bytes":err.len(),
            "truncated":text.len() > MAX_OUTPUT_BYTES,
            "output_order":"stdout_then_stderr",
        });
        let mut text = tail_truncate(text);

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
