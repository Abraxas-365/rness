use super::*;
use rness_protocol::events::{
    AssistantMessage, ChunkDelta, StopReason, ToolResult, TurnOutcome, Usage,
};
use rness_protocol::frames::Frame;

fn run(recovered: bool) -> Run {
    Run {
        parent: "parent".into(),
        call: "parent-call".into(),
        child: "child".into(),
        args: json!({"background_mode":"continuable"}),
        after: None,
        started: Instant::now(),
        lines: vec![],
        activity: "Starting".into(),
        status: "running".into(),
        live: String::new(),
        finished_ms: None,
        streams: Default::default(),
        tools: vec![],
        recovered,
        start_ms: None,
    }
}

fn activity() -> SubagentActivity {
    SubagentActivity {
        runs: Mutex::new(HashMap::from([("child".into(), run(false))])),
    }
}

fn text_event(text: String) -> SessionEvent {
    SessionEvent::AssistantMessage(AssistantMessage {
        model: "test".into(),
        content: vec![ContentPart::Text { text }],
        stop: StopReason::EndTurn,
        usage: Usage::default(),
        estimated_input: 0,
        chunks: vec![],
    })
}

#[test]
fn elapsed_uses_each_turn_timestamps_and_freezes_while_idle() {
    let mut run = run(false);
    run.apply_event(SessionEvent::TurnStarted { turn: 1 }, Some(1000));
    run.apply_event(
        SessionEvent::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed,
        },
        Some(3500),
    );
    assert_eq!(run.elapsed_ms(), 2500);
    assert_eq!(run.status, "completed");
    run.apply_event(SessionEvent::TurnStarted { turn: 2 }, Some(100_000));
    assert_eq!(run.status, "running");
    assert_eq!(run.activity, "Thinking");
    assert_eq!(run.finished_ms, None);
    run.apply_event(
        SessionEvent::TurnEnded {
            turn: 2,
            outcome: TurnOutcome::Completed,
        },
        Some(100_030),
    );
    assert_eq!(run.elapsed_ms(), 30);
    assert_eq!(run.elapsed_ms(), 30);
}

#[test]
fn recovery_uses_last_turn_and_freezes_unfinished_work() {
    let mut run = run(true);
    run.apply_event(SessionEvent::TurnStarted { turn: 1 }, Some(1000));
    run.apply_event(
        SessionEvent::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed,
        },
        Some(3500),
    );
    run.apply_event(SessionEvent::TurnStarted { turn: 2 }, Some(100_000));
    run.apply_event(text_event("unfinished".into()), Some(100_050));
    assert_eq!(run.status, "interrupted");
    assert_eq!(run.elapsed_ms(), 50);
    run.apply_event(
        SessionEvent::TurnEnded {
            turn: 2,
            outcome: TurnOutcome::Completed,
        },
        Some(100_100),
    );
    assert_eq!(run.status, "completed");
    assert_eq!(run.elapsed_ms(), 100);
}

#[test]
fn step_started_does_not_reset_duration_within_a_turn() {
    let activity = activity();
    activity
        .runs
        .lock()
        .unwrap()
        .get_mut("child")
        .unwrap()
        .start_ms = Some(123);
    activity.observe(&Frame::StepStarted {
        session: "child".into(),
        turn: 1,
    });
    assert_eq!(activity.runs.lock().unwrap()["child"].start_ms, Some(123));
    activity.failed("child");
    activity.observe(&Frame::StepStarted {
        session: "child".into(),
        turn: 2,
    });
    let runs = activity.runs.lock().unwrap();
    assert_eq!(runs["child"].start_ms, None);
    assert_eq!(runs["child"].finished_ms, None);
    assert_eq!(runs["child"].status, "running");
}

#[test]
fn text_and_streams_keep_bounded_utf8_tails() {
    let activity = activity();
    let text = "🦀".repeat(MAX_TEXT_BYTES);
    for _ in 0..2 {
        activity.observe(&Frame::Delta {
            session: "child".into(),
            chunk: ChunkDelta::Text { t: text.clone() },
        });
        activity.stream("child", "tool", &text);
    }
    activity.stream("child", "tool", "recent");
    let mut runs = activity.runs.lock().unwrap();
    let run = runs.get_mut("child").unwrap();
    assert!(run.live.len() <= MAX_TEXT_BYTES);
    assert!(run.streams["tool"].len() <= MAX_TEXT_BYTES);
    assert!(run.streams["tool"].ends_with("recent"));
    run.apply_event(text_event(text), None);
    run.apply_event(text_event("line\n".repeat(MAX_LINES * 2)), None);
    assert!(run.lines.len() <= MAX_LINES);
    assert!(run.lines.iter().map(|line| line.len() + 1).sum::<usize>() <= MAX_TEXT_BYTES);
    assert_eq!(run.lines.last().unwrap(), "line");
}

#[test]
fn tool_started_is_timely_and_does_not_overwrite_committed_args_or_streams() {
    let activity = activity();
    activity.stream("child", "tool", "early output");
    activity.observe(&Frame::ToolStarted {
        session: "child".into(),
        call: "tool".into(),
        name: "Bash".into(),
    });
    {
        let mut runs = activity.runs.lock().unwrap();
        let run = runs.get_mut("child").unwrap();
        assert_eq!(run.activity, "Bash");
        assert_eq!(run.tools[0]["name"], "Bash");
        assert_eq!(run.tools[0]["stream"], "early output");
        SubagentActivity::tool_start(run, "tool", "Bash", json!({"command":"pwd"}));
    }
    activity.observe(&Frame::ToolStarted {
        session: "child".into(),
        call: "tool".into(),
        name: "Bash".into(),
    });
    let runs = activity.runs.lock().unwrap();
    assert_eq!(runs["child"].tools[0]["args"]["command"], "pwd");
    assert_eq!(runs["child"].tools.len(), 1);
}

#[test]
fn tool_details_and_stream_keys_are_bounded() {
    let activity = activity();
    for call in 0..MAX_TOOLS * 2 {
        activity.stream("child", &call.to_string(), "output");
    }
    let runs = activity.runs.lock().unwrap();
    let run = &runs["child"];
    assert_eq!(run.tools.len(), MAX_TOOLS);
    assert_eq!(run.streams.len(), MAX_TOOLS);
    assert!(!run.streams.contains_key("0"));
    assert_eq!(
        run.tools.last().unwrap()["call"],
        (MAX_TOOLS * 2 - 1).to_string()
    );
}

#[test]
fn results_are_bounded_and_not_duplicated_in_lines() {
    let mut run = run(false);
    SubagentActivity::tool_start(
        &mut run,
        "tool",
        "Bash",
        json!({"command":"x".repeat(MAX_TEXT_BYTES * 2)}),
    );
    assert_eq!(run.tools[0]["args"]["truncated"], true);
    // Deserialize to remain independent of optional protocol result fields.
    let result: ToolResult = serde_json::from_value(json!({
        "call":"tool", "name":"Bash", "is_error":false, "duration_ms":0,
        "output":"🦀".repeat(MAX_TEXT_BYTES),
    }))
    .unwrap();
    run.apply_event(SessionEvent::ToolResult(result), None);
    assert!(run.lines.is_empty());
    assert!(run.tools[0]["output"].as_str().unwrap().len() <= MAX_TEXT_BYTES);
    assert_eq!(run.tools[0]["status"], "done");
    assert!(run.tools[0].get("stream").is_none());
}

#[test]
fn cancellation_clears_live_and_settles_running_tools() {
    let mut run = run(false);
    run.live = "partial".into();
    SubagentActivity::tool_start(&mut run, "tool", "Bash", Value::Null);
    run.apply_event(SessionEvent::TurnStarted { turn: 1 }, Some(100));
    run.apply_event(
        SessionEvent::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Cancelled,
        },
        Some(90),
    );
    assert_eq!(run.status, "cancelled");
    assert_eq!(run.tools[0]["status"], "cancelled");
    assert!(run.live.is_empty());
    assert_eq!(run.elapsed_ms(), 0);
}
