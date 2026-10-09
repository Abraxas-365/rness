//! WP-1 perf probes: session log / store / replay / compaction / search.
//! Release only, all `#[ignore]`; JSON-lines records in the WP-0 perf.py
//! format are appended to $RNESS_E2E_PERF_OUT or $TMPDIR/rness-e2e-perf/<probe>.jsonl
//! and echoed to stdout.
//!
//!   cargo test --release -p rness-engine --test session_store_perf -- --ignored --nocapture --test-threads 1
//!   # single probe:  ... -- --ignored --nocapture perf_replay_matrix
//!   # bigger open sizes (MB, comma list; default 10,100,600):  RNESS_WP1_OPEN_MB=10,100
//!
//! Fixtures live in $TMPDIR/rness-e2e-wp1-perf-* (tempdir, removed on drop).
//! Never touches ~/.rness.

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use rness_engine::invariants::assert_model_visible_logged;
use rness_engine::session::branch::SessionStore;
use rness_engine::session::log::{read_session, SessionLog};
use rness_engine::session::projection::model_context;
use rness_engine::session::replay::replay;
use rness_engine::session_search::{QueryRequest, SqliteSessionSearch};
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::compaction::{self, Meter, Policy};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::{run_turn, TurnConfig};
use rness_protocol::events::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

// ------------------------------------------------------------- reporter

struct Reporter {
    probe: String,
    out: PathBuf,
}

impl Reporter {
    fn new(probe: &str) -> Self {
        let out = std::env::var_os("RNESS_E2E_PERF_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("rness-e2e-perf"));
        fs::create_dir_all(&out).unwrap();
        Self {
            probe: probe.into(),
            out: out.join(format!("{probe}.jsonl")),
        }
    }
    fn sample(&self, metric: &str, value: f64, unit: &str, extra: serde_json::Value) {
        let mut rec = json!({
            "probe": self.probe, "metric": metric, "value": (value * 1000.0).round() / 1000.0,
            "unit": unit, "wp": 1, "bin": "cargo-test-release", "commit": "22490f1+wt",
            "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64(),
        });
        if let serde_json::Value::Object(m) = extra {
            for (k, v) in m {
                rec[k] = v;
            }
        }
        println!("{rec}");
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.out)
            .unwrap();
        writeln!(f, "{rec}").unwrap();
    }
    /// Run `f` `n` times, record median/min/max/p90 in ms.
    fn time<T>(
        &self,
        metric: &str,
        n: usize,
        extra: serde_json::Value,
        mut f: impl FnMut() -> T,
    ) -> f64 {
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            std::hint::black_box(f());
            v.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let s = stats(&mut v);
        let mut extra = extra;
        extra["n"] = json!(n);
        extra["min"] = json!(s.1);
        extra["max"] = json!(s.2);
        extra["p90"] = json!(s.3);
        self.sample(metric, s.0, "ms", extra);
        s.0
    }
}

/// (median, min, max, p90)
fn stats(v: &mut [f64]) -> (f64, f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    (
        v[n / 2],
        v[0],
        v[n - 1],
        v[((n as f64 * 0.9) as usize).min(n - 1)],
    )
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn tmp(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("rness-e2e-wp1-perf-{tag}-"))
        .tempdir_in(std::env::temp_dir())
        .unwrap()
}

fn max_rss_mb() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    // macOS reports bytes, Linux KiB.
    if cfg!(target_os = "macos") {
        ru.ru_maxrss as f64 / 1e6
    } else {
        ru.ru_maxrss as f64 / 1e3
    }
}

fn cur_rss_mb() -> f64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

/// Shared WP-0 fixture generator (scripts/e2e/fixtures/gen_session.py).
fn gen(root: &Path, args: &[&str]) -> Vec<String> {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/e2e/fixtures/gen_session.py");
    let out = Command::new("python3")
        .arg(script)
        .arg("gen")
        .arg(root)
        .args(["--workspace", "/w", "--attempts", "0", "--seed", "7"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.split('\t').next().unwrap().to_string())
        .collect()
}

fn file_mb(root: &Path, sid: &str) -> f64 {
    fs::metadata(root.join(sid).join("session.v1.jsonl"))
        .unwrap()
        .len() as f64
        / 1e6
}

fn line_count(root: &Path, sid: &str) -> usize {
    let b = fs::read(root.join(sid).join("session.v1.jsonl")).unwrap();
    b.iter().filter(|c| **c == b'\n').count()
}

// --------------------------------------------- replay vs events × checkpoints

#[test]
#[ignore = "perf"]
fn perf_replay_matrix() {
    let rep = Reporter::new("wp1-replay");
    let sizes: Vec<usize> = std::env::var("RNESS_WP1_EVENTS")
        .map(|s| s.split(',').map(|x| x.parse().unwrap()).collect())
        .unwrap_or(vec![1_000, 10_000, 50_000, 100_000]);
    for &events in &sizes {
        for compactions in [0usize, 25, 200] {
            let dir = tmp("replay");
            // 8 events/turn with 2 tools/turn, 512 B results.
            let turns = (events / 8).max(2);
            let sid = gen(
                dir.path(),
                &[
                    "--turns",
                    &turns.to_string(),
                    "--tools-per-turn",
                    "2",
                    "--tool-result-bytes",
                    "512",
                    "--compactions",
                    &compactions.to_string(),
                ],
            )
            .remove(0);
            let n = line_count(dir.path(), &sid);
            let mb = file_mb(dir.path(), &sid);
            let extra = json!({"events": n, "compactions": compactions, "mb": mb});
            let reps = match events {
                e if e >= 100_000 => 2,
                e if e >= 50_000 => 3,
                _ => 15,
            };
            // Cold: fresh store each time (resume path).
            rep.time("replay_cold_ms", reps, extra.clone(), || {
                let store = SessionStore::new(dir.path());
                replay(&store, &sid).unwrap().context.turns.len()
            });
            // Warm: cached reader, nothing appended (per-step path).
            let store = SessionStore::new(dir.path());
            replay(&store, &sid).unwrap();
            rep.time("replay_warm_ms", reps * 2, extra.clone(), || {
                replay(&store, &sid).unwrap().context.turns.len()
            });
            // Breakdown of the warm path.
            rep.time("history_warm_ms", reps * 2, extra.clone(), || {
                store.history(&sid).unwrap().len()
            });
            let history = store.history(&sid).unwrap();
            rep.time("model_context_ms", reps * 2, extra.clone(), || {
                model_context(&history).turns.len()
            });
            let ctx = model_context(&history);
            rep.time("invariant_check_ms", reps.min(5), extra.clone(), || {
                assert_model_visible_logged(&ctx, &history).is_ok()
            });
            rep.sample(
                "context_sources",
                ctx.sources.len() as f64,
                "count",
                extra.clone(),
            );
        }
    }
}

// ------------------------------------------- per-step replay amplification

struct OneStep;
#[async_trait]
impl Provider for OneStep {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        let text = if request.system == "WP1 summarizer" {
            "SUMMARY"
        } else {
            "ok"
        };
        StepOutcome::Committed(AssistantMessage {
            model: "fake-1".into(),
            content: vec![ContentPart::Text { text: text.into() }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

fn default_policy() -> Policy {
    // flavors/default/init.lua:95 budgets.
    Policy {
        meter: Default::default(),
        summary_profile: None,
        threshold_tokens: 165_000,
        retain_tokens: 24_000,
        summary_tokens: 4096,
        system_prompt: "WP1 summarizer".into(),
        prompt: "Summarize.".into(),
        max_overflow_retries: 1,
        max_compactions: 2,
        prune_threshold: 8192,
        prune_head: 4096,
        prune_tail: 1024,
    }
}

/// One 1-step turn on a big session, with and without a compaction policy,
/// compared to one warm replay: ratio ≈ replays (and equivalents) per step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf"]
async fn perf_turn_overhead_vs_replay() {
    let rep = Reporter::new("wp1-turn-overhead");
    for events in [10_000usize, 50_000] {
        let dir = tmp("turn");
        // compactions=1 so the live context stays small (realistic): the
        // cost is history-bound, not context-bound.
        let sid = gen(
            dir.path(),
            &[
                "--turns",
                &(events / 8).to_string(),
                "--tools-per-turn",
                "2",
                "--tool-result-bytes",
                "512",
                "--compactions",
                "1",
            ],
        )
        .remove(0);
        let n = line_count(dir.path(), &sid);
        let store = SessionStore::new(dir.path());
        let mut log = store.open(&sid).unwrap();
        replay(&store, &sid).unwrap();
        let warm = {
            let mut v: Vec<f64> = (0..20)
                .map(|_| {
                    let t = Instant::now();
                    replay(&store, &sid).unwrap();
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            stats(&mut v).0
        };
        let tools = ToolRegistry::default();
        for (label, config) in [
            ("no_policy", TurnConfig::default()),
            (
                "default_policy",
                TurnConfig {
                    compaction: [("default".to_string(), default_policy())].into(),
                    ..Default::default()
                },
            ),
        ] {
            let mut v = Vec::new();
            for turn in 0..8u32 {
                log.append(&SessionEvent::UserMessage(UserMessage {
                    intent: UserIntent::Followup,
                    content: vec![ContentPart::Text { text: "hi".into() }],
                    source: None,
                }))
                .unwrap();
                let t = Instant::now();
                run_turn(
                    &store,
                    &mut log,
                    &OneStep,
                    &tools,
                    &config,
                    &CancellationToken::new(),
                    &mut Vec::new,
                    10_000 + turn,
                    &|_| {},
                    None,
                )
                .await
                .unwrap();
                v.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            let med = stats(&mut v).0;
            let extra = json!({"events": n, "config": label, "warm_replay_ms": warm});
            rep.sample("turn_1step_ms", med, "ms", extra.clone());
            rep.sample("turn_over_replay_ratio", med / warm, "x", extra);
        }
    }
}

// ------------------------------------------------ open / read_session sizes

#[test]
#[ignore = "perf"]
fn perf_open_and_read_by_size() {
    let rep = Reporter::new("wp1-open");
    let sizes: Vec<usize> = std::env::var("RNESS_WP1_OPEN_MB")
        .map(|s| s.split(',').map(|x| x.parse().unwrap()).collect())
        .unwrap_or(vec![10, 100, 600]);
    for mb in sizes {
        let dir = tmp("open");
        // ~105 KB per turn (2 × 50 KB results + text).
        let turns = (mb * 1_000_000 / 105_000).max(2);
        let sid = gen(
            dir.path(),
            &[
                "--turns",
                &turns.to_string(),
                "--tools-per-turn",
                "2",
                "--tool-result-bytes",
                "51200",
            ],
        )
        .remove(0);
        let real_mb = file_mb(dir.path(), &sid);
        let extra = json!({"mb": real_mb, "events": line_count(dir.path(), &sid)});
        let rss0 = cur_rss_mb();
        let max0 = max_rss_mb();
        rep.time("log_open_ms", 5, extra.clone(), || {
            SessionLog::open(dir.path(), &sid).unwrap()
        });
        rep.sample(
            "log_open_peak_rss_delta_mb",
            max_rss_mb() - max0,
            "MB",
            extra.clone(),
        );
        let store = SessionStore::new(dir.path());
        rep.time("store_open_ms", 5, extra.clone(), || {
            store.open(&sid).unwrap()
        });
        rep.time("read_session_cold_ms", 3, extra.clone(), || {
            read_session(dir.path(), &sid).unwrap().len()
        });
        rep.time("replay_cold_ms", 3, extra.clone(), || {
            let s = SessionStore::new(dir.path());
            replay(&s, &sid).unwrap().history.len()
        });
        let s = SessionStore::new(dir.path());
        replay(&s, &sid).unwrap();
        rep.sample(
            "rss_after_warm_replay_mb",
            cur_rss_mb(),
            "MB",
            extra.clone(),
        );
        rep.sample("rss_before_mb", rss0, "MB", extra.clone());
        rep.sample("peak_rss_mb", max_rss_mb(), "MB", extra);
        drop(s);
        drop(store);
    }
}

// --------------------------------------------------------- append latency

#[test]
#[ignore = "perf"]
fn perf_append_latency() {
    let rep = Reporter::new("wp1-append");
    for (bytes, n) in [(1024usize, 10_000usize), (100 * 1024, 2_000)] {
        let dir = tmp("append");
        let store = SessionStore::new(dir.path());
        let mut log = store.create(None).unwrap();
        let ev = SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text {
                text: "a".repeat(bytes),
            }],
            source: None,
        });
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            log.append(&ev).unwrap();
            v.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let extra = json!({"bytes": bytes, "n": n, "fsync": "sync_data"});
        rep.sample("append_p50_ms", pct(&mut v, 0.5), "ms", extra.clone());
        rep.sample("append_p99_ms", pct(&mut v, 0.99), "ms", extra.clone());
        rep.sample("append_max_ms", pct(&mut v, 1.0), "ms", extra.clone());
        // Same serialization + write without fsync (what a knob would buy).
        let path = dir.path().join("nosync.jsonl");
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut w = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            let env = Envelope {
                id: ulid::Ulid::new().to_string(),
                at: "2026-01-01T00:00:00.000Z".into(),
                event: ev.clone(),
            };
            let mut line = serde_json::to_string(&env).unwrap();
            line.push('\n');
            f.write_all(line.as_bytes()).unwrap();
            w.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let extra = json!({"bytes": bytes, "n": n, "fsync": "none"});
        rep.sample("append_p50_ms", pct(&mut w, 0.5), "ms", extra.clone());
        rep.sample("append_p99_ms", pct(&mut w, 0.99), "ms", extra);
    }
}

// ------------------------------------------------------ fork chain history

#[test]
#[ignore = "perf"]
fn perf_history_fork_chains() {
    let rep = Reporter::new("wp1-history");
    for depth in [1usize, 5, 10, 50] {
        let dir = tmp("forks");
        let store = SessionStore::new(dir.path());
        // 1000 events per hop (fast buffered writes), forks at tip.
        let mut ids: Vec<String> = Vec::new();
        for hop in 0..=depth {
            let log = if hop == 0 {
                store.create(Some("/w".into())).unwrap()
            } else {
                store.fork(ids.last().unwrap(), None).unwrap()
            };
            let path = log.path().to_owned();
            ids.push(log.session().clone());
            drop(log);
            let mut w = BufWriter::new(OpenOptions::new().append(true).open(&path).unwrap());
            for i in 0..1000 {
                let env = Envelope {
                    id: ulid::Ulid::new().to_string(),
                    at: "2026-01-01T00:00:00.000Z".into(),
                    event: SessionEvent::UserMessage(UserMessage {
                        intent: UserIntent::Followup,
                        content: vec![ContentPart::Text {
                            text: format!("hop {hop} msg {i} {}", "x".repeat(200)),
                        }],
                        source: None,
                    }),
                };
                serde_json::to_writer(&mut w, &env).unwrap();
                w.write_all(b"\n").unwrap();
            }
        }
        let leaf = ids.last().unwrap().clone();
        let extra = json!({"depth": depth, "events": (depth + 1) * 1000});
        rep.time("history_cold_ms", 5, extra.clone(), || {
            SessionStore::new(dir.path()).history(&leaf).unwrap().len()
        });
        let warm = SessionStore::new(dir.path());
        warm.history(&leaf).unwrap();
        rep.time("history_warm_ms", 20, extra.clone(), || {
            warm.history(&leaf).unwrap().len()
        });
        rep.time("replay_warm_ms", 20, extra.clone(), || {
            replay(&warm, &leaf).unwrap().context.turns.len()
        });
        // Thrash: alternate the leaf with its root ancestor's own history.
        rep.time(
            "history_alternating_leaf_root_ms",
            20,
            extra.clone(),
            || {
                warm.history(&ids[0]).unwrap();
                warm.history(&leaf).unwrap().len()
            },
        );
        rep.time("ancestry_ms", 20, extra.clone(), || {
            warm.ancestry(&leaf).unwrap().len()
        });
    }
}

// --------------------------------------------------------------- reduce()

fn write_reduce_fixture(
    root: &Path,
    events_target: usize,
    big_results: usize,
) -> (SessionStore, String) {
    let store = SessionStore::new(root);
    let log = store.create(Some("/w".into())).unwrap();
    let sid = log.session().clone();
    let path = log.path().to_owned();
    drop(log);
    let mut w = BufWriter::new(OpenOptions::new().append(true).open(&path).unwrap());
    let emit = |w: &mut BufWriter<fs::File>, event: SessionEvent| {
        let env = Envelope {
            id: ulid::Ulid::new().to_string(),
            at: "2026-01-01T00:00:00.000Z".into(),
            event,
        };
        serde_json::to_writer(&mut *w, &env).unwrap();
        w.write_all(b"\n").unwrap();
    };
    // Turn = user, assistant(tool_use), result, assistant = 4 events.
    let turns = events_target / 4;
    let every = (turns / big_results).max(1);
    for t in 0..turns {
        emit(
            &mut w,
            SessionEvent::UserMessage(UserMessage {
                intent: UserIntent::Followup,
                content: vec![ContentPart::Text {
                    text: format!("u{t}"),
                }],
                source: None,
            }),
        );
        let call = format!("c{t}");
        emit(
            &mut w,
            SessionEvent::AssistantMessage(AssistantMessage {
                model: "fake-1".into(),
                content: vec![ContentPart::ToolUse {
                    call: call.clone(),
                    name: "Bash".into(),
                    args: json!({}),
                }],
                stop: StopReason::ToolUse,
                usage: Usage::default(),
                estimated_input: 0,
                chunks: vec![],
            }),
        );
        let big = t % every == 0;
        let output = if big {
            "b".repeat(10_000)
        } else {
            "small".into()
        };
        emit(
            &mut w,
            SessionEvent::ToolResult(ToolResult {
                call,
                name: "Bash".into(),
                content: vec![],
                output,
                is_error: false,
                duration_ms: 1,
                tasks: None,
                plan_review: None,
                presentation: None,
            }),
        );
        emit(
            &mut w,
            SessionEvent::AssistantMessage(AssistantMessage {
                model: "fake-1".into(),
                content: vec![ContentPart::Text { text: "ok".into() }],
                stop: StopReason::EndTurn,
                usage: Usage::default(),
                estimated_input: 0,
                chunks: vec![],
            }),
        );
    }
    w.flush().unwrap();
    (store, sid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf"]
async fn perf_reduce_prune_pass() {
    let rep = Reporter::new("wp1-reduce");
    let dir = tmp("reduce");
    let (store, sid) = write_reduce_fixture(dir.path(), 50_000, 2_000);
    let extra = json!({"events": line_count(dir.path(), &sid), "big_results": 2000, "mb": file_mb(dir.path(), &sid)});
    let mut log = store.open(&sid).unwrap();
    let policy = default_policy();
    let syncs0 = log.sync_count();
    let t = Instant::now();
    let changed = compaction::reduce(
        &store,
        &mut log,
        &OneStep,
        "",
        &[],
        &policy,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let history = read_session(dir.path(), &sid).unwrap();
    let prunes = history
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::Prune(_)))
        .count();
    let ckpts = history
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::Compaction(_)))
        .count();
    let mut e = extra.clone();
    e["prunes"] = json!(prunes);
    e["checkpoints"] = json!(ckpts);
    e["changed"] = json!(changed);
    e["syncs"] = json!(log.sync_count() - syncs0);
    rep.sample("reduce_first_ms", ms, "ms", e);
    // Steady state: nothing to do, but reduce still runs every step.
    rep.time("reduce_noop_ms", 5, extra.clone(), || {
        futures_lite_block(compaction::reduce(
            &store,
            &mut log,
            &OneStep,
            "",
            &[],
            &policy,
            false,
            &CancellationToken::new(),
        ))
    });
    // Meter on the resulting context and on a synthetic ~200k-token context.
    let ctx = replay(&store, &sid).unwrap().context;
    let meter = Meter::default();
    rep.time(
        "meter_measure_ms",
        20,
        json!({"turns": ctx.turns.len()}),
        || meter.measure(&ctx, "", &[]),
    );
}

fn futures_lite_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

#[test]
#[ignore = "perf"]
fn perf_meter_200k_tokens() {
    let rep = Reporter::new("wp1-meter");
    let dir = tmp("meter");
    // ~800 KB of model-visible text ≈ 200k tokens at 4 B/token, as 400
    // turns of tool calls with 2 KB results.
    let (store, sid) = write_reduce_fixture(dir.path(), 1_600, 1);
    let ctx = replay(&store, &sid).unwrap().context;
    let tokens = Meter::default().measure(&ctx, "", &[]);
    let extra = json!({"turns": ctx.turns.len(), "tokens": tokens});
    rep.time("meter_measure_ms", 50, extra.clone(), || {
        Meter::default().measure(&ctx, "", &[])
    });
    let mut big = ctx.clone();
    for t in big.turns.iter_mut() {
        if let rness_engine::session::projection::ModelTurn::ToolResults { results } = t {
            for r in results.iter_mut() {
                r.output = "z".repeat(2000);
            }
        }
    }
    let tokens = Meter::default().measure(&big, "", &[]);
    rep.time(
        "meter_measure_ms",
        50,
        json!({"turns": big.turns.len(), "tokens": tokens}),
        || Meter::default().measure(&big, "", &[]),
    );
}

// -------------------------------------------------- session search re-index

#[test]
#[ignore = "perf"]
fn perf_search_append_reindex() {
    let rep = Reporter::new("wp1-search");
    let mb: usize = std::env::var("RNESS_WP1_SEARCH_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let dir = tmp("search");
    let root = dir.path().join("sessions");
    // ~8 KB per turn: 2 × 3 KB results.
    let sid = gen(
        &root,
        &[
            "--turns",
            &(mb * 1_000_000 / 8_000).to_string(),
            "--tools-per-turn",
            "2",
            "--tool-result-bytes",
            "3072",
        ],
    )
    .remove(0);
    // A few small sibling sessions in the same workspace.
    gen(&root, &["--turns", "20", "--sessions", "20", "--seed", "8"]);
    let store = SessionStore::new(&root);
    let extra = json!({"mb": file_mb(&root, &sid), "events": line_count(&root, &sid)});
    let index = dir.path().join("index.sqlite3");
    let mut search = SqliteSessionSearch::new(index.clone());
    let q = |search: &mut SqliteSessionSearch, query: &str| {
        let request: QueryRequest = serde_json::from_value(json!({"query": query})).unwrap();
        search
            .execute(&store, &sid, "session_search", request)
            .unwrap()
    };
    rep.time("initial_index_ms", 1, extra.clone(), || {
        q(&mut search, "zzzrare")
    });
    rep.sample(
        "index_mb",
        fs::metadata(&index).unwrap().len() as f64 / 1e6,
        "MB",
        extra.clone(),
    );
    rep.time("warm_noop_refresh_ms", 5, extra.clone(), || {
        q(&mut search, "zzzrare")
    });
    let mut log = store.open(&sid).unwrap();
    rep.time("append_one_then_search_ms", 3, extra.clone(), || {
        log.append(&SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text {
                text: "freshappend".into(),
            }],
            source: None,
        }))
        .unwrap();
        q(&mut search, "freshappend")
    });
    drop(log);
    let _ = Arc::new(()); // keep import used
}
