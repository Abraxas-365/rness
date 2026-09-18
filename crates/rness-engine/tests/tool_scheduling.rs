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
