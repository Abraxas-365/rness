use std::sync::Arc;
use rness_engine::{plan::{ExitPlan, PlanConfig}, questions::{Questions, Answers, Answer}, session::branch::SessionStore, tools::{ToolRegistry, ToolCall}};
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

struct ReviewProvider(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl rness_engine::turn::provider::Provider for ReviewProvider {
    fn model(&self) -> &str { "plan-test" }
    async fn step(&self, request: rness_engine::turn::provider::StepRequest<'_>, _: &CancellationToken) -> rness_engine::turn::provider::StepOutcome {
        let first = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        assert_eq!(request.system.contains("Plan mode is active"), first);
        rness_engine::turn::provider::StepOutcome::Committed(AssistantMessage {
            model: "plan-test".into(), content: if first { vec![ContentPart::ToolUse { call: "review".into(), name: "exit_plan_mode".into(), args: serde_json::json!({"plan":"# Implementation\nAdd tests"}) }] } else { vec![ContentPart::Text { text: "execution begins".into() }] },
            stop: if first { StopReason::ToolUse } else { StopReason::EndTurn }, usage: Default::default(), estimated_input: 0, chunks: vec![],
        })
    }
}

#[tokio::test]
async fn approval_continues_next_step_but_dismissal_stops() {
    for (dismiss, edited) in [(false, false), (false, true), (true, false)] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SessionStore::new(dir.path()));
        let mut log = store.create(None).unwrap();
        let session = log.session().clone();
        let questions = Arc::new(Questions::default());
        questions.set_available(true);
        let tools = ToolRegistry::default();
        tools.register(Arc::new(ExitPlan { config: PlanConfig::default(), store: store.clone(), questions: questions.clone(), alive: CancellationToken::new() }));
        tools.plan_selections.select(&session, true);
        let mut events = questions.subscribe();
        let (q, s) = (questions.clone(), session.clone());
        let answer = tokio::spawn(async move {
            events.recv().await.unwrap();
            if dismiss { q.dismiss(&s, "review"); } else { q.resolve(&s, "review", Answers { answers: vec![Answer { edited_markdown: edited.then(|| "# User revision\nImplement this instead".into()), id: "plan-review".into(), selected: vec!["Approve".into()], custom: None }] }).unwrap(); }
        });
        let provider = ReviewProvider(Default::default());
        rness_engine::turn::run_turn(&store, &mut log, &provider, &tools, &Default::default(), &CancellationToken::new(), &mut Vec::new, 1, &|_| {}).await.unwrap();
        answer.await.unwrap();
        assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), if dismiss { 1 } else { 2 });
        let history = store.history(&session).unwrap();
        assert_eq!(PlanState::from_history(&history).active, dismiss);
        let at = history.iter().find(|env| matches!(env.event, SessionEvent::ToolResult(_))).unwrap().id.clone();
        drop(log);
        let child = store.fork(&session, Some(at)).unwrap();
        let child_history = store.history(child.session()).unwrap();
        if edited {
            assert!(child_history.iter().any(|env| matches!(&env.event, SessionEvent::ToolResult(result) if result.output.contains("# User revision\nImplement this instead"))));
        }
        let inherited = PlanState::from_history(&child_history);
        assert!(inherited.active);
        assert_eq!(inherited.pending, if dismiss { None } else { Some(false) });
    }
}

#[tokio::test]
async fn reviews_are_explicit_and_cancelable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SessionStore::new(dir.path()));
    let mut log = store.create(None).unwrap();
    let session = log.session().clone();
    log.append(&SessionEvent::PlanMode { active: true }).unwrap();
    let questions = Arc::new(Questions::default());
    let alive = CancellationToken::new();
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ExitPlan { config: PlanConfig::default(), store: store.clone(), questions: questions.clone(), alive: alive.clone() }));
    let call = ToolCall { call: "review".into(), name: "exit_plan_mode".into(), args: serde_json::json!({"plan":"# Plan\nDo the work"}) };
    let unavailable = tools.dispatch(&session, &[call.clone()], 1, &CancellationToken::new()).await;
    assert!(unavailable[0].is_error);
    questions.set_available(true);
    for mode in ["approve", "edited", "feedback", "dismiss", "cancel", "unload"] {
        let token = CancellationToken::new();
        let (t, s, c, cancel) = (tools.clone(), session.clone(), call.clone(), token.clone());
        let mut events = questions.subscribe();
        let worker = tokio::spawn(async move { t.dispatch(&s, &[c], 1, &cancel).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await.unwrap().unwrap();
        match mode {
            "dismiss" => { questions.dismiss(&session, "review"); }
            "cancel" => token.cancel(),
            "unload" => alive.cancel(),
            _ => questions.resolve(&session, "review", Answers { answers: vec![Answer { edited_markdown: (mode == "edited").then(|| "# Revised plan\nUser-authored implementation".into()), id: "plan-review".into(), selected: vec![if mode == "approve" || mode == "edited" { "Approve" } else { "Keep planning" }.into()], custom: if mode == "feedback" { Some("add tests".into()) } else { None } }] }).unwrap(),
        }
        let result = worker.await.unwrap().remove(0);
        assert_eq!(result.plan_review, match mode { "approve" | "edited" => Some(PlanReview::Approved), "feedback" => Some(PlanReview::KeepPlanning), "dismiss" => Some(PlanReview::Dismissed), _ => None });
        if mode == "edited" { assert!(result.output.contains("# Revised plan\nUser-authored implementation")); }
        assert!(questions.pending().is_empty());
        assert!(PlanState::from_history(&store.history(&session).unwrap()).active);
    }
}
