use std::{sync::Arc, collections::BTreeSet};
use rness_engine::tools::{Tool, ToolRegistry, ToolCall, exposure::{Exposure, Mode, program}};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Echo;

#[test]
fn search_ranks_partial_matches_reports_limits_and_respects_scope() {
    struct Named(String);
    #[async_trait::async_trait]
    impl Tool for Named {
        fn name(&self) -> &str { &self.0 }
        async fn execute(&self, _: Value) -> Result<String, String> { Ok(String::new()) }
    }
    let tools = ToolRegistry::default();
    for i in 0..25 { tools.register(Arc::new(Named(format!("chrome_{i:02}")))); }
    tools.register(Arc::new(Named("chrome_browser".into())));
    let exposure = Exposure::default();
    let (output, names) = exposure.search(&tools, &json!({"query":"chrome MCP browser"})).unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(names.len(), 20);
    assert_eq!(names[0], "chrome_browser");
    assert_eq!(output["total_matches"], 26);
    assert_eq!(output["truncated"], true);
    let requested = (0..25).map(|i| format!("CHROME_{i:02}")).collect::<Vec<_>>().join(",");
    let (output, names) = exposure.search(&tools, &json!({"query":format!(" select:{requested},missing ")})).unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(names.len(), 25);
    assert_eq!(output["truncated"], false);
    assert_eq!(output["missing"], json!(["missing"]));
    let (output, names) = exposure.search(&tools.restricted(&[]), &json!({"query":"chrome"})).unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert!(names.is_empty());
    assert_eq!(output["total_matches"], 0);
    assert!(output["guidance"].as_str().unwrap().contains("No permitted"));
    assert!(exposure.search(&tools, &json!({"query":"select: , "})).is_err());
}

#[tokio::test(flavor="multi_thread")]
async fn parallel_calls_overlap_and_keep_input_order() {
    struct BarrierTool(Arc<tokio::sync::Barrier>);
    #[async_trait::async_trait]
    impl Tool for BarrierTool {
        fn name(&self)->&str { "Barrier" }
        fn concurrency_safe(&self, _: &Value) -> bool { true }
        async fn execute(&self,args:Value)->Result<String,String> { self.0.wait().await; Ok(args["index"].to_string()) }
    }
    let registry = Arc::new(ToolRegistry::default());
    registry.register(Arc::new(BarrierTool(Arc::new(tokio::sync::Barrier::new(4)))));
    let call = ToolCall {call:"p".into(),name:"run_code".into(),args:json!({"code":"local calls = {}; for i=1,8 do calls[i]={name='Barrier',args={index=i}} end; local results=tools.parallel(calls); for i=1,8 do assert(results[i].output == tostring(i)) end; return results"})};
    let (result,nested) = tokio::time::timeout(std::time::Duration::from_secs(5),program(registry,"s".into(),call,CancellationToken::new(),None)).await.unwrap();
    assert!(!result.is_error,"{}",result.output); assert_eq!(nested.len(),8);
    for (index,(call,_)) in nested.iter().enumerate() { assert_eq!(call.call,format!("p/{index}")); }
}

#[tokio::test]
async fn background_admission_uses_permissions_not_turn_exposure() {
    use rness_protocol::events::*;
    struct Named(&'static str);
    #[async_trait::async_trait]
    impl Tool for Named {
        fn name(&self) -> &str { self.0 }
        fn starts_background_job(&self, _: &Value) -> bool { self.0 == "Background" }
        async fn execute(&self, _: Value) -> Result<String, String> { Ok("executed".into()) }
    }
    struct BackgroundFlow { step: std::sync::atomic::AtomicUsize, mixed: bool }
    #[async_trait::async_trait]
    impl rness_engine::turn::provider::Provider for BackgroundFlow {
        fn model(&self) -> &str { "test" }
        async fn step(&self, request: rness_engine::turn::provider::StepRequest<'_>, _: &CancellationToken) -> rness_engine::turn::provider::StepOutcome {
            let first = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            assert!(request.tools.iter().all(|tool| !tool.name.starts_with("job_")));
            let mut content = Vec::new();
            if first {
                if self.mixed {
                    content.push(ContentPart::ToolUse { call:"search".into(), name:"ToolSearch".into(), args:json!({"query":"select:Background"}) });
                }
                content.push(ContentPart::ToolUse { call:"start".into(), name:"Background".into(), args:json!({}) });
                // Knowing a deferred tool's name still must not permit a direct call.
                content.push(ContentPart::ToolUse { call:"hidden".into(), name:"job_output".into(), args:json!({}) });
            }
            rness_engine::turn::provider::StepOutcome::Committed(AssistantMessage {
                model:"test".into(), content, stop:if first { StopReason::ToolUse } else { StopReason::EndTurn },
                usage:Usage::default(), chunks:vec![],
            })
        }
    }
    for mixed in [false, true] {
        for permitted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = rness_engine::session::branch::SessionStore::new(dir.path());
            let mut log = store.create(None).unwrap();
            let session = log.session().clone();
            let tools = ToolRegistry::default();
            for name in ["Background", "job_output", "job_list", "job_kill"] { tools.register(Arc::new(Named(name))); }
            if !permitted {
                log.append(&SessionEvent::RequestConfig(CallConfig {
                    tool_ceiling: Some(vec!["Background".into()]), ..Default::default()
                })).unwrap();
            }
            let config = rness_engine::turn::TurnConfig {
                tool_exposure: Exposure { mode:Mode::Native, deferred:["job_output", "job_list", "job_kill"].map(str::to_owned).to_vec() },
                ..Default::default()
            };
            let provider = BackgroundFlow { step:0.into(), mixed };
            rness_engine::turn::run_turn(&store, &mut log, &provider, &tools, &config, &CancellationToken::new(), &mut || vec![], 1, &|_| {}).await.unwrap();
            drop(log);
            let history = store.history(&session).unwrap();
            let results: Vec<_> = history.iter().filter_map(|event| match &event.event {
                SessionEvent::ToolResult(result) => Some(result), _ => None,
            }).collect();
            let start = results.iter().find(|result| result.call == "start").unwrap();
            assert_eq!(start.is_error, !permitted, "{}", start.output);
            assert_eq!(start.output.contains("executed"), permitted);
            assert!(results.iter().find(|result| result.call == "hidden").unwrap().is_error);
        }
    }
}

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
