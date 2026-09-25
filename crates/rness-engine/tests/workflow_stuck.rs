//! A script stuck inside one long C call (no instruction hooks fire) is
//! abandoned by the async side. Its own test binary: the detached worker
//! keeps spinning until the process exits, which must not slow the
//! timing-sensitive tests in `workflow.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rness_engine::workflow::{
    self, ChildOutcome, ChildRequest, ChildRunner, WorkflowLimits, WorkflowStop,
};
use rness_protocol::events::SessionId;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Instant_;

#[async_trait]
impl ChildRunner for Instant_ {
    fn check_role(&self, _: Option<&str>) -> Result<(), String> {
        Ok(())
    }

    async fn run(
        &self,
        request: ChildRequest,
        _: CancellationToken,
        started: Box<dyn FnOnce(SessionId) + Send>,
    ) -> ChildOutcome {
        started(format!("child-{}", request.seq));
        ChildOutcome::Completed {
            text: "ok".into(),
            structured: None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_stuck_in_a_c_call_is_abandoned() {
    let meta = workflow::validate_meta(&json!({"name":"stuck","description":"d"})).unwrap();
    let limits = WorkflowLimits {
        script_budget: Duration::from_millis(100),
        dispose_grace: Duration::from_secs(1),
        ..WorkflowLimits::default()
    };
    let begin = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        workflow::run(
            meta,
            r#"local _ = agent("one")
               local s = string.rep("a", 200000)
               return s:find(string.rep("a-", 40) .. "b")"#
                .into(),
            Value::Null,
            limits,
            Arc::new(Instant_),
            Arc::new(|_| {}),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("a stuck script must not hang the run");
    assert_eq!(result.stop, WorkflowStop::Error);
    let error = result.error.unwrap();
    assert!(error.contains("single library call"), "{error}");
    // The detached run still reports the agents it started.
    assert_eq!(result.agents_started, 1);
    assert!(begin.elapsed() < Duration::from_secs(6));
}
