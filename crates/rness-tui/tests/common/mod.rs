//! Shared helpers for rness-tui integration tests (WP-4 e2e/perf).
//!
//! Builds a real `App` (chat + input + statusline mounted the way the CLI
//! composition root does) over a fake `Backend` that serves a synthetic
//! session log, and renders it into ratatui buffers / TestBackend.
#![allow(dead_code)]

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use rness_protocol::api::{ClientRequest, History};
use rness_protocol::events::{Envelope, SessionId};
use rness_tui::app::{Action, App, Backend, Model};
use rness_tui::slots::Slots;
use serde_json::{json, Value};

/// Backend that serves a fixed history (replaceable for append tests).
pub struct FakeBackend {
    pub history: std::sync::RwLock<History>,
}

impl Backend for FakeBackend {
    fn request(&self, _request: ClientRequest) {}
    fn history(&self, _session: &SessionId) -> History {
        self.history.read().unwrap().clone()
    }
}

/// The default flavor's `rness.ui.messagebox` (flavors/default/lua/theme.lua),
/// minus Lua renderers and the gruvbox palette (Theme::default is used).
pub fn flavor_config() -> Value {
    json!({
        "selection": {"style": {"bg": "#504945"}, "marker": {"text": "▎", "style": {"fg": "#83a598", "bold": true}}},
        "keys": {"select_message": "alt+m", "toggle_hidden": "alt+i"},
        "user": {
            "style": {"fg": "#ebdbb2", "bg": "#3c3836"}, "padding": {"left": 1, "right": 1},
            "marker": false, "label": {"text": "You", "style": "user_prefix"},
            "sources": {
                "hook": {"visible": false, "display": "collapsed", "label": {"text": "Hook", "style": "dim"}},
                "instructions": {"visible": false, "display": "collapsed"},
                "context": {"visible": false, "display": "collapsed"},
                "job": {"display": "preview", "preview_lines": 6},
                "external": {"label": {"text": "External", "style": "tool_name"}}
            }
        },
        "assistant": {"style": "assistant_text", "marker": false},
        "thinking": {"display": "preview", "preview_lines": 3, "style": "thinking"},
        "compaction": {
            "style": {"fg": "#ebdbb2", "bg": "#3c3836"},
            "border": {"kind": "rounded", "style": {"fg": "#665c54"}},
            "padding": {"left": 1, "right": 1}, "display": "preview", "preview_lines": 6,
            "header": {"style": "tool_name"}
        },
        "tool": {
            "style": {"fg": "#ebdbb2", "bg": "#3c3836"},
            "border": {"kind": "rounded", "style": {"fg": "#665c54"}},
            "padding": {"left": 1, "right": 1}, "display": "preview", "preview_lines": 8,
            "header": {"show_name": true, "show_status": true, "show_duration": true},
            "states": {"error": {"display": "expanded", "style": "error"}}
        },
        "error": {"style": "error", "display": "expanded"}
    })
}

/// App with chat/input/statusline mounted; `config` = messagebox config
/// (Value::Null = unconfigured legacy rendering).
pub fn new_app(session: &str, history: History, config: Value) -> (App, Arc<FakeBackend>) {
    let backend = Arc::new(FakeBackend {
        history: std::sync::RwLock::new(history),
    });
    let mut slots = Slots::default();
    rness_tui::modules::chat::install(&mut slots);
    rness_tui::modules::input::install(&mut slots);
    rness_tui::modules::statusline::install(&mut slots);
    let mut app = App::new(
        Model::new(session.into(), "fake".into()),
        slots,
        backend.clone(),
    );
    if !config.is_null() {
        app.apply(Action::Custom("chat:messagebox-config".into(), config));
    }
    app.reconcile();
    (app, backend)
}

pub fn render(app: &mut App, w: u16, h: u16) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    app.render(area, &mut buf);
    buf
}

/// Like [`render`] but without the final control-character scrub, so
/// content checks see what the text sources produced.
#[allow(dead_code)]
pub fn render_unscrubbed(app: &mut App, w: u16, h: u16) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    app.render_unscrubbed(area, &mut buf);
    buf
}

/// Rows as text, skipping cells hidden behind wide glyphs.
pub fn rows(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (0..a.height)
        .map(|y| {
            let mut s = String::new();
            let mut skip = 0usize;
            for x in 0..a.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let sym = buf[(x, y)].symbol();
                s.push_str(sym);
                skip = unicode_width::UnicodeWidthStr::width(sym).saturating_sub(1);
            }
            s
        })
        .collect()
}

pub fn text(buf: &Buffer) -> String {
    rows(buf).join("\n")
}

/// Terminal-safety violations in a buffer: control characters in a cell
/// (raw ESC/CR/BS reach the terminal) and visible cells whose symbol has
/// display width 0 (ratatui's backend then skips the MoveTo and the rest of
/// the row shifts left by one column on a real terminal).
pub fn cell_violations(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    let mut out = Vec::new();
    for y in 0..a.height {
        let mut skip = 0usize;
        for x in 0..a.width {
            let sym = buf[(x, y)].symbol();
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let w = unicode_width::UnicodeWidthStr::width(sym);
            if sym.chars().any(|c| c.is_control()) {
                out.push(format!("control char at ({x},{y}): {sym:?}"));
            } else if w == 0 {
                out.push(format!("zero-width cell at ({x},{y}): {sym:?}"));
            }
            skip = w.saturating_sub(1);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Synthetic session logs (same event vocabulary the engine writes).

pub struct LogBuilder {
    pub session: String,
    pub envelopes: Vec<Envelope>,
    n: u64,
    /// Model-visible ids since the last checkpoint (compaction `replaces`).
    visible: Vec<String>,
}

impl LogBuilder {
    pub fn new(session: &str) -> Self {
        let mut b = Self {
            session: session.into(),
            envelopes: Vec::new(),
            n: 0,
            visible: Vec::new(),
        };
        b.emit(json!({"type": "session/header", "version": 1, "session": session, "workspace": "/tmp/w"}));
        b
    }

    pub fn emit(&mut self, event: Value) -> String {
        self.n += 1;
        let id = format!("01J{:023}", self.n);
        let mut v = event;
        v["id"] = json!(id);
        v["at"] = json!("2026-10-08T00:00:00.000Z");
        let env: Envelope = serde_json::from_value(v.clone())
            .unwrap_or_else(|e| panic!("bad fixture event {v}: {e}"));
        self.envelopes.push(env);
        id
    }

    pub fn user(&mut self, text: &str) -> String {
        let id = self.emit(json!({"type": "user/message", "intent": "followup",
            "content": [{"kind": "text", "text": text}]}));
        self.visible.push(id.clone());
        id
    }

    pub fn assistant(&mut self, text: &str) -> String {
        let id = self.emit(json!({"type": "assistant/message", "model": "fake",
            "content": [{"kind": "text", "text": text}], "stop": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}, "chunks": []}));
        self.visible.push(id.clone());
        id
    }

    pub fn assistant_thinking(&mut self, thinking: &str, text: &str) -> String {
        let id = self.emit(json!({"type": "assistant/message", "model": "fake",
            "content": [{"kind": "thinking", "text": thinking}, {"kind": "text", "text": text}],
            "stop": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}, "chunks": []}));
        self.visible.push(id.clone());
        id
    }

    /// Assistant tool_use + tool/result.
    pub fn tool(&mut self, call: &str, name: &str, args: Value, output: &str, is_error: bool) {
        let a = self.emit(json!({"type": "assistant/message", "model": "fake",
            "content": [{"kind": "text", "text": "running a tool"},
                        {"kind": "tool_use", "call": call, "name": name, "args": args}],
            "stop": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1}, "chunks": []}));
        let r = self.emit(json!({"type": "tool/result", "call": call, "name": name,
            "content": [{"kind": "text", "text": output}], "output": output,
            "is_error": is_error, "duration_ms": 12}));
        self.visible.push(a);
        self.visible.push(r);
    }

    /// compaction/started → summary{replaces all visible} → finished.
    pub fn compact(&mut self, summary: &str) {
        if self.visible.is_empty() {
            return;
        }
        let sources = std::mem::take(&mut self.visible);
        let started = self.emit(json!({"type": "compaction/started", "model": "fake",
            "sources": sources, "estimated_input": 1000}));
        let sum = self.emit(json!({"type": "compaction/summary", "replaces": sources,
            "model": "fake", "summary": summary}));
        self.emit(
            json!({"type": "compaction/finished", "started": started, "outcome": "committed",
            "usage": {"input_tokens": 1, "output_tokens": 1}, "chunks": []}),
        );
        self.visible.push(sum);
    }

    pub fn history(&self) -> History {
        History::new(self.session.clone(), self.envelopes.clone())
    }
}

/// Deterministic filler prose (unique per seed so caches cannot alias).
pub fn prose(seed: usize, bytes: usize) -> String {
    const W: &[&str] = &[
        "alpha",
        "beta",
        "**gamma**",
        "`delta`",
        "epsilon",
        "session",
        "engine",
        "render",
        "cache",
        "compaction",
        "provider",
        "stream",
        "tool",
        "result",
        "viewport",
        "layout",
    ];
    let mut s = format!("p{seed} ");
    let mut i = seed.wrapping_mul(2654435761);
    while s.len() < bytes {
        i = i
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push_str(W[(i >> 33) as usize % W.len()]);
        s.push(if (i >> 20) % 11 == 0 { '\n' } else { ' ' });
    }
    s
}

/// A realistic long session: `turns` turns, each user → assistant(+tool)
/// → tool result → assistant; a compaction every `compact_every` turns.
pub fn long_session(
    session: &str,
    turns: usize,
    tools_per_turn: usize,
    compact_every: usize,
) -> LogBuilder {
    let mut b = LogBuilder::new(session);
    for t in 0..turns {
        if compact_every > 0 && t > 0 && t % compact_every == 0 {
            b.compact(&format!("## Summary {t}\n\n{}", prose(t * 7 + 1, 600)));
        }
        b.user(&format!("turn {t}: {}", prose(t * 7 + 2, 120)));
        for k in 0..tools_per_turn {
            let out: String = (0..20)
                .map(|l| {
                    format!(
                        "{t}.{k}.{l} {}\n",
                        prose(t * 31 + k * 7 + l, 60).replace('\n', " ")
                    )
                })
                .collect();
            b.tool(
                &format!("call-{t}-{k}"),
                "Bash",
                json!({"command": format!("echo {t} {k}")}),
                &out,
                false,
            );
        }
        b.assistant(&format!(
            "done {t}. {}\n\n```rust\nfn f{t}() -> usize {{ {t} }}\n```\n",
            prose(t * 7 + 3, 300)
        ));
    }
    b
}

// ---------------------------------------------------------------------------
// Perf output: JSON lines compatible with scripts/e2e/perf.py records.

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Print one perf record (and append it to $RNESS_E2E_PERF_OUT if set).
pub fn record(probe: &str, metric: &str, value: f64, unit: &str, extra: Value) {
    let mut rec = json!({"probe": probe, "metric": metric, "value": (value * 1000.0).round() / 1000.0,
        "unit": unit, "wp": 4, "bin": "cargo-test-release",
        "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()});
    if let (Some(r), Some(e)) = (rec.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            r.insert(k.clone(), v.clone());
        }
    }
    let line = rec.to_string();
    println!("{line}");
    if let Ok(path) = std::env::var("RNESS_E2E_PERF_OUT") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn env_list(name: &str, default: &[usize]) -> Vec<usize> {
    std::env::var(name)
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| default.to_vec())
}
