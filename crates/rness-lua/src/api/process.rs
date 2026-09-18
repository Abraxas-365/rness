//! Explicit external programs, isolated in a Unix process group.
use mlua::{Lua, LuaSerdeExt, Table};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<std::path::PathBuf>,
    timeout_ms: Option<u64>,
}

pub fn install(lua: &Lua, rness: &Table) -> mlua::Result<()> {
    let process = lua.create_table()?;
    process.set(
        "run",
        lua.create_function(|lua, spec: Table| {
            let spec: Spec = lua.from_value(mlua::Value::Table(spec))?;
            if spec.program.is_empty() || spec.timeout_ms == Some(0) {
                return Err(mlua::Error::runtime(
                    "program must be nonempty and timeout_ms must be positive",
                ));
            }
            let cancel = lua
                .app_data_ref::<tokio_util::sync::CancellationToken>()
                .map(|token| token.clone())
                .unwrap_or_default();
            #[cfg(unix)]
            {
                let result = run(spec, cancel).map_err(mlua::Error::external)?;
                lua.to_value(&result)
            }
            #[cfg(not(unix))]
            {
                let _ = (spec, cancel);
                Err::<mlua::Value, _>(mlua::Error::runtime(
                    "rness.process.run requires Unix process-group support on this platform",
                ))
            }
        })?,
    )?;
    rness.set("process", process)
}

#[cfg(unix)]
fn run(
    spec: Spec,
    cancel: tokio_util::sync::CancellationToken,
) -> std::io::Result<serde_json::Value> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // Files avoid pipe deadlocks and readers left waiting on inherited handles.
    // A fixed capture limit also bounds the data returned to the Lua VM.
    const LIMIT: u64 = 1024 * 1024;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    if cancel.is_cancelled() {
        return Err(std::io::Error::other("command cancelled"));
    }
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    if let Some(cwd) = spec.cwd {
        command.current_dir(cwd);
    }
    let child = command.spawn()?;
    let mut process = Group {
        child,
        reaped: false,
    };
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(spec.timeout_ms.unwrap_or(30_000)))
        .ok_or_else(|| std::io::Error::other("timeout_ms is too large"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let (status, reason) = runtime.block_on(async {
        loop {
            let reason = if cancel.is_cancelled() {
                Some("command cancelled")
            } else if Instant::now() >= deadline {
                Some("process timed out")
            } else if stdout.metadata()?.len() > LIMIT || stderr.metadata()?.len() > LIMIT {
                Some("process output limit exceeded")
            } else {
                None
            };
            if let Some(reason) = reason {
                process.kill()?;
                let status = process.child.wait()?;
                process.reaped = true;
                return Ok::<_, std::io::Error>((status, Some(reason)));
            }
            if process.exited()? {
                // Keep the leader unreaped until group cleanup, preventing PID reuse.
                if let Err(error) = process.kill() {
                    // Darwin reports EPERM for a group containing only zombies.
                    // Do not mask this error elsewhere (especially cancellation).
                    #[cfg(target_os = "macos")]
                    if error.raw_os_error() != Some(libc::EPERM) {
                        return Err(error);
                    }
                    #[cfg(not(target_os = "macos"))]
                    return Err(error);
                }
                let status = process.child.wait()?;
                process.reaped = true;
                return Ok((status, None));
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                _ = tokio::time::sleep(Duration::from_millis(10)) => {},
            }
        }
    })?;
    if let Some(reason) = reason {
        return Err(std::io::Error::other(reason));
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    (&mut stdout).take(LIMIT).read_to_end(&mut out)?;
    (&mut stderr).take(LIMIT).read_to_end(&mut err)?;
    Ok(serde_json::json!({
        "code": status.code(), "signal": status.signal(), "success": status.success(),
        "stdout": String::from_utf8_lossy(&out), "stderr": String::from_utf8_lossy(&err),
        "truncated": stdout.metadata()?.len() > LIMIT || stderr.metadata()?.len() > LIMIT,
    }))
}

#[cfg(unix)]
struct Group {
    child: std::process::Child,
    reaped: bool,
}

#[cfg(unix)]
impl Group {
    fn exited(&self) -> std::io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    fn kill(&self) -> std::io::Result<()> {
        // The child created its own group before exec; negative PID addresses it.
        let result = unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
impl Drop for Group {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.kill();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn spec(program: &str, args: &[&str]) -> Spec {
        Spec {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            timeout_ms: Some(2000),
        }
    }

    #[test]
    fn captures_exit_arguments_and_cwd_without_shell_interpolation() {
        let result = run(
            spec(
                "/usr/bin/printf",
                &["%s", "$(touch should-not-exist); space"],
            ),
            CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result["stdout"], "$(touch should-not-exist); space");
        let dir = tempfile::tempdir().unwrap();
        let mut input = spec("/bin/pwd", &[]);
        input.cwd = Some(dir.path().canonicalize().unwrap());
        let result = run(input, CancellationToken::new()).unwrap();
        assert_eq!(
            result["stdout"].as_str().unwrap().trim(),
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
        let result = run(
            spec("/bin/sh", &["-c", "printf error >&2; exit 7"]),
            CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result["code"], 7);
        assert_eq!(result["stderr"], "error");
    }

    #[test]
    fn timeout_and_early_cancellation() {
        let mut input = spec("/bin/sh", &["-c", "while :; do :; done"]);
        input.timeout_ms = Some(30);
        assert!(run(input, CancellationToken::new())
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(run(spec("missing-program", &[]), cancel)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }

    #[test]
    fn cancellation_kills_group_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ready");
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let path = marker.clone();
        let worker = std::thread::spawn(move || {
            run(
                spec(
                    "/bin/sh",
                    &[
                        "-c",
                        "sleep 100 & echo $! > \"$1\"; wait",
                        "sh",
                        path.to_str().unwrap(),
                    ],
                ),
                token,
            )
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        cancel.cancel();
        assert!(worker
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(marker.exists());
    }
}
