//! Real shared-registry integration; inspection never consumes model output.
use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::service::SessionService;
use rness_engine::session::branch::SessionStore;
use rness_engine::tools::ToolRegistry;
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_lua::plugin_host::LuaHost;
use rness_protocol::events::{ContentPart, UserIntent};
use rness_tools::jobs::{JobRegistry, JobStatus};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn jobs_api_installation_visibility_and_reload() {
    let host = LuaHost::spawn().unwrap();
    host.load("unmounted", r#"
        for _, name in ipairs({'count', 'list', 'inspect', 'stop'}) do
            local ok, err = pcall(rness.jobs[name], 'owner', 'j1')
            assert(not ok and tostring(err):find('rness.jobs is not installed', 1, true))
        end
    "#).await.unwrap();
    let jobs = JobRegistry::new();
    let (id, writer) = jobs.start_owned("bash", "private".into(), Some(&"owner".into()));
    let (shared, shared_writer) = jobs.start("subagent", "shared".into());
    writer.append(&vec![b'x'; 9000]);
    host.install_jobs(jobs.clone()).await.unwrap();
    host.reload(vec![]).await.unwrap();
    host.load("check", &format!(r#"
        assert(rness.jobs.count('owner') == 2)
        assert(rness.jobs.count('foreign') == 1)
        local list = rness.jobs.list('foreign')
        assert(#list == 1 and list[1].job_id == {shared:?})
        for _, name in ipairs({{'inspect', 'stop'}}) do
            assert(not pcall(rness.jobs[name], 'foreign', {id:?}))
            assert(not pcall(rness.jobs[name], 'owner', 'missing'))
        end
        local job = rness.jobs.inspect('owner', {id:?})
        assert(job.job_id == {id:?} and job.kind == 'bash' and job.label == 'private')
        assert(job.status == 'running' and job.running and not job.cancellation_requested)
        assert(job.exit_code == nil)
        assert(job.output == string.rep('x', 8192) and job.output_bytes == 9000)
        assert(rness.jobs.inspect('owner', {id:?}).output == job.output)
        assert(rness.jobs.stop('owner', {id:?}) == true)
        assert(rness.jobs.stop('owner', {id:?}) == false)
        job = rness.jobs.inspect('owner', {id:?})
        assert(job.status == 'cancelling' and job.running and job.cancellation_requested)
        assert(rness.jobs.count('owner') == 2)
    "#)).await.unwrap();
    writer.settle(JobStatus::Exited(Some(0)));
    shared_writer.settle(JobStatus::Exited(Some(7)));
    host.load("settled", &format!(r#"
        assert(rness.jobs.count('owner') == 0)
        assert(#rness.jobs.list('owner') == 2)
        local job = rness.jobs.inspect('owner', {id:?})
        assert(job.status == 'killed' and not job.running and job.cancellation_requested)
        assert(job.exit_code == nil)
        assert(rness.jobs.inspect('owner', {shared:?}).exit_code == 7)
        assert(rness.jobs.list('foreign')[1].exit_code == 7)
        assert(rness.jobs.stop('owner', {id:?}) == false)
    "#)).await.unwrap();
}

struct BlockedProvider {
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl Provider for BlockedProvider {
    fn model(&self) -> &str { "blocked" }
    async fn step(&self, _: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.notify_one();
        cancel.cancelled().await;
        StepOutcome::Cancelled { partial: vec![] }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_command_works_while_busy_without_history_or_model_activity() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Arc::new(ToolRegistry::default());
    let provider = Arc::new(BlockedProvider {
        calls: std::sync::atomic::AtomicUsize::new(0), entered: Default::default(),
    });
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()), provider.clone(), tools.clone(), Default::default(),
        Arc::new(rness_kernel::EventBus::default()),
    ));
    let host = LuaHost::spawn().unwrap();
    host.install_session(sessions.clone(), Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(), 3)),
        tools.clone(), Default::default(), tokio::runtime::Handle::current(), "blocked".into()).await.unwrap();
    let jobs = JobRegistry::new();
    tools.register(Arc::new(rness_tools::jobs::JobOutputTool::new(jobs.clone())));
    host.install_jobs(jobs.clone()).await.unwrap();
    host.load("jobs", include_str!("../../../flavors/default/plugins/jobs.lua")).await.unwrap();
    let session = sessions.create(None).unwrap();
    let (id, writer) = jobs.start_owned("bash", "test".into(), Some(&session));
    writer.append(b"model still needs this output");
    sessions.send(&session, UserIntent::Followup, vec![ContentPart::Text { text: "start".into() }]).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), provider.entered.notified()).await.unwrap();
    let before = serde_json::to_value(sessions.store().history(&session).unwrap()).unwrap();
    for command in ["/jobs".into(), "/jobs list".into(), format!("/jobs {id}"), format!("/jobs stop {id}"), format!("/jobs stop {id}")] {
        let result = sessions.send(&session, UserIntent::Followup, vec![ContentPart::Text { text: command }]).unwrap();
        assert!(matches!(result, rness_engine::inbox::Disposition::Command(_)));
    }
    assert_eq!(jobs.count(&session), 1);
    assert!(writer.cancelled().is_cancelled());
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(serde_json::to_value(sessions.store().history(&session).unwrap()).unwrap(), before);
    let output = tools.dispatch(&session, &[rness_engine::tools::ToolCall {
        call: "read".into(), name: "job_output".into(), args: serde_json::json!({"job_id":id}),
    }], 1, &CancellationToken::new()).await;
    assert!(output[0].output.contains("model still needs this output"));
    writer.settle(JobStatus::Killed);
    assert_eq!(jobs.count(&session), 0);
    sessions.cancel(&session);
}
