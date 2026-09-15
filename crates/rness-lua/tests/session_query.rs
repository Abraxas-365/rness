use std::{sync::Arc, time::Duration};
use async_trait::async_trait;
use rness_engine::{service::SessionService, session::branch::SessionStore, tools::ToolRegistry,
    turn::{TurnConfig, provider::{Provider, StepOutcome, StepRequest}}};
use rness_kernel::EventBus;
use rness_lua::plugin_host::LuaHost;
use rness_protocol::events::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Unused;
#[async_trait]
impl Provider for Unused {
    fn model(&self) -> &str { "test" }
    async fn step(&self, _: StepRequest<'_>, _: &CancellationToken) -> StepOutcome { panic!("no model calls expected") }
}

#[tokio::test(flavor = "multi_thread")]
async fn opt_in_query_yields_while_sqlite_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    let id = log.session().clone();
    log.append(&SessionEvent::UserMessage(UserMessage { intent:UserIntent::Followup,
        content:vec![ContentPart::Text { text:"needle saved".into() }],source:None })).unwrap();
    let registry = Arc::new(ToolRegistry::default());
    let sessions = Arc::new(SessionService::new(store,Arc::new(Unused),registry.clone(),TurnConfig::default(),Arc::new(EventBus::default())));
    let host = LuaHost::spawn().unwrap();
    host.install_session(sessions.clone(),Arc::new(rness_engine::subagent::SubagentRuntime::new(sessions.clone(),3)),
        registry,Default::default(),tokio::runtime::Handle::current(),"test".into()).await.unwrap();
    let path = dir.path().join("session-search-v2.sqlite3");
    assert!(!path.exists());
    assert!(host.tool_specs().await.is_empty());
    assert!(host.load("tools-missing-provider",include_str!("../../../examples/plugins/session-search.lua")).await.is_err());
    // Failed activation must not leave a live provider behind.
    assert!(host.load("failed-provider", &(include_str!("../../../examples/plugins/session-search-sqlite.lua").to_string()+"\nerror('failed')")).await.is_err());
    host.load("provider",include_str!("../../../examples/plugins/session-search-sqlite.lua")).await.unwrap();
    assert!(host.tool_specs().await.is_empty());
    assert!(!path.exists());
    host.load("tools",include_str!("../../../examples/plugins/session-search.lua")).await.unwrap();
    assert_eq!(host.tool_specs().await.len(),5);
    let output = host.call_tool_context("session_search",json!({"query":"needle"}),json!({"session":id})).await.unwrap();
    assert!(output.contains("needle"),"{output}");
    assert!(path.exists());
    // A separate writer gates refresh; the VM must continue serving requests.
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    let query_host = host.clone();
    let query = tokio::spawn(async move {
        query_host.call_tool_context("session_search",json!({"query":"needle"}),json!({"session":id})).await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!query.is_finished());
    let tools = tokio::time::timeout(Duration::from_secs(1),host.tool_specs()).await.expect("Lua VM remains responsive");
    assert_eq!(tools.len(),5);
    lock.execute_batch("ROLLBACK;").unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(10),query).await.unwrap().unwrap().unwrap().contains("needle"));
}
