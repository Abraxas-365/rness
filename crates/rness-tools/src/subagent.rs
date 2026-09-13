//! The model-facing delegation tool (dsh tool-subagent). Thin: all
//! mechanics live in the engine's [`SubagentRuntime`]; this maps tool
//! args to a request and a settled run to a tool result.
//!
//! A failed child is an is_error TOOL RESULT, never an exception — the
//! parent turn continues and the model decides what to do about it.
//! Background runs are ordinary jobs: observe with `job_output`, cancel
//! with `job_kill` (kind-independent controls, dsh-style).

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::subagent::{StopReason, SubagentRequest, SubagentRuntime};
use rness_engine::tools::Tool;
use rness_protocol::events::SessionId;
use serde_json::{json, Value};

use crate::jobs::{JobRegistry, JobStatus};

pub struct SubagentTool {
    runtime: Arc<SubagentRuntime>,
    jobs: JobRegistry,
}

impl SubagentTool {
    pub fn new(runtime: Arc<SubagentRuntime>, jobs: JobRegistry) -> Self {
        Self { runtime, jobs }
    }
}

fn render(run: &rness_engine::subagent::SubagentRun) -> Result<String, String> {
    match run.stop {
        StopReason::Completed => Ok(format!(
            "[subagent session: {}]\n{}",
            run.session,
            if run.output.is_empty() { "(no output)" } else { &run.output }
        )),
        StopReason::Aborted => Err(format!("subagent run {} was cancelled", run.session)),
        StopReason::Error => Err(format!(
            "subagent run {} failed{}",
            run.session,
            if run.output.is_empty() {
                String::new()
            } else {
                format!(": {}", run.output)
            }
        )),
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Delegate a task to a child agent running in its own session. \
         provider 'fork' seeds the child with this conversation's completed \
         history (it knows what you know); provider 'spawn' starts fresh \
         (describe the task fully). Foreground by default: the result is \
         the child's final answer. With run_in_background the child runs \
         as a one-shot job — read it with job_output, cancel with job_kill. \
         Job IDs cannot be used with send_message. Continue independent work, \
         avoid busy-polling or duplicating the child task, and collect relevant \
         results before dependent decisions or claiming task completion. If no \
         independent work remains, tell the user you are waiting and end your \
         turn instead of repeatedly polling job_output; the background job \
         continues and completion resumes the owning session. With \
         background_mode 'continuable' the child keeps running as a named \
         agent: message it with send_message, stop its turn with \
         interrupt_agent, list with list_agents; its results arrive as \
         settle notices in this conversation."
    }

    fn input_schema(&self) -> Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "enum": ["spawn", "fork"],
                    "description": "How the child starts: 'fork' inherits this session's completed history, 'spawn' starts fresh",
                },
                "agent": {
                    "type": "string",
                    "enum": self.runtime.roster().keys().collect::<Vec<_>>(),
                    "description": format!("Optional named role. Omit to inherit generation settings without a role. Available agents: {}", serde_json::to_string(&self.runtime.roster()).unwrap()),
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the child agent",
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Run in the background (default false). One-shot mode returns job_id for job_output/job_kill, not send_message. Use background_mode='continuable' for a messageable agent.",
                },
                "background_mode": {
                    "type": "string",
                    "enum": ["one-shot", "continuable"],
                    "description": "'one-shot' (default) returns a result, or job_id when run_in_background=true; it cannot receive messages. 'continuable' immediately returns agent_id for send_message/interrupt_agent and keeps the child available for later turns.",
                },
            },
            "required": ["provider", "prompt"],
        });
        if self.runtime.roster().is_empty() {
            schema["properties"].as_object_mut().unwrap().remove("agent");
        }
        schema
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("subagent requires a calling session".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        self.run_presented(session, args, None).await.map(|(output, _)| output)
    }

    async fn execute_presented(&self, session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let presentation = Some((_call.clone(), args.clone()));
        let (output, metadata) = self.run_presented(session, args, presentation).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(metadata)))
    }
}

impl SubagentTool {
    async fn run_presented(&self, session: &SessionId, args: Value, presentation: Option<(String, Value)>) -> Result<(String, Value), String> {
        let provider = crate::required_str(&args, "provider")?.to_string();
        let prompt = crate::required_str(&args, "prompt")?.to_string();
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let continuable = args["background_mode"].as_str() == Some("continuable");
        let agent = match args.get("agent") {
            None => None,
            Some(Value::String(name)) => Some(name.clone()),
            Some(_) => return Err("agent must be a string".into()),
        };
        let request = SubagentRequest { agent, parent: session.clone(), prompt };

        if continuable {
            // Continuable: start and return the child's durable id.
            // Settles are injected into this session by the runtime's
            // settle watch; steering goes through send_message.
            let child = self
                .runtime
                .start_continuable_presented(&provider, request, presentation)
                .map_err(|e| e.to_string())?;
            return Ok((format!(
                "started continuable agent {child} — message it with \
                 send_message (agent_id: {child}), stop its turn with interrupt_agent; its \
                 results arrive here as settle notices"
            ), json!({"version":1,"kind":"subagent","mode":"continuable","session":child,"agent_id":child,"accepted":true})));
        }

        if !background {
            let run = self
                .runtime
                .start_presented(&provider, request, presentation)
                .await
                .map_err(|e| e.to_string())?;
            return render(&run).map(|output| (output, json!({"version":1,"kind":"subagent","mode":"foreground","session":run.session,"status":"completed"})));
        }

        let label = format!(
            "subagent [{provider}]: {}",
            request.prompt.chars().take(80).collect::<String>()
        );
        let (id, writer) = self.jobs.start_owned("subagent", label, Some(session));
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            match runtime.start_presented(&provider, request, presentation).await {
                Ok(run) => match render(&run) {
                    Ok(text) => {
                        writer.append(text.as_bytes());
                        writer.settle(JobStatus::Exited(Some(0)));
                    }
                    Err(e) => {
                        writer.append(e.as_bytes());
                        writer.settle(JobStatus::Exited(Some(1)));
                    }
                },
                Err(e) => {
                    writer.append(format!("subagent failed: {e}").as_bytes());
                    writer.settle(JobStatus::Exited(Some(1)));
                }
            }
        });
        Ok((format!(
             "started background subagent as job {id} — completion notifies this session; read with job_output, cancel with job_kill. This is a one-shot job: its job_id cannot be used with send_message. For a messageable child, use background_mode='continuable' instead."
        ), json!({"version":1,"kind":"subagent","mode":"background","job_id":id,"accepted":true})))
    }
}
