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
//! A session id is reserved while any process is in the session, but once
//! every member is gone the number can be taken by an unrelated session
//! (another rness instance's shell, say). So a session is only swept once
//! it is verified to still be ours: its leader is alive with the start time
//! recorded at registration, or (after `end`) one of the members recorded
//! when rness reaped the leader is alive with its recorded start time and
//! still in the session. Either proves the session never emptied, so every
//! current member is ours. A session that can't be verified (leader gone,
//! no live recorded member, unknown start time) is never touched.
//!
//! Bash shells lead their own sessions too (`setsid`) and are registered
//! the same way, so background jobs and `&` children of foreground calls
//! die with rness however it ends. When rness reaps a finished shell it
//! sends `end`: the helper records the session's members (pid and start
//! time) at that moment, or forgets the session if it is empty. The helper
//! also prunes sessions with nothing left to sweep on a timer. Processes
//! that leave the session (`setsid`, double-fork daemons) escape, as they
//! do from a closed terminal.
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

/// rness reaped the leader of session `sid`: keep sweeping only the
/// members the session has now (`&` children that outlived the shell).
pub(crate) fn end(sid: i32) {
    send(&mut state(), format!("end {sid}\n"));
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

/// One protocol line: `add <sid> [<start>]`, `end <sid>` or `del <sid>`.
/// Unknown verbs and malformed lines are ignored, so newer senders stay
/// compatible.
#[derive(Debug, PartialEq)]
enum Message {
    Add(i32, Option<u64>),
    End(i32),
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
        "end" => Some(Message::End(sid)),
        "del" => Some(Message::Del(sid)),
        _ => None,
    }
}

/// What the helper knows about one watched session.
#[derive(Debug, Clone, Default, PartialEq)]
struct Watch {
    /// The leader's start time, while rness hasn't reaped it. `None` if it
    /// couldn't be read at registration: never trusted.
    leader: Option<u64>,
    /// Members (pid, start time) seen while the session was verified ours:
    /// at `end`, and on each refresh while the leader runs. One of them
    /// still in the session proves the session never emptied.
    members: Vec<(i32, u64)>,
    /// rness reported the leader reaped (`end`).
    ended: bool,
}

type Watched = HashMap<i32, Watch>;

/// Registrations kept between prunes of emptied sessions.
const PRUNE_EVERY: usize = 64;
/// Refresh recorded members and prune at least this often. A terminal's
/// shell can die of the hangup before the helper sweeps; what it started
/// up to the last refresh is still swept.
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

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
        let (lines, queue) = std::sync::mpsc::channel::<String>();
        // The reader ends at EOF (rness gone), which disconnects `queue`.
        let reader = std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let mut watched = serve(&queue, REFRESH_INTERVAL);
        let _ = reader.join();
        // Record what joined since the last refresh while the leaders may
        // still be alive (a terminal's shell gets the hangup now too).
        prune(&mut watched);
        sweep(&watched);
    }
    0
}

/// Apply protocol lines until the sender disconnects, pruning every
/// [`PRUNE_EVERY`] registrations and refreshing at least every `interval`.
#[cfg(unix)]
fn serve(queue: &std::sync::mpsc::Receiver<String>, interval: std::time::Duration) -> Watched {
    use std::sync::mpsc::RecvTimeoutError;
    let mut watched = Watched::new();
    let mut added = 0usize;
    let mut next = Instant::now() + interval;
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        match queue.recv_timeout(wait) {
            Ok(line) => match parse(&line) {
                Some(Message::Add(sid, leader)) => {
                    // No start time means the leader can't be verified;
                    // never guess it later (the number may be reused).
                    watched.insert(
                        sid,
                        Watch {
                            leader,
                            ..Watch::default()
                        },
                    );
                    added += 1;
                    if added.is_multiple_of(PRUNE_EVERY) {
                        prune(&mut watched);
                    }
                }
                Some(Message::End(sid)) => {
                    if let Some(watch) = watched.get_mut(&sid) {
                        end_session(sid, watch);
                        if watch.members.is_empty() {
                            watched.remove(&sid);
                        }
                    }
                }
                Some(Message::Del(sid)) => {
                    watched.remove(&sid);
                }
                None => {}
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return watched,
        }
        if Instant::now() >= next {
            prune(&mut watched);
            next = Instant::now() + interval;
        }
    }
}

/// rness reaped the leader of session `sid`: record the members it has
/// now and stop trusting the leader's number. The reaped leader's pid was
/// held until just now, so a process leading `sid` already means reuse:
/// then only members recorded earlier and still in the session count.
#[cfg(unix)]
fn end_session(sid: i32, watch: &mut Watch) {
    watch.leader = None;
    watch.ended = true;
    let members = members_of(&HashSet::from([sid]));
    if members.iter().any(|(pid, _)| *pid == sid) {
        watch
            .members
            .retain(|(pid, start)| is_member(*pid, *start, sid));
        return;
    }
    watch.members = members
        .into_iter()
        .filter_map(|(pid, _)| start_time(pid).map(|start| (pid, start)))
        .collect();
}

/// `pid` is the process that started at `start` and is in session `sid`.
#[cfg(unix)]
fn is_member(pid: i32, start: u64, sid: i32) -> bool {
    // SAFETY: getsid only reads process metadata.
    start_time(pid) == Some(start) && unsafe { libc::getsid(pid) } == sid
}

/// The session is provably still ours: its registered leader runs, or a
/// recorded member is still in it. See the module docs.
#[cfg(unix)]
fn verified(sid: i32, watch: &Watch) -> bool {
    watch.leader.is_some_and(|start| is_member(sid, start, sid))
        || watch
            .members
            .iter()
            .any(|(pid, start)| is_member(*pid, *start, sid))
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

/// Refresh what each watch can prove and drop the ones with nothing left
/// to sweep. A session whose leader is verified gets its current members
/// recorded. Otherwise only recorded members still in it are kept, and
/// the watch goes once none are left, unless rness hasn't reported the
/// leader reaped yet (its `end` will record the members then).
#[cfg(unix)]
fn prune(watched: &mut Watched) {
    if watched.is_empty() {
        return;
    }
    let sids: HashSet<i32> = watched.keys().copied().collect();
    let mut live: HashMap<i32, Vec<i32>> = HashMap::new();
    for (pid, sid) in members_of(&sids) {
        live.entry(sid).or_default().push(pid);
    }
    watched.retain(|sid, watch| {
        let Some(pids) = live.get(sid) else {
            return false;
        };
        if watch
            .leader
            .is_some_and(|start| is_member(*sid, start, *sid))
        {
            watch.members = pids
                .iter()
                .filter_map(|pid| start_time(*pid).map(|start| (*pid, start)))
                .collect();
            return true;
        }
        watch
            .members
            .retain(|(pid, start)| is_member(*pid, *start, *sid));
        // A live leader with another start time reused the number: ours
        // is gone, and so is anything `end` could still tell us.
        let reused = watch
            .leader
            .is_some_and(|start| start_time(*sid).is_some_and(|now| now != start));
        !watch.members.is_empty() || (!watch.ended && !reused)
    });
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

/// SIGHUP everything in the watched sessions that are verified to still be
/// ours, a short grace, SIGKILL.
#[cfg(unix)]
fn sweep(watched: &Watched) {
    let sids: HashSet<i32> = watched
        .iter()
        .filter(|(sid, watch)| verified(**sid, watch))
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
        sweep(&HashMap::from([(
            sid,
            Watch {
                leader: start_time(sid),
                ..Watch::default()
            },
        )]));
        assert!(gone(&mut child), "session {sid} survived the sweep");
    }

    #[test]
    fn protocol_accepts_start_times_and_ignores_unknown_lines() {
        assert_eq!(parse("add 42"), Some(Message::Add(42, None)));
        assert_eq!(parse("add 42 1234"), Some(Message::Add(42, Some(1234))));
        assert_eq!(parse("add 42 junk"), Some(Message::Add(42, None)));
        assert_eq!(parse("del 42"), Some(Message::Del(42)));
        assert_eq!(parse("end 42"), Some(Message::End(42)));
        assert_eq!(parse("add 1"), None, "never init");
        assert_eq!(parse("addj 42 /tmp/x"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("del x"), None);
    }

    /// A Bash call's shell (session leader) that left a `&` child which
    /// ignores the hangup: returns the reaped leader's sid and start time
    /// once the child holds the session alone.
    fn finished_shell_with_child() -> (i32, Option<u64>) {
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
        (sid, start)
    }

    fn emptied(sid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !members_of(&HashSet::from([sid])).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        members_of(&HashSet::from([sid])).is_empty()
    }

    fn kill_members(sid: i32) {
        for (pid, _) in members_of(&HashSet::from([sid])) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    /// Kills what is left of a test's session even when the test panics,
    /// so a failure never leaks a HUP-ignoring loop.
    struct SessionGuard(i32);

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            unsafe { libc::kill(-self.0, libc::SIGKILL) };
            kill_members(self.0);
        }
    }

    #[test]
    fn bash_style_session_with_background_child_is_swept() {
        let (sid, start) = finished_shell_with_child();
        let mut watched = HashMap::from([(
            sid,
            Watch {
                leader: start,
                ..Watch::default()
            },
        )]);
        // rness reaped the shell and said so: the child is recorded.
        end_session(sid, watched.get_mut(&sid).unwrap());
        let _guard = SessionGuard(sid);
        assert!(!watched[&sid].members.is_empty(), "{:?}", watched[&sid]);
        prune(&mut watched);
        assert!(watched.contains_key(&sid), "live session kept");
        sweep(&watched);
        assert!(emptied(sid), "child survived");
        prune(&mut watched);
        assert!(watched.is_empty(), "emptied session pruned");
    }

    #[test]
    fn sweep_spares_a_session_whose_leader_is_gone_and_nothing_was_recorded() {
        // The leader is gone and no member was ever recorded: the number
        // could belong to anyone now, so it is never swept.
        let (sid, start) = finished_shell_with_child();
        let _guard = SessionGuard(sid);
        for leader in [start, None] {
            let watched = HashMap::from([(
                sid,
                Watch {
                    leader,
                    ..Watch::default()
                },
            )]);
            sweep(&watched);
            assert!(!members_of(&HashSet::from([sid])).is_empty(), "swept");
        }
        // Recorded members that exited prove nothing either.
        let gone = Watch {
            leader: None,
            members: vec![(sid, start.unwrap())],
            ended: true,
        };
        sweep(&HashMap::from([(sid, gone)]));
        assert!(!members_of(&HashSet::from([sid])).is_empty(), "swept");
        kill_members(sid);
        assert!(emptied(sid));
    }

    #[test]
    fn sweep_spares_a_live_leader_with_an_unknown_start_time() {
        // `add <sid>` without a start (the shell was already gone when
        // rness read it): a leader alive under that number now is not
        // provably ours.
        let mut child = stubborn_session();
        let sid = child.id() as i32;
        let _guard = SessionGuard(sid);
        std::thread::sleep(Duration::from_millis(100));
        sweep(&HashMap::from([(sid, Watch::default())]));
        assert!(
            child.try_wait().unwrap().is_none(),
            "unverified session killed"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn end_of_an_empty_session_forgets_it_and_refresh_prunes_on_a_timer() {
        let (lines, queue) = std::sync::mpsc::channel::<String>();
        let serve = std::thread::spawn(move || serve(&queue, Duration::from_millis(100)));
        // A shell that exited with nothing left behind, reaped.
        let mut empty = Command::new("true").spawn().unwrap();
        let empty_sid = empty.id() as i32;
        empty.wait().unwrap();
        // A finished shell whose `end` never arrives (e.g. still in flight
        // when its child exits): only the timer can prune it.
        let (orphan_sid, orphan_start) = finished_shell_with_child();
        let _orphan = SessionGuard(orphan_sid);
        // A running session the refresh records members of.
        let mut live = stubborn_session();
        let live_sid = live.id() as i32;
        let _live = SessionGuard(live_sid);
        std::thread::sleep(Duration::from_millis(100));
        let live_start = start_time(live_sid).unwrap();
        lines.send(format!("add {empty_sid} 1")).unwrap();
        lines.send(format!("end {empty_sid}")).unwrap();
        lines
            .send(format!("add {orphan_sid} {}", orphan_start.unwrap()))
            .unwrap();
        lines.send(format!("add {live_sid} {live_start}")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        kill_members(orphan_sid);
        assert!(emptied(orphan_sid));
        std::thread::sleep(Duration::from_millis(400));
        drop(lines);
        let watched = serve.join().unwrap();
        assert!(!watched.contains_key(&empty_sid), "{watched:?}");
        assert!(!watched.contains_key(&orphan_sid), "{watched:?}");
        let watch = &watched[&live_sid];
        assert!(
            watch.members.contains(&(live_sid, live_start)),
            "refreshed: {watch:?}"
        );
        let _ = live.kill();
        let _ = live.wait();
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
        let _guard = SessionGuard(sid);
        // Registered with another start time: this leader is not ours.
        let other = start_time(sid).map(|t| t + 1);
        assert!(other.is_some());
        sweep(&HashMap::from([(
            sid,
            Watch {
                leader: other,
                ..Watch::default()
            },
        )]));
        assert!(
            child.try_wait().unwrap().is_none(),
            "unrelated session killed"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
