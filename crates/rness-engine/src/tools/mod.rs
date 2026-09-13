//! Tool registry + dispatcher: parallel execution, model-order commits.
//!
//! The dispatcher runs all requested calls concurrently (bounded by
//! `max_concurrency`) but returns results in MODEL ORDER — the order the
//! model emitted the tool_use parts. History stays deterministic no
//! matter how execution interleaves (invariant #8).

pub mod exposure;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use rness_protocol::events::{SessionId, ToolCallId, ToolResult, ToolResultContentPart};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::approval::{ApprovalRequest, Approvals, Decision};

/// A tool implementation. Kept deliberately minimal at the engine seam;
/// schemas/descriptions live with registration metadata later.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn plan_config(&self) -> Option<crate::plan::PlanConfig> {
        None
    }
    async fn review_plan(
        &self,
        _session: &str,
        _call: &str,
        _args: serde_json::Value,
        _cancel: &CancellationToken,
    ) -> Result<(String, rness_protocol::events::PlanReview), String> {
        Err("not a plan tool".into())
    }
    /// Bind workspace-dependent tools without mutating shared registrations.
    fn for_workspace(
        &self,
        _session: &SessionId,
        _workspace: &std::path::Path,
    ) -> Option<Arc<dyn Tool>> {
        None
    }
    /// Bind workspace-dependent tools with the immutable filesystem policy
    /// selected for this session. Default preserves existing tools.
    fn for_workspace_with_policy(
        &self,
        session: &SessionId,
        workspace: &std::path::Path,
        _sandbox: rness_protocol::sandbox::SandboxMode,
    ) -> Option<Arc<dyn Tool>> {
        self.for_workspace(session, workspace)
    }
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
    async fn execute_call(
        &self,
        session: &SessionId,
        _call: &str,
        args: serde_json::Value,
        _cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.execute_in(session, args).await
    }
    /// Typed durable effects are committed by the turn's existing log writer.
    async fn execute_with_tasks(
        &self,
        session: &SessionId,
        call: &str,
        args: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<(String, Option<rness_protocol::events::TaskSnapshot>), String> {
        self.execute_call(session, call, args, cancel)
            .await
            .map(|output| (output, None))
    }
    /// Rich-output seam. The default preserves all existing text-only tools;
    /// adapters that return binary attachments override this and must admit
    /// every image through the session-bound image store first.
    async fn execute_rich(
        &self,
        session: &SessionId,
        call: &str,
        args: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<
        (
            Vec<ToolResultContentPart>,
            Option<rness_protocol::events::TaskSnapshot>,
            bool,
        ),
        String,
    > {
        self.execute_with_tasks(session, call, args, cancel)
            .await
            .map(|(output, tasks)| {
                (
                    vec![ToolResultContentPart::Text { text: output }],
                    tasks,
                    false,
                )
            })
    }
    async fn execute_presented(
        &self,
        session: &SessionId,
        call: &ToolCallId,
        args: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<
        (
            Vec<ToolResultContentPart>,
            Option<rness_protocol::events::TaskSnapshot>,
            bool,
            Option<serde_json::Value>,
        ),
        String,
    > {
        self.execute_rich(session, call, args, cancel)
            .await
            .map(|(content, tasks, error)| (content, tasks, error, None))
    }
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
    pub images: Arc<std::sync::OnceLock<Arc<crate::images::ImageStore>>>,
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    deferred: RwLock<HashSet<String>>,
    /// Composition-owned approval seam. Default policy is `Allow`, which
    /// behaves exactly as if the seam didn't exist.
    pub file_references: Arc<crate::file_references::FileReferences>,
    pub plan_selections: Arc<crate::plan::PlanSelections>,
    approvals: Arc<Approvals>,
}

impl ToolRegistry {
    pub fn for_workspace(&self, session: &SessionId, workspace: &std::path::Path) -> Self {
        self.for_workspace_with_policy(
            session,
            workspace,
            rness_protocol::sandbox::SandboxMode::DangerFullAccess,
        )
    }

    pub fn for_workspace_with_policy(
        &self,
        session: &SessionId,
        workspace: &std::path::Path,
        sandbox: rness_protocol::sandbox::SandboxMode,
    ) -> Self {
        let tools = self
            .tools
            .read()
            .expect("registry lock")
            .iter()
            .map(|(name, tool)| {
                (
                    name.clone(),
                    tool.for_workspace_with_policy(session, workspace, sandbox)
                        .unwrap_or_else(|| Arc::clone(tool)),
                )
            })
            .collect();
        Self {
            images: self.images.clone(),
            tools: RwLock::new(tools),
            deferred: RwLock::new(self.deferred.read().expect("registry lock").clone()),
            approvals: Arc::clone(&self.approvals),
            plan_selections: self.plan_selections.clone(),
            file_references: self.file_references.clone(),
        }
    }

    pub fn restricted(&self, allowed: &[String]) -> Self {
        let tools = self
            .tools
            .read()
            .expect("registry lock")
            .iter()
            .filter(|(name, _)| allowed.contains(name))
            .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
            .collect();
        Self {
            images: self.images.clone(),
            tools: RwLock::new(tools),
            deferred: RwLock::new(self.deferred.read().expect("registry lock").clone()),
            approvals: Arc::clone(&self.approvals),
            plan_selections: self.plan_selections.clone(),
            file_references: self.file_references.clone(),
        }
    }

    pub fn register(&self, tool: Arc<dyn Tool>) {
        self.try_register(tool)
            .expect("duplicate tool registration; use replace explicitly");
    }

    pub fn try_register(&self, tool: Arc<dyn Tool>) -> Result<(), String> {
        let mut tools = self.tools.write().expect("registry lock");
        let name = tool.name().to_string();
        if name == "ToolSearch" || name == "run_code" {
            return Err(format!("reserved engine tool name: {name}"));
        }
        if tools.contains_key(&name) {
            return Err(format!("tool already registered: {name}"));
        }
        tools.insert(name, tool);
        Ok(())
    }

    /// Replace only an existing registration; readers keep their previous Arc.
    pub fn replace(&self, tool: Arc<dyn Tool>) -> Result<Arc<dyn Tool>, String> {
        let mut tools = self.tools.write().expect("registry lock");
        let name = tool.name().to_string();
        let entry = tools
            .get_mut(&name)
            .ok_or_else(|| format!("unknown tool: {name}"))?;
        Ok(std::mem::replace(entry, tool))
    }

    /// Compare-and-replace for owners retaining their installed implementation.
    pub fn replace_if_current(
        &self,
        expected: &Arc<dyn Tool>,
        tool: Arc<dyn Tool>,
    ) -> Result<(), String> {
        if expected.name() != tool.name() {
            return Err("replacement name differs".into());
        }
        let mut tools = self.tools.write().expect("registry lock");
        let entry = tools
            .get_mut(expected.name())
            .ok_or("registration no longer exists")?;
        if !Arc::ptr_eq(entry, expected) {
            return Err("registration ownership changed".into());
        }
        *entry = tool;
        Ok(())
    }

    pub fn unregister_if_current(&self, expected: &Arc<dyn Tool>) -> bool {
        let mut tools = self.tools.write().expect("registry lock");
        if tools
            .get(expected.name())
            .is_some_and(|entry| Arc::ptr_eq(entry, expected))
        {
            tools.remove(expected.name());
            true
        } else {
            false
        }
    }

    /// Remove a tool by name. Returns whether it was present. In-flight
    /// dispatches keep their `Arc` — removal affects the next turn.
    pub fn unregister(&self, name: &str) -> bool {
        if self
            .tools
            .write()
            .expect("registry lock")
            .remove(name)
            .is_some()
        {
            self.deferred.write().expect("registry lock").remove(name);
            true
        } else {
            false
        }
    }

    /// Hide registered tools from model tool lists until `ToolSearch` has
    /// selected them for the current session. They remain dispatchable.
    pub fn defer(&self, names: impl IntoIterator<Item = String>) {
        self.deferred.write().expect("registry lock").extend(names);
    }

    pub fn is_deferred(&self, name: &str) -> bool {
        self.deferred.read().expect("registry lock").contains(name)
    }

    pub fn has_deferred(&self) -> bool {
        !self.deferred.read().expect("registry lock").is_empty()
    }

    /// The approval seam: set policy / mount answerers here.
    pub fn approvals(&self) -> &Arc<Approvals> {
        &self.approvals
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().expect("registry lock").get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<_> = self
            .tools
            .read()
            .expect("registry lock")
            .keys()
            .cloned()
            .collect();
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
                let mut plan_review = None;
                let mut presentation = None;
                let mut failure_kind = "execution_failed";
                let outcome = match tool {
                    Some(t) => {
                        {
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
                                biased;
                                d = approvals.check_tool(&request, t.sensitive()) => d,
                                _ = cancel.cancelled() => Decision::Cancelled,
                            };
                            match decision {
                                Decision::Allowed if t.plan_config().is_some() => {
                                    t.review_plan(&session, &call.call, call.args.clone(), &cancel).await.map(|(output, review)| { plan_review = Some(review); (vec![ToolResultContentPart::Text { text: output }], None, false) })
                                }
                                Decision::Allowed => t.execute_presented(&session, &call.call, call.args.clone(), &cancel).await.map(|(content, tasks, error, metadata)| {
                                    presentation = metadata.filter(|value| {
                                        let valid = serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= 256 * 1024);
                                        if !valid { tracing::warn!(tool = %call.name, "discarding oversized tool presentation metadata"); }
                                        valid
                                    });
                                    (content, tasks, error)
                                }),
                                Decision::Rejected => {
                                    failure_kind = "approval_rejected";
                                    Err("the user rejected this tool call".into())
                                }
                                Decision::Cancelled => {
                                    failure_kind = "approval_cancelled";
                                    Err("approval request was cancelled".into())
                                }
                                Decision::Unavailable => {
                                    failure_kind = "approval_unavailable";
                                    Err("approval required but no approver is available — the call was blocked".into())
                                },
                            }
                        }
                    }
                    None => { failure_kind = "unknown_tool"; Err(format!("unknown tool '{}'", call.name)) },
                };
                let (content, tasks, is_error) = match outcome {
                    Ok((content, tasks, is_error)) => (content, tasks, is_error),
                    Err(e) => (vec![ToolResultContentPart::Text { text: e }], None, true),
                };
                let output = ToolResult::text_output(&content);
                let duration_ms = started.elapsed().as_millis() as u64;
                if presentation.is_none() {
                    presentation = Some(serde_json::json!({
                        "version":1,"kind":if tasks.is_some() { "tasks" } else if plan_review.is_some() { "plan_review" } else { "tool_result" },"name":call.name,
                        "outcome":if is_error { failure_kind } else { "completed" },
                        "is_error":is_error,"duration_ms":duration_ms,
                        "content_parts":content.len(),"has_tasks":tasks.is_some(),
                        "has_plan_review":plan_review.is_some(),
                    }));
                }
                ToolResult {
                    plan_review,
                    presentation,
                    tasks,
                    call: call.call,
                    name: call.name,
                    content,
                    output,
                    is_error,
                    duration_ms,
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

    #[tokio::test]
    async fn unknown_tool_has_durable_error_presentation() {
        let registry = ToolRegistry::default();
        let results = registry
            .dispatch(
                &"s".into(),
                &[ToolCall {
                    call: "missing".into(),
                    name: "missing".into(),
                    args: serde_json::json!({}),
                }],
                1,
                &CancellationToken::new(),
            )
            .await;
        let result = &results[0];
        assert!(result.is_error);
        let metadata = result.presentation.as_ref().unwrap();
        assert_eq!(metadata["kind"], "tool_result");
        assert_eq!(metadata["is_error"], true);
        assert_eq!(metadata["duration_ms"], result.duration_ms);
        assert_eq!(result.output, "unknown tool 'missing'");
    }

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
        let results = reg
            .dispatch(&"s".to_string(), &calls, 4, &CancellationToken::new())
            .await;
        // Concurrency: total should be ~80ms, not ~81+ sequential… allow slack.
        assert!(started.elapsed() < Duration::from_millis(160));
        assert_eq!(results[0].output, "slow");
        assert_eq!(results[1].output, "fast");
        assert_eq!(results[0].call, "c1");
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error_result() {
        let reg = ToolRegistry::default();
        let calls = vec![ToolCall {
            call: "c".into(),
            name: "nope".into(),
            args: serde_json::Value::Null,
        }];
        let results = reg
            .dispatch(&"s".to_string(), &calls, 1, &CancellationToken::new())
            .await;
        assert!(results[0].is_error);
        assert!(results[0].output.contains("unknown tool"));
    }
}
