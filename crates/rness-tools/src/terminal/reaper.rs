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
//!
//! Bash shells lead their own sessions too (`setsid`) and are registered
//! the same way, so background jobs and `&` children of foreground calls
//! die with rness however it ends. A shell that finished stays registered
//! while `&` children keep its session alive; the helper prunes sessions
//! that emptied on its own. Processes that leave the session (`setsid`,
//! double-fork daemons) escape, as they do from a closed terminal.
//! Windows has no helper; TODO(windows): a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` would give the same guarantee.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::Instant;

enum State {
    Unconfigured,
    Ready(Command),
    /// Lines for the writer thread, which owns the helper's stdin: a
    /// stalled helper never blocks a caller (Bash spawns on async tasks).
    Running(Sender<String>),
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

/// Sweep session `sid` if rness dies before [`unwatch`]. Call right after
/// spawning its leader: the leader's start time read here pins the number.
pub(crate) fn watch(sid: i32) {
    let mut state = state();
    if matches!(*state, State::Unconfigured | State::Off) {
        return;
    }
    let line = match start_time(sid) {
        Some(start) => format!("add {sid} {start}\n"),
        None => format!("add {sid}\n"),
    };
    if let State::Ready(command) = &mut *state {
        *state = start(command);
    }
    send(&mut state, line);
}

/// Session `sid` was cleaned up by rness itself.
pub(crate) fn unwatch(sid: i32) {
    send(&mut state(), format!("del {sid}\n"));
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
            Some(mut stdin) => {
                let (lines, queue) = std::sync::mpsc::channel::<String>();
                let spawned = std::thread::Builder::new()
                    .name("rness-reaper-writer".into())
                    .spawn(move || {
                        // Holds the only write end until rness exits.
                        let _child = child;
                        for line in queue {
                            if stdin.write_all(line.as_bytes()).is_err() {
                                break;
                            }
                        }
                    });
                match spawned {
                    Ok(_) => State::Running(lines),
                    Err(_) => State::Off,
                }
            }
            None => State::Off,
        },
        Err(error) => {
            tracing::warn!(%error, "terminal cleanup helper did not start; a killed rness may leave nohup'd processes behind");
            State::Off
        }
    }
}

fn send(state: &mut State, line: String) {
    if let State::Running(lines) = state {
        if lines.send(line).is_err() {
            *state = State::Off;
        }
    }
}

/// One protocol line: `add <sid> [<start>]` or `del <sid>`. Unknown verbs
/// and malformed lines are ignored, so newer senders stay compatible.
#[derive(Debug, PartialEq)]
enum Message {
    Add(i32, Option<u64>),
    Del(i32),
}

fn parse(line: &str) -> Option<Message> {
    let mut words = line.split_whitespace();
    let verb = words.next()?;
    let sid = words.next()?.parse::<i32>().ok()?;
    match verb {
        "add" if sid > 1 => Some(Message::Add(
            sid,
            words.next().and_then(|start| start.parse().ok()),
        )),
        "del" => Some(Message::Del(sid)),
        _ => None,
    }
}

/// Registrations kept between prunes of emptied sessions.
const PRUNE_EVERY: usize = 64;

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
        let mut added = 0usize;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            match parse(&line) {
                Some(Message::Add(sid, start)) => {
                    watched.insert(sid, start.or_else(|| start_time(sid)));
                    added += 1;
                    // Finished Bash calls stay registered; forget the
                    // sessions nothing lives in any more.
                    if added.is_multiple_of(PRUNE_EVERY) {
                        prune(&mut watched);
                    }
                }
                Some(Message::Del(sid)) => {
                    watched.remove(&sid);
                }
                None => {}
            }
        }
        sweep(&watched);
    }
    0
}

/// Session id of every process, read once (one pass for many sessions).
#[cfg(unix)]
fn members_of(sids: &HashSet<i32>) -> Vec<(i32, i32)> {
    super::all_pids()
        .into_iter()
        .filter(|pid| *pid > 1)
        // SAFETY: getsid only reads process metadata.
        .map(|pid| (pid, unsafe { libc::getsid(pid) }))
        .filter(|(_, sid)| sids.contains(sid))
        .collect()
}

/// Drop registrations whose session has no process left.
#[cfg(unix)]
fn prune(watched: &mut HashMap<i32, Option<u64>>) {
    let sids: HashSet<i32> = watched.keys().copied().collect();
    let live: HashSet<i32> = members_of(&sids).into_iter().map(|(_, sid)| sid).collect();
    watched.retain(|sid, _| live.contains(sid));
}

/// SIGKILL every process still in session `sid` (a dead rness's job whose
/// leader start time was just verified by the caller).
#[cfg(unix)]
pub(crate) fn kill_session(sid: i32) {
    for (pid, sid) in members_of(&HashSet::from([sid])) {
        // SAFETY: re-checked right before the signal, as in `sweep`.
        if unsafe { libc::getsid(pid) } == sid {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// SIGHUP everything in the watched sessions, a short grace, SIGKILL.
#[cfg(unix)]
fn sweep(watched: &HashMap<i32, Option<u64>>) {
    let sids: HashSet<i32> = watched
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
    // One process-table pass per round, however many sessions are watched.
    let members = || members_of(&sids);
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
pub(crate) fn start_time(pid: i32) -> Option<u64> {
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
pub(crate) fn start_time(pid: i32) -> Option<u64> {
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
pub(crate) fn start_time(_pid: i32) -> Option<u64> {
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
    fn protocol_accepts_start_times_and_ignores_unknown_lines() {
        assert_eq!(parse("add 42"), Some(Message::Add(42, None)));
        assert_eq!(parse("add 42 1234"), Some(Message::Add(42, Some(1234))));
        assert_eq!(parse("add 42 junk"), Some(Message::Add(42, None)));
        assert_eq!(parse("del 42"), Some(Message::Del(42)));
        assert_eq!(parse("add 1"), None, "never init");
        assert_eq!(parse("addj 42 /tmp/x"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("del x"), None);
    }

    #[test]
    fn bash_style_session_with_background_child_is_swept() {
        // A Bash call's shell (session leader) that left a `&` child: the
        // leader has exited, the child still holds the session.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "trap '' HUP; (trap '' HUP; sleep 60) & exit 0"]);
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                libc::setsid();
                Ok(())
            });
        }
        let mut leader = cmd.spawn().unwrap();
        let sid = leader.id() as i32;
        let start = start_time(sid);
        leader.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while members_of(&HashSet::from([sid])).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !members_of(&HashSet::from([sid])).is_empty(),
            "background child (subshell + sleep) in session {sid}"
        );
        let mut watched = HashMap::from([(sid, start)]);
        prune(&mut watched);
        assert!(watched.contains_key(&sid), "live session kept");
        sweep(&watched);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !members_of(&HashSet::from([sid])).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            members_of(&HashSet::from([sid])).is_empty(),
            "child survived"
        );
        prune(&mut watched);
        assert!(watched.is_empty(), "emptied session pruned");
    }

    #[test]
    fn kill_session_kills_every_member() {
        let mut child = stubborn_session();
        let sid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(100));
        kill_session(sid);
        assert!(gone(&mut child), "session {sid} survived");
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
