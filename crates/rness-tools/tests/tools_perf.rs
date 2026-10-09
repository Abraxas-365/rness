//! WP-2 perf probes for the built-in tools. All `#[ignore]`; run with
//! `cargo test --release -p rness-tools --test tools_perf -- --ignored --nocapture --test-threads=1`.
//! Each probe prints `{"probe":..,"n":..,"median_ms":..,...}` JSON lines.
//! Sizes: RNESS_BENCH_FILES (comma list, default "10000,100000"),
//! RNESS_BENCH_READ_MB (default 512), RNESS_BENCH_FLOOD_MB (default 100),
//! RNESS_BENCH_SPILL_FILES (default 10000), RNESS_BENCH_JOB_RECORDS (default 20000),
//! RNESS_BENCH_ITERS (default 3).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rness_engine::tools::{ToolCall, ToolRegistry};
use rness_tools::jobs::JobRegistry;
use rness_tools::terminal::{register_terminal_tools, TerminalRegistry};
use rness_tools::{register_all, Workspace};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
fn iters() -> usize {
    env_num("RNESS_BENCH_ITERS", 3) as usize
}

fn tmp(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rness-e2e-wp2-perf-{tag}-"))
        .tempdir()
        .unwrap()
}

fn emit(probe: &str, n: u64, samples: &mut [f64], extra: Value) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    let mut v =
        json!({"probe":probe,"n":n,"median_ms":(median*10.0).round()/10.0,"samples_ms":samples});
    if let Value::Object(m) = extra {
        for (k, x) in m {
            v[k] = x;
        }
    }
    println!("{v}");
}

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
        .map_or(0, |p| p * 4096)
}
fn cpu_time() -> Duration {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

struct Peak {
    stop: Arc<std::sync::atomic::AtomicBool>,
    h: std::thread::JoinHandle<u64>,
    base: u64,
}
fn peak_start() -> Peak {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = stop.clone();
    let base = rss_bytes();
    let h = std::thread::spawn(move || {
        let mut p = 0;
        while !s.load(std::sync::atomic::Ordering::Relaxed) {
            p = p.max(rss_bytes());
            std::thread::sleep(Duration::from_millis(3));
        }
        p.max(rss_bytes())
    });
    Peak { stop, h, base }
}
/// Peak RSS growth over the base, MB.
fn peak_end(p: Peak) -> f64 {
    p.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let peak = p.h.join().unwrap();
    (peak.saturating_sub(p.base)) as f64 / 1e6
}

async fn call(r: &ToolRegistry, name: &str, args: Value) -> rness_protocol::events::ToolResult {
    r.dispatch(
        &"s".into(),
        &[ToolCall {
            call: "c".into(),
            name: name.into(),
            args,
        }],
        4,
        &CancellationToken::new(),
    )
    .await
    .remove(0)
}

fn make_tree(root: &Path, files: usize, per_dir: usize) {
    let body = "lorem ipsum dolor sit amet\n".repeat(40);
    for i in 0..files {
        let dir = root.join(format!("d{:04}", i / per_dir));
        if i % per_dir == 0 {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let ext = if i % 10 == 0 { "rs" } else { "txt" };
        std::fs::write(dir.join(format!("f{i}.{ext}")), &body).unwrap();
    }
}

fn registry(root: &Path) -> (ToolRegistry, JobRegistry) {
    let r = ToolRegistry::default();
    let j = register_all(&r, Workspace::new(root));
    (r, j)
}

/// Grep (no match / many matches) and Glob (narrow / broad / no match) at
/// several tree sizes: wall time and RSS growth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn perf_grep_glob_scale() {
    let sizes: Vec<usize> = std::env::var("RNESS_BENCH_FILES")
        .unwrap_or_else(|_| "10000,100000".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    for n in sizes {
        let dir = tmp("scale");
        let root = dir.path().canonicalize().unwrap();
        let t = Instant::now();
        make_tree(&root, n, 1000);
        let build = t.elapsed().as_millis();
        let (r, _) = registry(&root);
        call(&r, "Glob", json!({"pattern":"**/*.zz"})).await; // warm cache
        let cases: Vec<(&str, &str, Value)> = vec![
            ("grep_nomatch", "Grep", json!({"pattern":"zzz_never"})),
            ("grep_files_all_match", "Grep", json!({"pattern":"lorem"})),
            (
                "grep_content_all_match",
                "Grep",
                json!({"pattern":"lorem","output_mode":"content"}),
            ),
            (
                "grep_count_all_match",
                "Grep",
                json!({"pattern":"lorem","output_mode":"count"}),
            ),
            ("glob_nomatch", "Glob", json!({"pattern":"**/*.zz"})),
            ("glob_10pct", "Glob", json!({"pattern":"**/*.rs"})),
            ("glob_all", "Glob", json!({"pattern":"**/*"})),
        ];
        for (probe, tool, args) in cases {
            let mut samples = Vec::new();
            let mut rss = 0f64;
            let mut cpu = 0f64;
            let mut out_len = 0;
            for _ in 0..iters() {
                let p = peak_start();
                let c0 = cpu_time();
                let t = Instant::now();
                let res = call(&r, tool, args.clone()).await;
                samples.push(t.elapsed().as_secs_f64() * 1000.0);
                cpu = cpu.max((cpu_time() - c0).as_secs_f64() * 1000.0);
                rss = rss.max(peak_end(p));
                assert!(!res.is_error, "{}", res.output);
                out_len = res.output.len();
            }
            emit(
                &format!("{probe}"),
                n as u64,
                &mut samples,
                json!({"rss_peak_delta_mb":rss,"cpu_ms":cpu,"out_bytes":out_len,"tree_build_ms":build}),
            );
        }
    }
}

/// One file with 1M matching lines: Grep's per-file hit vector is not
/// capped before the 250-line output cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn perf_grep_massive_matches_single_file() {
    let dir = tmp("massive");
    let root = dir.path().canonicalize().unwrap();
    let lines = env_num("RNESS_BENCH_GREP_LINES", 1_000_000);
    let line = format!("match {}\n", "q".repeat(190));
    let mut f = std::io::BufWriter::new(std::fs::File::create(root.join("m.txt")).unwrap());
    for _ in 0..lines {
        std::io::Write::write_all(&mut f, line.as_bytes()).unwrap();
    }
    drop(f);
    let (r, _) = registry(&root);
    for mode in ["content", "files_with_matches"] {
        let mut s = Vec::new();
        let mut rss = 0f64;
        for _ in 0..iters() {
            let p = peak_start();
            let t = Instant::now();
            let res = call(&r, "Grep", json!({"pattern":"match","output_mode":mode})).await;
            s.push(t.elapsed().as_secs_f64() * 1000.0);
            rss = rss.max(peak_end(p));
            assert!(!res.is_error);
        }
        emit(
            &format!("grep_massive_{mode}"),
            lines,
            &mut s,
            json!({"file_mb":lines*200/1_000_000,"rss_peak_delta_mb":rss}),
        );
    }
}

/// Read loads the whole file before applying limits (plan §4 #12).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn perf_read_large_file_no_size_cap() {
    let dir = tmp("readbig");
    let root = dir.path().canonicalize().unwrap();
    let mb = env_num("RNESS_BENCH_READ_MB", 512);
    let chunk = "0123456789abcdef".repeat(4).to_string() + "\n"; // 65 bytes
    let block = chunk.repeat(16_000);
    let mut f = std::io::BufWriter::new(std::fs::File::create(root.join("big.log")).unwrap());
    let mut written = 0u64;
    while written < mb * 1_000_000 {
        std::io::Write::write_all(&mut f, block.as_bytes()).unwrap();
        written += block.len() as u64;
    }
    drop(f);
    let (r, _) = registry(&root);
    for (probe, args) in [
        ("read_big_default", json!({"path":"big.log"})),
        ("read_big_limit1", json!({"path":"big.log","limit":1})),
        (
            "read_big_tail",
            json!({"path":"big.log","offset": written / 65 - 5}),
        ),
    ] {
        let mut s = Vec::new();
        let mut rss = 0f64;
        let mut out = 0;
        for _ in 0..iters() {
            let p = peak_start();
            let t = Instant::now();
            let res = call(&r, "Read", args.clone()).await;
            s.push(t.elapsed().as_secs_f64() * 1000.0);
            rss = rss.max(peak_end(p));
            assert!(!res.is_error, "{}", res.output);
            out = res.output.len();
        }
        emit(
            probe,
            mb,
            &mut s,
            json!({"file_mb":written/1_000_000,"rss_peak_delta_mb":rss,"out_bytes":out}),
        );
    }
}

/// 20 concurrency-safe Reads in one batch vs 20 single-call dispatches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn perf_batch_reads_vs_sequential() {
    let dir = tmp("batch");
    let root = dir.path().canonicalize().unwrap();
    for i in 0..20 {
        let body: String = (0..20_000)
            .map(|l| format!("file {i} line {l} some text here\n"))
            .collect();
        std::fs::write(root.join(format!("r{i}.txt")), body).unwrap();
    }
    let (r, _) = registry(&root);
    let calls: Vec<ToolCall> = (0..20)
        .map(|i| ToolCall {
            call: format!("c{i}"),
            name: "Read".into(),
            args: json!({"path":format!("r{i}.txt")}),
        })
        .collect();
    for (probe, conc) in [
        ("reads20_batch_conc10", 10usize),
        ("reads20_batch_conc1", 1),
    ] {
        let mut s = Vec::new();
        for _ in 0..iters().max(5) {
            let t = Instant::now();
            let res = r
                .dispatch(&"s".into(), &calls, conc, &CancellationToken::new())
                .await;
            s.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(res.iter().all(|x| !x.is_error));
        }
        emit(probe, 20, &mut s, json!({}));
    }
    let mut s = Vec::new();
    for _ in 0..iters().max(5) {
        let t = Instant::now();
        for c in &calls {
            let res = r
                .dispatch(
                    &"s".into(),
                    std::slice::from_ref(c),
                    10,
                    &CancellationToken::new(),
                )
                .await;
            assert!(!res[0].is_error);
        }
        s.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    emit("reads20_sequential_dispatch", 20, &mut s, json!({}));
}

/// Background job output throughput: in-memory spool vs persistent
/// (sync_data per 8 KiB chunk), with `yes | head -c N`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn perf_job_append_fsync() {
    let mb = env_num("RNESS_BENCH_FLOOD_MB", 100);
    for persistent in [false, true] {
        let mut s = Vec::new();
        let mut cpu = 0f64;
        for _ in 0..iters() {
            let dir = tmp("append");
            let ws = dir.path().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let (r, jobs) = registry(&ws);
            if persistent {
                jobs.enable_persistence(&dir.path().join("jobs")).unwrap();
            }
            let c0 = cpu_time();
            let t = Instant::now();
            let res = call(&r, "Bash", json!({"command":format!("yes | head -c {}", mb*1_000_000),"description":"x","run_in_background":true})).await;
            let id = res.output.split_whitespace().last().unwrap().to_string();
            while jobs.inspect("s", &id).unwrap().job.running {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            s.push(t.elapsed().as_secs_f64() * 1000.0);
            cpu = cpu.max((cpu_time() - c0).as_secs_f64() * 1000.0);
        }
        let med = {
            let mut c = s.clone();
            c.sort_by(|a, b| a.partial_cmp(b).unwrap());
            c[c.len() / 2]
        };
        emit(
            if persistent {
                "job_flood_persistent"
            } else {
                "job_flood_spool"
            },
            mb,
            &mut s,
            json!({"mb_per_s": mb as f64 / (med/1000.0), "cpu_ms": cpu}),
        );
    }
    // Same flood through foreground Bash (retain_tail + capture artifact).
    for persistent in [false, true] {
        let mut s = Vec::new();
        for _ in 0..iters() {
            let dir = tmp("append-fg");
            let ws = dir.path().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let (r, jobs) = registry(&ws);
            if persistent {
                jobs.enable_persistence(&dir.path().join("jobs")).unwrap();
            }
            let t = Instant::now();
            let res = call(
                &r,
                "Bash",
                json!({"command":format!("yes | head -c {}", mb*1_000_000),"description":"x"}),
            )
            .await;
            assert!(!res.is_error);
            s.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        emit(
            if persistent {
                "bash_fg_flood_persistent"
            } else {
                "bash_fg_flood_spool"
            },
            mb,
            &mut s,
            json!({}),
        );
    }
    // Raw pipe baseline.
    let mut s = Vec::new();
    for _ in 0..iters() {
        let t = Instant::now();
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("yes | head -c {} > /dev/null", mb * 1_000_000))
            .status()
            .unwrap();
        s.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    emit("raw_pipe_baseline", mb, &mut s, json!({}));
}

/// sweep_spill_files over N sessions x 1 spill file each, half expired.
#[test]
#[ignore]
fn perf_spill_sweep() {
    let n = env_num("RNESS_BENCH_SPILL_FILES", 10_000);
    let mut s = Vec::new();
    let mut removed = 0;
    for _ in 0..iters() {
        let dir = tmp("sweep");
        for i in 0..n {
            let sp = dir.path().join(format!("01SESSION{i:08}")).join("spill");
            std::fs::create_dir_all(&sp).unwrap();
            std::fs::write(sp.parent().unwrap().join("session.v1.jsonl"), "").unwrap();
            let f = sp.join("c-Bash.txt");
            std::fs::write(&f, "x").unwrap();
            if i % 2 == 0 {
                let old = std::time::SystemTime::now() - Duration::from_secs(30 * 86400);
                std::fs::File::options()
                    .write(true)
                    .open(&f)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        }
        let t = Instant::now();
        let sw = rness_engine::tools::sweep_spill_files(dir.path(), Duration::from_secs(7 * 86400));
        s.push(t.elapsed().as_secs_f64() * 1000.0);
        removed = sw.files;
    }
    emit("spill_sweep", n, &mut s, json!({"removed":removed}));
}

/// Terminal PTY read loop with `yes` for 10 s: CPU of the reader in this
/// process, scrollback bound, and terminal_read latency during the flood.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn perf_terminal_yes_cpu() {
    let dir = tmp("termyes");
    let root = dir.path().canonicalize().unwrap();
    let (r, jobs) = registry(&root);
    let terms = TerminalRegistry::new();
    register_terminal_tools(&r, terms.clone(), Workspace::new(&root), Some(jobs));
    let open = call(&r, "terminal_open", json!({})).await;
    let id = open.output.split_whitespace().nth(2).unwrap().to_string();
    call(
        &r,
        "terminal_send",
        json!({"session_id":id,"text":"yes","wait_ms":100}),
    )
    .await;
    let secs = env_num("RNESS_BENCH_TERM_SECS", 10);
    let c0 = cpu_time();
    let p = peak_start();
    let t = Instant::now();
    let mut lat = Vec::new();
    while t.elapsed() < Duration::from_secs(secs) {
        let t1 = Instant::now();
        let rd = call(&r, "terminal_read", json!({"session_id":id})).await;
        lat.push(t1.elapsed().as_secs_f64() * 1000.0);
        assert!(!rd.is_error);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let cpu = (cpu_time() - c0).as_secs_f64();
    let rss = peak_end(p);
    call(
        &r,
        "terminal_signal",
        json!({"session_id":id,"signal":"INT"}),
    )
    .await;
    terms.close_all();
    emit(
        "terminal_yes_read_latency",
        secs,
        &mut lat,
        json!({"reader_cpu_pct": cpu / secs as f64 * 100.0, "rss_peak_delta_mb": rss}),
    );
}

/// Startup job recovery with N settled records in one stale owner dir
/// (the real ~/.rness/sessions/jobs had ~25k records in 505 dirs).
#[test]
#[ignore]
fn perf_job_recovery() {
    let n = env_num("RNESS_BENCH_JOB_RECORDS", 20_000);
    let dirs = env_num("RNESS_BENCH_JOB_DIRS", 500);
    let dir = tmp("recovery");
    let root = dir.path().join("jobs");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let out = "x".repeat(2000);
    for d in 0..dirs {
        let od = root.join(format!("01OWNER{d:019}"));
        std::fs::create_dir_all(&od).unwrap();
        std::fs::write(od.join("owner.lock"), "").unwrap();
        for i in 0..(n / dirs) {
            let id = format!("01JOB{d:05}{i:016}");
            std::fs::write(od.join(format!("{id}.json")), json!({"kind":"bash-output","label":"echo","status":{"Exited":0},"read_from":0,"settled":true,"owner":"01OLD","delivered":true,"started_at_ms":now-1000,"settled_at_ms":now-500,"output_error":null}).to_string()).unwrap();
            std::fs::write(od.join(format!("{id}.output")), &out).unwrap();
        }
    }
    let mut s = Vec::new();
    let mut rss = 0f64;
    for _ in 0..iters() {
        let p = peak_start();
        let t = Instant::now();
        let jobs = JobRegistry::new();
        jobs.enable_persistence(&root).unwrap();
        let sync = t.elapsed().as_secs_f64() * 1000.0;
        jobs.wait_recovery();
        s.push(t.elapsed().as_secs_f64() * 1000.0);
        rss = rss.max(peak_end(p));
        println!(
            "{}",
            json!({"probe":"job_recovery_sync_part","median_ms":sync})
        );
        drop(jobs);
    }
    emit(
        "job_recovery",
        n,
        &mut s,
        json!({"dirs":dirs,"rss_peak_delta_mb":rss}),
    );
}
