//! Tool registry + dispatcher: parallel execution, model-order commits.
//!
//! Consecutive explicitly concurrency-safe calls overlap (bounded by
//! `max_concurrency`); all other calls form exclusive batch-local barriers.
//! Results remain in MODEL ORDER regardless of completion order (invariant #8).

pub mod exposure;
pub mod hooks;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use rness_protocol::events::{SessionId, ToolCallId, ToolResult, ToolResultContentPart};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::approval::{ApprovalRequest, Approvals, Decision};
use hooks::{ExecuteOutcome, HookContext, PostToolDecision, PreToolDecision, ToolHookEvent, ToolHooks};

/// Max bytes of tool-result text kept inline for the model. Larger results
/// are spilled to a file and replaced with a head/tail preview.
const MAX_INLINE_BYTES: usize = 50 * 1024;
/// How many bytes of head and tail to keep in the inline preview.
const SPILL_PREVIEW_BYTES: usize = 2048;

/// Side data of the most recent tool-body run under `tool_execute`.
#[derive(Default)]
struct BodyRun {
    outcome: Option<ExecuteOutcome>,
    tasks: Option<rness_protocol::events::TaskSnapshot>,
    presentation: Option<serde_json::Value>,
    plan_review: Option<rness_protocol::events::PlanReview>,
}

/// Shared by every registry derived from one composition root, so hooks
/// installed on the main registry also govern subagent sessions.
#[derive(Default)]
struct HookState {
    hooks: RwLock<Option<Arc<dyn ToolHooks>>>,
    contexts: Mutex<Vec<(SessionId, HookContext)>>,
    /// Current turn number, set by the turn loop before dispatch.
    /// Used to populate `ToolHookEvent.turn` for audit events.
    current_turn: std::sync::atomic::AtomicU32,
}

/// A tool implementation. Kept deliberately minimal at the engine seam;
/// schemas/descriptions live with registration metadata later.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    /// Usage guidance this tool contributes to the system prompt. It is sent
    /// only on steps where this tool is usable, so it follows the tool
    /// through registration, role restriction, deferral and plugin unload.
    fn prompt_section(&self) -> Option<crate::prompt::ToolPrompt> {
        None
    }
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
    /// Explicit opt-in to overlap with other safe calls in the same batch.
    /// Unknown/custom/mutating tools are exclusive by default. This governs
    /// dispatch, not work detached into background jobs or other sessions.
    fn concurrency_safe(&self, _args: &serde_json::Value) -> bool {
        false
    }
    /// Whether this call detaches work into the owning session's job registry.
    /// Dispatch requires all job controls in the effective registry before
    /// approval or execution. Continuable agents use separate controls.
    fn starts_background_job(&self, _args: &serde_json::Value) -> bool {
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
    /// In-memory structured-output attachments by child session; shared by
    /// every scoped/restricted clone (never durable, see `structured`).
    pub structured: Arc<crate::structured::StructuredOutputs>,
    approvals: Arc<Approvals>,
    hooks: Arc<HookState>,
    /// Root directory for spill files (oversized tool output).
    /// When set, tool results exceeding `MAX_INLINE_BYTES` are saved here
    /// and the model gets a head/tail preview with a file pointer.
    spill_root: RwLock<Option<std::path::PathBuf>>,
}

/// Save oversized tool-result text to a spill file and replace the content
/// with a head/tail preview pointing at the file. If the combined text of
/// all content parts is within `MAX_INLINE_BYTES`, or if no `spill_root`
/// is configured, the content is returned unchanged.
fn spill_if_oversized(
    content: Vec<ToolResultContentPart>,
    spill_root: &Option<std::path::PathBuf>,
    session: &SessionId,
    call_id: &ToolCallId,
    tool_name: &str,
) -> Vec<ToolResultContentPart> {
    let Some(root) = spill_root else {
        return content;
    };
    // Compute total text size across all parts.
    let total_bytes: usize = content
        .iter()
        .map(|part| match part {
            ToolResultContentPart::Text { text } => text.len(),
            _ => 0,
        })
        .sum();
    if total_bytes <= MAX_INLINE_BYTES {
        return content;
    }
    // Concatenate all text parts for the spill file.
    let mut full_text = String::with_capacity(total_bytes);
    let mut non_text = Vec::new();
    for part in content {
        match part {
            ToolResultContentPart::Text { text } => full_text.push_str(&text),
            other => non_text.push(other),
        }
    }
    // Write spill file: <root>/<session>/spill/<call_id>-<tool>.txt
    let spill_dir = root.join(session.as_str()).join("spill");
    let safe_name = tool_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();
    let file_name = format!("{}-{}.txt", call_id, safe_name);
    let file_path = spill_dir.join(&file_name);
    if let Err(e) = std::fs::create_dir_all(&spill_dir) {
        tracing::warn!(%e, "spill: cannot create directory, returning inline");
        return vec![ToolResultContentPart::Text { text: full_text }];
    }
    if let Err(e) = std::fs::write(&file_path, &full_text) {
        tracing::warn!(%e, "spill: cannot write file, returning inline");
        return vec![ToolResultContentPart::Text { text: full_text }];
    }
    // Build head/tail preview.
    let head_end = char_boundary_before(&full_text, SPILL_PREVIEW_BYTES);
    let tail_start = char_boundary_after(&full_text, full_text.len().saturating_sub(SPILL_PREVIEW_BYTES));
    let omitted = full_text.len() - head_end - (full_text.len() - tail_start);
    let preview = format!(
        "{}\n\n(Omitted {} bytes. Full result: {}. Use Read with offset/limit to page, or Grep to search.)\n\n{}",
        &full_text[..head_end],
        omitted,
        file_path.display(),
        &full_text[tail_start..],
    );
    tracing::debug!(
        tool = tool_name,
        total_bytes,
        spill_path = %file_path.display(),
        "tool output spilled to file"
    );
    let mut result = vec![ToolResultContentPart::Text { text: preview }];
    result.extend(non_text);
    result
}

/// Find the largest byte offset ≤ `target` that is a char boundary.
fn char_boundary_before(s: &str, target: usize) -> usize {
    let mut pos = target.min(s.len());
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Find the smallest byte offset ≥ `target` that is a char boundary.
fn char_boundary_after(s: &str, target: usize) -> usize {
    let mut pos = target.min(s.len());
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos
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
            hooks: Arc::clone(&self.hooks),
            plan_selections: self.plan_selections.clone(),
            structured: self.structured.clone(),
            file_references: self.file_references.clone(),
            spill_root: RwLock::new(self.spill_root.read().expect("registry lock").clone()),
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
            hooks: Arc::clone(&self.hooks),
            plan_selections: self.plan_selections.clone(),
            structured: self.structured.clone(),
            file_references: self.file_references.clone(),
            spill_root: RwLock::new(self.spill_root.read().expect("registry lock").clone()),
        }
    }

    /// A clone with one extra (or overriding) scoped tool — used for the
    /// child-only `structured_output` capture tool.
    pub fn with_tool(&self, tool: Arc<dyn Tool>) -> Self {
        let clone = self.restricted(&self.names());
        clone
            .tools
            .write()
            .expect("registry lock")
            .insert(tool.name().to_string(), tool);
        clone
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
            self.deferred
                .write()
                .expect("registry lock")
                .remove(expected.name());
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

    /// Install (or clear) the tool-pipeline hooks. Shared with every
    /// derived registry, including subagent sessions'.
    pub fn set_hooks(&self, hooks: Option<Arc<dyn ToolHooks>>) {
        *self.hooks.hooks.write().expect("hooks lock") = hooks;
    }

    /// Record the current turn number so tool hooks include it in audit events.
    pub fn set_current_turn(&self, turn: u32) {
        self.hooks.current_turn.store(turn, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the root directory for spill files. Session-scoped subdirectories
    /// are created on demand (e.g. `<root>/<session>/spill/`).
    pub fn set_spill_root(&self, root: std::path::PathBuf) {
        *self.spill_root.write().expect("registry lock") = Some(root);
    }

    fn tool_hooks(&self) -> Option<Arc<dyn ToolHooks>> {
        self.hooks.hooks.read().expect("hooks lock").clone()
    }

    /// Drain `post_tool` additional contexts produced for `session`, in
    /// dispatch (model) order.
    pub fn take_hook_contexts(&self, session: &SessionId) -> Vec<HookContext> {
        let mut pending = self.hooks.contexts.lock().expect("hook contexts lock");
        let (mine, rest): (Vec<_>, Vec<_>) =
            pending.drain(..).partition(|(owner, _)| owner == session);
        *pending = rest;
        mine.into_iter().map(|(_, context)| context).collect()
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

    /// Sections contributed by the tools in this registry (`Tool::prompt_section`),
    /// each tied to its own tool.
    pub fn prompt_sections(&self) -> Vec<crate::prompt::PromptSection> {
        self.tools
            .read()
            .expect("registry lock")
            .values()
            .filter_map(|tool| {
                tool.prompt_section()
                    .map(|prompt| crate::prompt::PromptSection {
                        name: format!("tool:{}", tool.name()),
                        order: prompt.order,
                        tools: vec![tool.name().to_owned()],
                        text: prompt.text,
                    })
            })
            .collect()
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
        self.dispatch_exposed(session, calls, max_concurrency, cancel, None)
            .await
    }

    /// Exposure limits which tools may be called, not the session's permissions
    /// used for dependency checks (deferred controls can still be discovered).
    pub(crate) async fn dispatch_exposed(
        &self,
        session: &SessionId,
        calls: &[ToolCall],
        max_concurrency: usize,
        cancel: &CancellationToken,
        exposed: Option<&[String]>,
    ) -> Vec<ToolResult> {
        let mut results = Vec::with_capacity(calls.len());
        let mut start = 0;
        while start < calls.len() {
            let safe = |call: &ToolCall| {
                self.get(&call.name)
                    .filter(|_| exposed.is_none_or(|names| names.contains(&call.name)))
                    .is_some_and(|tool| tool.concurrency_safe(&call.args))
            };
            let mut end = start + 1;
            if safe(&calls[start]) {
                while end < calls.len() && safe(&calls[end]) {
                    end += 1;
                }
            }
            results.extend(
                self.dispatch_batch(
                    session,
                    &calls[start..end],
                    max_concurrency,
                    cancel,
                    exposed,
                )
                .await,
            );
            start = end;
        }
        results
    }

    async fn dispatch_batch(
        &self,
        session: &SessionId,
        calls: &[ToolCall],
        max_concurrency: usize,
        cancel: &CancellationToken,
        exposed: Option<&[String]>,
    ) -> Vec<ToolResult> {
        let sem = Arc::new(Semaphore::new(max_concurrency.max(1)));
        let mut handles = Vec::with_capacity(calls.len());
        // Check permission, not exposure: deferred controls remain usable via
        // ToolSearch/PTC. This registry already reflects role and inherited ceilings.
        let missing_job_controls: Vec<_> = ["job_output", "job_list", "job_kill"]
            .into_iter()
            .filter(|name| self.get(name).is_none())
            .collect();
        for call in calls {
            let sem = Arc::clone(&sem);
            let tool = self
                .get(&call.name)
                .filter(|_| exposed.is_none_or(|names| names.contains(&call.name)));
            let background_error = tool.as_ref()
                .filter(|tool| tool.starts_background_job(&call.args) && !missing_job_controls.is_empty())
                .map(|_| format!(
                    "background jobs unavailable: this session lacks required job controls: {}. Run in the foreground instead.",
                    missing_job_controls.join(", "),
                ));
            let approvals = Arc::clone(&self.approvals);
            let hook_state = Arc::clone(&self.hooks);
            let hooks = self.tool_hooks();
            let spill_root = self.spill_root.read().expect("registry lock").clone();
            let call = call.clone();
            let session = session.clone();
            let cancel = cancel.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore open");
                let started = Instant::now();
                let mut plan_review = None;
                let mut presentation = None;
                let mut failure_kind = "execution_failed";
                let event = ToolHookEvent {
                    session: session.clone(),
                    call: call.call.clone(),
                    tool: call.name.clone(),
                    args: call.args.clone(),
                    turn: hook_state.current_turn.load(std::sync::atomic::Ordering::Relaxed),
                };
                // dsh: a throwing pre-tool hook (or an unknown tool) is a
                // final result; denials and executions reach post_tool.
                let mut run_post = true;
                let outcome = match tool {
                    _ if cancel.is_cancelled() => {
                        failure_kind = "approval_cancelled";
                        Err("tool call cancelled before execution".into())
                    }
                    Some(_) if background_error.is_some() => {
                        failure_kind = "background_jobs_unavailable";
                        Err(background_error.unwrap())
                    }
                    Some(t) => 'gate: {
                        // pre_tool → [ask] → guard → policy → execute.
                        let mut asked = false;
                        let mut reason = None;
                        if let Some(h) = &hooks {
                            let decision = tokio::select! {
                                biased;
                                d = h.pre_tool(&event, &cancel) => d,
                                _ = cancel.cancelled() => Err("cancelled".into()),
                            };
                            match decision {
                                Err(_) if cancel.is_cancelled() => {
                                    failure_kind = "approval_cancelled";
                                    break 'gate Err("tool call cancelled before execution".into());
                                }
                                Err(e) => {
                                    failure_kind = "hook_failed";
                                    run_post = false;
                                    break 'gate Err(format!("pre_tool hook failed: {e}"));
                                }
                                Ok(PreToolDecision::Deny { reason }) => {
                                    failure_kind = "hook_denied";
                                    break 'gate Err(reason);
                                }
                                Ok(PreToolDecision::Ask { reason: r }) => {
                                    asked = true;
                                    reason = r;
                                }
                                Ok(PreToolDecision::Allow) => {}
                            }
                        }
                        let request = ApprovalRequest {
                            session: session.clone(),
                            call: call.call.clone(),
                            tool: call.name.clone(),
                            args: call.args.clone(),
                            reason,
                        };
                        // The question must not outlive the turn: a cancel
                        // while the user deliberates withdraws it (the
                        // dropped check future cleans up answerer state).
                        if asked {
                            let decision = tokio::select! {
                                biased;
                                d = approvals.ask(&request) => d,
                                _ = cancel.cancelled() => Decision::Cancelled,
                            };
                            if let Some((kind, error)) = refusal(decision) {
                                failure_kind = kind;
                                break 'gate Err(error);
                            }
                        }
                        if let Some(h) = &hooks {
                            match h.guard(&event, &cancel).await {
                                Ok(None) => {}
                                Ok(Some(reason)) => {
                                    failure_kind = "guard_denied";
                                    break 'gate Err(reason);
                                }
                                Err(e) => {
                                    failure_kind = "guard_denied";
                                    break 'gate Err(format!("guard failed: {e}"));
                                }
                            }
                        }
                        // A granted hook ask is this call's approval.
                        if !asked {
                            let decision = tokio::select! {
                                biased;
                                d = approvals.check_tool(&request, t.sensitive()) => d,
                                _ = cancel.cancelled() => Decision::Cancelled,
                            };
                            if let Some((kind, error)) = refusal(decision) {
                                failure_kind = kind;
                                break 'gate Err(error);
                            }
                        }
                        if cancel.is_cancelled() {
                            failure_kind = "approval_cancelled";
                            break 'gate Err("tool call cancelled before execution".into());
                        }
                        // The body may run zero or more times under a
                        // tool_execute wrapper; side data follows the last run.
                        let body = Arc::new(Mutex::new(BodyRun::default()));
                        let run_body = {
                            let (body, t, session, id, args, cancel, name) = (Arc::clone(&body), Arc::clone(&t), session.clone(), call.call.clone(), call.args.clone(), cancel.clone(), call.name.clone());
                            move || -> hooks::ExecuteFuture {
                                let (body, t, session, id, args, cancel, name) = (Arc::clone(&body), Arc::clone(&t), session.clone(), id.clone(), args.clone(), cancel.clone(), name.clone());
                                Box::pin(async move {
                                    let mut run = BodyRun::default();
                                    let result = if t.plan_config().is_some() {
                                        t.review_plan(&session, &id, args, &cancel).await.map(|(output, review)| { run.plan_review = Some(review); (vec![ToolResultContentPart::Text { text: output }], false) })
                                    } else {
                                        t.execute_presented(&session, &id, args, &cancel).await.map(|(content, tasks, error, metadata)| {
                                            run.tasks = tasks;
                                            run.presentation = metadata.filter(|value| {
                                                let valid = serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= 256 * 1024);
                                                if !valid { tracing::warn!(tool = %name, "discarding oversized tool presentation metadata"); }
                                                valid
                                            });
                                            (content, error)
                                        })
                                    };
                                    let outcome = match result {
                                        Ok((content, is_error)) => ExecuteOutcome { content, is_error },
                                        Err(e) => ExecuteOutcome { content: vec![ToolResultContentPart::Text { text: e }], is_error: true },
                                    };
                                    run.outcome = Some(outcome.clone());
                                    *body.lock().expect("tool body lock") = run;
                                    outcome
                                })
                            }
                        };
                        let executed = match &hooks {
                            Some(h) => h.tool_execute(&event, &run_body, &cancel).await,
                            None => Ok(run_body().await),
                        };
                        let run = std::mem::take(&mut *body.lock().expect("tool body lock"));
                        match executed {
                            Err(_) if cancel.is_cancelled() => {
                                failure_kind = "approval_cancelled";
                                Err("tool call cancelled".into())
                            }
                            Err(e) => {
                                failure_kind = "hook_failed";
                                Err(format!("tool_execute hook failed: {e}"))
                            }
                            Ok(outcome) => {
                                // Tool-authored presentation describes the body's own
                                // output; a wrapper that replaced it gets the generic one.
                                if run.outcome.as_ref() == Some(&outcome) {
                                    presentation = run.presentation;
                                }
                                plan_review = run.plan_review;
                                Ok((outcome.content, run.tasks, outcome.is_error))
                            }
                        }
                    }
                    None => {
                        run_post = false;
                        failure_kind = "unknown_tool";
                        Err(format!("unknown tool '{}'", call.name))
                    }
                };
                let (mut content, tasks, mut is_error) = match outcome {
                    Ok((content, tasks, is_error)) => (content, tasks, is_error),
                    Err(e) => (vec![ToolResultContentPart::Text { text: e }], None, true),
                };
                if let Some(h) = hooks.as_ref().filter(|_| run_post) {
                    let draft = ToolResult {
                        plan_review,
                        presentation: None,
                        tasks: tasks.clone(),
                        call: call.call.clone(),
                        name: call.name.clone(),
                        output: ToolResult::text_output(&content),
                        content: content.clone(),
                        is_error,
                        duration_ms: started.elapsed().as_millis() as u64,
                    };
                    // Post hooks see the settled result even after a turn
                    // cancel; the host bounds them with its own timeout.
                    let decision = h.post_tool(&event, &draft, &CancellationToken::new()).await;
                    let contexts = match decision {
                        Ok(PostToolDecision::Accept { content: replacement, additional_contexts }) => {
                            if let Some(replacement) = replacement {
                                content = replacement;
                            }
                            additional_contexts
                        }
                        Ok(PostToolDecision::Block { feedback, additional_contexts }) => {
                            failure_kind = "hook_blocked";
                            content = feedback;
                            is_error = true;
                            additional_contexts
                        }
                        Err(e) => {
                            failure_kind = "hook_failed";
                            content = vec![ToolResultContentPart::Text { text: format!("post_tool hook failed: {e}") }];
                            is_error = true;
                            Vec::new()
                        }
                    };
                    let contexts = contexts.into_iter().filter(|m| !m.text.trim().is_empty()).map(|m| (session.clone(), HookContext { call: call.call.clone(), text: m.text, tag: m.tag }));
                    hook_state.contexts.lock().expect("hook contexts lock").extend(contexts);
                }
                // Spill: if the text output exceeds the inline budget, save the
                // full result to a file and replace with head/tail + file pointer.
                let content = spill_if_oversized(content, &spill_root, &session, &call.call, &call.name);
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
                let result = ToolResult {
                    plan_review,
                    presentation,
                    tasks,
                    call: call.call,
                    name: call.name,
                    content,
                    output,
                    is_error,
                    duration_ms,
                };
                if let Some(h) = &hooks {
                    h.tool_result(&event, &result);
                }
                result
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

/// Map a non-allowing approval decision to its failure kind and the
/// model-visible error; `None` means allowed.
fn refusal(decision: Decision) -> Option<(&'static str, String)> {
    match decision {
        Decision::Allowed => None,
        Decision::Rejected => Some(("approval_rejected", "the user rejected this tool call".into())),
        Decision::Cancelled => Some(("approval_cancelled", "approval request was cancelled".into())),
        Decision::Unavailable => Some((
            "approval_unavailable",
            "approval required but no approver is available — the call was blocked".into(),
        )),
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
        fn concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
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

    /// Scripted hooks: deny `rm`, ask for `ask`, guard-deny `guarded`,
    /// block `blocked` output, annotate everything else; records phases.
    #[derive(Default)]
    struct Script(std::sync::Mutex<Vec<String>>);
    #[async_trait]
    impl ToolHooks for Script {
        async fn pre_tool(&self, e: &ToolHookEvent, _: &CancellationToken) -> Result<PreToolDecision, String> {
            let say = e.args["say"].as_str().unwrap_or("").to_string();
            self.0.lock().unwrap().push(format!("pre:{say}"));
            Ok(match say.as_str() {
                "rm" => PreToolDecision::Deny { reason: "no rm".into() },
                "ask" => PreToolDecision::Ask { reason: Some("why".into()) },
                "boom" => return Err("exploded".into()),
                _ => PreToolDecision::Allow,
            })
        }
        async fn guard(&self, e: &ToolHookEvent, _: &CancellationToken) -> Result<Option<String>, String> {
            self.0.lock().unwrap().push("guard".into());
            Ok((e.args["say"] == "guarded").then(|| "guard says no".into()))
        }
        async fn tool_execute(&self, e: &ToolHookEvent, next: &hooks::ExecuteNext, _: &CancellationToken) -> Result<ExecuteOutcome, String> {
            match e.args["say"].as_str() {
                Some("cached") => Ok(ExecuteOutcome { content: vec![ToolResultContentPart::Text { text: "from cache".into() }], is_error: false }),
                Some("retry") => {
                    let first = next().await;
                    let second = next().await;
                    Ok(ExecuteOutcome { content: [first.content, second.content].concat(), is_error: false })
                }
                Some("wrapfail") => Err("wrapper broke".into()),
                _ => Ok(next().await),
            }
        }
        async fn post_tool(&self, e: &ToolHookEvent, r: &ToolResult, _: &CancellationToken) -> Result<PostToolDecision, String> {
            self.0.lock().unwrap().push(format!("post:{}", r.output));
            Ok(if e.args["say"] == "blocked" {
                PostToolDecision::Block { feedback: vec![ToolResultContentPart::Text { text: "try again".into() }], additional_contexts: vec![] }
            } else {
                PostToolDecision::Accept { content: None, additional_contexts: vec![
                    format!("saw {}", r.output).into(),
                    hooks::HookMessage { text: "lint ok".into(), tag: Some("lint".into()) },
                    "   ".into(),
                ] }
            })
        }
        fn tool_result(&self, _: &ToolHookEvent, r: &ToolResult) {
            self.0.lock().unwrap().push(format!("result:{}", r.is_error));
        }
    }

    struct Answer(Decision, std::sync::Mutex<Option<ApprovalRequest>>);
    #[async_trait]
    impl crate::approval::Answerer for Answer {
        async fn answer(&self, request: &ApprovalRequest) -> Decision {
            *self.1.lock().unwrap() = Some(request.clone());
            self.0
        }
    }

    async fn run(reg: &ToolRegistry, say: &str) -> ToolResult {
        let call = ToolCall { call: say.into(), name: "sleep_echo".into(), args: serde_json::json!({"say": say}) };
        reg.dispatch(&"s".into(), &[call], 1, &CancellationToken::new()).await.remove(0)
    }

    #[tokio::test]
    async fn hooks_gate_transform_and_observe_tool_calls() {
        let reg = ToolRegistry::default();
        reg.register(Arc::new(SleepEcho));
        let script = Arc::new(Script::default());
        reg.set_hooks(Some(script.clone()));

        let ok = run(&reg, "hi").await;
        assert_eq!((ok.output.as_str(), ok.is_error), ("hi", false));
        assert_eq!(*script.0.lock().unwrap(), ["pre:hi", "guard", "post:hi", "result:false"]);
        let contexts = reg.take_hook_contexts(&"s".into());
        assert_eq!(contexts, [
            HookContext { call: "hi".into(), text: "saw hi".into(), tag: None },
            HookContext { call: "hi".into(), text: "lint ok".into(), tag: Some("lint".into()) },
        ]);
        assert!(reg.take_hook_contexts(&"s".into()).is_empty());

        script.0.lock().unwrap().clear();
        let denied = run(&reg, "rm").await;
        assert!(denied.is_error);
        assert_eq!(denied.output, "no rm");
        assert_eq!(denied.presentation.as_ref().unwrap()["outcome"], "hook_denied");
        // Denials skip guard and the body but still reach post_tool.
        assert_eq!(*script.0.lock().unwrap(), ["pre:rm", "post:no rm", "result:true"]);

        let guarded = run(&reg, "guarded").await;
        assert_eq!((guarded.output.as_str(), guarded.is_error), ("guard says no", true));

        let blocked = run(&reg, "blocked").await;
        assert_eq!((blocked.output.as_str(), blocked.is_error), ("try again", true));
        assert_eq!(blocked.presentation.as_ref().unwrap()["outcome"], "hook_blocked");

        script.0.lock().unwrap().clear();
        let failed = run(&reg, "boom").await;
        assert!(failed.is_error && failed.output.contains("exploded"));
        // A failing pre hook is final: no post_tool.
        assert_eq!(*script.0.lock().unwrap(), ["pre:boom", "result:true"]);
    }

    #[tokio::test]
    async fn hook_ask_prompts_even_under_allow_and_carries_reason() {
        let reg = ToolRegistry::default();
        reg.register(Arc::new(SleepEcho));
        reg.set_hooks(Some(Arc::new(Script::default())));
        let answer = Arc::new(Answer(Decision::Rejected, Default::default()));
        reg.approvals().set_answerer(answer.clone());

        let rejected = run(&reg, "ask").await;
        assert_eq!(rejected.presentation.as_ref().unwrap()["outcome"], "approval_rejected");
        assert_eq!(answer.1.lock().unwrap().as_ref().unwrap().reason.as_deref(), Some("why"));

        // No approver: fail closed.
        let reg2 = ToolRegistry::default();
        reg2.register(Arc::new(SleepEcho));
        reg2.set_hooks(Some(Arc::new(Script::default())));
        assert_eq!(run(&reg2, "ask").await.presentation.unwrap()["outcome"], "approval_unavailable");

        // Policy `never` rejects without prompting.
        reg.approvals().set_policy(crate::approval::Policy::Never);
        *answer.1.lock().unwrap() = None;
        assert!(run(&reg, "ask").await.is_error);
        assert!(answer.1.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn tool_execute_wraps_retries_and_short_circuits_the_body() {
        let reg = ToolRegistry::default();
        reg.register(Arc::new(SleepEcho));
        let script = Arc::new(Script::default());
        reg.set_hooks(Some(script.clone()));

        let cached = run(&reg, "cached").await;
        assert_eq!((cached.output.as_str(), cached.is_error), ("from cache", false));
        let retried = run(&reg, "retry").await;
        assert_eq!(retried.output, "retry\nretry");

        script.0.lock().unwrap().clear();
        let failed = run(&reg, "wrapfail").await;
        assert!(failed.is_error && failed.output.contains("wrapper broke"), "{}", failed.output);
        assert_eq!(failed.presentation.as_ref().unwrap()["outcome"], "hook_failed");
        // A failing wrapper is still a settled result: post_tool sees it.
        assert_eq!(script.0.lock().unwrap()[2..], ["post:tool_execute hook failed: wrapper broke".to_string(), "result:true".into()]);
    }

    #[tokio::test]
    async fn hooks_are_shared_with_derived_registries() {
        let reg = ToolRegistry::default();
        reg.register(Arc::new(SleepEcho));
        let child = reg.restricted(&["sleep_echo".into()]);
        reg.set_hooks(Some(Arc::new(Script::default())));
        assert_eq!(run(&child, "rm").await.output, "no rm");
    }
}
