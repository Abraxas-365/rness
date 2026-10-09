//! WP-4 render perf probes. Turn the chat.rs `diagnostic_*` workloads into
//! baseline-producing probes: every probe prints JSON lines compatible with
//! scripts/e2e/perf.py (`{"probe","metric","value","unit",...}`), also
//! appended to $RNESS_E2E_PERF_OUT when set, so
//! `scripts/e2e/compare_baseline.py $RNESS_E2E_PERF_OUT` compares them.
//!
//! All probes are #[ignore]; run in release, single-threaded:
//!   RNESS_E2E_PERF_OUT=/tmp/x.jsonl cargo test --release -p rness-tui \
//!     --test render_perf -- --ignored --nocapture --test-threads=1
//!
//! Knobs: RNESS_BENCH_HISTORY (entries list, default 1000,10000,50000),
//! RNESS_BENCH_FRAMES (frames per phase, default 30),
//! RNESS_BENCH_LIVE_KB (live text sizes, default 1,100,1024).
//! Real corpus: RNESS_PERF_LOG=<copy of session.v1.jsonl> for `real_session_probe`.
mod common;

use common::*;
use rness_protocol::events::ChunkDelta;
use rness_protocol::frames::Frame;
use rness_tui::app::{Action, App};
use serde_json::json;
use std::time::Instant;

fn frames() -> usize {
    env_usize("RNESS_BENCH_FRAMES", 30)
}

/// Per-frame wall times (ms) of `n` renders, with `before` run untimed
/// before each frame.
fn time_frames(
    app: &mut App,
    w: u16,
    h: u16,
    n: usize,
    mut before: impl FnMut(&mut App, usize),
) -> Vec<f64> {
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        before(app, i);
        let t = Instant::now();
        let b = render(app, w, h);
        v.push(t.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(b);
    }
    v
}

/// Render until background (progressive) layout is done: 20 consecutive
/// frames under 1 ms. Returns frames spent.
fn drain_refill(app: &mut App, w: u16, h: u16) -> usize {
    let mut quiet = 0;
    for i in 0..50_000 {
        let t = Instant::now();
        render(app, w, h);
        if t.elapsed().as_secs_f64() < 0.001 {
            quiet += 1;
            if quiet >= 20 {
                return i;
            }
        } else {
            quiet = 0;
        }
    }
    50_000
}

fn emit(probe: &str, phase: &str, mut v: Vec<f64>, extra: serde_json::Value) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut e = extra.clone();
    e["n"] = json!(v.len());
    record(
        probe,
        &format!("{phase}_p50_ms"),
        percentile(&v, 0.5),
        "ms",
        e.clone(),
    );
    record(
        probe,
        &format!("{phase}_p99_ms"),
        percentile(&v, 0.99),
        "ms",
        e.clone(),
    );
    record(
        probe,
        &format!("{phase}_max_ms"),
        *v.last().unwrap_or(&f64::NAN),
        "ms",
        e,
    );
}

/// Session with ~`entries` chat entries (≈6 entries per turn: user, 2×(assistant
/// tool_use + result), assistant) and a compaction every 7 turns.
fn session_with_entries(id: &str, entries: usize) -> LogBuilder {
    long_session(id, entries.div_ceil(6), 2, 7)
}

/// History scaling + resize + scroll cost vs history length (default flavor
/// config). Replaces chat.rs diagnostic_streaming_resize_performance /
/// diagnostic_real_session_resize with thresholded, recorded probes.
#[test]
#[ignore = "perf probe"]
fn history_scaling_probe() {
    let n = frames();
    for entries in env_list("RNESS_BENCH_HISTORY", &[1000, 10_000, 50_000]) {
        let probe = format!("tui-render-history-{entries}");
        let b = session_with_entries("s-h", entries);
        let t = Instant::now();
        let (mut app, _be) = new_app("s-h", b.history(), flavor_config());
        let project_ms = t.elapsed().as_secs_f64() * 1000.0;
        let extra = json!({"entries": app.model.entries.len(), "events": b.envelopes.len()});
        record(&probe, "project_ms", project_ms, "ms", extra.clone());
        let t = Instant::now();
        render(&mut app, 169, 46);
        record(
            &probe,
            "first_frame_ms",
            t.elapsed().as_secs_f64() * 1000.0,
            "ms",
            extra.clone(),
        );
        // Time until progressive layout settles (stale estimates refilled).
        let t = Instant::now();
        let mut settle_frames = 0;
        let mut last = rows(&render(&mut app, 169, 46));
        for _ in 0..2000 {
            settle_frames += 1;
            let cur = rows(&render(&mut app, 169, 46));
            if cur == last {
                break;
            }
            last = cur;
        }
        record(
            &probe,
            "settle_ms",
            t.elapsed().as_secs_f64() * 1000.0,
            "ms",
            json!({"frames": settle_frames}),
        );
        emit(
            &probe,
            "idle_during_refill",
            time_frames(&mut app, 169, 46, n, |_, _| {}),
            extra.clone(),
        );
        let drained = drain_refill(&mut app, 169, 46);
        record(
            &probe,
            "refill_frames",
            drained as f64,
            "frames",
            extra.clone(),
        );
        emit(
            &probe,
            "idle",
            time_frames(&mut app, 169, 46, n, |_, _| {}),
            extra.clone(),
        );
        // Scroll by a page per frame (PageUp held down).
        emit(
            &probe,
            "scroll_page",
            time_frames(&mut app, 169, 46, n, |a, _| a.apply(Action::ScrollUp(10))),
            extra.clone(),
        );
        // Jump far up (long PageUp burst coalesced into one frame).
        emit(
            &probe,
            "scroll_jump",
            time_frames(&mut app, 169, 46, n.min(10), |a, _| {
                a.apply(Action::ScrollUp(2000))
            }),
            extra.clone(),
        );
        app.model.scroll_from_bottom = 0;
        render(&mut app, 169, 46);
        // Resize alternating wide/narrow: every frame is a width change.
        emit(
            &probe,
            "resize",
            time_frames(&mut app, 169, 46, n, |_, _| {})
                .into_iter()
                .collect(),
            extra.clone(),
        );
        let mut alt = Vec::new();
        for i in 0..n {
            let (w, h) = if i % 2 == 0 { (86, 32) } else { (169, 46) };
            let t = Instant::now();
            render(&mut app, w, h);
            alt.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        emit(&probe, "resize_alternating", alt, extra.clone());
        // Resize while scrolled up (anchor path + refills).
        app.apply(Action::ScrollUp(500));
        let mut alt = Vec::new();
        for i in 0..n {
            let (w, h) = if i % 2 == 0 { (86, 32) } else { (169, 46) };
            let t = Instant::now();
            render(&mut app, w, h);
            alt.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        emit(&probe, "resize_scrolled", alt, extra.clone());
        // Append one entry (turn commit) at the bottom: append-only path.
        app.model.scroll_from_bottom = 0;
        render(&mut app, 169, 46);
        let mut app_times = Vec::new();
        let mut bb = b;
        for i in 0..n.min(20) {
            bb.user(&format!("appended {i}"));
            *_be.history.write().unwrap() = bb.history();
            let t = Instant::now();
            app.reconcile();
            render(&mut app, 169, 46);
            app_times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        emit(&probe, "append_reconcile", app_times, extra);
    }
}

/// Streaming frame time at {1 KB, 100 KB, 1 MB} of live text: each frame adds
/// one small delta (the real loop draws on every frame wake, 33 ms tick).
#[test]
#[ignore = "perf probe"]
fn streaming_frame_time_probe() {
    let n = frames();
    for kb in env_list("RNESS_BENCH_LIVE_KB", &[1, 100, 1024]) {
        for history in [100usize, 10_000] {
            let probe = format!("tui-stream-{kb}kb-h{history}");
            let b = session_with_entries("s-live", history);
            let (mut app, _be) = new_app("s-live", b.history(), flavor_config());
            drain_refill(&mut app, 169, 46);
            app.apply_frame(&Frame::StepStarted {
                session: "s-live".into(),
                turn: 1,
            });
            let base = prose(99, kb * 1024);
            app.apply_frame(&Frame::Delta {
                session: "s-live".into(),
                chunk: ChunkDelta::Text { t: base },
            });
            render(&mut app, 169, 46);
            let v = time_frames(&mut app, 169, 46, n, |a, i| {
                a.apply_frame(&Frame::Delta {
                    session: "s-live".into(),
                    chunk: ChunkDelta::Text {
                        t: format!(" delta {i} with **bold** and `code`."),
                    },
                })
            });
            emit(
                &probe,
                "frame",
                v,
                json!({"live_bytes": kb * 1024, "entries": app.model.entries.len()}),
            );
            // Same, with a code fence open at the end (highlight path every frame).
            app.apply_frame(&Frame::Delta {
                session: "s-live".into(),
                chunk: ChunkDelta::Text {
                    t: "\n```rust\n".into(),
                },
            });
            let v = time_frames(&mut app, 169, 46, n, |a, i| {
                a.apply_frame(&Frame::Delta {
                    session: "s-live".into(),
                    chunk: ChunkDelta::Text {
                        t: format!("let x{i} = {i};\n"),
                    },
                })
            });
            emit(
                &probe,
                "frame_in_fence",
                v,
                json!({"live_bytes": kb * 1024}),
            );
            // Thinking stream (sanitize + wrap of whole thinking text per frame).
            let v = time_frames(&mut app, 169, 46, n, |a, i| {
                a.apply_frame(&Frame::Delta {
                    session: "s-live".into(),
                    chunk: ChunkDelta::Thinking {
                        t: format!("{} thought {i}\n", prose(i, 200)),
                    },
                })
            });
            emit(
                &probe,
                "frame_thinking",
                v,
                json!({"live_bytes": kb * 1024}),
            );
        }
    }
}

/// Idle redraw cost: the loop redraws every 200 ms tick even with nothing
/// changed (app.rs:1771). Measures the per-frame cost of a no-op redraw at
/// large history so the idle CPU is predictable.
#[test]
#[ignore = "perf probe"]
fn idle_redraw_probe() {
    for entries in env_list("RNESS_BENCH_HISTORY", &[1000, 10_000, 50_000]) {
        let probe = format!("tui-idle-{entries}");
        let b = session_with_entries("s-idle", entries);
        let (mut app, _be) = new_app("s-idle", b.history(), flavor_config());
        // Background (progressive) layout after open: frames until a no-op
        // redraw is cheap (< 1 ms for 20 consecutive frames), and the CPU
        // spent until then. The real loop draws at most every 33/200 ms.
        let mut spent = 0.0;
        let mut quiet = 0;
        let mut frames_until_cheap = 0;
        for i in 0..20_000 {
            let t = Instant::now();
            render(&mut app, 169, 46);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            spent += ms;
            if ms < 1.0 {
                quiet += 1;
                if quiet >= 20 {
                    frames_until_cheap = i + 1 - 20;
                    break;
                }
            } else {
                quiet = 0;
                frames_until_cheap = i + 1;
            }
        }
        record(
            &probe,
            "refill_frames_after_open",
            frames_until_cheap as f64,
            "frames",
            json!({"entries": entries}),
        );
        record(
            &probe,
            "refill_cpu_after_open_ms",
            spent,
            "ms",
            json!({"entries": entries}),
        );
        let v = time_frames(&mut app, 169, 46, 200, |_, _| {});
        let mut s = v.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // CPU % at 5 fps idle tick = p50 * 5 / 1000 * 100
        record(
            &probe,
            "idle_cpu_pct_at_5fps",
            percentile(&s, 0.5) * 5.0 / 10.0,
            "pct",
            json!({"entries": entries}),
        );
        emit(&probe, "idle", v, json!({"entries": entries}));
    }
}

/// Markdown + highlight of a 10k-line code block: first render (uncached),
/// warm (markdown cache hit), and > 1 MiB sources which are never cached
/// (render.rs:70) so every frame re-renders them.
#[test]
#[ignore = "perf probe"]
fn markdown_highlight_probe() {
    use rness_tui::core::render::render_markdown;
    use rness_tui::theme::Theme;
    let theme = Theme::default();
    let t = Instant::now();
    rness_tui::core::highlight::highlight_code("fn warm() {}", "rust");
    record(
        "tui-markdown",
        "syntect_init_ms",
        t.elapsed().as_secs_f64() * 1000.0,
        "ms",
        json!({}),
    );
    for lines in [1000usize, 10_000, 30_000] {
        let src = format!(
            "intro\n\n```rust\n{}```\n",
            (0..lines)
                .map(|i| format!("    let value_{i} = compute(&items[{i}..]); // line {i}\n"))
                .collect::<String>()
        );
        let extra = json!({"lines": lines, "bytes": src.len()});
        let mut v = Vec::new();
        for i in 0..5 {
            // Unique source → uncached.
            let s = format!("{i}{src}");
            let t = Instant::now();
            std::hint::black_box(render_markdown(&s, 120, &theme));
            v.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        emit(
            "tui-markdown",
            &format!("code{lines}_uncached"),
            v,
            extra.clone(),
        );
        let mut v = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            std::hint::black_box(render_markdown(&src, 120, &theme));
            v.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        emit("tui-markdown", &format!("code{lines}_repeat"), v, extra);
    }
    // Plain prose of 2 MiB (above the never-cache threshold).
    let big = prose(1, 2 * 1024 * 1024);
    let mut v = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        std::hint::black_box(render_markdown(&big, 120, &theme));
        v.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    emit(
        "tui-markdown",
        "prose2mib_repeat",
        v,
        json!({"bytes": big.len()}),
    );
}

/// Display-cache eviction cost (chat.rs:2562-2582: collect + sort every
/// resident index each over-budget frame) with ~50k entries.
#[test]
#[ignore = "perf probe"]
fn eviction_probe() {
    let n = frames();
    for entries in env_list("RNESS_BENCH_EVICT", &[10_000, 50_000]) {
        let b = session_with_entries("s-ev", entries);
        for budget in [0u64, 1024 * 1024, 8 * 1024 * 1024, 1 << 40] {
            let probe = format!("tui-evict-{entries}");
            let mut cfg = flavor_config();
            cfg["cache_bytes"] = json!(budget);
            let (mut app, _be) = new_app("s-ev", b.history(), cfg);
            // Lay out everything (exact) by scrolling through the whole
            // transcript, which makes the resident set as large as possible.
            render(&mut app, 169, 46);
            let t = Instant::now();
            let mut f = 0;
            loop {
                app.apply(Action::ScrollUp(40));
                render(&mut app, 169, 46);
                f += 1;
                if app.model.scroll_from_bottom == u16::MAX || f > 4000 {
                    break;
                }
                // Stop when the view no longer moves (top reached).
                if f % 50 == 0 {
                    let a = rows(&render(&mut app, 169, 46));
                    app.apply(Action::ScrollUp(40));
                    let bb = rows(&render(&mut app, 169, 46));
                    if a == bb {
                        break;
                    }
                }
            }
            let walk = t.elapsed().as_secs_f64() * 1000.0;
            let label = if budget == 1 << 40 {
                "unbounded".to_string()
            } else {
                format!("{}k", budget / 1024)
            };
            record(
                &probe,
                &format!("walk_to_top_{label}_ms"),
                walk,
                "ms",
                json!({"frames": f, "budget": budget}),
            );
            // Per-frame cost of scrolling back down with eviction active.
            let v = time_frames(&mut app, 169, 46, n, |a, _| a.apply(Action::ScrollDown(40)));
            emit(
                &probe,
                &format!("scroll_{label}"),
                v,
                json!({"budget": budget}),
            );
        }
    }
}

/// Real-corpus probe (keeps the RNESS_PERF_LOG path of the old diagnostic).
/// Point it at a COPY (scripts/e2e/fixtures/gen_session.py real …).
#[test]
#[ignore = "perf probe; needs RNESS_PERF_LOG"]
fn real_session_probe() {
    let Ok(path) = std::env::var("RNESS_PERF_LOG") else {
        eprintln!("RNESS_PERF_LOG unset");
        return;
    };
    let text = std::fs::read_to_string(&path).unwrap();
    let t = Instant::now();
    let envelopes: Vec<rness_protocol::events::Envelope> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let parse_ms = t.elapsed().as_secs_f64() * 1000.0;
    let session = match &envelopes[0].event {
        rness_protocol::events::SessionEvent::Header(h) => h.session.clone(),
        _ => panic!("no header"),
    };
    let probe = "tui-real-session";
    let extra = json!({"events": envelopes.len(), "bytes": text.len()});
    record(probe, "parse_ms", parse_ms, "ms", extra.clone());
    let history = rness_protocol::api::History::new(session.clone(), envelopes);
    let t = Instant::now();
    let (mut app, _be) = new_app(&session, history, flavor_config());
    record(
        probe,
        "project_ms",
        t.elapsed().as_secs_f64() * 1000.0,
        "ms",
        json!({"entries": app.model.entries.len()}),
    );
    let t = Instant::now();
    render(&mut app, 169, 46);
    record(
        probe,
        "first_frame_ms",
        t.elapsed().as_secs_f64() * 1000.0,
        "ms",
        extra.clone(),
    );
    emit(
        probe,
        "idle",
        time_frames(&mut app, 169, 46, 30, |_, _| {}),
        extra.clone(),
    );
    let mut alt = Vec::new();
    for i in 0..20 {
        let (w, h) = if i % 2 == 0 { (86, 46) } else { (169, 46) };
        let t = Instant::now();
        render(&mut app, w, h);
        alt.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    emit(probe, "resize_alternating", alt, extra.clone());
    emit(
        probe,
        "scroll_page",
        time_frames(&mut app, 169, 46, 30, |a, _| a.apply(Action::ScrollUp(10))),
        extra,
    );
}
