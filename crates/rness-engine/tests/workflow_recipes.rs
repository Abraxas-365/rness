//! Runs every recipe in `examples/workflows/` through the real scheduler with
//! a fake child runner, so the shipped examples cannot rot: each must parse,
//! pass `agent()` option and schema validation, and complete — both when every
//! member succeeds and when some members fail (the recipes' nil handling).
//!
//! A recipe declares its sample input in two header comments:
//! `-- meta: {json}` and `-- args: {json}`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rness_engine::workflow::{
    self, ChildOutcome, ChildRequest, ChildRunner, WorkflowLimits, WorkflowStop,
};
use rness_protocol::events::SessionId;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Completes every member, synthesizing a minimal value that satisfies the
/// requested schema. With `fail_even`, members with an even seq fail.
struct Fake {
    fail_even: bool,
    requests: Mutex<Vec<ChildRequest>>,
}

fn sample(schema: &Value) -> Value {
    if let Some(value) = schema.get("const") {
        return value.clone();
    }
    if let Some(first) = schema.get("enum").and_then(|e| e.get(0)) {
        return first.clone();
    }
    if let Some(first) = schema.get("oneOf").and_then(|o| o.get(0)) {
        return sample(first);
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let mut out = serde_json::Map::new();
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (key, prop) in props {
                    out.insert(key.clone(), sample(prop));
                }
            }
            Value::Object(out)
        }
        Some("array") => json!([sample(schema.get("items").unwrap_or(&json!({})))]),
        Some("integer") | Some("number") => json!(1),
        Some("boolean") => json!(true),
        _ => json!("sample"),
    }
}

#[async_trait]
impl ChildRunner for Fake {
    fn check_role(&self, _role: Option<&str>) -> Result<(), String> {
        Ok(())
    }

    async fn run(
        &self,
        request: ChildRequest,
        _cancel: CancellationToken,
        started: Box<dyn FnOnce(SessionId) + Send>,
    ) -> ChildOutcome {
        started(format!("child-{}", request.seq));
        self.requests.lock().unwrap().push(request.clone());
        if self.fail_even && request.seq % 2 == 0 {
            return ChildOutcome::Failed("fake failure".into());
        }
        ChildOutcome::Completed {
            text: format!("text for {}", request.label),
            structured: request.schema.as_ref().map(sample),
        }
    }
}

fn header(script: &str, key: &str) -> Value {
    let prefix = format!("-- {key}: ");
    let line = script
        .lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("recipe is missing its `{prefix}` header"));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("bad {key} header: {e}"))
}

async fn run_recipe(
    script: &str,
    fail_even: bool,
) -> (workflow::WorkflowResult, Vec<ChildRequest>) {
    let meta = workflow::validate_meta(&header(script, "meta")).unwrap();
    let limits = WorkflowLimits {
        max_concurrent_agents: 4,
        dispose_grace: Duration::from_secs(2),
        ..WorkflowLimits::default()
    };
    workflow::check_script(&meta, script, &limits).unwrap();
    let fake = Arc::new(Fake {
        fail_even,
        requests: Mutex::default(),
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        workflow::run(
            meta,
            script.into(),
            header(script, "args"),
            limits,
            fake.clone(),
            Arc::new(|_| {}),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("recipe must settle");
    let requests = fake.requests.lock().unwrap().clone();
    (result, requests)
}

#[tokio::test(flavor = "multi_thread")]
async fn every_recipe_completes_with_and_without_member_failures() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/workflows");
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".lua"))
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["adversarial.lua", "audit.lua", "migrate.lua", "review.lua"]
    );
    for name in names {
        let script = std::fs::read_to_string(format!("{dir}/{name}")).unwrap();
        for fail_even in [false, true] {
            let (result, requests) = run_recipe(&script, fail_even).await;
            assert_eq!(
                result.stop,
                WorkflowStop::Completed,
                "{name} (fail_even={fail_even}): {:?}",
                result.error
            );
            assert!(requests.len() >= 2, "{name} should fan out");
            assert!(
                requests.iter().all(|r| r.role.is_some()),
                "{name}: recipes name their roles explicitly"
            );
            let value = result.value.unwrap();
            assert!(!value.is_null(), "{name} returned nothing");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recipes_return_what_their_comments_promise() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/workflows");
    let read = |name: &str| std::fs::read_to_string(format!("{dir}/{name}")).unwrap();

    // audit: two targets, scan+verify each; all confirmed when nothing fails.
    let (result, requests) = run_recipe(&read("audit.lua"), false).await;
    assert_eq!(requests.len(), 4);
    let value = result.value.unwrap();
    assert_eq!(value["confirmed"].as_array().unwrap().len(), 2);
    assert_eq!(value["failed_targets"], json!([]));
    // With failures, the failed targets are reported instead of dropped.
    let (result, _) = run_recipe(&read("audit.lua"), true).await;
    let value = result.value.unwrap();
    assert!(!value["failed_targets"].as_array().unwrap().is_empty());

    // review: one member per angle plus the merge; the merge's text wins.
    let (result, requests) = run_recipe(&read("review.lua"), false).await;
    assert_eq!(requests.len(), 5);
    assert_eq!(result.value.unwrap(), json!("text for merge"));

    // adversarial: finder, then one skeptic per candidate.
    let (result, requests) = run_recipe(&read("adversarial.lua"), false).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(result.value.unwrap()["checked"], json!(1));

    // migrate: edit+check per file, then the build.
    let (result, requests) = run_recipe(&read("migrate.lua"), false).await;
    assert_eq!(requests.len(), 5);
    let value = result.value.unwrap();
    assert_eq!(value["migrated"].as_array().unwrap().len(), 2);
    assert_eq!(value["build"]["ok"], json!(true));
}
