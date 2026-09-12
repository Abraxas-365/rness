use std::{sync::Arc, collections::BTreeSet};
use rness_engine::tools::{Tool, ToolRegistry, ToolCall, exposure::{Exposure, Mode, program}};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Echo;
struct Flow(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl rness_engine::turn::provider::Provider for Flow {
    fn model(&self) -> &str { "test" }
    async fn step(&self, request: rness_engine::turn::provider::StepRequest<'_>, _: &CancellationToken) -> rness_engine::turn::provider::StepOutcome {
        use rness_protocol::events::*;
        let step = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(request.tools.iter().any(|s| s.name == "Echo"), step > 0);
        let content = match step {
            0 => vec![ContentPart::ToolUse {call:"search".into(),name:"ToolSearch".into(),args:json!({"query":"select:Echo"})}],
            1 => vec![ContentPart::ToolUse {call:"program".into(),name:"run_code".into(),args:json!({"code":"return tools.call('Echo', {value=42})"})}],
            _ => vec![ContentPart::Text {text:"done".into()}],
        };
        rness_engine::turn::provider::StepOutcome::Committed(AssistantMessage {model:"test".into(),content,stop:if step < 2 {StopReason::ToolUse} else {StopReason::EndTurn},usage:Usage::default(),chunks:vec![]})
    }
}
#[tokio::test]
async fn activation_and_nested_audit_survive_replay() {
    use rness_protocol::events::*;
    let dir = tempfile::tempdir().unwrap();
    let store = rness_engine::session::branch::SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let session = log.session().clone();
    let tools = ToolRegistry::default(); tools.register(Arc::new(Echo));
    let config = rness_engine::turn::TurnConfig {tool_exposure:Exposure {mode:Mode::Both,deferred:vec!["Echo".into()]},..Default::default()};
    rness_engine::turn::run_turn(&store,&mut log,&Flow(0.into()),&tools,&config,&CancellationToken::new(),&mut || vec![],1,&|_| {}).await.unwrap();
    drop(log);
    let replayed = rness_engine::session::replay::replay(&store,&session).unwrap();
    assert!(Exposure::activated(&replayed.history).contains("Echo"));
    let started = replayed.history.iter().position(|e| matches!(e.event,SessionEvent::ProgramToolStarted {..})).unwrap();
    let ended = replayed.history.iter().position(|e| matches!(e.event,SessionEvent::ProgramToolResult {..})).unwrap();
    assert!(started < ended);
    assert!(replayed.history.iter().any(|e| matches!(&e.event,SessionEvent::ToolResult(result) if result.name == "run_code" && !result.is_error && result.output.contains("42"))));
}

#[async_trait::async_trait]
impl Tool for Echo {
    fn name(&self) -> &str { "Echo" }
    async fn execute(&self, args: Value) -> Result<String, String> { Ok(args.to_string()) }
}
#[test]
fn discovery_respects_scope_and_modes() {
    let tools = ToolRegistry::default(); tools.register(Arc::new(Echo));
    let config = Exposure { mode:Mode::Both, deferred:vec!["Echo".into()] };
    assert!(!config.specs(&tools, &BTreeSet::new()).iter().any(|s| s.name == "Echo"));
    let (_, names) = config.search(&tools, &json!({"query":"select:Echo"})).unwrap();
    assert!(config.specs(&tools, &names.into_iter().collect()).iter().any(|s| s.name == "Echo"));
    assert!(config.search(&tools.restricted(&[]), &json!({"query":"Echo"})).unwrap().1.is_empty());
    let ptc = Exposure { mode:Mode::Ptc, ..config };
    assert!(!ptc.specs(&tools, &BTreeSet::from(["Echo".into()])).iter().any(|s| s.name == "Echo"));
}
#[tokio::test]
async fn programs_are_isolated_and_budgeted() {
    let tools = Arc::new(ToolRegistry::default()); tools.register(Arc::new(Echo));
    for (code, error) in [
        ("return tools.call('Echo', {hello='world'})", false),
        ("return io.open('/etc/passwd')", true),
        ("return os.execute('true')", true),
        ("return require('socket')", true),
        ("return tools.call('run_code', {})", true),
        ("for i=1,33 do tools.call('Echo', {}) end", true),
    ] {
        let (result, _) = program(tools.clone(), "s".into(), ToolCall {call:"c".into(),name:"run_code".into(),args:json!({"code":code})}, CancellationToken::new(), None).await;
        assert_eq!(result.is_error, error, "{}: {}", code, result.output);
    }
    tools.approvals().set_rules(std::collections::BTreeMap::from([("Echo".into(), rness_engine::approval::ToolPolicy::Deny)]));
    let (_, nested) = program(tools.clone(), "s".into(), ToolCall {call:"denied".into(),name:"run_code".into(),args:json!({"code":"return tools.call('Echo', {})"})}, CancellationToken::new(), None).await;
    assert!(nested[0].1.is_error);
    assert!(nested[0].1.output.contains("rejected"));
    let (_, nested) = program(Arc::new(tools.restricted(&[])), "s".into(), ToolCall {call:"hidden".into(),name:"run_code".into(),args:json!({"code":"return tools.call('Echo', {})"})}, CancellationToken::new(), None).await;
    assert!(nested[0].1.output.contains("unknown tool"));
    let cancel = CancellationToken::new(); cancel.cancel();
    let (result, _) = program(tools, "s".into(), ToolCall {call:"c".into(),name:"run_code".into(),args:json!({"code":"while true do end"})}, cancel, None).await;
    assert!(result.is_error);
}
