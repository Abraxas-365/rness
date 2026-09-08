//! Tool registry + dispatcher: parallel execution, model-order commits.
//!
//! The dispatcher runs all requested calls concurrently (bounded by
//! `max_concurrency`) but returns results in MODEL ORDER — the order the
//! model emitted the tool_use parts. History stays deterministic no
//! matter how execution interleaves (invariant #8).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use rness_protocol::events::{SessionId, ToolCallId, ToolResult};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::approval::{ApprovalRequest, Approvals, Decision};

/// A tool implementation. Kept deliberately minimal at the engine seam;
/// schemas/descriptions live with registration metadata later.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    /// Shown to the model in the tool list.
    fn description(&self) -> &str {
        ""
    }
    /// JSON Schema for `args`. Defaults to "any object".
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    /// Sensitive tools pause for approval under the `ask` policy
    /// (mutations, shell). Non-sensitive tools never ask.
    fn sensitive(&self) -> bool {
        false
    }
    /// Execute with JSON args. `Err` becomes an is_error result — tools
    /// never abort a turn.
    async fn execute(&self, args: serde_json::Value) -> Result<String, String>;
    /// Session-aware entry point the dispatcher calls. Most tools don't
    /// care who called them — the default drops the session. Tools that
    /// delegate (subagent) override this instead of `execute`.
    async fn execute_in(
        &self,
        session: &SessionId,
        args: serde_json::Value,
    ) -> Result<String, String> {
        let _ = session;
        self.execute(args).await
    }
}

/// What a provider needs to advertise a tool to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One requested call, in model order.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call: ToolCallId,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Default)]
pub struct ToolRegistry {
    /// Interior-mutable so composition roots holding `Arc<ToolRegistry>`
    /// can re-sync tools at runtime (Lua plugin hot reload).
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    /// Composition-owned approval seam. Default policy is `Allow`, which
    /// behaves exactly as if the seam didn't exist.
    approvals: Arc<Approvals>,
}

impl ToolRegistry {
    pub fn restricted(&self, allowed: &[String]) -> Self {
        let tools = self.tools.read().expect("registry lock").iter()
            .filter(|(name, _)| allowed.contains(name))
            .map(|(name, tool)| (name.clone(), Arc::clone(tool))).collect();
        Self { tools: RwLock::new(tools), approvals: Arc::clone(&self.approvals) }
    }

    pub fn register(&self, tool: Arc<dyn Tool>) {
        self.try_register(tool).expect("duplicate tool registration; use replace explicitly");
    }

    pub fn try_register(&self, tool: Arc<dyn Tool>) -> Result<(), String> {
        let mut tools = self.tools.write().expect("registry lock");
        let name = tool.name().to_string();
        if tools.contains_key(&name) { return Err(format!("tool already registered: {name}")); }
        tools.insert(name, tool);
        Ok(())
    }

    /// Replace only an existing registration; readers keep their previous Arc.
    pub fn replace(&self, tool: Arc<dyn Tool>) -> Result<Arc<dyn Tool>, String> {
        let mut tools = self.tools.write().expect("registry lock");
        let name = tool.name().to_string();
        let entry = tools.get_mut(&name).ok_or_else(|| format!("unknown tool: {name}"))?;
        Ok(std::mem::replace(entry, tool))
    }

    /// Compare-and-replace for owners retaining their installed implementation.
    pub fn replace_if_current(&self, expected: &Arc<dyn Tool>, tool: Arc<dyn Tool>) -> Result<(), String> {
        if expected.name() != tool.name() { return Err("replacement name differs".into()); }
        let mut tools = self.tools.write().expect("registry lock");
        let entry = tools.get_mut(expected.name()).ok_or("registration no longer exists")?;
        if !Arc::ptr_eq(entry, expected) { return Err("registration ownership changed".into()); }
        *entry = tool;
        Ok(())
    }

    pub fn unregister_if_current(&self, expected: &Arc<dyn Tool>) -> bool {
        let mut tools = self.tools.write().expect("registry lock");
        if tools.get(expected.name()).is_some_and(|entry| Arc::ptr_eq(entry, expected)) {
            tools.remove(expected.name());
            true
        } else { false }
    }

    /// Remove a tool by name. Returns whether it was present. In-flight
    /// dispatches keep their `Arc` — removal affects the next turn.
    pub fn unregister(&self, name: &str) -> bool {
        self.tools.write().expect("registry lock").remove(name).is_some()
    }

    /// The approval seam: set policy / mount answerers here.
    pub fn approvals(&self) -> &Arc<Approvals> {
        &self.approvals
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().expect("registry lock").get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<_> = self.tools.read().expect("registry lock").keys().cloned().collect();
        v.sort();
        v
    }

    /// Specs for every registered tool, name-sorted — what providers
    /// advertise to the model.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut v: Vec<_> = self
            .tools
            .read()
            .expect("registry lock")
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Execute `calls` concurrently (up to `max_concurrency` at once) and
    /// return results in the SAME order as `calls`. An unknown tool yields
    /// an is_error result, not a crash. `session` is the calling session
    /// (delegating tools need to know their parent). `cancel` interrupts
    /// WAITING — a pending approval resolves as cancelled — but a tool
    /// already executing runs to completion (its result still commits).
    pub async fn dispatch(
        &self,
        session: &SessionId,
        calls: &[ToolCall],
        max_concurrency: usize,
        cancel: &CancellationToken,
    ) -> Vec<ToolResult> {
        let sem = Arc::new(Semaphore::new(max_concurrency.max(1)));
        let mut handles = Vec::with_capacity(calls.len());
        for call in calls {
            let sem = Arc::clone(&sem);
            let tool = self.get(&call.name);
            let approvals = Arc::clone(&self.approvals);
            let call = call.clone();
            let session = session.clone();
            let cancel = cancel.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore open");
                let started = Instant::now();
                let outcome = match tool {
                    Some(t) => {
                        if t.sensitive() {
                            let request = ApprovalRequest {
                                session: session.clone(),
                                call: call.call.clone(),
                                tool: call.name.clone(),
                                args: call.args.clone(),
                            };
                            // The question must not outlive the turn: a
                            // cancel while the user deliberates withdraws
                            // it (the dropped check future cleans up
                            // answerer-side state).
                            let decision = tokio::select! {
                                d = approvals.check(&request) => d,
                                _ = cancel.cancelled() => Decision::Cancelled,
                            };
                            match decision {
                                Decision::Allowed => t.execute_in(&session, call.args.clone()).await,
                                Decision::Rejected => {
                                    Err("the user rejected this tool call".into())
                                }
                                Decision::Cancelled => {
                                    Err("approval request was cancelled".into())
                                }
                                Decision::Unavailable => Err(
                                    "approval required but no approver is available — \
                                     the call was blocked"
                                        .into(),
                                ),
                            }
                        } else {
                            t.execute_in(&session, call.args.clone()).await
                        }
                    }
                    None => Err(format!("unknown tool '{}'", call.name)),
                };
                let (output, is_error) = match outcome {
                    Ok(o) => (o, false),
                    Err(e) => (e, true),
                };
                ToolResult {
                    call: call.call,
                    name: call.name,
                    output,
                    is_error,
                    duration_ms: started.elapsed().as_millis() as u64,
                }
            }));
        }
        // Await in model order — commit order == call order regardless of
        // completion order.
        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            results.push(h.await.expect("tool task never panics"));
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct SleepEcho;
    #[async_trait]
    impl Tool for SleepEcho {
        fn name(&self) -> &str {
            "sleep_echo"
        }
        async fn execute(&self, args: serde_json::Value) -> Result<String, String> {
            let ms = args["ms"].as_u64().unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(args["say"].as_str().unwrap_or("").to_string())
        }
    }

    #[tokio::test]
    async fn parallel_execution_commits_in_model_order() {
        let reg = ToolRegistry::default();
        reg.register(Arc::new(SleepEcho));
        // First call is SLOW, second is fast — results still come back
        // slow-first (model order).
        let calls = vec![
            ToolCall {
                call: "c1".into(),
                name: "sleep_echo".into(),
                args: serde_json::json!({"ms": 80, "say": "slow"}),
            },
            ToolCall {
                call: "c2".into(),
                name: "sleep_echo".into(),
                args: serde_json::json!({"ms": 1, "say": "fast"}),
            },
        ];
        let started = Instant::now();
        let results = reg.dispatch(&"s".to_string(), &calls, 4, &CancellationToken::new()).await;
        // Concurrency: total should be ~80ms, not ~81+ sequential… allow slack.
        assert!(started.elapsed() < Duration::from_millis(160));
        assert_eq!(results[0].output, "slow");
        assert_eq!(results[1].output, "fast");
        assert_eq!(results[0].call, "c1");
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error_result() {
        let reg = ToolRegistry::default();
        let calls = vec![ToolCall { call: "c".into(), name: "nope".into(), args: serde_json::Value::Null }];
        let results =
            reg.dispatch(&"s".to_string(), &calls, 1, &CancellationToken::new()).await;
        assert!(results[0].is_error);
        assert!(results[0].output.contains("unknown tool"));
    }
}
