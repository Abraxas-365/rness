//! WP-2 E2E / adversarial tests for the built-in tools (bash, jobs, terminal,
//! read/edit/write, grep/glob, dispatch cancellation).
//!
//! Every test prints one or more `WP2 {json}` lines with what it measured, so
//! `cargo test -p rness-tools --test tools_e2e -- --nocapture --test-threads=1 2>&1 | grep '^WP2'`
//! gives the numbers for `e2e-findings/wp-2.md`.
//!
//! Tests that confirm a known product bug assert the CURRENT (buggy) behaviour
//! and are named `bug_*`; when the bug is fixed they fail and should be flipped.
//! Sizes are bounded (floods <= 200 MB, trees <= 100k files by default;
//! `RNESS_BENCH_GREP_FILES` raises the cancel tree).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rness_engine::tools::{Tool, ToolCall, ToolRegistry};
use rness_tools::jobs::{JobOutputTool, JobRegistry, JobStatus, Retention};
use rness_tools::terminal::{register_terminal_tools, TerminalRegistry};
use rness_tools::{register_all, Workspace};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

// ── helpers ──────────────────────────────────────────────────────────

fn tmp(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rness-e2e-wp2-{tag}-"))
        .tempdir()
        .unwrap()
}

fn report(probe: &str, value: Value) {
    let mut v = value;
    v["probe"] = json!(probe);
    println!("WP2 {v}");
}

/// Current resident set size of this process (bytes).
#[cfg(target_os = "macos")]
fn rss_bytes() -> u64 {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if n == size {
        info.pti_resident_size
    } else {
        0
    }
}
#[cfg(not(target_os = "macos"))]
fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096)
}

/// User+system CPU time of this process.
fn cpu_time() -> Duration {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// Peak RSS while `f` runs, sampled every 5 ms on a side thread.
struct RssPeak {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<u64>>,
    base: u64,
}
impl RssPeak {
    fn start() -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let base = rss_bytes();
        let handle = std::thread::spawn(move || {
            let mut peak = 0;
            while !s.load(std::sync::atomic::Ordering::Relaxed) {
                peak = peak.max(rss_bytes());
                std::thread::sleep(Duration::from_millis(5));
            }
            peak.max(rss_bytes())
        });
        Self {
            stop,
            handle: Some(handle),
            base,
        }
    }
    /// (base MB, peak MB)
    fn finish(mut self) -> (f64, f64) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let peak = self.handle.take().unwrap().join().unwrap();
        (self.base as f64 / 1e6, peak as f64 / 1e6)
    }
}

fn fd_count() -> usize {
    std::fs::read_dir("/dev/fd").map_or(0, |d| d.count())
}

fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    registry: ToolRegistry,
    jobs: JobRegistry,
}

fn env(tag: &str) -> Env {
    let dir = tmp(tag);
    let root = dir.path().canonicalize().unwrap().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let registry = ToolRegistry::default();
    let jobs = register_all(&registry, Workspace::new(&root));
    registry.set_spill_root(dir.path().canonicalize().unwrap().join("sessions"));
    Env {
        _dir: dir,
        root,
        registry,
        jobs,
    }
}

async fn call(
    registry: &ToolRegistry,
    name: &str,
    args: Value,
) -> rness_protocol::events::ToolResult {
    call_with(registry, name, args, &CancellationToken::new()).await
}

async fn call_with(
    registry: &ToolRegistry,
    name: &str,
    args: Value,
    cancel: &CancellationToken,
) -> rness_protocol::events::ToolResult {
    let mut r = registry
        .dispatch(
            &"s".into(),
            &[ToolCall {
                call: format!("c{}", ulid_like()),
                name: name.into(),
                args,
            }],
            4,
            cancel,
        )
        .await;
    r.remove(0)
}

fn ulid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn bash(cmd: &str) -> Value {
    json!({"command": cmd, "description": "wp2 e2e"})
}

fn job_id_in(text: &str) -> Option<String> {
    let at = text.find("job_id=\"")? + 8;
    Some(text[at..].split('"').next()?.to_string())
}

// ── 1. output floods through Bash ────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_bash_foreground_200mb_yes_is_tail_bounded_and_pageable() {
    let e = env("flood-yes");
    let mb: u64 = std::env::var("RNESS_BENCH_FLOOD_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let peak = RssPeak::start();
    let t = Instant::now();
    let r = call(
        &e.registry,
        "Bash",
        bash(&format!("yes | head -c {}", mb * 1_000_000)),
    )
    .await;
    let elapsed = t.elapsed();
    let (base, peak) = peak.finish();
    assert!(!r.is_error, "{}", r.output);
    // 64 KiB tail + headers; must never be the whole flood.
    assert!(
        r.output.len() < 70 * 1024,
        "inline output {} bytes",
        r.output.len()
    );
    assert!(
        r.output.contains("output truncated"),
        "{}",
        &r.output[..300]
    );
    assert!(r.output.ends_with("[exit code: 0]"));
    let id = job_id_in(&r.output).expect("artifact id");
    // Paging: first page, a middle page, end, beyond end, negative.
    let page = call(
        &e.registry,
        "job_output",
        json!({"job_id": id, "offset": 0}),
    )
    .await;
    assert!(!page.is_error, "{}", page.output);
    assert!(
        page.output.contains(&format!("of {}", mb * 1_000_000)),
        "{}",
        &page.output[page.output.len() - 200..]
    );
    let end = call(
        &e.registry,
        "job_output",
        json!({"job_id": id, "offset": mb * 1_000_000}),
    )
    .await;
    assert!(!end.is_error, "{}", end.output);
    let beyond = call(
        &e.registry,
        "job_output",
        json!({"job_id": id, "offset": mb * 1_000_000 + 1}),
    )
    .await;
    assert!(
        beyond.is_error && beyond.output.contains("exceeds"),
        "{}",
        beyond.output
    );
    let negative = call(
        &e.registry,
        "job_output",
        json!({"job_id": id, "offset": -5}),
    )
    .await;
    assert!(negative.is_error, "{}", negative.output);
    report(
        "flood_bash_yes",
        json!({"mb":mb,"elapsed_ms":elapsed.as_millis(),"inline_bytes":r.output.len(),
               "rss_base_mb":base,"rss_peak_mb":peak,"rss_delta_mb":peak-base,
               "throughput_mb_s": mb as f64 / elapsed.as_secs_f64()}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_bash_binary_urandom_is_lossy_utf8_and_bounded() {
    let e = env("flood-bin");
    let peak = RssPeak::start();
    let t = Instant::now();
    let r = call(&e.registry, "Bash", bash("head -c 50000000 /dev/urandom")).await;
    let (base, peak) = peak.finish();
    assert!(!r.is_error, "{}", &r.output[..200.min(r.output.len())]);
    assert!(r.output.len() < 70 * 1024 * 3, "inline {}", r.output.len());
    assert!(r.output.contains('\u{fffd}'));
    report(
        "flood_bash_urandom_50mb",
        json!({"elapsed_ms":t.elapsed().as_millis(),"inline_bytes":r.output.len(),"rss_delta_mb":peak-base}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_bash_10mb_single_line_and_1m_short_lines() {
    let e = env("flood-lines");
    let t = Instant::now();
    let r = call(
        &e.registry,
        "Bash",
        bash("python3 -c \"import sys; sys.stdout.write('x'*10_000_000)\""),
    )
    .await;
    assert!(!r.is_error);
    assert!(r.output.len() < 70 * 1024);
    let single = t.elapsed();
    let t = Instant::now();
    let r = call(
        &e.registry,
        "Bash",
        bash("awk 'BEGIN{for(i=1;i<=1000000;i++)print i}'"),
    )
    .await;
    assert!(!r.is_error);
    if !r.output.contains("1000000\n[exit code: 0]") {
        let tail: String = r
            .output
            .chars()
            .rev()
            .take(300)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let head: String = r.output.chars().take(300).collect();
        panic!(
            "seq tail missing; len={} head={head:?} tail={tail:?}",
            r.output.len()
        );
    }
    assert!(r.output.len() < 70 * 1024);
    report(
        "flood_bash_line_shapes",
        json!({"single_10mb_line_ms":single.as_millis(),"seq_1m_ms":t.elapsed().as_millis()}),
    );
}

/// A tool result over 50 KiB that the tool does not bound itself is spilled
/// to a 0600 file and the model sees a head/tail preview.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flood_grep_content_result_spills_to_private_file() {
    let e = env("spill");
    let line = format!("needle {}\n", "y".repeat(1500));
    std::fs::write(e.root.join("big.txt"), line.repeat(400)).unwrap();
    let r = call(
        &e.registry,
        "Grep",
        json!({"pattern":"needle","output_mode":"content"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output.contains("Full result:"),
        "not spilled: {} bytes",
        r.output.len()
    );
    assert!(r.output.len() < 8 * 1024);
    let path = r
        .output
        .split("Full result: ")
        .nth(1)
        .unwrap()
        .split(". Use Read")
        .next()
        .unwrap();
    let meta = std::fs::metadata(path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    // Read of the spill file itself is not re-spilled.
    let page = call(&e.registry, "Read", json!({"path": path, "limit": 5})).await;
    assert!(!page.output.contains("Full result:"));
    report(
        "spill_grep_content",
        json!({"spill_bytes":meta.len(),"inline_bytes":r.output.len()}),
    );
}

// ── 2. background jobs: floods, quota, retention, paging ─────────────

async fn wait_settled(jobs: &JobRegistry, id: &str, limit: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < limit {
        if !jobs.inspect("s", id).unwrap().job.running {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// A 100 MB background flood is fully retained on disk under the core
/// default (256 MiB per job); a 10 MB per-job quota cancels a bigger flood
/// with an explicit output error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_flood_unlimited_by_default_and_quota_cancels() {
    // (a) core default: unlimited, persistent (fsync per chunk!).
    let dir = tmp("jobs-flood");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let registry = ToolRegistry::default();
    let jobs = register_all(&registry, Workspace::new(&ws));
    jobs.enable_persistence(&dir.path().join("jobs")).unwrap();
    let t = Instant::now();
    let r = call(
        &registry,
        "Bash",
        json!({"command":"yes | head -c 100000000","description":"x","run_in_background":true}),
    )
    .await;
    let id = r.output.split_whitespace().last().unwrap().to_string();
    assert!(
        wait_settled(&jobs, &id, Duration::from_secs(120)).await,
        "job did not settle"
    );
    let persistent_ms = t.elapsed().as_millis();
    let disk: u64 = walk_size(&dir.path().join("jobs"));
    assert!(disk >= 100_000_000, "disk {disk}");
    let insp = jobs.inspect("s", &id).unwrap();
    assert_eq!(insp.output_bytes, 100_000_000);
    assert!(insp.job.output_error.is_none());

    // (b) quota: 10 MB per job.
    let registry = ToolRegistry::default();
    let jobs = register_all(&registry, Workspace::new(&ws));
    jobs.configure_retention(Retention {
        max_job_bytes: 10_000_000,
        max_total_bytes: 0,
        max_age_secs: 0,
        max_bash_captures: 0,
        cleanup_interval_secs: 60,
    })
    .unwrap();
    let r = call(
        &registry,
        "Bash",
        json!({"command":"yes","description":"x","run_in_background":true}),
    )
    .await;
    let id = r.output.split_whitespace().last().unwrap().to_string();
    assert!(
        wait_settled(&jobs, &id, Duration::from_secs(30)).await,
        "quota did not stop `yes`"
    );
    let insp = jobs.inspect("s", &id).unwrap();
    assert!(insp.output_bytes <= 10_000_000, "{}", insp.output_bytes);
    assert!(
        insp.job
            .output_error
            .as_deref()
            .unwrap_or("")
            .contains("quota"),
        "{:?}",
        insp.job.output_error
    );
    let out = call(&registry, "job_output", json!({"job_id": id})).await;
    assert!(
        out.output.contains("output error"),
        "{}",
        &out.output[out.output.len().saturating_sub(300)..]
    );
    // Foreground Bash under the same quota: error mentions the artifact.
    let fg = call(&registry, "Bash", bash("yes | head -c 30000000")).await;
    report(
        "jobs_flood",
        json!({"persistent_100mb_ms":persistent_ms,"disk_bytes":disk,
               "quota_status":format!("{:?}", insp.job.status),
               "foreground_over_quota_is_error":fg.is_error,
               "foreground_over_quota_output":fg.output.chars().take(200).collect::<String>()}),
    );
}

fn walk_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let m = e.metadata().unwrap();
            total += if m.is_dir() {
                walk_size(&e.path())
            } else {
                m.len()
            };
        }
    }
    total
}

/// 200 concurrent background jobs: fd cost, list cost, all killable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_200_concurrent_background() {
    let e = env("jobs200");
    let fds0 = fd_count();
    let t = Instant::now();
    let mut ids = Vec::new();
    for i in 0..200 {
        let r = call(&e.registry, "Bash", json!({"command":format!("echo start{i}; sleep 60"),"description":"x","run_in_background":true})).await;
        assert!(!r.is_error, "{}", r.output);
        ids.push(r.output.split_whitespace().last().unwrap().to_string());
    }
    let start_ms = t.elapsed().as_millis();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let fds = fd_count();
    let t = Instant::now();
    let list = call(&e.registry, "job_list", json!({})).await;
    let list_ms = t.elapsed().as_millis();
    let t = Instant::now();
    for id in &ids {
        let r = call(&e.registry, "job_kill", json!({"job_id": id})).await;
        assert!(!r.is_error, "{}", r.output);
    }
    for id in &ids {
        assert!(wait_settled(&e.jobs, id, Duration::from_secs(20)).await);
        assert_eq!(e.jobs.inspect("s", id).unwrap().job.status, "killed");
    }
    let kill_ms = t.elapsed().as_millis();
    // Killed jobs release their pipes and output files. Poll briefly: other
    // tests in this binary open fds concurrently, so take the lowest sample.
    let mut fds_after = usize::MAX;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && fds_after > fds0 + 10 {
        fds_after = fds_after.min(fd_count());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    report(
        "jobs_200",
        json!({"start_ms":start_ms,"fds_before":fds0,"fds_running":fds,"fds_per_job":(fds as f64 - fds0 as f64)/200.0,
               "fds_after_kill":fds_after,"list_ms":list_ms,"list_bytes":list.output.len(),"kill_all_ms":kill_ms}),
    );
    // Alone this is <= 10 (measured 0); the bound tolerates parallel tests
    // while still catching one leaked fd per killed job.
    assert!(
        fds_after < fds0 + 100,
        "fds still open after kill: {fds0} -> {fds_after}"
    );
}

// ── 3. cancellation ───────────────────────────────────────────────────

fn make_tree(root: &Path, files: usize, per_dir: usize, body: &str) {
    for i in 0..files {
        let dir = root.join(format!("d{:04}", i / per_dir));
        if i % per_dir == 0 {
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(dir.join(format!("f{i}.txt")), body).unwrap();
    }
}

/// Grep runs in `spawn_blocking` and ignores the cancel token: the
/// dispatcher abandons it after CANCEL_GRACE (2 s) but the walk keeps
/// burning CPU until it finishes on its own. (Plan §4 #6.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bug_grep_cancel_is_abandoned_but_keeps_burning_cpu() {
    let e = env("grep-cancel");
    let files: usize = std::env::var("RNESS_BENCH_GREP_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let t = Instant::now();
    make_tree(
        &e.root,
        files,
        1000,
        &"lorem ipsum dolor sit amet\n".repeat(40),
    );
    let build_ms = t.elapsed().as_millis();

    // Uncancelled baseline (also warms the page cache).
    let t = Instant::now();
    let full = call(&e.registry, "Grep", json!({"pattern":"zzz_never_matches"})).await;
    assert!(!full.is_error);
    let full_ms = t.elapsed().as_millis();

    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        c2.cancel();
    });
    let cpu0 = cpu_time();
    let t = Instant::now();
    let r = call_with(
        &e.registry,
        "Grep",
        json!({"pattern":"zzz_never_matches"}),
        &cancel,
    )
    .await;
    let settle_ms = t.elapsed().as_millis();
    // After settling, keep measuring CPU to see whether the abandoned walk
    // is still running.
    let mut burn_after = Vec::new();
    let mut quiet_at = None;
    let t2 = Instant::now();
    let mut last = cpu_time();
    while t2.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let now = cpu_time();
        let d = (now - last).as_millis();
        burn_after.push(d);
        last = now;
        if d < 30 {
            quiet_at = Some(t2.elapsed().as_millis());
            break;
        }
    }
    let total_cpu = (cpu_time() - cpu0).as_millis();
    report(
        "grep_cancel",
        json!({"files":files,"build_ms":build_ms,"uncancelled_grep_ms":full_ms,
               "cancel_at_ms":150,"settled_after_ms":settle_ms,"is_error":r.is_error,
               "output":r.output,"cpu_ms_per_250ms_after_settle":burn_after,
               "kept_burning_ms_after_settle":quiet_at,"total_cpu_ms":total_cpu}),
    );
    assert!(r.is_error);
    if full_ms > 2500 {
        // Tree big enough that the walk outlives the grace period.
        assert!(r.output.contains("abandoned"), "{}", r.output);
        assert!(settle_ms >= 2000 && settle_ms < 3000, "{settle_ms}");
        // The walk is I/O-bound (~40 ms CPU per 250 ms on this box); it keeps
        // going for seconds after the dispatcher reported "abandoned".
        assert!(
            burn_after.len() >= 3 && burn_after[0] >= 30,
            "abandoned walk should still burn CPU: {burn_after:?}"
        );
    }
}

/// Read of a FIFO blocks inside tokio::fs::read (a blocking-pool thread)
/// with no reader-side timeout: cancel abandons it after 2 s and the thread
/// stays stuck until a writer appears. (Uncancellable Read.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bug_read_fifo_blocks_and_is_abandoned_thread_leaks() {
    let e = env("fifo");
    let fifo = e.root.join("pipe");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let r = call_with(&e.registry, "Read", json!({"path":"pipe"}), &cancel).await;
    let settle = t.elapsed();
    assert!(r.is_error && r.output.contains("abandoned"), "{}", r.output);
    // Without cancel it would hang forever: no timeout on Read.
    let uncancelled = tokio::time::timeout(
        Duration::from_secs(3),
        call(&e.registry, "Read", json!({"path":"pipe"})),
    )
    .await;
    // Unblock the stuck threads so the test runtime can shut down.
    for _ in 0..4 {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags_nonblock()
            .open(&fifo);
    }
    report(
        "read_fifo",
        json!({"settled_after_ms":settle.as_millis(),"output":r.output,
               "uncancelled_read_still_blocked_after_3s":uncancelled.is_err()}),
    );
    assert!(uncancelled.is_err(), "Read of a FIFO returned on its own?");
}

trait NonBlock {
    fn custom_flags_nonblock(&mut self) -> &mut Self;
}
impl NonBlock for std::fs::OpenOptions {
    fn custom_flags_nonblock(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.custom_flags(libc::O_NONBLOCK)
    }
}

/// Foreground Bash honours cancel promptly and kills its process group
/// (including `&` children). A `setsid` daemon escapes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_cancel_kills_group_but_setsid_escapes() {
    let e = env("bash-cancel");
    let pidfile = e.root.join("pids");
    let cmd = format!(
        "sleep 1000 & echo $! > {p}; python3 -c 'import os,time; os.setsid(); open(\"{p}.daemon\",\"w\").write(str(os.getpid())); time.sleep(1000)' & sleep 1000",
        p = pidfile.display()
    );
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let r = call_with(&e.registry, "Bash", bash(&cmd), &cancel).await;
    let settle = t.elapsed();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let child: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let daemon: i32 = std::fs::read_to_string(pidfile.with_extension("daemon"))
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0);
    let child_alive = pid_alive(child);
    let daemon_alive = daemon > 0 && pid_alive(daemon);
    if daemon > 0 {
        unsafe { libc::kill(daemon, libc::SIGKILL) };
    }
    report(
        "bash_cancel",
        json!({"settled_after_ms":settle.as_millis(),"output":r.output,"bg_child_alive":child_alive,
               "setsid_daemon_alive":daemon_alive}),
    );
    assert!(r.is_error && r.output.contains("cancelled"));
    assert!(settle < Duration::from_millis(1500), "{settle:?}");
    assert!(
        !child_alive,
        "`&` child in the shell's group survived cancel"
    );
    assert!(
        daemon_alive,
        "setsid daemon is expected to escape (documented)"
    );
}

/// `cmd &` keeps stdout open, so a foreground Bash that starts a background
/// process waits for the pipe EOF, i.e. until the timeout, and then reports
/// a timeout error instead of the shell's output.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bug_bash_backgrounded_child_holds_pipe_until_timeout() {
    let e = env("bash-amp");
    let t = Instant::now();
    let r = call(
        &e.registry,
        "Bash",
        json!({"command":"sleep 30 & echo started","description":"x","timeout_ms":3000}),
    )
    .await;
    let elapsed = t.elapsed();
    // Redirected background is fine.
    let t = Instant::now();
    let ok = call(&e.registry, "Bash", json!({"command":"sleep 30 >/dev/null 2>&1 & echo started","description":"x","timeout_ms":3000})).await;
    let ok_elapsed = t.elapsed();
    report(
        "bash_amp_pipe",
        json!({"elapsed_ms":elapsed.as_millis(),"is_error":r.is_error,"output":r.output,
               "redirected_elapsed_ms":ok_elapsed.as_millis(),"redirected_output":ok.output}),
    );
    assert!(
        elapsed >= Duration::from_millis(2900),
        "returned early: {elapsed:?}"
    );
    assert!(r.is_error && r.output.contains("timed out"), "{}", r.output);
    assert!(ok_elapsed < Duration::from_secs(2));
}

/// timeout_ms edge values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bash_timeout_edges() {
    let e = env("bash-timeout");
    let mut rows = Vec::new();
    for t in [
        json!(0),
        json!(-1),
        json!(1),
        json!("100"),
        json!(1.5),
        json!(u64::MAX),
    ] {
        let started = Instant::now();
        let r = call(
            &e.registry,
            "Bash",
            json!({"command":"echo hi","description":"x","timeout_ms":t}),
        )
        .await;
        rows.push(json!({"timeout_ms":t,"ms":started.elapsed().as_millis(),"is_error":r.is_error,"out":r.output.chars().take(120).collect::<String>()}));
    }
    report("bash_timeout_edges", json!({"rows":rows}));
    // timeout_ms=0 means "time out immediately", not "no timeout".
    assert!(rows[0]["is_error"].as_bool().unwrap(), "{}", rows[0]);
}

/// terminal_send with wait_ms=60000 returns promptly on cancel; the command
/// keeps running in the terminal (by design).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_cancel_long_wait_and_flood() {
    let e = env("term");
    let terms = TerminalRegistry::new();
    register_terminal_tools(
        &e.registry,
        terms.clone(),
        Workspace::new(&e.root),
        Some(e.jobs.clone()),
    );
    let open = call(&e.registry, "terminal_open", json!({})).await;
    assert!(!open.is_error, "{}", open.output);
    let id = open.output.split_whitespace().nth(2).unwrap().to_string();

    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let r = call_with(
        &e.registry,
        "terminal_send",
        json!({"session_id":id,"text":"sleep 100","wait_ms":60000}),
        &cancel,
    )
    .await;
    let cancel_ms = t.elapsed().as_millis();
    let sig = call(
        &e.registry,
        "terminal_signal",
        json!({"session_id":id,"signal":"INT"}),
    )
    .await;

    // Flood: `yes` for 3 s; tail-bounded result, scrollback bounded.
    let cpu0 = cpu_time();
    let peak = RssPeak::start();
    let t = Instant::now();
    let flood = call(
        &e.registry,
        "terminal_send",
        json!({"session_id":id,"text":"yes","wait_ms":3000}),
    )
    .await;
    let flood_ms = t.elapsed().as_millis();
    let cpu_flood = (cpu_time() - cpu0).as_millis();
    let int = call(
        &e.registry,
        "terminal_signal",
        json!({"session_id":id,"signal":"INT"}),
    )
    .await;
    let (base, peak) = peak.finish();
    // Invalid UTF-8 and \r progress bars.
    let bin = call(&e.registry, "terminal_send", json!({"session_id":id,"text":"head -c 200000 /dev/urandom; echo; echo END","wait_ms":10000})).await;
    let bar = call(&e.registry, "terminal_send", json!({"session_id":id,"text":"for i in 1 2 3 4 5; do printf '\\rprogress %d%%' $((i*20)); sleep 0.05; done; echo","wait_ms":5000})).await;
    let read = call(&e.registry, "terminal_read", json!({"session_id":id})).await;
    let close = call(&e.registry, "terminal_close", json!({"session_id":id})).await;
    terms.close_all();
    report(
        "terminal",
        json!({"cancel_wait_returned_ms":cancel_ms,"cancel_output":r.output,"cancel_is_error":r.is_error,
               "signal":sig.output,"flood_ms":flood_ms,"flood_inline_bytes":flood.output.len(),
               "flood_status":flood.output.lines().last(),"flood_cpu_ms_in_test_process":cpu_flood,
               "flood_rss_delta_mb":peak-base,"int":int.output.lines().last(),
               "binary_ok":!bin.is_error && bin.output.contains("END"),"binary_bytes":bin.output.len(),
               "progress_output":bar.output,"read_bytes":read.output.len(),"close":close.output}),
    );
    assert!(cancel_ms < 1500, "{cancel_ms}");
    assert!(flood.output.len() < 70 * 1024 * 4);
    assert!(read.output.len() < 70 * 1024 * 4);
}

// ── 4. Edit / Write / Read matrix ─────────────────────────────────────

/// CRLF: Read strips `\r` (str::lines), so the model sees LF lines; an
/// old_string spanning lines (copied from Read) never matches.
#[tokio::test]
async fn bug_edit_crlf_multiline_old_string_from_read_never_matches() {
    let e = env("crlf");
    std::fs::write(e.root.join("w.txt"), "alpha\r\nbeta\r\ngamma\r\n").unwrap();
    let read = call(&e.registry, "Read", json!({"path":"w.txt"})).await;
    assert!(
        !read.output.contains('\r'),
        "Read shows CR? {:?}",
        read.output
    );
    let multi = call(
        &e.registry,
        "Edit",
        json!({"path":"w.txt","old_string":"alpha\nbeta","new_string":"ALPHA\nBETA"}),
    )
    .await;
    let single = call(
        &e.registry,
        "Edit",
        json!({"path":"w.txt","old_string":"gamma","new_string":"GAMMA"}),
    )
    .await;
    let after = std::fs::read(e.root.join("w.txt")).unwrap();
    report(
        "edit_crlf",
        json!({"multiline_is_error":multi.is_error,"multiline_output":multi.output,"single_is_error":single.is_error,"after":String::from_utf8_lossy(&after)}),
    );
    assert!(multi.is_error && multi.output.contains("not found"));
    assert!(!single.is_error);
    assert_eq!(after, b"alpha\r\nbeta\r\nGAMMA\r\n");
}

#[tokio::test]
async fn edit_matrix_bom_invalid_utf8_empty_long_lines_paths() {
    let e = env("matrix");
    let r = &e.registry;
    let mut rows = serde_json::Map::new();
    // UTF-8 BOM: Read shows it in line 1; edit inside the line works.
    std::fs::write(e.root.join("bom.txt"), "\u{feff}first\nsecond\n").unwrap();
    let read = call(r, "Read", json!({"path":"bom.txt"})).await;
    let ed = call(
        r,
        "Edit",
        json!({"path":"bom.txt","old_string":"first","new_string":"FIRST"}),
    )
    .await;
    assert!(!ed.is_error);
    assert!(std::fs::read(e.root.join("bom.txt"))
        .unwrap()
        .starts_with(&[0xef, 0xbb, 0xbf]));
    rows.insert(
        "bom_read_line1_has_bom".into(),
        json!(read.output.contains('\u{feff}')),
    );

    // Invalid UTF-8: Read fails => file can never be marked seen => Write
    // over it is refused forever (must use Bash).
    std::fs::write(e.root.join("bin.dat"), [0xffu8, 0xfe, b'a', b'\n']).unwrap();
    let read = call(r, "Read", json!({"path":"bin.dat"})).await;
    let w = call(r, "Write", json!({"path":"bin.dat","content":"text"})).await;
    let ed = call(
        r,
        "Edit",
        json!({"path":"bin.dat","old_string":"a","new_string":"b"}),
    )
    .await;
    rows.insert(
        "invalid_utf8".into(),
        json!({"read":read.output,"write_err":w.is_error,"write":w.output,"edit":ed.output}),
    );
    assert!(read.is_error && w.is_error && ed.is_error);

    // Empty file: Read, then Edit (nothing to match), then Write works.
    std::fs::write(e.root.join("empty.txt"), "").unwrap();
    let read = call(r, "Read", json!({"path":"empty.txt"})).await;
    let ed = call(
        r,
        "Edit",
        json!({"path":"empty.txt","old_string":"x","new_string":"y"}),
    )
    .await;
    let w = call(r, "Write", json!({"path":"empty.txt","content":"now\n"})).await;
    rows.insert(
        "empty".into(),
        json!({"read":read.output,"edit":ed.output,"write_ok":!w.is_error}),
    );
    assert!(!w.is_error);

    // Write over an unread existing file is refused.
    std::fs::write(e.root.join("unread.txt"), "data").unwrap();
    let w = call(r, "Write", json!({"path":"unread.txt","content":"clobber"})).await;
    assert!(w.is_error && w.output.contains("has not been read"));

    // Edit with a non-existent parent dir; Write creates parents.
    let ed = call(
        r,
        "Edit",
        json!({"path":"no/such/dir/f.txt","old_string":"a","new_string":"b"}),
    )
    .await;
    let w = call(
        r,
        "Write",
        json!({"path":"new/deep/dir/f.txt","content":"x"}),
    )
    .await;
    rows.insert(
        "missing_parent".into(),
        json!({"edit":ed.output,"write_ok":!w.is_error}),
    );
    assert!(ed.is_error && !w.is_error);

    // Path with `..` in danger-full-access: escapes the workspace freely.
    let w = call(
        r,
        "Write",
        json!({"path":"../outside-wp2.txt","content":"x"}),
    )
    .await;
    rows.insert("dotdot_write_full_access_ok".into(), json!(!w.is_error));

    // >2000-byte lines: truncated to 2000 bytes, no marker in the text.
    let long = format!("{}TAILMARK\n", "z".repeat(5000));
    std::fs::write(e.root.join("long.txt"), long.repeat(3)).unwrap();
    let read = call(r, "Read", json!({"path":"long.txt"})).await;
    rows.insert("long_lines".into(), json!({"bytes":read.output.len(),"shows_tail":read.output.contains("TAILMARK"),"has_cut_marker":read.output.contains('…')}));
    assert!(!read.output.contains("TAILMARK"));

    // 2000+ line paging.
    let body: String = (1..=4500).map(|i| format!("line {i}\n")).collect();
    std::fs::write(e.root.join("many.txt"), body).unwrap();
    let p1 = call(r, "Read", json!({"path":"many.txt"})).await;
    assert!(p1.output.contains("continue with offset=2001"));
    let p3 = call(r, "Read", json!({"path":"many.txt","offset":4001})).await;
    assert!(p3.output.contains("line 4500") && !p3.output.contains("more lines"));
    let past = call(r, "Read", json!({"path":"many.txt","offset":4501})).await;
    assert!(past.is_error);
    let zero = call(r, "Read", json!({"path":"many.txt","offset":0})).await;
    assert!(zero.is_error);

    // 256 KiB byte cap with 2000-byte lines.
    let wide = format!("{}\n", "w".repeat(1999));
    std::fs::write(e.root.join("wide.txt"), wide.repeat(2000)).unwrap();
    let read = call(r, "Read", json!({"path":"wide.txt"})).await;
    rows.insert("wide_read_bytes".into(), json!(read.output.len()));
    assert!(read.output.len() <= 256 * 1024 + 200);
    report("edit_matrix", Value::Object(rows));
}

/// Freshness is mtime+len: same-length content with the mtime restored
/// bypasses the "changed on disk" check (design limit — document).
#[tokio::test]
async fn edit_freshness_bypass_same_mtime_and_len() {
    let e = env("fresh");
    let p = e.root.join("f.txt");
    std::fs::write(&p, "value=1111\nkeep\n").unwrap();
    let r = &e.registry;
    call(r, "Read", json!({"path":"f.txt"})).await;
    let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::fs::write(&p, "value=2222\nkeep\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let ed = call(
        r,
        "Edit",
        json!({"path":"f.txt","old_string":"keep","new_string":"KEEP"}),
    )
    .await;
    let after = std::fs::read_to_string(&p).unwrap();
    report(
        "freshness_bypass",
        json!({"edit_ok":!ed.is_error,"after":after}),
    );
    assert!(!ed.is_error, "bypass no longer possible: {}", ed.output);
    assert!(after.contains("2222"));
}

/// Symlink pointing outside, hard link, and `..` in workspace-write mode.
#[tokio::test]
async fn sandbox_workspace_write_symlink_hardlink_dotdot() {
    let e = env("sbx");
    let outside = tmp("sbx-out");
    let out_file = outside.path().join("target.txt");
    std::fs::write(&out_file, "outside\n").unwrap();
    std::os::unix::fs::symlink(&out_file, e.root.join("link.txt")).unwrap();
    std::fs::write(e.root.join("orig.txt"), "orig\n").unwrap();
    std::fs::hard_link(e.root.join("orig.txt"), e.root.join("hard.txt")).unwrap();
    let mut rows = serde_json::Map::new();
    for mode in [
        rness_protocol::sandbox::SandboxMode::WorkspaceWrite,
        rness_protocol::sandbox::SandboxMode::ReadOnly,
        rness_protocol::sandbox::SandboxMode::DangerFullAccess,
    ] {
        let bound = e
            .registry
            .for_workspace_with_policy(&"s".into(), &e.root, mode);
        let read_link = call(&bound, "Read", json!({"path":"link.txt"})).await;
        let edit_link = call(
            &bound,
            "Edit",
            json!({"path":"link.txt","old_string":"outside","new_string":"pwned"}),
        )
        .await;
        call(&bound, "Read", json!({"path":"hard.txt"})).await;
        let edit_hard = call(
            &bound,
            "Edit",
            json!({"path":"hard.txt","old_string":"orig","new_string":"changed"}),
        )
        .await;
        let dotdot = call(
            &bound,
            "Write",
            json!({"path":format!("sub/../../{}", "escape-wp2.txt"),"content":"x"}),
        )
        .await;
        rows.insert(
            format!("{mode:?}"),
            json!({"read_outside_symlink_ok":!read_link.is_error,"edit_outside_symlink_denied":edit_link.is_error,
                   "edit_hardlink_denied":edit_hard.is_error,"dotdot_write_denied":dotdot.is_error,
                   "edit_link_msg":edit_link.output}),
        );
        std::fs::write(&out_file, "outside\n").unwrap();
        std::fs::write(e.root.join("orig.txt"), "orig\n").unwrap();
    }
    report("sandbox_paths", Value::Object(rows.clone()));
    let ww = &rows["WorkspaceWrite"];
    assert_eq!(ww["edit_outside_symlink_denied"], true);
    assert_eq!(ww["edit_hardlink_denied"], true);
    assert_eq!(ww["dotdot_write_denied"], true);
}

// ── 5. grep / glob edge trees ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_glob_edge_trees() {
    let e = env("edge");
    let r = &e.registry;
    let mut rows = serde_json::Map::new();
    // Deep nesting (depth 200).
    let mut deep = e.root.join("deep");
    for i in 0..200 {
        deep = deep.join(format!("n{i}"));
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("bottom.txt"), "deepneedle\n").unwrap();
    // Unicode, newline and space in names.
    std::fs::write(e.root.join("ünï cødé.txt"), "uni needle\n").unwrap();
    std::fs::write(e.root.join("new\nline.txt"), "nlneedle\n").unwrap();
    // Binary file with a match after a NUL.
    let mut bin = vec![0u8; 1000];
    bin.extend_from_slice(b"\nbinneedle\n");
    std::fs::write(e.root.join("blob.bin"), bin).unwrap();
    // Symlink loop and broken link.
    std::os::unix::fs::symlink(&e.root, e.root.join("loop")).unwrap();
    std::os::unix::fs::symlink(e.root.join("nope"), e.root.join("broken")).unwrap();
    // .git dir and .gitignore.
    std::fs::create_dir_all(e.root.join(".git/objects")).unwrap();
    std::fs::write(e.root.join(".git/objects/x"), "gitneedle\n").unwrap();
    std::fs::write(e.root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(e.root.join("ignored.txt"), "ignneedle\n").unwrap();
    // 50 MB minified single line with a match at the end.
    let mut min = "var a=1;".repeat(50_000_000 / 8);
    min.push_str("minneedle\n");
    std::fs::write(e.root.join("app.min.js"), min).unwrap();

    let t = Instant::now();
    let g = call(
        r,
        "Grep",
        json!({"pattern":"needle","output_mode":"content"}),
    )
    .await;
    rows.insert("grep_ms".into(), json!(t.elapsed().as_millis()));
    for (k, needle) in [
        ("deep", "deepneedle"),
        ("unicode", "ünï cødé.txt"),
        ("newline_name", "nlneedle"),
        ("binary", "binneedle"),
        ("dot_git", "gitneedle"),
        ("gitignored", "ignneedle"),
        ("minified", "minneedle"),
    ] {
        rows.insert(format!("grep_finds_{k}"), json!(g.output.contains(needle)));
    }
    rows.insert("grep_inline_bytes".into(), json!(g.output.len()));
    // A match past byte 2000 of a long line is found but invisible in content mode.
    let f = call(r, "Grep", json!({"pattern":"minneedle"})).await;
    rows.insert(
        "grep_files_finds_minified".into(),
        json!(f.output.contains("app.min.js")),
    );
    let gl = call(r, "Glob", json!({"pattern":"**/*.txt"})).await;
    rows.insert(
        "glob_finds_deep".into(),
        json!(gl.output.contains("bottom.txt")),
    );
    rows.insert(
        "glob_lists_dot_git_objects".into(),
        json!(call(r, "Glob", json!({"pattern":".git/**"}))
            .await
            .output
            .contains("objects")),
    );
    rows.insert(
        "glob_output_has_raw_newline_name".into(),
        json!(gl.output.contains("new\nline.txt")),
    );
    report("grep_glob_edges", Value::Object(rows.clone()));
    assert_eq!(rows["grep_finds_deep"], true);
    assert_eq!(rows["grep_files_finds_minified"], true);
    assert_eq!(
        rows["grep_finds_minified"], false,
        "content preview now shows the match past 2000 bytes"
    );
    assert_eq!(rows["grep_finds_gitignored"], false);
}

// ── 6. terminal sweeps & retirement ───────────────────────────────────

/// MAX_RETIRED = 64: the 65th retirement clears the whole map, so the
/// reason for earlier closes is forgotten (minor).
#[test]
fn terminal_retired_reasons_reset_after_64() {
    let terms = TerminalRegistry::new();
    let dir = tmp("retired");
    let mut first = None;
    let mut ids = Vec::new();
    for i in 0..65 {
        let owner = format!("o{i}");
        let opened = terms
            .open(
                None,
                rness_tools::terminal::ShellChoice::Controlled,
                dir.path().to_path_buf(),
                owner.clone(),
            )
            .unwrap();
        terms.close_owned_by(&owner, "owner finished");
        if i == 0 {
            first = Some((opened.id.clone(), owner.clone()));
        }
        ids.push((opened.id, owner));
    }
    let (id, owner) = first.unwrap();
    let err_first = terms.read(&id, None, &owner).unwrap_err();
    let (id64, owner64) = ids.last().unwrap();
    let err_last = terms.read(id64, None, owner64).unwrap_err();
    report(
        "terminal_retired",
        json!({"first":err_first,"last":err_last}),
    );
    assert!(err_last.contains("owner finished"));
}

/// Recovery locks every stale owner directory it visits and keeps the lock
/// (and the fd) for the life of the process; empty directories are never
/// removed. N past runs => N open fds in every new rness process.
#[test]
fn bug_job_recovery_holds_one_fd_per_stale_dir_forever() {
    let dir = tmp("recover-fds");
    let jobs_root = dir.path().join("jobs");
    let runs = 300;
    for _ in 0..runs {
        let jobs = JobRegistry::new();
        jobs.enable_persistence(&jobs_root).unwrap();
        jobs.wait_recovery();
        drop(jobs);
    }
    let fds0 = fd_count();
    let t = Instant::now();
    let jobs = JobRegistry::new();
    jobs.enable_persistence(&jobs_root).unwrap();
    jobs.wait_recovery();
    let ms = t.elapsed().as_millis();
    let fds = fd_count();
    let dirs = std::fs::read_dir(&jobs_root).unwrap().count();
    report(
        "job_recovery_fds",
        json!({"stale_dirs":dirs,"recovery_ms":ms,"fds_before":fds0,"fds_after":fds,"fds_held":fds - fds0}),
    );
    assert!(dirs > runs, "empty owner dirs are never removed");
    assert!(
        fds - fds0 >= runs,
        "expected one held lock fd per stale dir"
    );
}

/// Recovered (settled, delivered) records beyond max_age are removed by
/// cleanup; recovered tails are kept in RAM until then.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_recovery_after_simulated_crash_and_offset_paging() {
    let dir = tmp("recover");
    let jobs_root = dir.path().join("jobs");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let id;
    {
        let registry = ToolRegistry::default();
        let jobs = register_all(&registry, Workspace::new(&ws));
        jobs.enable_persistence(&jobs_root).unwrap();
        let r = call(
            &registry,
            "Bash",
            json!({"command":"seq 1 300000; sleep 30","description":"x","run_in_background":true}),
        )
        .await;
        id = r.output.split_whitespace().last().unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        // "crash": leak the registry (no settle, lock released only by drop
        // of the file handle) — emulate by forgetting the kill and dropping.
        call(&registry, "job_kill", json!({"job_id":id})).await;
        assert!(wait_settled(&jobs, &id, Duration::from_secs(10)).await);
    }
    let registry = ToolRegistry::default();
    let jobs = register_all(&registry, Workspace::new(&ws));
    jobs.enable_persistence(&jobs_root).unwrap();
    jobs.wait_recovery();
    let tool = JobOutputTool::new(jobs.clone());
    let first = tool
        .execute_in(&"s".into(), json!({"job_id":id,"offset":0}))
        .await;
    report(
        "job_recovery",
        json!({"recovered":first.is_ok(),"first":first.as_ref().map(|s| s.chars().take(80).collect::<String>()).unwrap_or_else(|e| e.clone())}),
    );
    let _ = JobStatus::Running;
}

/// Plan §4 #11 (fixed by P2 phase 2): every foreground Bash call creates a
/// `bash-output` capture. Retention is bounded by default (200 captures,
/// tails of settled+delivered jobs leave RAM) and small ephemeral captures
/// need no spool fd, so RAM, fds and records stay flat across many calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_bounded_ram_and_fds_per_bash_call() {
    let n: usize = std::env::var("RNESS_BENCH_BASH_CALLS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    let mut rows = serde_json::Map::new();
    for persistent in [false, true] {
        let dir = tmp("retention");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let registry = ToolRegistry::default();
        let jobs = register_all(&registry, Workspace::new(&ws));
        if persistent {
            jobs.enable_persistence(&dir.path().join("jobs")).unwrap();
        }
        let fds0 = fd_count();
        let rss0 = rss_bytes();
        let t = Instant::now();
        let mut rss_half = 0;
        for i in 0..n {
            if i == n / 2 {
                rss_half = rss_bytes();
            }
            let r = call(
                &registry,
                "Bash",
                bash("head -c 60000 /dev/zero | tr '\\0' a"),
            )
            .await;
            assert!(!r.is_error);
        }
        let ms = t.elapsed().as_millis();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let retained = jobs.list("s").len();
        let swept = jobs.cleanup().unwrap_or(0);
        // Concurrent tests open fds briefly; the lowest of a few samples
        // measures what this registry keeps.
        let mut fd_growth = i64::MAX;
        for _ in 0..5 {
            fd_growth = fd_growth.min(fd_count() as i64 - fds0 as i64);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let records = count_ext(&dir.path().join("jobs"), "json");
        rows.insert(
            if persistent { "persistent" } else { "ephemeral" }.into(),
            json!({"calls":n,"ms":ms,"ms_per_call":ms as f64 / n as f64,"fd_growth":fd_growth,
                   "rss_growth_mb":(rss_bytes() as f64 - rss0 as f64)/1e6,
                   "rss_growth_second_half_mb":(rss_bytes() as f64 - rss_half as f64)/1e6,
                   "listed":retained,"cleanup_removed":swept,"records":records,
                   "resident_output_mb":jobs.resident_output_bytes() as f64 / 1e6,"disk_bytes":walk_size(dir.path())}),
        );
    }
    report("retention_bounded", Value::Object(rows.clone()));
    for row in ["ephemeral", "persistent"] {
        let row = &rows[row];
        assert!(row["fd_growth"].as_i64().unwrap() <= 10, "fds leak: {row}");
        // RSS is reported but too noisy to assert while other tests run in
        // parallel; RAM held by the registry is measured exactly instead.
        // Ephemeral captures (<= 64 KiB) stay in memory up to the 200 cap.
        let resident = row["resident_output_mb"].as_f64().unwrap();
        let cap_mb = if row == &rows["ephemeral"] {
            200.0 * 0.06
        } else {
            0.0
        };
        assert!(resident <= cap_mb + 0.5, "RAM grows per call: {row}");
        assert!(row["listed"].as_u64().unwrap() <= 200, "{row}");
    }
    assert!(
        rows["persistent"]["records"].as_u64().unwrap() <= 200,
        "captures not evicted: {}",
        rows["persistent"]
    );
    assert!(
        rows["persistent"]["ms_per_call"].as_f64().unwrap() < 25.0,
        "{}",
        rows["persistent"]
    );
}

/// Files with extension `ext` anywhere under `p`.
fn count_ext(p: &Path, ext: &str) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() {
                count_ext(&path, ext)
            } else {
                u64::from(path.extension().is_some_and(|x| x == ext))
            }
        })
        .sum()
}
