//! M1 exit criteria for the service layer: live sessions driven entirely
//! through [`SessionService`] — idle/running phases, queued followups,
//! mid-turn steers, cancellation, independent sessions, and the kernel
//! plugin seam.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rness_engine::inbox::{Disposition, Phase};
use rness_engine::service::{
    SessionIdleEv, SessionService, SessionsPlugin, TurnEndedEv, TurnStartedEv,
};
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::{Tool, ToolRegistry};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::{EventBus, Kernel};
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn job_completion_id_survives_lost_ack_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![StepOutcome::Committed(assistant("done",StopReason::EndTurn,vec![]))]);
    let first = service(dir.path(),provider);
    let id = first.create(None).unwrap();
    let (a,b) = tokio::join!(first.notify_job_once(&id,"job-1","finished".into()), first.notify_job_once(&id,"job-1","changed retry text".into()));
    a.unwrap(); b.unwrap();
    first.join(&id).await;
    // Simulate losing the job-side acknowledgment after the session commit.
    drop(first);
    let recovered = service(dir.path(),Scripted::new(vec![]));
    assert!(recovered.notify_job_once(&id,"job-1","retry after crash".into()).await.unwrap());
    let history = recovered.store().history(&id).unwrap();
    assert_eq!(history.iter().filter(|e| matches!(&e.event,SessionEvent::UserMessage(m) if matches!(&m.source,Some(MessageSource::JobCompletion {id}) if id == "job-1"))).count(),1);
}

#[tokio::test]
async fn text_only_model_rejects_images_before_logging() {
    use rness_engine::config::{ModelCapabilities, ModelDeclaration, ModelRegistry};
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![]);
    let mut models = ModelRegistry::default();
    models.declare_model(ModelDeclaration { provider: "test".into(), model: "text-only".into(),
        capabilities: ModelCapabilities { image_input: Some(false), ..Default::default() } }).unwrap();
    let resolved = provider.clone();
    let svc = service(dir.path(), provider)
        .with_agents(Default::default(), models)
        .with_provider_resolver(CallConfig { selection: Some(ModelSelection { route: "test".into(), model: "text-only".into() }), ..Default::default() }, Arc::new(move |_| Ok(resolved.clone())));
    svc.set_images(Arc::new(rness_engine::images::ImageStore::new(dir.path().join("images"), Default::default()).unwrap())).unwrap();
    let sid = svc.create(None).unwrap();
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2).write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    let attachment = svc.admit_image(&sid, bytes.get_ref(), "image/png").unwrap();
    let before = svc.store().history(&sid).unwrap().len();
    let error = svc.send(&sid, UserIntent::Followup, vec![ContentPart::Image { attachment }]).unwrap_err();
    assert!(error.to_string().contains("disables image input"));
    assert_eq!(svc.store().history(&sid).unwrap().len(), before);
}

#[tokio::test]
async fn workspace_survives_resume_and_controls_instructions_and_input() {
    let logs = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("RULES"), "project-specific-rule").unwrap();
    std::fs::write(launch.path().join("RULES"), "wrong-launch-rule").unwrap();
    let provider = Scripted::new(vec![StepOutcome::Committed(assistant("done", StopReason::EndTurn, vec![]))]);
    let build = || SessionService::new(SessionStore::new(logs.path()), provider.clone(), Arc::new(ToolRegistry::default()), TurnConfig::default(), Arc::new(EventBus::default()));
    let first = build();
    first.set_default_workspace(project.path().to_str().unwrap().into()).unwrap();
    let id = first.create(None).unwrap();
    let expected = project.path().canonicalize().unwrap();
    drop(first);
    let resumed = build();
    resumed.set_default_workspace(launch.path().to_str().unwrap().into()).unwrap();
    resumed.set_instructions(rness_engine::instructions::InstructionsConfig {
        cwd: launch.path().into(), candidates: vec!["RULES".into()], max_bytes: 10000,
    });
    let expected_input = expected.clone();
    resumed.set_input_resolver(Arc::new(move |workspace, content| {
        assert_eq!(workspace, Some(expected_input.as_path()));
        Ok(content)
    }));
    resumed.send(&id, UserIntent::Followup, vec![ContentPart::Text { text: "hello".into() }]).unwrap();
    resumed.join(&id).await;
    let seen = provider.seen().join("\n");
    assert!(seen.contains("project-specific-rule"));
    assert!(!seen.contains("wrong-launch-rule"));
    let spawned = resumed.create_delegated(None, rness_protocol::branch::Delegation {
        parent: id.clone(), call: None, depth: 1, mode: Default::default(),
    }).unwrap();
    assert_eq!(resumed.store().workspace(&spawned).unwrap().as_deref(), expected.to_str());
    let child = resumed.fork(&id, None).unwrap();
    assert_eq!(resumed.store().workspace(&child).unwrap().as_deref(), expected.to_str());
    assert!(resumed.create(Some(project.path().join("missing").to_string_lossy().into_owned())).is_err());
}

#[tokio::test]
async fn retry_resumes_failed_context_without_duplicate_user_message() {
    let logs = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![
        StepOutcome::Failed { error: rness_engine::turn::provider::ProviderError { code: "PROVIDER", retry_after: None, message: "offline".into(), retryable: false }, partial: vec![] },
        StepOutcome::Committed(assistant("recovered", StopReason::EndTurn, vec![])),
    ]);
    let service = SessionService::new(SessionStore::new(logs.path()), provider, Arc::new(ToolRegistry::default()), TurnConfig::default(), Arc::new(EventBus::default()));
    let id = service.create(None).unwrap();
    assert!(service.retry(&id).is_err());
    service.send(&id, UserIntent::Followup, vec![ContentPart::Text { text: "hello".into() }]).unwrap();
    service.join(&id).await;
    service.retry(&id).unwrap();
    service.join(&id).await;
    let history = service.store().history(&id).unwrap();
    assert_eq!(history.iter().filter(|e| matches!(e.event, SessionEvent::UserMessage(_))).count(), 1);
    assert!(matches!(history.last().unwrap().event, SessionEvent::TurnEnded { outcome: TurnOutcome::Completed, .. }));
    assert!(service.retry(&id).is_err());
}

struct PanicCommand;
impl rness_engine::interaction::Command for PanicCommand {
    fn name(&self) -> &str { "panic" }
    fn description(&self) -> &str { "Test unwinding" }
    fn execute(&self, _: &SessionService, _: rness_engine::interaction::CommandInvocation<'_>) -> Result<rness_engine::interaction::CommandResult, rness_engine::service::ServiceError> {
        panic!("handler panic");
    }
}

#[tokio::test]
async fn command_admission_cancellation_drop_and_panic_release_reservations() {
    let logs = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![]);
    let service = Arc::new(SessionService::new(SessionStore::new(logs.path()), provider.clone(), Arc::new(ToolRegistry::default()), TurnConfig::default(), Arc::new(EventBus::default())));
    service.commands().register(Arc::new(PanicCommand)).unwrap();
    let id = service.create(None).unwrap();
    let before = service.store().history(&id).unwrap();
    let input = || vec![ContentPart::Text { text: "/panic".into() }];

    let pending = service.send_async(id.clone(), UserIntent::Followup, input());
    assert!(service.command_running(&id));
    assert!(matches!(service.compact(&id, 0).await, Err(rness_engine::service::ServiceError::Busy)));
    assert!(matches!(service.set_config(&id, service.config(&id).unwrap()), Err(rness_engine::service::ServiceError::Busy)));
    assert!(matches!(service.select_agent(&id, "any"), Err(rness_engine::service::ServiceError::Busy)));
    assert!(matches!(service.prune_tool_results(&id, rness_engine::service::PruneOptions { threshold_chars: 100, head_chars: 20, tail_chars: 20, keep_turns: 1 }), Err(rness_engine::service::ServiceError::Busy)));
    assert!(service.try_extension_maintenance().is_err());
    assert!(service.send(&id, UserIntent::Followup, vec![ContentPart::Text { text: "must not start".into() }]).is_err());
    service.cancel(&id);
    assert!(pending.await.unwrap_err().to_string().contains("cancelled"));
    assert!(!service.command_running(&id));

    let abandoned = service.send_async(id.clone(), UserIntent::Followup, input());
    drop(abandoned);
    assert!(!service.command_running(&id));
    assert!(service.try_extension_maintenance().is_ok());

    let error = service.send_async(id.clone(), UserIntent::Followup, input()).await.unwrap_err();
    assert!(error.to_string().contains("panic"));
    assert!(!service.command_running(&id));
    assert!(service.try_extension_maintenance().is_ok());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| service.send(&id, UserIntent::Followup, input()))).is_err());
    assert!(!service.command_running(&id));
    assert!(service.try_extension_maintenance().is_ok());
    assert_eq!(service.store().history(&id).unwrap(), before);
    assert!(provider.seen().is_empty());
    assert!(matches!(service.send(&id, UserIntent::Followup, vec![ContentPart::Text { text: "/help".into() }]).unwrap(), Disposition::Command(_)));
}

#[tokio::test]
async fn active_turn_rejects_command_admission_without_losing_cancellation() {
    let logs = tempfile::tempdir().unwrap();
    let provider = Scripted::gated(vec![], Arc::new(tokio::sync::Semaphore::new(0)));
    let service = Arc::new(SessionService::new(SessionStore::new(logs.path()), provider, Arc::new(ToolRegistry::default()), TurnConfig::default(), Arc::new(EventBus::default())));
    let id = service.create(None).unwrap();
    service.send(&id, UserIntent::Followup, vec![ContentPart::Text { text: "turn".into() }]).unwrap();
    assert!(matches!(service.prepare_command(&id, "/help"), Err(rness_engine::service::ServiceError::Busy)));
    assert!(!service.command_running(&id));
    assert!(service.try_extension_maintenance().is_err());
    service.cancel(&id);
    tokio::time::timeout(std::time::Duration::from_secs(3), service.join(&id)).await.unwrap();
    assert!(service.prepare_command(&id, "/help").unwrap().is_some());
    assert!(service.try_extension_maintenance().is_ok());
}

// -- fakes -----------------------------------------------------------------

fn assistant(text: &str, stop: StopReason, tool_calls: Vec<(&str, &str)>) -> AssistantMessage {
    let mut content = vec![ContentPart::Text { text: text.into() }];
    for (call, name) in tool_calls {
        content.push(ContentPart::ToolUse {
            call: call.into(),
            name: name.into(),
            args: serde_json::json!({}),
        });
    }
    AssistantMessage {
        model: "fake-1".into(),
        content,
        stop,
        usage: Usage { input_tokens: 1, output_tokens: 1, ..Default::default() },
        chunks: vec![TimedChunk { ms: 1, delta: ChunkDelta::Text { t: text.into() } }],
    }
}

/// Plays scripted outcomes; records the flattened user text of every
/// request it saw. Waits on an optional gate (semaphore permits
/// accumulate, so tests can release steps ahead of time) to make phases
/// observable.
struct Scripted {
    steps: Mutex<Vec<StepOutcome>>,
    seen: Mutex<Vec<String>>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl Scripted {
    fn new(steps: Vec<StepOutcome>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps), seen: Mutex::new(vec![]), gate: None })
    }

    fn gated(steps: Vec<StepOutcome>, gate: Arc<tokio::sync::Semaphore>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps), seen: Mutex::new(vec![]), gate: Some(gate) })
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for Scripted {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        let context = request.context;
        let texts: Vec<String> = context
            .turns
            .iter()
            .filter_map(|t| match t {
                rness_engine::session::projection::ModelTurn::User { content } => {
                    content.iter().find_map(|p| match p {
                        ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect();
        self.seen.lock().unwrap().push(texts.join("|"));
        if let Some(gate) = &self.gate {
            tokio::select! {
                p = gate.acquire() => { p.unwrap().forget(); }
                _ = cancel.cancelled() => {
                    return StepOutcome::Cancelled { partial: vec![] };
                }
            }
        }
        let outcome = self.steps.lock().unwrap().remove(0);
        // Stream the committed chunks live, like a real adapter would.
        if let (StepOutcome::Committed(message), Some(sink)) = (&outcome, request.on_delta) {
            for chunk in &message.chunks {
                sink(&chunk.delta);
            }
        }
        outcome
    }
}

#[tokio::test]
async fn retry_preserves_committed_tool_results_and_allows_config_change() {
    struct Count(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl Tool for Count {
        fn name(&self) -> &str { "Count" }
        async fn execute(&self, _: serde_json::Value) -> Result<String, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("done".into())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(Count(count.clone())));
    let provider = Scripted::new(vec![
        StepOutcome::Committed(AssistantMessage { model: "test".into(), content: vec![ContentPart::ToolUse { call: "once".into(), name: "Count".into(), args: serde_json::json!({}) }], stop: StopReason::ToolUse, usage: Usage::default(), chunks: vec![] }),
        StepOutcome::Failed { error: rness_engine::turn::provider::ProviderError { code: "HTTP", retry_after: None, message: "offline".into(), retryable: false }, partial: vec![] },
        StepOutcome::Committed(assistant("recovered", StopReason::EndTurn, vec![])),
    ]);
    let svc = SessionService::new(SessionStore::new(dir.path()), provider, tools, TurnConfig::default(), Arc::new(EventBus::default()));
    let sid = svc.create(None).unwrap();
    svc.send(&sid, UserIntent::Followup, text("execute once")).unwrap();
    svc.join(&sid).await;
    svc.set_config(&sid, svc.config(&sid).unwrap()).unwrap();
    svc.retry(&sid).unwrap();
    assert!(svc.retry(&sid).is_err());
    svc.join(&sid).await;
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(svc.store().history(&sid).unwrap().iter().filter(|e| matches!(e.event, SessionEvent::ToolResult(_))).count(), 1);
}

#[tokio::test]
async fn cancellation_interrupts_retry_after_without_another_request() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![StepOutcome::Failed { error: rness_engine::turn::provider::ProviderError { code: "HTTP", retry_after: Some(std::time::Duration::from_secs(60)), message: "rate limited".into(), retryable: true }, partial: vec![] }]);
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();
    svc.send(&sid, UserIntent::Followup, text("go")).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if svc.store().history(&sid).unwrap().iter().any(|e| matches!(e.event, SessionEvent::AssistantAttempt(_))) { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    svc.cancel(&sid);
    tokio::time::timeout(std::time::Duration::from_secs(2), svc.join(&sid)).await.unwrap();
    assert_eq!(provider.seen().len(), 1);
    assert!(matches!(svc.store().history(&sid).unwrap().last().unwrap().event, SessionEvent::TurnEnded { outcome: TurnOutcome::Cancelled, .. }));
}

struct Echo;
#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "Echo"
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<String, String> {
        Ok("echo-output".into())
    }
}

fn service(dir: &std::path::Path, provider: Arc<dyn Provider>) -> SessionService {
    let mut tools = ToolRegistry::default();
    tools.register(Arc::new(Echo));
    SessionService::new(
        SessionStore::new(dir),
        provider,
        Arc::new(tools),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    )
}

fn text(t: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text { text: t.into() }]
}

// -- tests -----------------------------------------------------------------

#[tokio::test]
async fn extension_maintenance_excludes_bursts_and_new_input() {
    use rness_engine::service::ServiceError;
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = Scripted::gated(vec![
        StepOutcome::Committed(assistant("one", StopReason::EndTurn, vec![])),
        StepOutcome::Committed(assistant("two", StopReason::EndTurn, vec![])),
    ], gate.clone());
    let svc = service(dir.path(), provider);
    let sid = svc.create(None).unwrap();
    let other = svc.create(None).unwrap();
    let maintenance = svc.try_extension_maintenance().unwrap();
    assert!(matches!(svc.try_extension_maintenance(), Err(ServiceError::Busy)));
    assert!(matches!(svc.send(&sid, UserIntent::Followup, text("blocked")), Err(ServiceError::Busy)));
    assert!(matches!(svc.send(&other, UserIntent::Inject, text("blocked")), Err(ServiceError::Busy)));
    assert!(matches!(svc.compact(&sid, 1).await, Err(ServiceError::Busy)));
    assert!(svc.transcript(&sid).unwrap().items.is_empty());
    drop(maintenance);
    svc.send(&sid, UserIntent::Followup, text("one")).unwrap();
    svc.send(&sid, UserIntent::Followup, text("two")).unwrap();
    assert!(matches!(svc.try_extension_maintenance(), Err(ServiceError::Busy)));
    gate.add_permits(2);
    svc.join(&sid).await;
    let maintenance = svc.try_extension_maintenance().unwrap();
    assert!(matches!(svc.send(&other, UserIntent::Followup, text("blocked")), Err(ServiceError::Busy)));
    drop(maintenance);
    svc.send(&other, UserIntent::Inject, text("allowed")).unwrap();
}

#[tokio::test]
async fn send_while_idle_runs_a_turn_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![StepOutcome::Committed(assistant(
        "done",
        StopReason::EndTurn,
        vec![],
    ))]);
    let svc = service(dir.path(), provider.clone());

    let sid = svc.create(None).unwrap();
    assert_eq!(svc.phase(&sid), Phase::Idle);

    let disp = svc.send(&sid, UserIntent::Followup, text("hello")).unwrap();
    assert_eq!(disp, Disposition::StartTurn);
    svc.join(&sid).await;

    assert_eq!(svc.phase(&sid), Phase::Idle);
    assert_eq!(provider.seen(), vec!["hello"]);

    // Fully committed and replayable.
    let replayed = svc.replay(&sid).unwrap();
    assert_eq!(replayed.context.turns.len(), 2); // user + assistant
    let transcript = svc.transcript(&sid).unwrap();
    assert_eq!(transcript.items.len(), 2);
}

#[tokio::test]
async fn followup_while_running_queues_and_runs_after() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = Scripted::gated(
        vec![
            StepOutcome::Committed(assistant("first", StopReason::EndTurn, vec![])),
            StepOutcome::Committed(assistant("second", StopReason::EndTurn, vec![])),
        ],
        Arc::clone(&gate),
    );
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();

    svc.send(&sid, UserIntent::Followup, text("one")).unwrap();
    // The provider is now blocked on the gate → phase is Running.
    while svc.phase(&sid) != Phase::Running {
        tokio::task::yield_now().await;
    }
    let disp = svc.send(&sid, UserIntent::Followup, text("two")).unwrap();
    assert_eq!(disp, Disposition::Queued);

    gate.add_permits(1); // finish step 1
    gate.add_permits(1); // finish step 2 (queued followup's turn)
    svc.join(&sid).await;

    // Second request saw both user messages.
    assert_eq!(provider.seen(), vec!["one", "one|two"]);

    // Two full turns in the log.
    let history = svc.store().history(&sid).unwrap();
    let turn_ends = history
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::TurnEnded { .. }))
        .count();
    assert_eq!(turn_ends, 2);
}

#[tokio::test]
async fn steer_while_running_lands_mid_turn() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = Scripted::gated(
        vec![
            StepOutcome::Committed(assistant("using tool", StopReason::ToolUse, vec![("c1", "Echo")])),
            StepOutcome::Committed(assistant("done", StopReason::EndTurn, vec![])),
        ],
        Arc::clone(&gate),
    );
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();

    svc.send(&sid, UserIntent::Followup, text("start")).unwrap();
    // Wait until the FIRST request is in flight (past its pre-step drain),
    // so the steer deterministically lands at the SECOND boundary.
    while provider.seen().is_empty() {
        tokio::task::yield_now().await;
    }
    // Steer while the first step is in flight.
    let disp = svc.send(&sid, UserIntent::Steer, text("actually stop")).unwrap();
    assert_eq!(disp, Disposition::Queued);

    gate.add_permits(1);
    gate.add_permits(1);
    svc.join(&sid).await;

    // The steer is visible in the SECOND request (next step boundary).
    assert_eq!(provider.seen(), vec!["start", "start|actually stop"]);
}

#[tokio::test]
async fn inject_while_idle_logs_without_running() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![]);
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();

    let disp = svc.send(&sid, UserIntent::Inject, text("context note")).unwrap();
    assert_eq!(disp, Disposition::LogOnly);
    assert_eq!(svc.phase(&sid), Phase::Idle);
    assert!(provider.seen().is_empty());

    // Committed to the log with intent preserved.
    let history = svc.store().history(&sid).unwrap();
    assert!(matches!(
        &history[1].event,
        SessionEvent::UserMessage(m) if m.intent == UserIntent::Inject
    ));
}

#[tokio::test]
async fn cancel_ends_the_burst_and_returns_to_idle() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = Scripted::gated(
        vec![StepOutcome::Committed(assistant("never", StopReason::EndTurn, vec![]))],
        Arc::clone(&gate),
    );
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();

    svc.send(&sid, UserIntent::Followup, text("long task")).unwrap();
    while svc.phase(&sid) != Phase::Running {
        tokio::task::yield_now().await;
    }
    svc.cancel(&sid);
    svc.join(&sid).await;
    assert_eq!(svc.phase(&sid), Phase::Idle);

    // Turn ended cancelled; attempt preserved; nothing model-visible lost.
    let history = svc.store().history(&sid).unwrap();
    assert!(history.iter().any(|e| matches!(
        e.event,
        SessionEvent::TurnEnded { outcome: TurnOutcome::Cancelled, .. }
    )));
    assert!(history
        .iter()
        .any(|e| matches!(e.event, SessionEvent::AssistantAttempt(_))));

    // The session is usable again after cancel.
    let replayed = svc.replay(&sid).unwrap();
    assert_eq!(replayed.context.turns.len(), 1); // just the user message
}

#[tokio::test]
async fn sessions_run_independently() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = Scripted::gated(
        vec![
            StepOutcome::Committed(assistant("a", StopReason::EndTurn, vec![])),
            StepOutcome::Committed(assistant("b", StopReason::EndTurn, vec![])),
        ],
        Arc::clone(&gate),
    );
    let svc = service(dir.path(), provider.clone());
    let s1 = svc.create(None).unwrap();
    let s2 = svc.create(None).unwrap();

    svc.send(&s1, UserIntent::Followup, text("to s1")).unwrap();
    while svc.phase(&s1) != Phase::Running {
        tokio::task::yield_now().await;
    }
    // s1 running does not block s2.
    assert_eq!(svc.phase(&s2), Phase::Idle);
    svc.send(&s2, UserIntent::Followup, text("to s2")).unwrap();

    gate.add_permits(1);
    gate.add_permits(1);
    svc.join(&s1).await;
    svc.join(&s2).await;

    assert_eq!(svc.transcript(&s1).unwrap().items.len(), 2);
    assert_eq!(svc.transcript(&s2).unwrap().items.len(), 2);
    assert_eq!(svc.list().unwrap().len(), 2);
}

#[tokio::test]
async fn fork_from_service_creates_working_branch() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![
        StepOutcome::Committed(assistant("root answer", StopReason::EndTurn, vec![])),
        StepOutcome::Committed(assistant("branch answer", StopReason::EndTurn, vec![])),
    ]);
    let svc = service(dir.path(), provider.clone());
    let root = svc.create(None).unwrap();

    svc.send(&root, UserIntent::Followup, text("hi")).unwrap();
    svc.join(&root).await;

    let child = svc.fork(&root, None).unwrap();
    svc.send(&child, UserIntent::Followup, text("branch off")).unwrap();
    svc.join(&child).await;

    // Child request replayed the root's prefix.
    assert_eq!(provider.seen(), vec!["hi", "hi|branch off"]);
    // Root untouched by the child's turn.
    assert_eq!(svc.transcript(&root).unwrap().items.len(), 2);
    assert_eq!(svc.transcript(&child).unwrap().items.len(), 4);
}

#[tokio::test]
async fn lifecycle_events_fire_on_the_bus() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![StepOutcome::Committed(assistant(
        "ok",
        StopReason::EndTurn,
        vec![],
    ))]);
    let bus = Arc::new(EventBus::default());
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();

    let s = Arc::clone(&seen);
    let _d1 = bus.on::<TurnStartedEv>(move |n| s.lock().unwrap().push(format!("start:{}", n.turn)));
    let s = Arc::clone(&seen);
    let _d2 = bus.on::<TurnEndedEv>(move |n| s.lock().unwrap().push(format!("end:{}", n.turn)));
    let s = Arc::clone(&seen);
    let _d3 = bus.on::<SessionIdleEv>(move |_| s.lock().unwrap().push("idle".into()));

    let svc = SessionService::new(
        SessionStore::new(dir.path()),
        provider,
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::clone(&bus),
    );
    let sid = svc.create(None).unwrap();
    svc.send(&sid, UserIntent::Followup, text("go")).unwrap();
    svc.join(&sid).await;

    assert_eq!(*seen.lock().unwrap(), vec!["start:1", "end:1", "idle"]);
}

#[tokio::test]
async fn frames_stream_on_the_bus_and_reconcile_on_commit() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![
        StepOutcome::Committed(assistant("using tool", StopReason::ToolUse, vec![("c1", "Echo")])),
        StepOutcome::Committed(assistant("done", StopReason::EndTurn, vec![])),
    ]);
    let bus = Arc::new(EventBus::default());
    let frames: Arc<Mutex<Vec<String>>> = Arc::default();

    let f = Arc::clone(&frames);
    let _d = bus.on::<rness_engine::service::FrameEv>(move |frame| {
        use rness_protocol::frames::Frame;
        f.lock().unwrap().push(match frame {
            Frame::StepStarted { .. } => "step".into(),
            Frame::Delta { chunk, .. } => match chunk {
                ChunkDelta::Text { t } => format!("delta:{t}"),
                _ => "delta:other".into(),
            },
            Frame::ToolStarted { name, .. } => format!("tool:{name}"),
            Frame::ToolOutput { output, .. } => format!("out:{output}"),
            Frame::StepCommitted { .. } => "committed".into(),
            Frame::TurnIdle { .. } => "idle".into(),
            Frame::HistoryChanged { .. } => "history".into(),
            Frame::ApprovalRequested { .. } => "ask".into(),
            Frame::ApprovalResolved { .. } => "answered".into(),
        });
    });

    let mut tools = ToolRegistry::default();
    tools.register(Arc::new(Echo));
    let svc = SessionService::new(
        SessionStore::new(dir.path()),
        provider,
        Arc::new(tools),
        TurnConfig::default(),
        Arc::clone(&bus),
    );
    let sid = svc.create(None).unwrap();
    svc.send(&sid, UserIntent::Followup, text("go")).unwrap();
    svc.join(&sid).await;

    assert_eq!(
        *frames.lock().unwrap(),
        vec![
            "step",
            "delta:using tool",
            "committed",
            "tool:Echo",
            "out:echo-output",
            "step",
            "delta:done",
            "committed",
            "idle",
        ]
    );
}

#[tokio::test]
async fn kernel_plugin_provides_the_sessions_service() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![StepOutcome::Committed(assistant(
        "via kernel",
        StopReason::EndTurn,
        vec![],
    ))]);

    let mut kernel = Kernel::new();
    kernel
        .mount(SessionsPlugin {
            root: dir.path().to_path_buf(),
            provider: provider.clone(),
            resolver: None,
            creation_seed: CallConfig::default(),
            agents: Default::default(),
            models: Default::default(),
            tools: Arc::new(ToolRegistry::default()),
            config: TurnConfig::default(),
        })
        .unwrap();

    let svc = kernel
        .services()
        .get::<SessionService>("sessions")
        .expect("plugin provided the service");
    let sid = svc.create(None).unwrap();
    svc.send(&sid, UserIntent::Followup, text("hello kernel")).unwrap();
    svc.join(&sid).await;
    assert_eq!(svc.transcript(&sid).unwrap().items.len(), 2);

    // Unmount removes the service (reversible effects).
    kernel.unmount("sessions").unwrap();
    assert!(kernel.services().get::<SessionService>("sessions").is_none());
}

// -- compaction ------------------------------------------------------------

/// Run `n` completed simple turns ("m1".."mn") against scripted answers.
async fn run_turns(svc: &SessionService, sid: &SessionId, n: usize) {
    for i in 1..=n {
        svc.send(sid, UserIntent::Followup, text(&format!("m{i}"))).unwrap();
        svc.join(sid).await;
    }
}

fn scripted_answers(n: usize) -> Vec<StepOutcome> {
    (1..=n)
        .map(|i| StepOutcome::Committed(assistant(&format!("a{i}"), StopReason::EndTurn, vec![])))
        .collect()
}

#[tokio::test]
async fn compact_folds_old_turns_and_keeps_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    // 3 turns + 1 summarizer request.
    let mut steps = scripted_answers(3);
    steps.push(StepOutcome::Committed(assistant("SUMMARY", StopReason::EndTurn, vec![])));
    let provider = Scripted::new(steps);
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();
    run_turns(&svc, &sid, 3).await;

    let report = svc.compact(&sid, 1).await.unwrap();
    assert_eq!(report.shadowed, 4); // turns 1-2: m1,a1,m2,a2
    assert_eq!(report.summary, "SUMMARY");

    // The summarizer request saw ONLY the folding region + instruction.
    let last_seen = provider.seen().last().unwrap().clone();
    assert!(last_seen.starts_with("m1|m2|"), "saw: {last_seen}");
    assert!(!last_seen.contains("m3"), "tail leaked into summarizer: {last_seen}");

    // Next projection: summary replaces turns 1-2, turn 3 verbatim.
    let replayed = svc.replay(&sid).unwrap();
    let first = &replayed.context.turns[0];
    match first {
        rness_engine::session::projection::ModelTurn::User { content } => {
            let ContentPart::Text { text } = &content[0] else { panic!() };
            assert!(text.contains("SUMMARY"), "summary not replayed: {text}");
        }
        _ => panic!("first turn should be the summary user message"),
    }
    // summary + m3 + a3
    assert_eq!(replayed.context.turns.len(), 3);

    // Transcript shows the checkpoint, hides the folded span.
    let transcript = svc.transcript(&sid).unwrap();
    use rness_engine::session::projection::TranscriptItem;
    assert!(matches!(
        &transcript.items[0],
        TranscriptItem::Compaction { shadowed: 4, .. }
    ));
    assert_eq!(transcript.items.len(), 3); // checkpoint + m3 + a3
}

#[tokio::test]
async fn recompaction_folds_the_previous_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = scripted_answers(2);
    steps.push(StepOutcome::Committed(assistant("S1", StopReason::EndTurn, vec![])));
    steps.push(StepOutcome::Committed(assistant("a3", StopReason::EndTurn, vec![])));
    steps.push(StepOutcome::Committed(assistant("S2", StopReason::EndTurn, vec![])));
    let provider = Scripted::new(steps);
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();

    run_turns(&svc, &sid, 2).await;
    svc.compact(&sid, 1).await.unwrap(); // folds turn 1 -> S1
    run_turns(&svc, &sid, 1).await; // turn 3 (scripted "a3" answer)
    let report = svc.compact(&sid, 1).await.unwrap(); // folds S1 + turn 2

    // S1's checkpoint is inside the new span (re-shadowed).
    assert!(report.shadowed >= 3, "shadowed {}", report.shadowed);
    let replayed = svc.replay(&sid).unwrap();
    // S2 + m3(the last turn kept) + its answer
    assert_eq!(replayed.context.turns.len(), 3);
    let rness_engine::session::projection::ModelTurn::User { content } =
        &replayed.context.turns[0]
    else {
        panic!()
    };
    let ContentPart::Text { text } = &content[0] else { panic!() };
    assert!(text.contains("S2") && !text.contains("S1"), "wrong summary: {text}");
}

#[tokio::test]
async fn compact_refuses_when_busy_or_empty() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = Scripted::gated(
        vec![StepOutcome::Committed(assistant("x", StopReason::EndTurn, vec![]))],
        Arc::clone(&gate),
    );
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();

    // Nothing to compact on a fresh session.
    let err = svc.compact(&sid, 1).await.unwrap_err();
    assert!(matches!(err, rness_engine::service::ServiceError::NothingToCompact));

    // Busy while a turn runs.
    svc.send(&sid, UserIntent::Followup, text("go")).unwrap();
    while svc.phase(&sid) != Phase::Running {
        tokio::task::yield_now().await;
    }
    let err = svc.compact(&sid, 1).await.unwrap_err();
    assert!(matches!(err, rness_engine::service::ServiceError::Busy));
    gate.add_permits(1);
    svc.join(&sid).await;

    // keep_turns covering everything -> nothing to fold.
    let err = svc.compact(&sid, 5).await.unwrap_err();
    assert!(matches!(err, rness_engine::service::ServiceError::NothingToCompact));
}

#[tokio::test]
async fn compacted_session_stays_replayable_and_forkable() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = scripted_answers(2);
    steps.push(StepOutcome::Committed(assistant("SUM", StopReason::EndTurn, vec![])));
    steps.push(StepOutcome::Committed(assistant("post", StopReason::EndTurn, vec![])));
    let provider = Scripted::new(steps);
    let svc = service(dir.path(), provider.clone());
    let sid = svc.create(None).unwrap();
    run_turns(&svc, &sid, 2).await;
    svc.compact(&sid, 1).await.unwrap();

    // A turn after compaction sees summary + tail, not the folded span.
    svc.send(&sid, UserIntent::Followup, text("after")).unwrap();
    svc.join(&sid).await;
    let last = provider.seen().last().unwrap().clone();
    assert!(last.contains("SUM") && last.contains("m2|after"), "saw: {last}");
    assert!(!last.contains("m1"), "folded turn leaked: {last}");

    // Fork of a compacted session replays identically (shadowing crosses
    // the branch boundary via history).
    let child = svc.fork(&sid, None).unwrap();
    let replayed = svc.replay(&child).unwrap();
    assert!(replayed.context.turns.len() >= 3);
}

// -- tool-result pruning ---------------------------------------------------

/// Tool whose output is deterministically large.
struct Big;
#[async_trait::async_trait]
impl Tool for Big {
    fn name(&self) -> &str {
        "Big"
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<String, String> {
        Ok(format!("HEAD-{}-TAIL", "x".repeat(500)))
    }
}

fn prune_opts(keep_turns: usize) -> rness_engine::service::PruneOptions {
    rness_engine::service::PruneOptions {
        threshold_chars: 100,
        head_chars: 20,
        tail_chars: 10,
        keep_turns,
    }
}

#[tokio::test]
async fn prune_shortens_old_tool_results_and_keeps_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![
        // turn 1: tool call + answer
        StepOutcome::Committed(assistant("using tool", StopReason::ToolUse, vec![("c1", "Big")])),
        StepOutcome::Committed(assistant("a1", StopReason::EndTurn, vec![])),
        // turn 2: another tool call + answer (protected tail)
        StepOutcome::Committed(assistant("again", StopReason::ToolUse, vec![("c2", "Big")])),
        StepOutcome::Committed(assistant("a2", StopReason::EndTurn, vec![])),
        // turn 3 after pruning
        StepOutcome::Committed(assistant("a3", StopReason::EndTurn, vec![])),
    ]);
    let mut tools = ToolRegistry::default();
    tools.register(Arc::new(Big));
    let svc = SessionService::new(
        SessionStore::new(dir.path()),
        provider.clone(),
        Arc::new(tools),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    );
    let sid = svc.create(None).unwrap();
    run_turns(&svc, &sid, 2).await;

    let pruned = svc.prune_tool_results(&sid, prune_opts(1)).unwrap();
    assert_eq!(pruned, 1); // turn 1's result only; turn 2 is protected

    // Projection: old result pruned (head+marker+tail), recent intact.
    let replayed = svc.replay(&sid).unwrap();
    let mut outputs: Vec<String> = Vec::new();
    for turn in &replayed.context.turns {
        if let rness_engine::session::projection::ModelTurn::ToolResults { results } = turn {
            outputs.extend(results.iter().map(|r| r.output.clone()));
        }
    }
    assert_eq!(outputs.len(), 2);
    assert!(outputs[0].starts_with("HEAD-") && outputs[0].contains("middle pruned"));
    assert!(outputs[0].len() < 100, "still long: {}", outputs[0].len());
    assert!(!outputs[1].contains("pruned"), "protected tail was pruned");

    // Idempotent: the pruned replay is short now, nothing new to prune.
    assert_eq!(svc.prune_tool_results(&sid, prune_opts(1)).unwrap(), 0);

    // Next turn's request sees the pruned output, not the original.
    svc.send(&sid, UserIntent::Followup, text("go on")).unwrap();
    svc.join(&sid).await;
    let last = provider.seen().last().unwrap().clone();
    assert!(!last.contains("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"), "original leaked");
}

#[tokio::test]
async fn compact_folds_pruned_results_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![
        StepOutcome::Committed(assistant("using tool", StopReason::ToolUse, vec![("c1", "Big")])),
        StepOutcome::Committed(assistant("a1", StopReason::EndTurn, vec![])),
        StepOutcome::Committed(assistant("a2", StopReason::EndTurn, vec![])),
        StepOutcome::Committed(assistant("SUM", StopReason::EndTurn, vec![])),
    ]);
    let mut tools = ToolRegistry::default();
    tools.register(Arc::new(Big));
    let svc = SessionService::new(
        SessionStore::new(dir.path()),
        provider.clone(),
        Arc::new(tools),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    );
    let sid = svc.create(None).unwrap();
    run_turns(&svc, &sid, 2).await;
    assert_eq!(svc.prune_tool_results(&sid, prune_opts(1)).unwrap(), 1);

    // Compacting over a pruned region folds the PRUNE event (it is what
    // the projection cites) — replay stays invariant-clean.
    svc.compact(&sid, 1).await.unwrap();
    let replayed = svc.replay(&sid).unwrap();
    let rness_engine::session::projection::ModelTurn::User { content } =
        &replayed.context.turns[0]
    else {
        panic!()
    };
    let ContentPart::Text { text: t } = &content[0] else { panic!() };
    assert!(t.contains("SUM"));
    // The pruned ORIGINAL must not resurface anywhere in the context.
    for turn in &replayed.context.turns {
        if let rness_engine::session::projection::ModelTurn::ToolResults { results } = turn {
            for r in results {
                assert!(r.output.len() < 100, "original resurfaced: {} chars", r.output.len());
            }
        }
    }
}

// -- workspace instructions (dsh agent-instructions model) ------------------

#[tokio::test]
async fn named_agent_selection_is_durable_and_unknown_names_do_not_mutate() {
    let dir = tempfile::tempdir().unwrap();
    let mut agents = std::collections::BTreeMap::new();
    agents.insert("reviewer".into(), rness_engine::config::AgentDefinition {
        subagent: false,
        description: "Review".into(), instructions: "Inspect without edits".into(),
        profile: None, tools: Some(vec![]),
    });
    let svc = service(dir.path(), Scripted::new(scripted_answers(1)))
        .with_agents(agents, Default::default());
    let sid = svc.create(None).unwrap();
    svc.select_agent(&sid, "reviewer").unwrap();
    let config = svc.config(&sid).unwrap();
    assert_eq!(config.agent.as_ref().unwrap().instructions, "Inspect without edits");
    assert!(svc.select_agent(&sid, "missing").is_err());
    assert_eq!(svc.config(&sid).unwrap(), config);
    let restored = service(dir.path(), Scripted::new(vec![]));
    assert_eq!(restored.config(&sid).unwrap(), config);
}

fn enable_instructions(svc: &SessionService, ws: &std::path::Path, max: usize) {
    svc.set_instructions(rness_engine::instructions::InstructionsConfig {
        cwd: ws.to_path_buf(),
        candidates: vec!["AGENTS.md".into()],
        max_bytes: max,
    });
}

/// Flattened text of the projected model context.
fn context_text(svc: &SessionService, sid: &SessionId) -> String {
    let replayed = svc.replay(sid).unwrap();
    replayed
        .context
        .turns
        .iter()
        .filter_map(|t| match t {
            rness_engine::session::projection::ModelTurn::User { content }
            | rness_engine::session::projection::ModelTurn::Assistant { content } => Some(
                content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("|"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|")
}

#[tokio::test]
async fn instructions_baseline_injected_once_before_the_first_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join(".git")).unwrap();
    std::fs::write(ws.path().join("AGENTS.md"), "REGLA: contesta en espanol").unwrap();

    let provider = Scripted::new(scripted_answers(2));
    let svc = service(dir.path(), provider.clone());
    enable_instructions(&svc, ws.path(), 4096);
    let sid = svc.create(None).unwrap();
    run_turns(&svc, &sid, 2).await;

    // The FIRST provider request already carried the baseline, before m1.
    let first_seen = provider.seen()[0].clone();
    let rule = first_seen.find("REGLA").expect("baseline missing from request");
    assert!(rule < first_seen.find("m1").unwrap(), "baseline must precede the prompt");

    // Injected once, not per turn.
    let history = svc.store().history(&sid).unwrap();
    let baselines = history
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::UserMessage(UserMessage { source: Some(_), .. })
            )
        })
        .count();
    assert_eq!(baselines, 1, "identity unchanged -> no re-injection");
}

#[tokio::test]
async fn instructions_reinjected_after_compaction_folds_them() {
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join(".git")).unwrap();
    std::fs::write(ws.path().join("AGENTS.md"), "REGLA: se conciso").unwrap();

    // 3 turns + summarizer + 1 post-compact turn.
    let mut steps = scripted_answers(3);
    steps.push(StepOutcome::Committed(assistant("SUMMARY", StopReason::EndTurn, vec![])));
    steps.push(StepOutcome::Committed(assistant("a4", StopReason::EndTurn, vec![])));
    let provider = Scripted::new(steps);
    let svc = service(dir.path(), provider.clone());
    enable_instructions(&svc, ws.path(), 4096);
    let sid = svc.create(None).unwrap();
    run_turns(&svc, &sid, 3).await;

    // keep_turns=1 folds turns 1-2 INCLUDING the baseline (it precedes m1).
    svc.compact(&sid, 1).await.unwrap();
    assert!(
        !context_text(&svc, &sid).contains("REGLA"),
        "baseline should be folded into the summary"
    );

    // Next turn: the pre-turn check re-injects a fresh baseline.
    svc.send(&sid, UserIntent::Followup, text("m4")).unwrap();
    svc.join(&sid).await;
    let last_seen = provider.seen().last().unwrap().clone();
    assert!(last_seen.contains("REGLA"), "baseline not re-injected: {last_seen}");

    // And it is a NEW durable event, not resurrection of the folded one.
    let history = svc.store().history(&sid).unwrap();
    let baselines = history
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::UserMessage(UserMessage { source: Some(_), .. })
            )
        })
        .count();
    assert_eq!(baselines, 2, "one folded + one fresh");
}

#[tokio::test]
async fn instructions_change_on_disk_reinjects_with_new_identity() {
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join(".git")).unwrap();
    std::fs::write(ws.path().join("AGENTS.md"), "version-uno").unwrap();

    let provider = Scripted::new(scripted_answers(2));
    let svc = service(dir.path(), provider.clone());
    enable_instructions(&svc, ws.path(), 4096);
    let sid = svc.create(None).unwrap();
    run_turns(&svc, &sid, 1).await;

    std::fs::write(ws.path().join("AGENTS.md"), "version-dos").unwrap();
    run_turns(&svc, &sid, 1).await;

    let last_seen = provider.seen().last().unwrap().clone();
    assert!(last_seen.contains("version-dos"), "fresh content missing: {last_seen}");
    // Old baseline is still durable history (append-only log).
    assert!(last_seen.contains("version-uno"), "history is append-only");
}

#[tokio::test]
async fn no_instruction_files_means_no_injection() {
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join(".git")).unwrap();

    let provider = Scripted::new(scripted_answers(1));
    let svc = service(dir.path(), provider.clone());
    enable_instructions(&svc, ws.path(), 4096);
    let sid = svc.create(None).unwrap();
    run_turns(&svc, &sid, 1).await;

    let history = svc.store().history(&sid).unwrap();
    assert!(history.iter().all(|e| !matches!(
        &e.event,
        SessionEvent::UserMessage(UserMessage { source: Some(_), .. })
    )));
}

// -- request config (dsh request/header model) ------------------------------

#[tokio::test]
async fn selection_seed_resolves_each_session_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let first = Scripted::new(scripted_answers(1));
    let second = Scripted::new(scripted_answers(1));
    let seed = CallConfig {
        selection: Some(ModelSelection { route: "test".into(), model: "first".into() }),
        reasoning: Some(Reasoning::Effort { effort: "high".into() }),
        ..Default::default()
    };
    let a = first.clone();
    let b = second.clone();
    let resolver: Arc<rness_engine::service::ProviderResolver> = Arc::new(move |selection| {
        match selection.model.as_str() {
            "first" => Ok(a.clone() as Arc<dyn Provider>),
            "second" => Ok(b.clone() as Arc<dyn Provider>),
            _ => Err("unknown model".into()),
        }
    });
    let svc = service(dir.path(), first.clone())
        .with_provider_resolver(seed.clone(), resolver.clone());
    let sid = svc.create(None).unwrap();
    assert_eq!(svc.config(&sid).unwrap(), seed);
    run_turns(&svc, &sid, 1).await;
    let child = svc.fork(&sid, None).unwrap();
    assert_eq!(svc.config(&child).unwrap(), seed);
    let changed = CallConfig {
        selection: Some(ModelSelection { route: "test".into(), model: "second".into() }),
        ..Default::default()
    };
    svc.set_config(&child, changed.clone()).unwrap();
    run_turns(&svc, &child, 1).await;
    assert_eq!(first.seen().len(), 1);
    assert_eq!(second.seen().len(), 1);
    drop(svc);
    let reopened = service(dir.path(), first).with_provider_resolver(seed.clone(), resolver);
    assert_eq!(reopened.config(&child).unwrap(), changed);
    assert_eq!(reopened.config(&sid).unwrap(), seed);
}

#[tokio::test]
async fn request_config_latest_wins_and_resume_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(dir.path(), Scripted::new(scripted_answers(1)));
    let sid = svc.create(None).unwrap();

    // Never set -> default (all None).
    assert_eq!(svc.config(&sid).unwrap(), CallConfig::default());

    svc.set_config(&sid, CallConfig { reasoning: Some(Reasoning::BudgetTokens { tokens: 8192 }), ..Default::default() }).unwrap();
    svc.set_config(&sid, CallConfig { reasoning: Some(Reasoning::BudgetTokens { tokens: 2048 }), ..Default::default() }).unwrap();
    assert_eq!(svc.config(&sid).unwrap().reasoning, Some(Reasoning::BudgetTokens { tokens: 2048 }));

    // Restating the effective config is a no-op: no new event logged.
    let before = svc.store().history(&sid).unwrap().len();
    svc.set_config(&sid, CallConfig { reasoning: Some(Reasoning::BudgetTokens { tokens: 2048 }), ..Default::default() }).unwrap();
    assert_eq!(svc.store().history(&sid).unwrap().len(), before);

    // Clearing IS a change: back to the provider's default, durably.
    svc.set_config(&sid, CallConfig::default()).unwrap();
    assert_eq!(svc.config(&sid).unwrap(), CallConfig::default());
    assert_eq!(svc.store().history(&sid).unwrap().len(), before + 1);

    // The projected context carries the effective config to providers.
    let replayed = svc.replay(&sid).unwrap();
    assert_eq!(replayed.context.config, CallConfig::default());
}

#[tokio::test]
async fn request_config_survives_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = scripted_answers(3);
    steps.push(StepOutcome::Committed(assistant("SUMMARY", StopReason::EndTurn, vec![])));
    let svc = service(dir.path(), Scripted::new(steps));
    let sid = svc.create(None).unwrap();
    svc.set_config(&sid, CallConfig { reasoning: Some(Reasoning::BudgetTokens { tokens: 8192 }), ..Default::default() }).unwrap();
    run_turns(&svc, &sid, 3).await;

    svc.compact(&sid, 1).await.unwrap();
    // Config is request-header state, not conversation: folding turns
    // must not lose it.
    assert_eq!(svc.config(&sid).unwrap().reasoning, Some(Reasoning::BudgetTokens { tokens: 8192 }));
}
