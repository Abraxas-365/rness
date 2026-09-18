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
    host.load(
        "unmounted",
        r#"
        for _, name in ipairs({'count', 'list', 'inspect', 'stop'}) do
            local ok, err = pcall(rness.jobs[name], 'owner', 'j1')
            assert(not ok and tostring(err):find('rness.jobs is not installed', 1, true))
        end
    "#,
    )
    .await
    .unwrap();
    let jobs = JobRegistry::new();
    let (id, writer) = jobs.start_owned("bash", "private".into(), Some(&"owner".into()));
    let (shared, shared_writer) = jobs.start("subagent", "shared".into());
    writer.append(&vec![b'x'; 9000]);
    host.install_jobs(jobs.clone()).await.unwrap();
    host.reload(vec![]).await.unwrap();
    host.load(
        "check",
        &format!(
            r#"
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
    "#
        ),
    )
    .await
    .unwrap();
    writer.settle(JobStatus::Exited(Some(0)));
    shared_writer.settle(JobStatus::Exited(Some(7)));
    host.load(
        "settled",
        &format!(
            r#"
        assert(rness.jobs.count('owner') == 0)
        assert(#rness.jobs.list('owner') == 2)
        local job = rness.jobs.inspect('owner', {id:?})
        assert(job.status == 'killed' and not job.running and job.cancellation_requested)
        assert(job.exit_code == nil)
        assert(rness.jobs.inspect('owner', {shared:?}).exit_code == 7)
        assert(rness.jobs.list('foreign')[1].exit_code == 7)
        assert(rness.jobs.stop('owner', {id:?}) == false)
    "#
        ),
    )
    .await
    .unwrap();
}

struct BlockedProvider {
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl Provider for BlockedProvider {
    fn model(&self) -> &str {
        "blocked"
    }
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
        calls: std::sync::atomic::AtomicUsize::new(0),
        entered: Default::default(),
    });
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        provider.clone(),
        tools.clone(),
        Default::default(),
        Arc::new(rness_kernel::EventBus::default()),
    ));
    let host = LuaHost::spawn().unwrap();
    host.install_session(
        sessions.clone(),
        Arc::new(rness_engine::subagent::SubagentRuntime::new(
            sessions.clone(),
            3,
        )),
        tools.clone(),
        Default::default(),
        tokio::runtime::Handle::current(),
        "blocked".into(),
    )
    .await
    .unwrap();
    let jobs = JobRegistry::new();
    tools.register(Arc::new(rness_tools::jobs::JobOutputTool::new(
        jobs.clone(),
    )));
    host.install_jobs(jobs.clone()).await.unwrap();
    host.load(
        "jobs",
        include_str!("../../../flavors/default/plugins/jobs.lua"),
    )
    .await
    .unwrap();
    let session = sessions.create(None).unwrap();
    let (id, writer) = jobs.start_owned("bash", "  cargo\n\t test  ".into(), Some(&session));
    let (exited, exited_writer) = jobs.start_owned("subagent", "界".repeat(125), Some(&session));
    exited_writer.settle(JobStatus::Exited(Some(7)));
    let (killed, killed_writer) = jobs.start_owned("bash", "界".repeat(120), Some(&session));
    killed_writer.settle(JobStatus::Killed);
    writer.append(b"model still needs this output");
    sessions
        .send(
            &session,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "start".into(),
            }],
        )
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.entered.notified(),
    )
    .await
    .unwrap();
    let before = serde_json::to_value(sessions.store().history(&session).unwrap()).unwrap();
    let complete = || {
        sessions
            .prepare_command(&session, "/jobs")
            .unwrap()
            .unwrap()
            .complete_items(&sessions)
            .unwrap()
    };
    let choices = complete();
    assert_eq!(choices.len(), 5);
    for expected in [
        ("".into(), "Open jobs monitor".into()),
        (
            "list".into(),
            "List active background jobs in this session".into(),
        ),
        ("stop".into(), "Choose a running job to stop".into()),
        (id.clone(), "bash · running · cargo test".into()),
        (format!("stop {id}"), "bash · running · cargo test".into()),
    ] {
        assert!(
            choices.contains(&expected),
            "missing {expected:?} in {choices:?}"
        );
    }
    assert!(!choices
        .iter()
        .any(|(value, _)| value.contains(&exited) || value.contains(&killed)));
    for command in ["/jobs list"] {
        let result = sessions
            .send(
                &session,
                UserIntent::Followup,
                vec![ContentPart::Text {
                    text: command.into(),
                }],
            )
            .unwrap();
        let rness_engine::inbox::Disposition::Command(result) = result else {
            panic!("expected command")
        };
        assert!(result.message.contains(&id));
        assert!(!result.message.contains(&exited) && !result.message.contains(&killed));
        assert_eq!(result.data.as_array().unwrap().len(), 1);
    }
    for command in [
        "/jobs".into(),
        "/jobs list".into(),
        format!("/jobs {id}"),
        format!("/jobs stop {id}"),
        format!("/jobs stop {id}"),
    ] {
        let result = sessions
            .send(
                &session,
                UserIntent::Followup,
                vec![ContentPart::Text { text: command }],
            )
            .unwrap();
        assert!(matches!(
            result,
            rness_engine::inbox::Disposition::Command(_)
        ));
    }
    let ctx = serde_json::json!({"session":session,"rows":10,"cols":100});
    for _ in 0..3 {
        let lines = host.app_view("jobs", ctx.clone()).await.unwrap();
        assert!(lines.join("\n").contains("model still needs this output"));
    }
    assert_eq!(jobs.count(&session), 1);
    assert!(writer.cancelled().is_cancelled());
    assert!(complete().contains(&(format!("stop {id}"), "bash · stopping · cargo test".into())));
    // Older hosts receive the exact same values as strings, without hint text.
    host.load(
        "legacy-completions",
        "rness.commands.completion_descriptions = nil",
    )
    .await
    .unwrap();
    let legacy = complete();
    assert_eq!(
        legacy.iter().map(|(value, _)| value).collect::<Vec<_>>(),
        choices.iter().map(|(value, _)| value).collect::<Vec<_>>()
    );
    assert!(legacy.iter().all(|(_, description)| description.is_empty()));
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_value(sessions.store().history(&session).unwrap()).unwrap(),
        before
    );
    let output = tools
        .dispatch(
            &session,
            &[rness_engine::tools::ToolCall {
                call: "read".into(),
                name: "job_output".into(),
                args: serde_json::json!({"job_id":id}),
            }],
            1,
            &CancellationToken::new(),
        )
        .await;
    assert!(output[0].output.contains("model still needs this output"));
    writer.settle(JobStatus::Killed);
    assert_eq!(jobs.count(&session), 0);
    assert_eq!(
        complete()
            .iter()
            .map(|(value, _)| value.as_str())
            .collect::<Vec<_>>(),
        ["", "list", "stop"]
    );
    let result = sessions
        .send(
            &session,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: "/jobs list".into(),
            }],
        )
        .unwrap();
    assert!(
        matches!(result, rness_engine::inbox::Disposition::Command(result)
        if result.message == "No active background jobs in this session.")
    );
    let result = sessions
        .send(
            &session,
            UserIntent::Followup,
            vec![ContentPart::Text {
                text: format!("/jobs {exited}"),
            }],
        )
        .unwrap();
    assert!(
        matches!(result, rness_engine::inbox::Disposition::Command(result)
        if result.data == serde_json::json!({"action":"app:open","app":"jobs","session":session}))
    );
    let lines = host.app_view("jobs", ctx).await.unwrap();
    assert!(lines[0].contains("exited (code 7)"));
    sessions.cancel(&session);
}

#[tokio::test]
async fn jobs_startup_setup_survives_mount_and_reload_with_callbacks() {
    let dir = tempfile::tempdir().unwrap();
    let init = dir.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
        rness.jobs.setup {
          title='Startup jobs', refresh_ms=700, layout={height=15,width=75},
          render=function(ctx,m,lines) lines[1]='From init: '..ctx.session; return lines end,
        }
    "#,
    )
    .unwrap();
    let (host, _) = LuaHost::spawn_from_init(init).unwrap();
    host.install_jobs(JobRegistry::new()).await.unwrap();
    let source = rness_lua::loader::PluginSource {
        dependencies: vec![],
        name: "jobs".into(),
        source: include_str!("../../../flavors/default/plugins/jobs.lua").into(),
    };
    host.load("jobs", &source.source).await.unwrap();
    for reload in [false, true] {
        if reload {
            host.reload(vec![source.clone()]).await.unwrap();
        }
        let apps = host.app_specs().await;
        let app = apps.iter().find(|app| app.name == "jobs").unwrap();
        assert_eq!(app.title, "Startup jobs");
        assert_eq!(app.refresh_ms, Some(700));
        assert!(app.capture_escape);
        assert_eq!(app.config["height"], 15);
        assert_eq!(app.config["width"], 75);
        assert_eq!(
            host.app_view("jobs", serde_json::json!({"session":"owner"}))
                .await
                .unwrap()[0],
            "From init: owner"
        );
    }
}

#[tokio::test]
async fn jobs_failed_reload_preserves_selection_options_and_setup_api() {
    let dir = tempfile::tempdir().unwrap();
    let init = dir.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
      original_setup=rness.jobs.setup
      rness.jobs.setup {title='Original', render=function(ctx,m,lines)
        lines[1]='Original '..(m.selected or 'list'); return lines
      end}
    "#,
    )
    .unwrap();
    let (host, _) = LuaHost::spawn_from_init(init).unwrap();
    let registry = JobRegistry::new();
    let (id, writer) = registry.start_owned("bash", "test".into(), Some(&"owner".into()));
    writer.append(b"retained output");
    host.install_jobs(registry).await.unwrap();
    // Capture command invocation without requiring a mounted session service.
    host.load(
        "command-fixture",
        "rness.commands.register=function(command) jobs_command=command end",
    )
    .await
    .unwrap();
    let plugin = include_str!("../../../flavors/default/plugins/jobs.lua");
    host.load("jobs", plugin).await.unwrap();
    host.load(
        "select",
        &format!(
            r#"
      assert(rness.jobs.setup==original_setup)
      original_jobs_command=jobs_command
      local result=jobs_command.run({{session='owner',raw_input={id:?}}})
      assert(result.data.app=='jobs')
      assert(not pcall(rness.jobs.setup, {{title='late'}}))
    "#
        ),
    )
    .await
    .unwrap();
    let ctx = serde_json::json!({"session":"owner","rows":10,"cols":100});
    let before = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert_eq!(before[0], format!("Original {id}"));
    assert!(before.join("\n").contains("retained output"));
    let bad = rness_lua::loader::PluginSource {
        dependencies: vec![],
        name: "jobs".into(),
        source: format!("{plugin}\nerror('failed reload')"),
    };
    assert!(host.reload(vec![bad]).await.is_err());
    assert_eq!(host.app_view("jobs", ctx.clone()).await.unwrap(), before);
    assert_eq!(host.app_specs().await[0].title, "Original");
    host.load("still-original", "assert(rness.jobs.setup==original_setup); original_jobs_command.run({session='owner',raw_input=''})").await.unwrap();
    assert_eq!(
        host.app_view("jobs", ctx).await.unwrap()[0],
        "Original list"
    );
    writer.settle(JobStatus::Exited(Some(0)));
}
