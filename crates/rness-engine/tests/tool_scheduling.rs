use async_trait::async_trait;
use rness_engine::tools::{Tool, ToolCall, ToolRegistry};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

struct Probe {
    name: &'static str,
    safe: bool,
    events: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        self.name
    }
    fn concurrency_safe(&self, _: &Value) -> bool {
        self.safe
    }
    async fn execute(&self, args: Value) -> Result<String, String> {
        let id = args["id"].as_str().unwrap();
        self.events.lock().unwrap().push(format!("start:{id}"));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.events.lock().unwrap().push(format!("end:{id}"));
        Ok(id.into())
    }
}

#[tokio::test]
async fn mutations_are_barriers_between_parallel_reads() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let registry = ToolRegistry::default();
    for (name, safe) in [("read", true), ("write", false)] {
        registry.register(Arc::new(Probe {
            name,
            safe,
            events: events.clone(),
        }));
    }
    let calls: Vec<_> = [("read", "a"), ("read", "b"), ("write", "c"), ("read", "d")]
        .into_iter()
        .map(|(name, id)| ToolCall {
            call: id.into(),
            name: name.into(),
            args: json!({"id":id}),
        })
        .collect();
    let results = registry
        .dispatch(&"s".into(), &calls, 4, &CancellationToken::new())
        .await;
    assert_eq!(
        results
            .iter()
            .map(|r| r.output.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c", "d"]
    );
    let events = events.lock().unwrap();
    assert!(
        events[..2].iter().all(|e| e.starts_with("start:")),
        "{events:?}"
    );
    assert_eq!(&events[4..], ["start:c", "end:c", "start:d", "end:d"]);
}

#[tokio::test]
async fn cancellation_prevents_later_exclusive_calls() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let registry = ToolRegistry::default();
    registry.register(Arc::new(Probe {
        name: "write",
        safe: false,
        events: events.clone(),
    }));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let results = registry
        .dispatch(
            &"s".into(),
            &[ToolCall {
                call: "a".into(),
                name: "write".into(),
                args: json!({"id":"a"}),
            }],
            4,
            &cancel,
        )
        .await;
    assert!(results[0].is_error);
    assert!(events.lock().unwrap().is_empty());
}

/// Ignores its cancel token, like a hung network call or a tool with only
/// `execute(args)`.
struct Stubborn;
#[async_trait]
impl Tool for Stubborn {
    fn name(&self) -> &str {
        "stubborn"
    }
    async fn execute(&self, _: Value) -> Result<String, String> {
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        Ok("finished".into())
    }
}

#[tokio::test]
async fn cancel_abandons_a_tool_that_ignores_its_token() {
    let registry = ToolRegistry::default();
    registry.register(Arc::new(Stubborn));
    let cancel = CancellationToken::new();
    let calls = [ToolCall {
        call: "a".into(),
        name: "stubborn".into(),
        args: json!({}),
    }];
    let session = "s".into();
    let dispatch = registry.dispatch(&session, &calls, 1, &cancel);
    tokio::pin!(dispatch);
    tokio::select! {
        _ = &mut dispatch => panic!("tool finished before cancellation"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
    }
    let cancelled = std::time::Instant::now();
    cancel.cancel();
    let results = tokio::time::timeout(std::time::Duration::from_secs(10), dispatch)
        .await
        .expect("cancel must settle a stuck tool within the grace period");
    assert!(cancelled.elapsed() < std::time::Duration::from_secs(5));
    assert!(results[0].is_error);
    assert!(
        results[0].output.contains("cancelled"),
        "{}",
        results[0].output
    );
    assert_eq!(
        results[0].presentation.as_ref().unwrap()["outcome"],
        "approval_cancelled"
    );
}

#[tokio::test]
async fn cancel_releases_calls_queued_behind_a_stuck_tool() {
    let registry = ToolRegistry::default();
    registry.register(Arc::new(Stubborn));
    let cancel = CancellationToken::new();
    let calls: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|id| ToolCall {
            call: id.into(),
            name: "stubborn".into(),
            args: json!({}),
        })
        .collect();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        canceller.cancel();
    });
    let results = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        registry.dispatch(&"s".into(), &calls, 1, &cancel),
    )
    .await
    .expect("queued calls must not wait for the stuck one");
    assert!(results.iter().all(|r| r.is_error), "{results:?}");
}
