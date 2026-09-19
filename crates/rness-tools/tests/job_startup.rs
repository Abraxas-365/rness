//! Interrupted jobs must not start turns before startup config is committed.
use async_trait::async_trait;
use rness_engine::{
    service::SessionService,
    session::branch::SessionStore,
    tools::ToolRegistry,
    turn::{
        provider::{Provider, StepOutcome, StepRequest},
        TurnConfig,
    },
};
use rness_kernel::EventBus;
use rness_protocol::events::*;
use rness_tools::jobs::JobRegistry;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

struct Answer;
#[async_trait]
impl Provider for Answer {
    fn model(&self) -> &str {
        "test"
    }
    async fn step(&self, _: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        StepOutcome::Committed(AssistantMessage {
            model: "test".into(),
            content: vec![],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }
}

#[tokio::test]
async fn recovered_job_waits_for_attachment_after_startup_config() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let session = store
        .create(Some(dir.path().display().to_string()))
        .unwrap()
        .session()
        .clone();
    let jobs = JobRegistry::new();
    jobs.enable_persistence(&dir.path().join("jobs")).unwrap();
    let (job, writer) = jobs.start_owned("bash", "interrupted at shutdown".into(), Some(&session));
    writer.append(b"retained output");
    drop(writer);
    drop(jobs);

    let recovered = JobRegistry::new();
    recovered
        .enable_persistence(&dir.path().join("jobs"))
        .unwrap();
    recovered.wait_recovery();
    assert_eq!(
        recovered.inspect(&session, &job).unwrap().job.status,
        "interrupted"
    );
    let sessions = Arc::new(SessionService::new(
        store,
        Arc::new(Answer),
        Arc::new(ToolRegistry::default()),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    tokio::task::yield_now().await;
    assert_eq!(sessions.store().history(&session).unwrap().len(), 1);
    let config = CallConfig {
        reasoning: Some(Reasoning::Effort {
            effort: "high".into(),
        }),
        ..Default::default()
    };
    sessions.set_config(&session, config.clone()).unwrap();
    recovered.attach_sessions(&sessions);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let history = sessions.store().history(&session).unwrap();
            let config_at = history.iter().position(|event| matches!(&event.event, SessionEvent::RequestConfig(value) if value == &config));
            let notice_at = history.iter().position(|event| matches!(&event.event,
                SessionEvent::UserMessage(message) if matches!(&message.source,
                    Some(MessageSource::JobCompletion { id }) if id == &job)));
            if let Some(notice_at) = notice_at {
                assert!(config_at.unwrap() < notice_at);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("recovered notice delivered after configuration");
    assert_eq!(
        recovered.inspect(&session, &job).unwrap().output,
        "retained output"
    );
    assert!(!rness_engine::service::ServiceError::Busy
        .to_string()
        .contains("compact"));
    sessions.cancel(&session);
}
