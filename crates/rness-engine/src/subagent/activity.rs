use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rness_protocol::events::{ContentPart, SessionEvent};
use serde_json::{json, Value};

use crate::service::SessionService;

const MAX_TOOLS: usize = 100;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_LINES: usize = 1000;
const MAX_ACTIVITY_BYTES: usize = 4096;

/// Keep a UTF-8-safe tail; full content belongs in the session drawer.
fn tail(text: &str, limit: usize) -> &str {
    let mut cut = text.len().saturating_sub(limit);
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    &text[cut..]
}

fn append_tail(output: &mut String, text: &str) {
    if text.len() >= MAX_TEXT_BYTES {
        *output = tail(text, MAX_TEXT_BYTES).to_owned();
    } else {
        let keep = MAX_TEXT_BYTES - text.len();
        let cut = output.len() - tail(output, keep).len();
        output.drain(..cut);
        output.push_str(text);
    }
}

pub struct ToolStreamEv;
impl rness_kernel::Event for ToolStreamEv {
    const NAME: &'static str = "tool/stream";
    type Payload = (String, String, String);
}

struct Run {
    parent: String,
    call: String,
    child: String,
    args: Value,
    after: Option<String>,
    started: Instant,
    lines: Vec<String>,
    activity: String,
    status: String,
    live: String,
    finished_ms: Option<u64>,
    streams: std::collections::BTreeMap<String, String>,
    tools: Vec<Value>,
    recovered: bool,
    start_ms: Option<u64>,
}

impl Run {
    fn elapsed_ms(&self) -> u64 {
        self.finished_ms.unwrap_or_else(|| {
            self.start_ms
                .map(|start| {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default();
                    (now.as_millis() as u64).saturating_sub(start)
                })
                .unwrap_or_else(|| self.started.elapsed().as_millis() as u64)
        })
    }

    fn push_text(&mut self, text: &str) {
        self.lines
            .extend(tail(text, MAX_TEXT_BYTES - 1).lines().map(str::to_owned));
        let mut bytes = 0;
        let keep = self
            .lines
            .iter()
            .rev()
            .take(MAX_LINES)
            .take_while(|line| {
                bytes += line.len() + 1;
                bytes <= MAX_TEXT_BYTES
            })
            .count();
        self.lines.drain(..self.lines.len() - keep);
    }

    fn trim_tools(&mut self) {
        while self.tools.len() > MAX_TOOLS {
            let removed = self.tools.remove(0);
            if let Some(call) = removed["call"].as_str() {
                self.streams.remove(call);
            }
        }
    }

    fn apply_event(&mut self, event: SessionEvent, event_ms: Option<u64>) {
        match event {
            SessionEvent::TurnStarted { .. } => {
                self.start_ms = event_ms;
                self.started = Instant::now();
                // Replaying an unfinished turn after restart must not make it live.
                self.status = if self.recovered {
                    "interrupted"
                } else {
                    "running"
                }
                .into();
                self.activity = if self.recovered {
                    "Interrupted"
                } else {
                    "Thinking"
                }
                .into();
                self.finished_ms = self.recovered.then_some(0);
            }
            SessionEvent::AssistantMessage(message) => {
                for part in message.content {
                    match part {
                        ContentPart::Text { text } => self.push_text(&text),
                        ContentPart::ToolUse { call, name, args } => {
                            let detail = args
                                .get("command")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .unwrap_or_else(|| args.to_string());
                            self.activity =
                                tail(&format!("{name} · {detail}"), MAX_ACTIVITY_BYTES).into();
                            self.push_text(&format!("> {}", self.activity));
                            SubagentActivity::tool_start(self, &call, &name, args);
                        }
                        _ => {}
                    }
                }
            }
            SessionEvent::ToolResult(result) => {
                self.streams.remove(&result.call);
                let output =
                    rness_protocol::events::ToolResult::text_output(&result.effective_content());
                // Tool output is kept once, in the bounded tool detail, not in lines too.
                let output = tail(&output, MAX_TEXT_BYTES);
                if let Some(tool) = self
                    .tools
                    .iter_mut()
                    .find(|tool| tool["call"] == result.call)
                {
                    tool["status"] = json!(if result.is_error { "failed" } else { "done" });
                    tool["output"] = json!(output);
                    if let Some(object) = tool.as_object_mut() {
                        object.remove("stream");
                    }
                } else {
                    self.tools.push(
                        json!({"call":result.call,"name":result.name,"args":Value::Null,
                        "status":if result.is_error { "failed" } else { "done" },"output":output}),
                    );
                    self.trim_tools();
                }
                self.activity = if result.is_error {
                    tail(&format!("{} failed", result.name), MAX_ACTIVITY_BYTES).into()
                } else {
                    "Thinking".into()
                };
            }
            SessionEvent::TurnEnded { outcome, .. } => {
                self.status = match outcome {
                    rness_protocol::events::TurnOutcome::Completed => "completed",
                    rness_protocol::events::TurnOutcome::Cancelled => "cancelled",
                    _ => "error",
                }
                .into();
                for tool in &mut self.tools {
                    if tool["status"] == "running" {
                        tool["status"] = json!(match self.status.as_str() {
                            "cancelled" => "cancelled",
                            "error" => "failed",
                            _ => "done",
                        });
                    }
                }
                self.activity = self.status.clone();
                self.live.clear();
                self.finished_ms = Some(
                    event_ms
                        .zip(self.start_ms)
                        .map(|(end, start)| end.saturating_sub(start))
                        .unwrap_or_else(|| self.elapsed_ms()),
                );
            }
            _ => {}
        }
        if self.recovered && self.status == "interrupted" {
            self.finished_ms = Some(
                event_ms
                    .zip(self.start_ms)
                    .map(|(end, start)| end.saturating_sub(start))
                    .unwrap_or(0),
            );
        }
    }
}

/// Presentation-only state. Child history remains authoritative and is never
/// copied into the parent's model context.
#[derive(Default)]
pub struct SubagentActivity {
    runs: Mutex<HashMap<String, Run>>,
}

impl SubagentActivity {
    pub(super) fn register(
        &self,
        sessions: &SessionService,
        parent: &str,
        call: &str,
        child: &str,
        args: Value,
    ) {
        let after = sessions
            .store()
            .history(&child.to_owned())
            .ok()
            .and_then(|history| history.last().map(|event| event.id.clone()));
        self.runs.lock().unwrap().insert(
            child.into(),
            Run {
                parent: parent.into(),
                call: call.into(),
                child: child.into(),
                args,
                after,
                started: Instant::now(),
                lines: Vec::new(),
                activity: "Starting".into(),
                status: "running".into(),
                live: String::new(),
                finished_ms: None,
                streams: Default::default(),
                tools: Vec::new(),
                recovered: false,
                start_ms: None,
            },
        );
    }

    pub(super) fn failed(&self, child: &str) {
        if let Some(run) = self.runs.lock().unwrap().get_mut(child) {
            run.status = "error".into();
            run.activity = "error".into();
            run.finished_ms = Some(run.elapsed_ms());
        }
    }

    fn tool_start(run: &mut Run, call: &str, name: &str, args: Value) {
        let serialized = args.to_string();
        let args = if serialized.len() > MAX_TEXT_BYTES {
            json!({"summary":tail(&serialized, MAX_TEXT_BYTES), "truncated":true})
        } else {
            args
        };
        if let Some(tool) = run.tools.iter_mut().find(|tool| tool["call"] == call) {
            if !name.is_empty() {
                tool["name"] = json!(name);
            }
            if !args.is_null() {
                tool["args"] = args;
            }
        } else {
            run.tools
                .push(json!({"call":call,"name":name,"args":args,"status":"running"}));
            run.trim_tools();
        }
    }

    pub fn stream(&self, session: &str, call: &str, text: &str) {
        if let Some(run) = self.runs.lock().unwrap().get_mut(session) {
            if !run.tools.iter().any(|tool| tool["call"] == call) {
                Self::tool_start(run, call, "", Value::Null);
            }
            let output = run.streams.entry(call.into()).or_default();
            append_tail(output, text);
            if let Some(tool) = run.tools.iter_mut().find(|tool| tool["call"] == call) {
                tool["stream"] = json!(output);
            }
        }
    }

    pub fn recover(
        &self,
        sessions: &SessionService,
        parent: &str,
    ) -> Result<(), crate::service::ServiceError> {
        let history = sessions.store().history(&parent.to_owned())?;
        let mut calls = HashMap::new();
        for event in history {
            if let SessionEvent::AssistantMessage(message) = event.event {
                for part in message.content {
                    if let ContentPart::ToolUse { call, name, args } = part {
                        if name == "subagent" {
                            calls.insert(call, args);
                        }
                    }
                }
            }
        }
        // Targeted lookup: only scan headers for children delegated from
        // this parent, instead of listing every session in the store.
        for (child, link) in sessions.store().delegated_children(&parent.to_owned())? {
            let Some(call) = link.call else { continue };
            let Some(args) = calls.get(&call) else {
                continue;
            };
            let mut runs = self.runs.lock().unwrap();
            if runs.contains_key(&child) {
                continue;
            }
            let after = sessions.store().parent(&child)?.map(|fork| fork.at);
            runs.insert(
                child.clone(),
                Run {
                    parent: parent.into(),
                    call,
                    child,
                    args: args.clone(),
                    after,
                    started: Instant::now(),
                    lines: Vec::new(),
                    activity: "Interrupted".into(),
                    status: "interrupted".into(),
                    live: String::new(),
                    finished_ms: Some(0),
                    streams: Default::default(),
                    tools: Vec::new(),
                    recovered: true,
                    start_ms: None,
                },
            );
        }
        Ok(())
    }

    pub fn observe(&self, frame: &rness_protocol::frames::Frame) {
        use rness_protocol::events::ChunkDelta;
        use rness_protocol::frames::Frame;
        let mut runs = self.runs.lock().unwrap();
        match frame {
            Frame::StepStarted { session, .. } => {
                if let Some(run) = runs.get_mut(session) {
                    if run.status != "running" {
                        run.started = Instant::now();
                        run.start_ms = None;
                    }
                    run.recovered = false;
                    run.status = "running".into();
                    run.finished_ms = None;
                    run.activity = "Thinking".into();
                }
            }
            Frame::ToolStarted {
                session,
                call,
                name,
            } => {
                if let Some(run) = runs.get_mut(session) {
                    run.activity = tail(name, MAX_ACTIVITY_BYTES).into();
                    Self::tool_start(run, call, name, Value::Null);
                }
            }
            Frame::StepCommitted { session, .. } => {
                if let Some(run) = runs.get_mut(session) {
                    run.live.clear();
                }
            }
            Frame::Delta {
                session,
                chunk: ChunkDelta::Text { t },
            } => {
                if let Some(run) = runs.get_mut(session) {
                    append_tail(&mut run.live, t);
                }
            }
            _ => {}
        }
    }

    pub fn snapshots(
        &self,
        sessions: &SessionService,
        parent: &str,
    ) -> Vec<(String, Value, Value)> {
        let mut runs = self.runs.lock().unwrap();
        runs.values_mut().filter(|run| run.parent == parent).map(|run| {
            let history = match &run.after {
                Some(after) => sessions.store().history_after(&run.child, after).ok().flatten().unwrap_or_else(|| {
                    sessions.store().history(&run.child).unwrap_or_default().into_iter()
                        .skip_while(|event| event.id != *after).skip(1).collect()
                }),
                None => sessions.store().history(&run.child).unwrap_or_default(),
            };
            for event in history {
                let event_ms = event.at.parse::<jiff::Timestamp>().ok()
                    .and_then(|at| u64::try_from(at.as_millisecond()).ok())
                    .or_else(|| event.id.parse::<ulid::Ulid>().ok().map(|id| id.timestamp_ms()));
                run.after = Some(event.id);
                run.apply_event(event.event, event_ms);
            }
            // A live ToolStarted can precede the first history refresh for this turn.
            if run.status == "running" && run.activity == "Thinking" {
                if let Some(name) = run.tools.iter().rev()
                    .find(|tool| tool["status"] == "running")
                    .and_then(|tool| tool["name"].as_str()).filter(|name| !name.is_empty()) {
                    run.activity = tail(name, MAX_ACTIVITY_BYTES).into();
                }
            }
            (run.call.clone(), run.args.clone(), json!({
                "kind":"subagent_activity", "session":run.child, "status":run.status,
                "activity":run.activity, "elapsed_ms":run.elapsed_ms(),
                "mode":run.args.get("background_mode").or_else(|| run.args.get("mode")),
                "lines":run.lines, "live":run.live, "streams":run.streams, "tools":run.tools,
            }))
        }).collect()
    }
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
