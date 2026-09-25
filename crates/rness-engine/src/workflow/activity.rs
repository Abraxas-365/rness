//! Live progress of running workflows, keyed by the parent's tool call:
//! the source of the TUI card (snapshots polled by the frontend) and of the
//! durable presentation metadata stored with the final tool result.
//!
//! Bounded: logs and rendered member rows are capped; member rows beyond
//! the cap are summarized by the per-status counts.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use rness_protocol::events::SessionId;
use serde_json::{json, Value};

use super::{AgentOutcome, WorkflowEvent, WorkflowResult, WorkflowStop};

/// Log lines kept per run (most recent).
const MAX_LOGS: usize = 6;
/// Member rows in a snapshot; the rest are only counted.
const MAX_MEMBERS: usize = 12;
/// Finished runs kept for the live card before the oldest is dropped.
const MAX_FINISHED: usize = 32;
/// Bytes kept per label / log line / error.
const MAX_TEXT: usize = 240;
/// Planned phase titles kept in the snapshot.
const MAX_PHASES: usize = 16;

/// One live card to render: the tool call, its args and the progress
/// snapshot (`presentation`). Only running runs have live cards; a finished
/// run's card comes from its durable tool result (authoritative after
/// post-tool hooks).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowCard {
    pub call: String,
    pub args: Value,
    pub presentation: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

struct Member {
    seq: usize,
    label: String,
    phase: Option<String>,
    session: Option<SessionId>,
    status: Status,
}

struct Progress {
    parent: SessionId,
    args: Value,
    name: String,
    description: String,
    phases: Vec<String>,
    phase: Option<String>,
    logs: VecDeque<String>,
    members: Vec<Member>,
    started: Instant,
    /// `None` while running; the final state afterwards.
    ended: Option<(WorkflowStop, u64, Option<String>)>,
    finished_order: u64,
}

fn clip(text: &str) -> String {
    if text.len() <= MAX_TEXT {
        return text.to_owned();
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

impl Progress {
    fn snapshot(&self) -> Value {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for member in &self.members {
            *counts.entry(member.status.as_str()).or_default() += 1;
        }
        // Running members first (what is happening now), then the most
        // recently accepted ones; shown in `agent()` order.
        let mut shown: Vec<&Member> = self
            .members
            .iter()
            .filter(|m| m.status == Status::Running)
            .collect();
        for member in self.members.iter().rev() {
            if shown.len() >= MAX_MEMBERS {
                break;
            }
            if member.status != Status::Running {
                shown.push(member);
            }
        }
        shown.truncate(MAX_MEMBERS);
        shown.sort_by_key(|m| m.seq);
        let (status, elapsed_ms, error) = match &self.ended {
            None => ("running", self.started.elapsed().as_millis() as u64, None),
            Some((stop, ms, error)) => (
                match stop {
                    WorkflowStop::Completed => "completed",
                    WorkflowStop::Error => "error",
                    WorkflowStop::Cancelled => "cancelled",
                },
                *ms,
                error.clone(),
            ),
        };
        json!({
            "version": 1,
            "kind": "workflow_activity",
            "name": self.name,
            "description": self.description,
            "phases": self.phases,
            "phase": self.phase,
            "status": status,
            "elapsed_ms": elapsed_ms,
            "error": error,
            "logs": self.logs,
            "total": self.members.len(),
            "counts": {
                "queued": counts.get("queued").copied().unwrap_or(0),
                "running": counts.get("running").copied().unwrap_or(0),
                "completed": counts.get("completed").copied().unwrap_or(0),
                "failed": counts.get("failed").copied().unwrap_or(0),
                "cancelled": counts.get("cancelled").copied().unwrap_or(0),
            },
            "members": shown.iter().map(|m| json!({
                "seq": m.seq,
                "label": m.label,
                "phase": m.phase,
                "status": m.status.as_str(),
                "session": m.session,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Shared registry of workflow runs (one per `workflow` tool call).
#[derive(Default)]
pub struct WorkflowActivity {
    runs: Mutex<HashMap<String, Progress>>,
    finished: Mutex<u64>,
}

impl WorkflowActivity {
    /// A run started for `call` in `parent`. `args` are the tool-call args
    /// (the card's input).
    pub fn begin(&self, parent: &SessionId, call: &str, args: Value, meta: &super::WorkflowMeta) {
        self.runs.lock().unwrap().insert(
            call.to_owned(),
            Progress {
                parent: parent.clone(),
                args,
                name: clip(&meta.name),
                description: clip(&meta.description),
                phases: meta
                    .phases
                    .iter()
                    .take(MAX_PHASES)
                    .map(|p| clip(&p.title))
                    .collect(),
                phase: None,
                logs: VecDeque::new(),
                members: Vec::new(),
                started: Instant::now(),
                ended: None,
                finished_order: 0,
            },
        );
    }

    /// Fold one engine progress event into the run's state.
    pub fn apply(&self, call: &str, event: WorkflowEvent) {
        let mut runs = self.runs.lock().unwrap();
        let Some(run) = runs.get_mut(call) else {
            return;
        };
        match event {
            WorkflowEvent::Phase(title) => run.phase = Some(clip(&title)),
            WorkflowEvent::Log(message) => {
                run.logs.push_back(clip(&message));
                while run.logs.len() > MAX_LOGS {
                    run.logs.pop_front();
                }
            }
            WorkflowEvent::AgentQueued { seq, label, phase } => run.members.push(Member {
                seq,
                label: clip(&label),
                phase: phase.map(|p| clip(&p)),
                session: None,
                status: Status::Queued,
            }),
            WorkflowEvent::AgentStarted { seq, child } => {
                if let Some(member) = run.members.iter_mut().find(|m| m.seq == seq) {
                    member.session = Some(child);
                    member.status = Status::Running;
                }
            }
            WorkflowEvent::AgentEnded { seq, outcome } => {
                if let Some(member) = run.members.iter_mut().find(|m| m.seq == seq) {
                    member.status = match outcome {
                        AgentOutcome::Completed => Status::Completed,
                        AgentOutcome::Failed => Status::Failed,
                        AgentOutcome::Cancelled => Status::Cancelled,
                    };
                }
            }
        }
    }

    /// The run settled: freeze its state and return the final snapshot
    /// (stored as the tool result's presentation metadata).
    pub fn finish(&self, call: &str, result: &WorkflowResult) -> Option<Value> {
        self.end(call, result.stop, result.error.clone())
    }

    /// Mark a run that ended without a result (its tool body was dropped).
    pub fn abandon(&self, call: &str) {
        let running = self
            .runs
            .lock()
            .unwrap()
            .get(call)
            .is_some_and(|run| run.ended.is_none());
        if running {
            self.end(call, WorkflowStop::Cancelled, None);
        }
    }

    fn end(&self, call: &str, stop: WorkflowStop, error: Option<String>) -> Option<Value> {
        let order = {
            let mut finished = self.finished.lock().unwrap();
            *finished += 1;
            *finished
        };
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(call)?;
        let elapsed = run.started.elapsed().as_millis() as u64;
        for member in &mut run.members {
            if matches!(member.status, Status::Queued | Status::Running) {
                member.status = Status::Cancelled;
            }
        }
        run.ended = Some((stop, elapsed, error.map(|e| clip(&e))));
        run.finished_order = order;
        let snapshot = run.snapshot();
        let mut ended: Vec<(u64, String)> = runs
            .iter()
            .filter(|(_, run)| run.ended.is_some())
            .map(|(call, run)| (run.finished_order, call.clone()))
            .collect();
        if ended.len() > MAX_FINISHED {
            ended.sort();
            for (_, call) in ended.iter().take(ended.len() - MAX_FINISHED) {
                runs.remove(call);
            }
        }
        Some(snapshot)
    }

    /// `(call, tool args, snapshot)` for every run started from `parent`.
    pub fn card_snapshots(&self, parent: &str) -> Vec<(String, Value, Value)> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, run)| run.parent == parent)
            .map(|(call, run)| (call.clone(), run.args.clone(), run.snapshot()))
            .collect()
    }

    /// Whether `call` is a run that has not finished yet.
    pub fn is_running(&self, call: &str) -> bool {
        self.runs
            .lock()
            .unwrap()
            .get(call)
            .is_some_and(|run| run.ended.is_none())
    }

    /// Live cards for the still-running runs started from `parent`.
    /// `elapsed_ms` is rounded down to whole seconds so an unchanged run
    /// compares equal between polls (the publisher re-renders on change).
    pub fn cards(&self, parent: &str) -> Vec<WorkflowCard> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, run)| run.parent == parent && run.ended.is_none())
            .map(|(call, run)| {
                let mut presentation = run.snapshot();
                let ms = presentation["elapsed_ms"].as_u64().unwrap_or(0);
                presentation["elapsed_ms"] = json!(ms / 1000 * 1000);
                WorkflowCard {
                    call: call.clone(),
                    args: run.args.clone(),
                    presentation,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::WorkflowMeta;

    fn meta() -> WorkflowMeta {
        WorkflowMeta {
            name: "audit".into(),
            description: "d".into(),
            when_to_use: None,
            phases: vec![],
        }
    }

    #[test]
    fn tracks_members_and_freezes_on_finish() {
        let activity = WorkflowActivity::default();
        activity.begin(&"p".into(), "c1", json!({"script":"x"}), &meta());
        activity.apply("c1", WorkflowEvent::Phase("Scan".into()));
        for seq in 0..3 {
            activity.apply(
                "c1",
                WorkflowEvent::AgentQueued {
                    seq,
                    label: format!("m{seq}"),
                    phase: Some("Scan".into()),
                },
            );
        }
        activity.apply(
            "c1",
            WorkflowEvent::AgentStarted {
                seq: 0,
                child: "s0".into(),
            },
        );
        activity.apply(
            "c1",
            WorkflowEvent::AgentEnded {
                seq: 0,
                outcome: AgentOutcome::Completed,
            },
        );
        activity.apply(
            "c1",
            WorkflowEvent::AgentStarted {
                seq: 1,
                child: "s1".into(),
            },
        );
        let snaps = activity.card_snapshots("p");
        assert_eq!(snaps.len(), 1);
        let snap = &snaps[0].2;
        assert_eq!(snap["status"], "running");
        assert_eq!(snap["phase"], "Scan");
        assert_eq!(snap["counts"]["completed"], 1);
        assert_eq!(snap["counts"]["running"], 1);
        assert_eq!(snap["counts"]["queued"], 1);
        assert_eq!(snap["members"][1]["session"], "s1");
        assert!(activity.card_snapshots("other").is_empty());

        let result = WorkflowResult::failed(WorkflowStop::Error, "boom".into(), 2);
        let last = activity.finish("c1", &result).unwrap();
        assert_eq!(last["status"], "error");
        assert_eq!(last["error"], "boom");
        // Unsettled members are cancelled with the run.
        assert_eq!(last["counts"]["cancelled"], 2);
        let frozen = last["elapsed_ms"].clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(activity.card_snapshots("p")[0].2["elapsed_ms"], frozen);
    }

    #[test]
    fn caps_rows_logs_and_finished_runs() {
        let activity = WorkflowActivity::default();
        activity.begin(&"p".into(), "c", json!({}), &meta());
        for seq in 0..40 {
            activity.apply(
                "c",
                WorkflowEvent::AgentQueued {
                    seq,
                    label: "x".repeat(1000),
                    phase: None,
                },
            );
            activity.apply("c", WorkflowEvent::Log(format!("log {seq}")));
        }
        let snap = &activity.card_snapshots("p")[0].2;
        assert_eq!(snap["total"], 40);
        assert_eq!(snap["members"].as_array().unwrap().len(), MAX_MEMBERS);
        assert_eq!(snap["members"][0]["seq"], 40 - MAX_MEMBERS);
        assert!(snap["members"][0]["label"].as_str().unwrap().len() <= MAX_TEXT + 3);
        assert_eq!(snap["logs"].as_array().unwrap().len(), MAX_LOGS);
        assert_eq!(snap["logs"][MAX_LOGS - 1], "log 39");

        let done = WorkflowResult::failed(WorkflowStop::Cancelled, "x".into(), 0);
        for i in 0..MAX_FINISHED + 5 {
            let call = format!("f{i}");
            activity.begin(&"p".into(), &call, json!({}), &meta());
            activity.finish(&call, &done);
        }
        activity.finish("c", &done);
        let snaps = activity.card_snapshots("p");
        assert_eq!(snaps.len(), MAX_FINISHED);
        assert!(snaps.iter().any(|(call, ..)| call == "c"));
        assert!(!snaps.iter().any(|(call, ..)| call == "f0"));
    }

    #[test]
    fn abandon_only_ends_running_runs() {
        let activity = WorkflowActivity::default();
        activity.begin(&"p".into(), "c", json!({}), &meta());
        activity.abandon("c");
        assert_eq!(activity.card_snapshots("p")[0].2["status"], "cancelled");
        activity.begin(&"p".into(), "d", json!({}), &meta());
        let ok = WorkflowResult {
            stop: WorkflowStop::Completed,
            value: Some(json!(1)),
            error: None,
            agents_started: 0,
        };
        activity.finish("d", &ok);
        activity.abandon("d");
        let snaps = activity.card_snapshots("p");
        let d = snaps.iter().find(|(call, ..)| call == "d").unwrap();
        assert_eq!(d.2["status"], "completed");
    }

    #[test]
    fn live_cards_are_only_for_running_runs() {
        let activity = WorkflowActivity::default();
        let mut long = meta();
        long.phases = (0..40)
            .map(|i| crate::workflow::MetaPhase {
                title: format!("{i}{}", "t".repeat(1000)),
                detail: None,
            })
            .collect();
        activity.begin(&"p".into(), "c", json!({}), &long);
        let cards = activity.cards("p");
        assert_eq!(cards.len(), 1);
        assert!(activity.is_running("c"));
        let phases = cards[0].presentation["phases"].as_array().unwrap();
        assert_eq!(phases.len(), MAX_PHASES);
        assert!(phases[0].as_str().unwrap().len() <= MAX_TEXT + 3);
        let done = WorkflowResult::failed(WorkflowStop::Cancelled, "x".into(), 0);
        activity.finish("c", &done);
        // The durable result renders the finished card, not the live path.
        assert!(activity.cards("p").is_empty());
        assert!(!activity.is_running("c"));
    }
}
