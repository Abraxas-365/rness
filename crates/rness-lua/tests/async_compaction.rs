//! A pending summarizer must not freeze the Lua statusline or release admission.
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rness_engine::inbox::Disposition;
use rness_engine::service::{FrameEv, SessionService};
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, ProviderError, StepOutcome, StepRequest};
use rness_engine::turn::TurnConfig;
use rness_kernel::{EventBus, presentation::TextProvider};
use rness_lua::plugin_host::LuaHost;
use rness_protocol::events::*;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct GatedSummary {
    started: Notify,
    release: Notify,
    fail: bool,
    unchanged: bool,
}

#[async_trait]
impl Provider for GatedSummary {
    fn model(&self) -> &str { "test" }
    async fn step(&self, _: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        self.started.notify_one();
        tokio::select! {
            _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
            _ = self.release.notified() => {}
        }
        if self.fail {
            return StepOutcome::Failed {
                error: ProviderError { code: "TEST", message: "summary failed".into(), retryable: false, retry_after: None },
                partial: vec![],
            };
        }
        StepOutcome::Committed(AssistantMessage {
            model: "test".into(),
            content: if self.unchanged { vec![] } else { vec![ContentPart::Text { text: "Short summary.".into() }] },
            stop: StopReason::EndTurn, usage: Usage::default(), chunks: vec![],
        })
    }
}

async fn exercise(fail: bool, cancel: bool, unchanged: bool) {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let session = log.session().clone();
    log.append(&SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: "source context ".repeat(1000) }], source: None,
    })).unwrap();
    drop(log);
    let provider = Arc::new(GatedSummary { started: Notify::new(), release: Notify::new(), fail, unchanged });
    let bus = Arc::new(EventBus::default());
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(store, provider.clone(), registry.clone(), TurnConfig::default(), bus.clone()));
    let host = LuaHost::spawn().unwrap();
    host.install_session(sessions.clone(), Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)),
        registry, Default::default(), tokio::runtime::Handle::current(), "test".into()).await.unwrap();
    let hook_host = host.clone();
    let unsubscribe = bus.on::<FrameEv>(move |frame| hook_host.fire_hook("frame", serde_json::to_value(frame).unwrap()));
    host.load("policy", r#"
        rness.compaction = { default = {
            system_prompt='Summarize.', prompt='Preserve context.', threshold_tokens=24000,
            retain_tokens=4000, summary_tokens=200, max_overflow_retries=1, max_compactions=2,
            prune_threshold=8192, prune_head=4096, prune_tail=1024
        } }
    "#).await.unwrap();
    host.load("commands", include_str!("../../../flavors/default/plugins/commands.lua")).await.unwrap();
    host.load("spinner", include_str!("../../../flavors/default/plugins/spinner.lua")).await.unwrap();
    let service = sessions.clone();
    let id = session.clone();
    let command = tokio::spawn(async move {
        service.send_async(id, UserIntent::Followup, vec![ContentPart::Text { text: "/compact".into() }]).await
    });
    let _cancel_on_drop = CancelOnDrop { sessions: sessions.clone(), session: session.clone() };
    tokio::time::timeout(Duration::from_secs(5), provider.started.notified()).await.expect("summarizer started");
    assert!(sessions.command_running(&session));
    assert!(sessions.prepare_command(&session, "/compact").is_err(), "reservation retained across yield");
    assert!(sessions.try_extension_maintenance().is_err(), "reload cannot invalidate suspended coroutine");
    assert!(tokio::time::timeout(Duration::from_secs(2), host.reload(vec![])).await.unwrap().is_err());
    assert!(tokio::time::timeout(Duration::from_secs(2), host.load("replacement", "")).await.unwrap().is_err());
    assert!(tokio::time::timeout(Duration::from_secs(2), host.unload("commands")).await.unwrap().is_err());
    // An unrelated session can still run a Lua command while this one is parked.
    let other = sessions.create(None).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), sessions.send_async(other.clone(), UserIntent::Followup,
        vec![ContentPart::Text { text: "/project session".into() }])).await.unwrap().unwrap();
    let Disposition::Command(result) = result else { panic!("expected independent command") };
    assert_eq!(result.message, other);
    let context = json!({"session":session,"model":"test"});
    for frame in ["⠋", "⠙", "⠹"] {
        let view = tokio::time::timeout(Duration::from_secs(2), host.status(context.clone())).await
            .expect("Lua responds before summarizer completes").unwrap().to_string();
        assert!(view.contains(&format!("{frame} compacting context")), "{view}");
        assert!(!command.is_finished());
    }
    if cancel { sessions.cancel(&session); } else { provider.release.notify_one(); }
    let result = tokio::time::timeout(Duration::from_secs(5), command).await.expect("command settled").unwrap();
    if cancel {
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    } else {
        let Disposition::Command(result) = result.unwrap() else { panic!("expected command result") };
        assert_eq!(result.message, if unchanged || fail { "No smaller summary produced; history unchanged." } else { "Context compacted." });
        if fail {
            // The existing reducer records provider failures and returns false.
            assert!(sessions.store().history(&session).unwrap().iter().any(|e| matches!(
                &e.event, SessionEvent::CompactionFinished { outcome, .. } if outcome.contains("summary failed")
            )));
        }
    }
    let after = host.status(context).await.unwrap().to_string();
    assert!(!after.contains("compacting context"), "{after}");
    assert!(after.contains("idle"));
    assert!(!sessions.command_running(&session));
    assert!(sessions.try_extension_maintenance().is_ok());
    assert!(sessions.prepare_command(&session, "/compact").unwrap().is_some());
    unsubscribe();
}

// Ensure even an assertion failure cancels the deliberately gated provider.
struct CancelOnDrop { sessions: Arc<SessionService>, session: String }
impl Drop for CancelOnDrop {
    fn drop(&mut self) { self.sessions.cancel(&self.session); }
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_keeps_lua_responsive_until_success() { exercise(false, false, false).await; }
#[tokio::test(flavor = "multi_thread")]
async fn compaction_keeps_lua_responsive_until_unchanged() { exercise(false, false, true).await; }
#[tokio::test(flavor = "multi_thread")]
async fn compaction_keeps_lua_responsive_until_failure() { exercise(true, false, false).await; }
#[tokio::test(flavor = "multi_thread")]
async fn compaction_keeps_lua_responsive_until_cancellation() { exercise(false, true, false).await; }
