//! Terminal cleanup that survives rness itself dying (SIGKILL, abort).
//!
//! rness normally closes every terminal on the way out, but a process that
//! is killed outright runs no code. Its PTYs still close, so shells and
//! their plain jobs get SIGHUP, but anything ignoring SIGHUP (`nohup`,
//! `trap '' HUP`) would live on. So on the first terminal, rness starts a
//! small helper (itself, re-executed, see [`configure`]) with a pipe on its
//! stdin and tells it each shell's session id. rness holds the only write
//! end, so the helper reads EOF exactly when rness is gone, however it
//! ended. It then does what closing a terminal does: SIGHUP every process
//! still in those sessions, a short grace, SIGKILL. Terminals rness closed
//! itself are unregistered first, so a normal exit leaves nothing to do.
//!
//! A session id is reserved while any process is in the session, so a
//! match can only be ours. The one reuse case, every member gone and the
//! number taken by a new session leader, is caught by the leader's start
//! time, recorded at registration.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

enum State {
    Unconfigured,
    Ready(Command),
    Running {
        stdin: ChildStdin,
        _child: Child,
    },
    /// Couldn't start, or the helper went away: nothing more to do.
    Off,
}

static REAPER: Mutex<State> = Mutex::new(State::Unconfigured);

fn state() -> std::sync::MutexGuard<'static, State> {
    REAPER.lock().unwrap_or_else(|e| e.into_inner())
}

/// How to start the helper: a command that ends up in [`run`]. Started
/// only when the first terminal opens. Without this call (tests, embedders)
/// there is no helper and terminals are cleaned up only by a living rness.
pub fn configure(command: Command) {
    let mut state = state();
    if matches!(*state, State::Unconfigured) {
        *state = State::Ready(command);
    }
}

/// Sweep session `sid` if rness dies before [`unwatch`].
pub(super) fn watch(sid: i32) {
    let mut state = state();
    if let State::Ready(command) = &mut *state {
        *state = start(command);
    }
    send(&mut state, &format!("add {sid}\n"));
}

/// Session `sid` was cleaned up by rness itself.
pub(super) fn unwatch(sid: i32) {
    send(&mut state(), &format!("del {sid}\n"));
}

fn start(command: &mut Command) -> State {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group: Ctrl-C or a hangup aimed at rness's group
    // must not take the helper down before it has swept.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    match command.spawn() {
        Ok(mut child) => match child.stdin.take() {
            Some(stdin) => State::Running {
                stdin,
                _child: child,
            },
            None => State::Off,
        },
        Err(error) => {
            tracing::warn!(%error, "terminal cleanup helper did not start; a killed rness may leave nohup'd processes behind");
            State::Off
        }
    }
}

fn send(state: &mut State, line: &str) {
    if let State::Running { stdin, .. } = state {
        if stdin.write_all(line.as_bytes()).is_err() {
            *state = State::Off;
        }
    }
}

/// The helper's main: collect session ids until EOF, then sweep what's
/// left. Returns the exit code.
pub fn run() -> i32 {
    #[cfg(unix)]
    {
        // SAFETY: plain FFI; ignoring these keeps the helper alive through
        // the terminal hangup and Ctrl-C that may be what ended rness.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
        let mut watched: HashMap<i32, Option<u64>> = HashMap::new();
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let (verb, sid) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            let Ok(sid) = sid.trim().parse::<i32>() else {
                continue;
            };
            match verb {
                "add" if sid > 1 => {
                    watched.insert(sid, start_time(sid));
                }
                "del" => {
                    watched.remove(&sid);
                }
                _ => {}
            }
        }
        sweep(&watched);
    }
    0
}

/// SIGHUP everything in the watched sessions, a short grace, SIGKILL.
#[cfg(unix)]
fn sweep(watched: &HashMap<i32, Option<u64>>) {
    let sids: Vec<i32> = watched
        .iter()
        // A live leader that started at another time is a new session
        // that reused the number.
        .filter(|(sid, started)| match start_time(**sid) {
            Some(now) => Some(now) == **started,
            None => true,
        })
        .map(|(sid, _)| *sid)
        .collect();
    if sids.is_empty() {
        return;
    }
    let members = || -> Vec<(i32, i32)> {
        sids.iter()
            .flat_map(|sid| super::session_members(*sid).into_iter().map(|p| (p, *sid)))
            .collect()
    };
    let mut pending = members();
    for (pid, _) in &pending {
        // SAFETY: plain FFI signal to a member just read from the table.
        unsafe { libc::kill(*pid, libc::SIGHUP) };
    }
    let deadline = Instant::now() + super::TERMINATE_GRACE;
    loop {
        pending.retain(|(pid, _)| super::alive(*pid));
        if pending.is_empty() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(super::POLL);
    }
    pending.extend(members());
    for (pid, sid) in pending {
        // Re-checked right before the signal, as in `terminate`.
        // SAFETY: getsid only reads process metadata.
        if unsafe { libc::getsid(pid) } == sid {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// When `pid` started, in a platform unit only compared for equality.
#[cfg(target_os = "linux")]
fn start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22 (starttime); fields after the ")" start at field 3.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
fn start_time(pid: i32) -> Option<u64> {
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
    (written == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn start_time(_pid: i32) -> Option<u64> {
    None
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A session leader that ignores SIGHUP, like a `nohup`'d job left in
    /// a terminal: only the SIGKILL step can end it.
    fn stubborn_session() -> std::process::Child {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "trap '' HUP; while :; do sleep 0.05; done"]);
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    }

    fn gone(child: &mut std::process::Child) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn sweep_kills_watched_sessions_that_ignore_hangup() {
        let mut child = stubborn_session();
        let sid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(100));
        sweep(&HashMap::from([(sid, start_time(sid))]));
        assert!(gone(&mut child), "session {sid} survived the sweep");
    }

    #[test]
    fn sweep_spares_a_session_that_reused_the_number() {
        let mut child = stubborn_session();
        let sid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(100));
        // Registered with another start time: this leader is not ours.
        let other = start_time(sid).map(|t| t + 1);
        assert!(other.is_some());
        sweep(&HashMap::from([(sid, other)]));
        assert!(
            child.try_wait().unwrap().is_none(),
            "unrelated session killed"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
