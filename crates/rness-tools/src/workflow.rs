//! The model-facing `workflow` tool (dsh tool-workflow): a model-written
//! Lua script that orchestrates subagents. Thin: the engine's
//! [`rness_engine::workflow`] runs the script; this validates the call,
//! wires members to the calling session, and maps the settled run to a
//! tool result. Foreground only (dsh): the call returns when the script
//! finishes. Progress is published to [`WorkflowActivity`] for the live card.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::subagent::SubagentRuntime;
use rness_engine::tools::Tool;
use rness_engine::workflow::{
    self, SubagentChildren, WorkflowActivity, WorkflowLimits, WorkflowStop,
};
use rness_protocol::events::{SessionId, ToolResultContentPart};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Tool configuration from `rness.workflow` (limits plus the rendered
/// result cap).
#[derive(Debug, Clone)]
pub struct WorkflowConfig {
    pub limits: WorkflowLimits,
    /// Rendered result cap in characters (dsh has no cap; tool output is
    /// model context, so rness bounds it).
    pub max_result_chars: usize,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            limits: WorkflowLimits::default(),
            max_result_chars: 50_000,
        }
    }
}

pub struct WorkflowTool {
    runtime: Arc<SubagentRuntime>,
    activity: Arc<WorkflowActivity>,
    config: WorkflowConfig,
    description: String,
}

/// The script-authoring contract, embedded in the tool description (dsh:
/// "this IS the model-facing spec").
const DESCRIPTION: &str = r#"Run a Lua workflow script that orchestrates subagents at scale. Use this for work that fans out across many independent pieces — an audit over many files, a migration, multi-angle research, adversarial verification of findings — where you write the orchestration as a script instead of delegating turn by turn. Only the script's return value comes back; child transcripts never enter this conversation.

`meta` is data: required `name` (short kebab-case) and `description`, optional `whenToUse` and `phases` ([{title, detail?}]). `script` is a plain Lua 5.4 chunk ending with `return <value>`; the value must be JSON-serializable (tables with only 1..n keys are arrays, string keys are objects) and is this tool's result. `args` (optional JSON object) is readable in the script as the global `args`.

Script globals:
- `agent(prompt, opts?)` — run one subagent to completion and return its final text; with `opts.schema` (an object-rooted JSON Schema using ONLY type/properties/required/additionalProperties/items/enum/const/oneOf/description) return the validated table instead. Returns nil when the child fails, is cancelled, or never reports its structured result. Other opts: `label` (display), `phase` (progress group), `role` (a configured agent role, as in the subagent tool's `agent`), `provider` ("spawn" default, fresh context; "fork" inherits this conversation). Anything else is rejected loudly.
- `pipeline(items, stage1, stage2, ...)` — run each item through the stages independently with NO barrier between stages (prefer this for multi-stage work). Each stage is called as `stage(prev, item, index)`; stage 1 gets prev = item. An ordinary error() in a stage, or a nil result, drops that ITEM to nil and skips its remaining stages. Returns the list of final values.
- `parallel(thunks)` — call zero-argument functions concurrently and wait for ALL (a barrier; use only when a step genuinely needs every prior result together). A thunk that raises an ordinary error yields nil.
- `compact(list)` — drop nil holes, keeping order (use on pipeline/parallel results). `json.encode(v)` / `json.decode(s)`.
- `phase(title)` — start a progress phase; `log(message)` — narrate progress.

Misuse (bad arguments, unknown options, unsupported schemas, tripped caps, unknown roles) ALWAYS kills the script — it never dissolves into a per-item nil, and pcall cannot catch it.

Constraints: concurrency and total-agent caps apply; the script's own CPU time between agent calls is budgeted (waiting on agents is free); no io/os/require/load/network and no __gc metamethods — the agents do the work, the script only coordinates them. Members cannot start workflows. The run executes in the foreground: this call returns when the whole script finishes.

Example:
local finding = { type = "object", required = { "issues" }, properties = {
  issues = { type = "array", items = { type = "object", required = { "file", "line", "why" },
    properties = { file = { type = "string" }, line = { type = "integer" }, why = { type = "string" } } } } } }
phase("Scan")
local reports = pipeline(args.crates,
  function(_, crate)
    return agent("List panics in non-test code of crates/" .. crate, { label = crate, phase = "Scan", schema = finding })
  end,
  function(report, crate)
    if #report.issues == 0 then return report end
    return agent("Verify each finding; drop false positives:\n" .. json.encode(report.issues),
      { label = crate .. " verify", phase = "Verify", schema = finding })
  end)
return compact(reports)"#;

const UNAVAILABLE: &str = "Workflows are unavailable: generic children are disabled and no configured roles are enabled for delegation. Do not call this tool; perform the task yourself or explain the limitation.";

impl WorkflowTool {
    pub fn new(
        runtime: Arc<SubagentRuntime>,
        activity: Arc<WorkflowActivity>,
        config: WorkflowConfig,
    ) -> Self {
        let description = if !runtime.allows_generic() && runtime.roster().is_empty() {
            UNAVAILABLE.to_owned()
        } else {
            let mut text = DESCRIPTION.to_owned();
            let roster = runtime.roster();
            if !runtime.allows_generic() {
                text.push_str(
                    "\n\nGeneric children are disabled: every agent() call MUST set opts.role.",
                );
            }
            if !roster.is_empty() {
                text.push_str("\n\nRoles for opts.role:");
                for (name, description) in roster {
                    text.push_str(&format!("\n- {name}: {description}"));
                }
            }
            text
        };
        Self {
            runtime,
            activity,
            config,
            description,
        }
    }

    fn render(&self, name: &str, agents: usize, value: &Value) -> String {
        let body = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
        let plural = if agents == 1 { "agent" } else { "agents" };
        let head = format!("workflow \"{name}\" completed ({agents} {plural}).");
        let cap = self.config.max_result_chars;
        let total = body.chars().count();
        if total <= cap {
            return format!("{head}\n{body}");
        }
        let kept: String = body.chars().take(cap).collect();
        format!(
            "{head}\n{kept}\n[workflow result truncated: showed {cap} of {total} characters; return a smaller value (e.g. a summary) to see it all]"
        )
    }

    /// `(output, is_error, card presentation)`. Argument errors are `Err`
    /// (no run, no card state); a run that started always yields `Ok`.
    async fn run(
        &self,
        session: &SessionId,
        call: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<(String, bool, Option<Value>), String> {
        if self.description == UNAVAILABLE {
            return Err(UNAVAILABLE.into());
        }
        let Some(record) = args.as_object() else {
            return Err("workflow arguments must be an object".into());
        };
        for key in record.keys() {
            if !matches!(key.as_str(), "meta" | "script" | "args") {
                return Err(format!(
                    "workflow argument \"{key}\" is not recognized (meta, script, args)"
                ));
            }
        }
        // Some models send the JSON-typed fields as JSON text; accept both.
        let json_field = |key: &str| -> Result<Value, String> {
            match record.get(key) {
                Some(Value::String(text)) => serde_json::from_str(text)
                    .map_err(|e| format!("{key} is a string but not valid JSON: {e}")),
                Some(value) => Ok(value.clone()),
                None => Ok(Value::Null),
            }
        };
        let meta = workflow::validate_meta(&json_field("meta")?)?;
        let script = record
            .get("script")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("script must be a non-empty string")?
            .to_owned();
        let script_args = json_field("args")?;
        if !matches!(script_args, Value::Object(_) | Value::Null) {
            return Err("args must be a JSON object".into());
        }
        workflow::check_script(&meta, &script, &self.config.limits)?;

        let name = meta.name.clone();
        self.activity.begin(session, call, args.clone(), &meta);
        // The body may be dropped mid-run (parent turn torn down): the card
        // must not stay "running" forever.
        struct Abandon<'a>(&'a WorkflowActivity, &'a str);
        impl Drop for Abandon<'_> {
            fn drop(&mut self) {
                self.0.abandon(self.1);
            }
        }
        let _abandon = Abandon(&self.activity, call);
        let observer: workflow::Observer = {
            let (activity, call) = (Arc::clone(&self.activity), call.to_owned());
            Arc::new(move |event| activity.apply(&call, event))
        };
        let runner = Arc::new(
            SubagentChildren::new(Arc::clone(&self.runtime), session.clone()).with_call(call),
        );
        // Dropping this body (turn torn down) must stop the script and its
        // members too, not leave them running headless.
        let token = cancel.child_token();
        let _stop_on_drop = token.clone().drop_guard();
        let result = workflow::run(
            meta,
            script,
            script_args,
            self.config.limits.clone(),
            runner,
            observer,
            token,
        )
        .await;
        let presentation = self.activity.finish(call, &result);
        let (output, is_error) = match result.stop {
            WorkflowStop::Completed => {
                let value = result.value.unwrap_or(Value::Null);
                (self.render(&name, result.agents_started, &value), false)
            }
            WorkflowStop::Error | WorkflowStop::Cancelled => {
                let what = if result.stop == WorkflowStop::Cancelled {
                    "was cancelled"
                } else {
                    "failed"
                };
                let plural = if result.agents_started == 1 {
                    "agent"
                } else {
                    "agents"
                };
                (
                    format!(
                        "workflow \"{name}\" {what} after starting {} {plural}: {}",
                        result.agents_started,
                        result.error.as_deref().unwrap_or("no details")
                    ),
                    true,
                )
            }
        };
        Ok((output, is_error, presentation))
    }
}

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        workflow::TOOL
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "meta": {
                    "type": "object",
                    "description": "Workflow identity (data, never evaluated).",
                    "properties": {
                        "name": {"type": "string", "description": "Short kebab-case name."},
                        "description": {"type": "string"},
                        "whenToUse": {"type": "string"},
                        "phases": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "title": {"type": "string"},
                                    "detail": {"type": "string"}
                                },
                                "required": ["title"]
                            }
                        }
                    },
                    "required": ["name", "description"]
                },
                "script": {
                    "type": "string",
                    "description": "Lua 5.4 chunk that ends with `return <JSON-serializable value>`."
                },
                "args": {
                    "type": "object",
                    "description": "Optional JSON object, readable in the script as `args`."
                }
            },
            "required": ["meta", "script"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("workflow requires a calling session".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        let call = format!("workflow-{}", ulid::Ulid::new());
        self.run(session, &call, args, &CancellationToken::new())
            .await
            .and_then(|(output, error, _)| if error { Err(output) } else { Ok(output) })
    }

    async fn execute_presented(
        &self,
        session: &SessionId,
        call: &String,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<
        (
            Vec<ToolResultContentPart>,
            Option<rness_protocol::events::TaskSnapshot>,
            bool,
            Option<Value>,
        ),
        String,
    > {
        let (output, error, presentation) = self.run(session, call, args, cancel).await?;
        Ok((
            vec![ToolResultContentPart::Text { text: output }],
            None,
            error,
            presentation,
        ))
    }
}
