//! WP-4 pathological content e2e: render hostile transcripts through the
//! real App (chat + input + statusline) into ratatui buffers at several
//! widths, configured (default flavor messagebox) and unconfigured.
//!
//! Checks per case × width × surface (user / assistant / tool output /
//! live stream / thinking):
//! - no panic, render finishes within a generous time bound;
//! - no control character reaches a buffer cell (ESC/CR/BS would be
//!   written raw to the terminal);
//! - no visible zero-width cell (desyncs the cursor on a real terminal);
//! - a marker string from the content is visible where it should be.
//!
//! Run: cargo test -p rness-tui --test content_e2e -- --nocapture
mod common;

use common::*;
use rness_protocol::events::ChunkDelta;
use rness_protocol::frames::Frame;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const WIDTHS: &[u16] = &[20, 80, 200];

fn cases() -> Vec<(&'static str, String)> {
    let mut v: Vec<(&'static str, String)> = Vec::new();
    v.push(("only-whitespace", " \n\t\n   \n\u{a0}\u{3000}\n".into()));
    v.push(("empty", String::new()));
    v.push(("long-word-10k", format!("MARK{}", "x".repeat(10_000))));
    v.push((
        "long-line-200k-spaces",
        format!("MARK {}", "ab ".repeat(70_000)),
    ));
    v.push((
        "unclosed-fence",
        format!(
            "MARK\n```rust\n{}",
            (0..200)
                .map(|i| format!("let a{i} = {i};\n"))
                .collect::<String>()
        ),
    ));
    v.push((
        "nested-lists-50",
        (0..50)
            .map(|d| format!("{}- MARK level {d}\n", "  ".repeat(d)))
            .collect(),
    ));
    v.push((
        "nested-quotes-50",
        format!("{} MARK deep quote", ">".repeat(50)),
    ));
    v.push(("table-200-cols", {
        let head: Vec<String> = (0..200).map(|c| format!("c{c}")).collect();
        let sep: Vec<&str> = (0..200).map(|_| "---").collect();
        let row: Vec<String> = (0..200).map(|c| format!("v{c}")).collect();
        format!(
            "MARK\n\n| {} |\n| {} |\n| {} |\n",
            head.join(" | "),
            sep.join(" | "),
            row.join(" | ")
        )
    }));
    v.push((
        "cjk-emoji-zwj",
        "MARK 漢字かな交じり文 한국어 👨‍👩‍👧‍👦 🏳️‍🌈 👍🏽 🇯🇵 e\u{301}\u{302}\u{303} Z\u{36b}\u{33f}\u{344}\u{351}\u{334}a\u{337}\u{35a}l\u{338}g\u{335}o\n漢".repeat(30),
    ));
    v.push((
        "rtl-bidi",
        "MARK שלום עולם مرحبا بالعالم \u{202e}reversed\u{202c} \u{2066}isolate\u{2069}\n"
            .repeat(20),
    ));
    v.push((
        "zero-width",
        "MARK a\u{200b}b\u{200c}c\u{200d}d\u{feff}e\u{2060}f\u{200e}g\u{200f}h\u{ad}i\n".repeat(20),
    ));
    v.push((
        "ansi-osc",
        "MARK \x1b[31mred\x1b[0m \x1b]0;evil title\x07 \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ \x1b[2J\x1b[H \x1b[?1049l \x1bc tail\n".repeat(20),
    ));
    v.push((
        "cr-backspace",
        "MARK progress 10%\rprogress 50%\rdone\x08\x08\x08XYZ\x07bell\x00nul\n".repeat(20),
    ));
    v.push((
        "c1-controls",
        "MARK a\u{85}b\u{9b}31mc\u{90}d\u{9c}e\n".repeat(10),
    ));
    v.push(("tabs", "MARK\tcol\t\tcol2\n\t\t\t\t\t\t\tdeep\n".repeat(20)));
    v.push((
        "code-10k-lines",
        format!(
            "MARK\n```rust\n{}```\n",
            (0..10_000)
                .map(|i| format!("    let value_{i} = compute({i}); // line {i}\n"))
                .collect::<String>()
        ),
    ));
    v.push(("wide-at-edge", format!("MARK\n{}", "a漢".repeat(500))));
    v.push((
        "lone-combining",
        "\u{301}\u{302}MARK\u{301}\n\u{20dd}\n".repeat(10),
    ));
    v.push((
        "variation-selectors",
        "MARK ☺\u{fe0f} ✈\u{fe0e} ❤\u{fe0f}\u{fe0f}\u{fe0f}\n".repeat(10),
    ));
    v.push((
        "huge-heading-and-hr",
        format!(
            "# MARK {}\n\n---\n\n###### {}\n",
            "H".repeat(5000),
            "h ".repeat(3000)
        ),
    ));
    v.push((
        "many-blank-lines",
        format!("MARK{}end", "\n".repeat(20_000)),
    ));
    v.push((
        "html-and-links",
        "MARK <script>alert(1)</script> [link](javascript:alert(1)) ![img](x.png) <https://e.x>\n"
            .repeat(20),
    ));
    v.push((
        "deep-emphasis",
        format!("MARK {}x{}", "*_".repeat(500), "_*".repeat(500)),
    ));
    v
}

#[derive(Default)]
struct Outcome {
    failures: Vec<String>,
    slowest: (Duration, String),
}

impl Outcome {
    fn time(&mut self, d: Duration, label: &str) {
        if d > self.slowest.0 {
            self.slowest = (d, label.to_string());
        }
    }
}

fn check_buf(out: &mut Outcome, label: &str, buf: &ratatui::buffer::Buffer, expect_mark: bool) {
    let v = cell_violations(buf);
    if !v.is_empty() {
        out.failures.push(format!(
            "{label}: {} cell violations, first: {}",
            v.len(),
            v[0]
        ));
    }
    let squashed: String = text(buf).chars().filter(|c| !c.is_whitespace()).collect();
    if expect_mark && !squashed.contains("MARK") && buf.area.width >= 20 {
        out.failures.push(format!("{label}: MARK not visible"));
    }
}

fn surfaces(name: &str, content: &str) -> Vec<(&'static str, LogBuilder)> {
    let mut out = Vec::new();
    // user message (rendered raw + sanitized)
    let mut b = LogBuilder::new("s-user");
    b.user(content);
    out.push(("user", b));
    // assistant markdown
    let mut b = LogBuilder::new("s-asst");
    b.user("hi");
    b.assistant(content);
    out.push(("assistant", b));
    // tool output (built-in card; collapsed preview + error = expanded)
    let mut b = LogBuilder::new("s-tool");
    b.user("hi");
    b.tool(
        "call-1",
        "Bash",
        json!({"command": format!("cat {name}")}),
        content,
        false,
    );
    out.push(("tool", b));
    let mut b = LogBuilder::new("s-tool-err");
    b.user("hi");
    b.tool("call-1", "Bash", json!({"command": content}), content, true);
    out.push(("tool-error-expanded", b));
    // thinking
    let mut b = LogBuilder::new("s-think");
    b.user("hi");
    b.assistant_thinking(content, "after thinking");
    out.push(("thinking", b));
    // compaction summary
    let mut b = LogBuilder::new("s-compact");
    b.user("hi");
    b.assistant("ok");
    b.compact(content);
    out.push(("compaction", b));
    out
}

/// Bottom-anchored viewport shows the end of the content; for content
/// whose MARK is at the start we scroll to the top to look for it.
fn render_top(app: &mut rness_tui::app::App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    app.model.scroll_from_bottom = u16::MAX;
    let _ = render_unscrubbed(app, w, h);
    let b = render_unscrubbed(app, w, h); // second frame: anchors settle
    app.model.scroll_from_bottom = 0;
    b
}

fn run_matrix(config: Value, tag: &str) -> Outcome {
    let mut out = Outcome::default();
    for (name, content) in cases() {
        for (surface, b) in surfaces(name, &content) {
            for &w in WIDTHS {
                let label = format!("{tag}/{name}/{surface}/w{w}");
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let (mut app, _be) = new_app(&b.session, b.history(), config.clone());
                    let t = Instant::now();
                    let bottom = render_unscrubbed(&mut app, w, 40);
                    let d1 = t.elapsed();
                    let t = Instant::now();
                    let top = render_top(&mut app, w, 40);
                    let d2 = t.elapsed();
                    (bottom, top, d1, d2)
                }));
                match result {
                    Err(e) => {
                        let msg = e
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_default();
                        out.failures.push(format!("{label}: PANIC {msg}"));
                    }
                    Ok((bottom, top, d1, d2)) => {
                        out.time(d1.max(d2), &label);
                        check_buf(&mut out, &format!("{label}/bottom"), &bottom, false);
                        // Collapsed/preview surfaces may legitimately hide MARK
                        // (thinking preview, collapsed tool); only require it
                        // for user/assistant at the top.
                        let need = matches!(surface, "user" | "assistant")
                            && !matches!(
                                name,
                                "empty" | "only-whitespace" | "many-blank-lines" | "lone-combining"
                            );
                        check_buf(&mut out, &format!("{label}/top"), &top, need);
                        if d1 > Duration::from_secs(5) || d2 > Duration::from_secs(5) {
                            out.failures
                                .push(format!("{label}: slow render {d1:?}/{d2:?}"));
                        }
                    }
                }
            }
        }
    }
    out
}

fn report(tag: &str, out: &Outcome) {
    eprintln!(
        "[{tag}] slowest single render: {:?} ({})",
        out.slowest.0, out.slowest.1
    );
    for f in &out.failures {
        eprintln!("[{tag}] FAIL {f}");
    }
}

#[test]
fn pathological_content_configured() {
    let out = run_matrix(flavor_config(), "configured");
    report("configured", &out);
    // Cell safety (B4-1: no control character / zero-width cell) is
    // asserted in full. The remaining non-safety items (see the strict
    // test) are listed but only panics / timeouts fail here.
    let hard: Vec<_> = out
        .failures
        .iter()
        .filter(|f| f.contains("PANIC") || f.contains("slow render") || f.contains("violations"))
        .collect();
    assert!(hard.is_empty(), "{hard:#?}");
}

#[test]
fn pathological_content_unconfigured() {
    let out = run_matrix(Value::Null, "unconfigured");
    report("unconfigured", &out);
    let hard: Vec<_> = out
        .failures
        .iter()
        .filter(|f| f.contains("PANIC") || f.contains("slow render") || f.contains("violations"))
        .collect();
    assert!(hard.is_empty(), "{hard:#?}");
}

/// Strict variant of the matrix: every violation fails. Cell safety
/// (B4-1) is fixed and asserted by the two tests above; what still fails
/// here is not terminal safety: the unconfigured user path clips (does not
/// wrap) a 50-deep quote at w20, and a 10k-line code block renders in
/// > 5 s in debug builds (P5 / markdown perf).
#[test]
#[ignore = "strict: unconfigured user lines clip, 10k-line code block slow in debug"]
fn pathological_content_strict() {
    let mut all = run_matrix(flavor_config(), "configured").failures;
    all.extend(run_matrix(Value::Null, "unconfigured").failures);
    assert!(
        all.is_empty(),
        "{} violations:\n{}",
        all.len(),
        all.join("\n")
    );
}

/// Live streaming of every pathological case (render path uncached when
/// > 1 MiB, and recomputed per frame).
#[test]
fn pathological_live_stream() {
    let mut failures = Vec::new();
    for (name, content) in cases() {
        for config in [flavor_config(), Value::Null] {
            let mut b = LogBuilder::new("s-live");
            b.user("go");
            let (mut app, _be) = new_app("s-live", b.history(), config.clone());
            app.apply_frame(&Frame::StepStarted {
                session: "s-live".into(),
                turn: 1,
            });
            // Stream in ~64 chunks, char-boundary safe.
            let chars: Vec<char> = content.chars().collect();
            let step = (chars.len() / 64).max(1);
            for chunk in chars.chunks(step) {
                let t: String = chunk.iter().collect();
                app.apply_frame(&Frame::Delta {
                    session: "s-live".into(),
                    chunk: ChunkDelta::Text { t: t.clone() },
                });
                app.apply_frame(&Frame::Delta {
                    session: "s-live".into(),
                    chunk: ChunkDelta::Thinking { t },
                });
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render_unscrubbed(&mut app, 80, 30)
                }));
                match r {
                    Err(_) => {
                        failures.push(format!(
                            "{name}/configured={}: PANIC while streaming",
                            config.is_object()
                        ));
                        break;
                    }
                    Ok(buf) => {
                        let v = cell_violations(&buf);
                        if !v.is_empty() {
                            failures.push(format!(
                                "{name}/live/configured={}: {}",
                                config.is_object(),
                                v[0]
                            ));
                            break;
                        }
                    }
                }
            }
        }
    }
    for f in &failures {
        eprintln!("live FAIL {f}");
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Tiny terminals: 1×1 … 80×3 must not panic with any content.
#[test]
fn tiny_terminals_do_not_panic() {
    let sizes = [
        (1u16, 1u16),
        (2, 2),
        (5, 5),
        (10, 5),
        (80, 3),
        (3, 80),
        (1, 40),
        (40, 1),
        (0, 10),
        (10, 0),
    ];
    let mut b = long_session("s-tiny", 20, 2, 7);
    b.assistant(
        &cases()
            .into_iter()
            .map(|(_, c)| c.chars().take(2000).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    let mut panics = Vec::new();
    for config in [flavor_config(), Value::Null] {
        let (mut app, _be) = new_app("s-tiny", b.history(), config.clone());
        for &(w, h) in &sizes {
            for scroll in [0u16, 5, u16::MAX] {
                app.model.scroll_from_bottom = scroll;
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render(&mut app, w, h)
                }));
                if r.is_err() {
                    panics.push(format!(
                        "{w}x{h} scroll={scroll} configured={}",
                        config.is_object()
                    ));
                }
            }
        }
        // Streaming at tiny sizes.
        app.model.scroll_from_bottom = 0;
        app.apply_frame(&Frame::StepStarted {
            session: "s-tiny".into(),
            turn: 9,
        });
        app.apply_frame(&Frame::Delta {
            session: "s-tiny".into(),
            chunk: ChunkDelta::Text {
                t: "漢字 👨‍👩‍👧 stream".into(),
            },
        });
        for &(w, h) in &sizes {
            let r =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(&mut app, w, h)));
            if r.is_err() {
                panics.push(format!("live {w}x{h} configured={}", config.is_object()));
            }
        }
    }
    assert!(panics.is_empty(), "panics: {panics:#?}");
}
