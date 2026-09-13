use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use rness_protocol::events::{ContentPart, SessionEvent};
use serde_json::{json, Value};

use crate::service::SessionService;

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
            run.finished_ms = Some(run.started.elapsed().as_millis() as u64);
        }
    }

    pub fn child_for_call(&self, parent: &str, call: &str) -> Option<String> {
        self.runs
            .lock()
            .unwrap()
            .values()
            .find(|run| run.parent == parent && run.call == call)
            .map(|run| run.child.clone())
    }

    fn tool_start(run: &mut Run, call: &str, name: &str, args: Value) {
        if let Some(tool) = run.tools.iter_mut().find(|tool| tool["call"] == call) {
            tool["name"] = json!(name);
            tool["args"] = args;
            tool["status"] = json!("running");
        } else {
            run.tools
                .push(json!({"call":call,"name":name,"args":args,"status":"running"}));
        }
    }

    pub fn stream(&self, session: &str, call: &str, text: &str) {
        if let Some(run) = self.runs.lock().unwrap().get_mut(session) {
            let output = run.streams.entry(call.into()).or_default();
            output.push_str(text);
            if output.len() > 65536 {
                let mut cut = output.len() - 65536;
                while !output.is_char_boundary(cut) {
                    cut += 1;
                }
                output.drain(..cut);
            }
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
        for child in sessions.list()? {
            let Some(link) = sessions.store().delegation(&child)? else {
                continue;
            };
            if link.parent != parent {
                continue;
            }
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
                    run.status = "running".into();
                    run.finished_ms = None;
                    run.activity = "Thinking".into();
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
                    run.live.push_str(t);
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
                let event_ms = event.id.parse::<ulid::Ulid>().ok().map(|id| id.timestamp_ms());
                run.after = Some(event.id);
                match event.event {
                    SessionEvent::TurnStarted { .. } => { run.start_ms = event_ms; }

                    SessionEvent::AssistantMessage(message) => {
                        for part in message.content {
                            match part {
                                ContentPart::Text { text } => run.lines.extend(text.lines().map(str::to_owned)),
                                ContentPart::ToolUse { call, name, args } => {
                                    let detail = args.get("command").and_then(Value::as_str)
                                        .map(str::to_owned).unwrap_or_else(|| args.to_string());
                                    run.activity = format!("{name} · {detail}");
                                    run.lines.push(format!("> {}", run.activity));
                                    Self::tool_start(run, &call, &name, args);
                                }
                                _ => {}
                            }
                        }
                    }
                    SessionEvent::ToolResult(result) => {
                        run.streams.remove(&result.call);
                        let output = rness_protocol::events::ToolResult::text_output(&result.effective_content());
                        run.lines.extend(output.lines().map(str::to_owned));
                        if let Some(tool) = run.tools.iter_mut().find(|tool| tool["call"] == result.call) {
                            tool["status"] = json!(if result.is_error { "failed" } else { "done" });
                            tool["output"] = json!(output);
                            if let Some(object) = tool.as_object_mut() { object.remove("stream"); }
                        } else {
                            run.tools.push(json!({"call":result.call,"name":result.name,"args":Value::Null,
                                "status":if result.is_error { "failed" } else { "done" },"output":output}));
                        }
                        run.activity = if result.is_error { format!("{} failed", result.name) } else { "Thinking".into() };
                    }
                    SessionEvent::TurnEnded { outcome, .. } => {
                        run.status = match outcome {
                            rness_protocol::events::TurnOutcome::Completed => "completed",
                            rness_protocol::events::TurnOutcome::Cancelled => "cancelled",
                            _ => "error",
                        }.into();
                        if run.status == "cancelled" {
                            for tool in &mut run.tools {
                                if tool["status"] == "running" { tool["status"] = json!("cancelled"); }
                            }
                        }
                        run.activity = run.status.clone();
                        run.finished_ms = Some(if run.recovered {
                            event_ms.zip(run.start_ms).map(|(end, start)| end.saturating_sub(start)).unwrap_or(0)
                        } else { run.started.elapsed().as_millis() as u64 });
                    }
                    _ => {}
                }
            }
            (run.call.clone(), run.args.clone(), json!({
                "kind":"subagent_activity", "session":run.child, "status":run.status,
                "activity":run.activity, "elapsed_ms":run.finished_ms.unwrap_or_else(|| run.started.elapsed().as_millis() as u64),
                "lines":run.lines, "live":run.live, "streams":run.streams, "tools":run.tools,
            }))
        }).collect()
    }
}
