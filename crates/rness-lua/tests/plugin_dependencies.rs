//! Dependency ordering and ownership through the public Lua actor API.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::{
    questions::Questions,
    service::SessionService,
    session::branch::SessionStore,
    subagent::SubagentRuntime,
    tools::ToolRegistry,
    turn::{
        provider::{Provider, StepOutcome, StepRequest},
        TurnConfig,
    },
};
use rness_kernel::EventBus;
use rness_lua::{loader::PluginSource, plugin_host::LuaHost};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn source(name: &str, lua: &str, dependencies: &[&str]) -> PluginSource {
    PluginSource {
        name: name.into(),
        source: lua.into(),
        dependencies: dependencies.iter().map(|name| (*name).into()).collect(),
    }
}

#[test]
fn default_flavor_selects_questions_without_startup_activation() {
    let init =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default/init.lua");
    let (_host, config) = LuaHost::spawn_from_init(init).unwrap();
    assert!(!config.ask_user);
    let questions = config
        .plugin_specs
        .iter()
        .find(|spec| spec.name == "questions")
        .unwrap();
    assert!(questions.enabled);
    let plan = config
        .plugin_specs
        .iter()
        .find(|spec| spec.name == "plan")
        .unwrap();
    assert!(plan.enabled);
    assert_eq!(plan.dependencies, vec!["questions"]);
}

#[tokio::test]
async fn failed_dependency_skips_consumer_without_executing_source() {
    let host = LuaHost::spawn().unwrap();
    assert!(host
        .load_with_dependencies("base", "error('base failed')", &[])
        .await
        .is_err());
    let error = host
        .load_with_dependencies(
            "consumer",
            "consumer_ran = true; rness.hook.on('tick', function() leaked = true end)",
            &["base".to_owned()],
        )
        .await
        .unwrap_err();
    assert!(error.contains("base"), "{error}");
    host.fire_hook("tick", json!({}));
    host.load(
        "independent",
        "assert(consumer_ran == nil); assert(leaked == nil)",
    )
    .await
    .unwrap();
    assert_eq!(host.plugin_names().await, vec!["independent"]);

    // A failed attempt must not reserve the consumer's name or dependency edge.
    host.load_with_dependencies("base", "", &[]).await.unwrap();
    host.load_with_dependencies("consumer", "", &["base".to_owned()])
        .await
        .unwrap();
}

#[tokio::test]
async fn dependency_unload_is_blocked_until_all_consumers_are_gone() {
    let host = LuaHost::spawn().unwrap();
    host.load_with_dependencies(
        "base",
        "rness.tool.register{name='base_tool', run=function() return 'live' end}",
        &[],
    )
    .await
    .unwrap();
    host.load_with_dependencies("middle", "", &["base".to_owned()])
        .await
        .unwrap();
    host.load_with_dependencies("leaf", "", &["middle".to_owned()])
        .await
        .unwrap();

    let error = host.unload("base").await.unwrap_err();
    assert!(error.contains("middle"), "{error}");
    assert_eq!(
        host.call_tool("base_tool", json!({})).await.unwrap(),
        "live"
    );
    assert!(host.unload("middle").await.is_err());
    assert!(host.unload("leaf").await.unwrap());
    assert!(host.unload("middle").await.unwrap());
    assert!(host.unload("base").await.unwrap());
    assert!(!host.unload("base").await.unwrap());
    assert!(host.tool_specs().await.is_empty());
}

#[tokio::test]
async fn reload_orders_dependencies_and_rolls_back_edges_tools_and_hooks() {
    let host = LuaHost::spawn().unwrap();
    let base = source(
        "base",
        "rness.hook.on('tick', function() old_ticks = (old_ticks or 0) + 1 end)",
        &[],
    );
    let consumer = source(
        "consumer",
        "rness.tool.register{name='version', run=function() return 'old' end}",
        &["base"],
    );
    // Reverse input order must still load the dependency first.
    host.reload(vec![consumer.clone(), base.clone()])
        .await
        .unwrap();
    assert!(host.unload("base").await.is_err());

    let replacement = source(
        "replacement",
        "rness.hook.on('tick', function() staged_ticks = (staged_ticks or 0) + 1 end)",
        &[],
    );
    let changed = source(
        "consumer",
        "rness.tool.register{name='version', run=function() return 'new' end}",
        &["replacement"],
    );
    let broken = source("broken", "rness.hook.on('tick', function() broken_ticks = (broken_ticks or 0) + 1 end); error('reload failed')", &["consumer"]);
    assert!(host
        .reload(vec![changed.clone(), replacement.clone(), broken])
        .await
        .is_err());
    assert_eq!(host.call_tool("version", json!({})).await.unwrap(), "old");
    assert!(host.unload("base").await.is_err());
    assert!(!host.unload("replacement").await.unwrap());
    host.fire_hook("tick", json!({}));
    host.load(
        "check_rollback",
        "assert(old_ticks == 1); assert(staged_ticks == nil); assert(broken_ticks == nil)",
    )
    .await
    .unwrap();

    // A successful reload replaces edges and removes the previous hook set.
    host.reload(vec![changed, replacement, source("base", "", &[])])
        .await
        .unwrap();
    assert_eq!(host.call_tool("version", json!({})).await.unwrap(), "new");
    assert!(host.unload("base").await.unwrap());
    assert!(host.unload("replacement").await.is_err());
    host.fire_hook("tick", json!({}));
    host.load(
        "check_success",
        "assert(old_ticks == 1); assert(staged_ticks == 1); assert(broken_ticks == nil)",
    )
    .await
    .unwrap();
    host.unload("consumer").await.unwrap();
    host.unload("replacement").await.unwrap();
    host.fire_hook("tick", json!({}));
    host.load("check_unload", "assert(staged_ticks == 1)")
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_reload_graph_preserves_previous_dependencies() {
    let host = LuaHost::spawn().unwrap();
    host.reload(vec![
        source("consumer", "", &["base"]),
        source("base", "", &[]),
    ])
    .await
    .unwrap();
    for invalid in [
        vec![source("consumer", "invalid_ran = true", &["missing"])],
        vec![
            source("consumer", "invalid_ran = true", &["base"]),
            source("base", "invalid_ran = true", &["consumer"]),
        ],
    ] {
        assert!(host.reload(invalid).await.is_err());
        assert!(host.unload("base").await.is_err());
    }
    host.load("check", "assert(invalid_ran == nil)")
        .await
        .unwrap();
    host.unload("consumer").await.unwrap();
    host.unload("base").await.unwrap();
}

#[tokio::test]
async fn reload_skips_failed_consumers_of_unloaded_dependencies() {
    let host = LuaHost::spawn().unwrap();
    let sources = vec![
        source("leaf", "leaf_ran = true", &["consumer"]),
        source("consumer", "consumer_runs = (consumer_runs or 0) + 1; error('consumer failed')", &["base"]),
        source("base", "base_runs = (base_runs or 0) + 1", &[]),
        source("other", "other_runs = (other_runs or 0) + 1; rness.tool.register{name='other_runs', run=function() return tostring(other_runs) end}", &[]),
    ];
    let errors = rness_lua::loader::load_all(&host, &sources).await;
    assert_eq!(errors.len(), 2);
    assert!(errors.iter().any(|(name, _)| name == "consumer"));
    assert!(errors.iter().any(|(name, _)| name == "leaf"));
    assert_eq!(host.call_tool("other_runs", json!({})).await.unwrap(), "1");
    assert!(host.unload("base").await.unwrap());

    // A watcher retains the original source list, including failed/skipped plugins.
    host.reload(sources.clone()).await.unwrap();
    assert_eq!(host.plugin_names().await, vec!["other"]);
    assert_eq!(host.call_tool("other_runs", json!({})).await.unwrap(), "2");

    // Invalid edges on excluded plugins must still fail original-graph validation.
    let mut missing = sources.clone();
    missing[1].dependencies.push("missing".into());
    let mut cyclic = sources;
    cyclic[2].dependencies.push("consumer".into());
    for invalid in [missing, cyclic] {
        assert!(host.reload(invalid).await.is_err());
        assert_eq!(host.plugin_names().await, vec!["other"]);
        assert_eq!(host.call_tool("other_runs", json!({})).await.unwrap(), "2");
    }
    host.load(
        "check",
        "assert(base_runs == 1); assert(consumer_runs == 1); assert(leaf_ran == nil)",
    )
    .await
    .unwrap();
}

struct NoProviderCalls;

#[async_trait]
impl Provider for NoProviderCalls {
    fn model(&self) -> &str {
        "test"
    }
    async fn step(&self, _: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        panic!("plugin dependency tests must not call a provider")
    }
}

#[tokio::test]
async fn bundled_plan_questions_dependencies_survive_reload_rollback_and_unload() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(
        SessionStore::new(dir.path()),
        Arc::new(NoProviderCalls),
        registry.clone(),
        TurnConfig::default(),
        Arc::new(EventBus::default()),
    ));
    let host = LuaHost::spawn().unwrap();
    host.install_session(
        sessions.clone(),
        Arc::new(SubagentRuntime::new(sessions.clone(), 3)),
        registry,
        Default::default(),
        tokio::runtime::Handle::current(),
        "test/model".into(),
    )
    .await
    .unwrap();
    let questions = Arc::new(Questions::default());
    host.install_questions(questions.clone()).await.unwrap();

    let questions_source = source(
        "questions",
        include_str!("../../../flavors/default/plugins/questions.lua"),
        &[],
    );
    let plan_source = source(
        "plan",
        include_str!("../../../flavors/default/plugins/plan.lua"),
        &["questions"],
    );
    assert!(host
        .load_with_dependencies(
            &plan_source.name,
            &plan_source.source,
            &plan_source.dependencies
        )
        .await
        .is_err());
    assert!(sessions.commands().resolve("/plan").is_none());
    assert!(!questions.is_available());
    host.reload(vec![plan_source.clone(), questions_source])
        .await
        .unwrap();
    assert!(questions.is_available());
    assert!(sessions.commands().resolve("/plan").is_some());
    let original = host
        .tool_specs()
        .await
        .into_iter()
        .find(|spec| spec.name == "exit_plan_mode")
        .unwrap()
        .plan
        .unwrap();
    assert!(Arc::ptr_eq(&original.questions, &questions));

    // Loading the dependency does not bypass the live headless availability check.
    let mut log = original.store.create(None).unwrap();
    let session = log.session().clone();
    log.append(&rness_protocol::events::SessionEvent::PlanMode { active: true })
        .unwrap();
    questions.set_available(false);
    let error = original
        .review(
            &session,
            "headless-review",
            json!({"plan": "# Implementation\nAdd dependency tests."}),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.contains("no Questions frontend"), "{error}");
    assert!(questions.pending().is_empty());
    assert!(
        rness_protocol::events::PlanState::from_history(&original.store.history(&session).unwrap())
            .active
    );
    questions.set_available(true);

    let mut events = questions.subscribe();
    let reviewing = original.clone();
    let review_session = session.clone();
    let review = tokio::spawn(async move {
        reviewing
            .review(
                &review_session,
                "approved-review",
                json!({"plan": "# Implementation\nAdd dependency tests."}),
                &CancellationToken::new(),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    questions
        .resolve(
            &session,
            "approved-review",
            rness_engine::questions::Answers {
                answers: vec![rness_engine::questions::Answer {
                    id: "plan-review".into(),
                    selected: vec!["Approve".into()],
                    custom: None,
                }],
            },
        )
        .unwrap();
    let (_, result) = tokio::time::timeout(std::time::Duration::from_secs(2), review)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result, rness_protocol::events::PlanReview::Approved);

    // Even a later failing chunk must roll back native Questions state and Plan tokens.
    assert!(host
        .reload(vec![
            source("questions", "rness.questions.disable()", &[]),
            plan_source,
            source("broken", "error('rollback native dependencies')", &["plan"]),
        ])
        .await
        .is_err());
    assert!(questions.is_available());
    assert!(!original.alive.is_cancelled());
    assert!(sessions.commands().resolve("/plan").is_some());
    assert!(host
        .unload_coordinated("questions", vec![], |_| panic!(
            "blocked unload must not apply frontend cleanup"
        ))
        .await
        .is_err());

    // Exercise the copyable examples as a successful replacement too.
    host.reload(vec![
        source(
            "plan",
            include_str!("../../../examples/plugins/plan.lua"),
            &["questions"],
        ),
        source(
            "questions",
            include_str!("../../../examples/plugins/questions.lua"),
            &[],
        ),
    ])
    .await
    .unwrap();
    assert!(original.alive.is_cancelled());
    let current = host
        .tool_specs()
        .await
        .into_iter()
        .find(|spec| spec.name == "exit_plan_mode")
        .unwrap()
        .plan
        .unwrap();
    assert!(Arc::ptr_eq(&current.questions, &questions));
    assert!(host
        .unload_coordinated("plan", vec![], |_| {})
        .await
        .unwrap());
    assert!(current.alive.is_cancelled());
    assert!(sessions.commands().resolve("/plan").is_none());
    assert!(questions.is_available());
    assert!(host
        .unload_coordinated("questions", vec![], |_| {})
        .await
        .unwrap());
    assert!(!questions.is_available());
    assert!(!host
        .tool_specs()
        .await
        .iter()
        .any(|spec| spec.name == "exit_plan_mode"));
}
