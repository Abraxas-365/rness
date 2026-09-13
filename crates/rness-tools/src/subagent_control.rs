//! Control tools for continuable children (dsh tool-subagent-control):
//! `send_message` steers across one parent/child edge, `interrupt_agent`
//! stops a child's current turn, `list_agents` lists continuable
//! children. Thin adapters over the engine's [`SubagentRuntime`] — the
//! tools perform no lifecycle routing; authorization (exact adjacency,
//! ancestry) belongs to the service.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::subagent::SubagentRuntime;
use rness_engine::tools::Tool;
use rness_protocol::events::SessionId;
use serde_json::{json, Value};

// -- send_message ------------------------------------------------------------

pub struct SendMessageTool {
    runtime: Arc<SubagentRuntime>,
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a message to a continuable agent named by agent_id: your \
         direct continuable child, or (if you are a continuable child) \
         your direct parent. A working target sees it at its next step; \
         an idle target starts a turn. Returns acceptance only, never a \
         reply — the target's answers arrive as settle notices."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": { "type": "string", "description": "Session id returned by a continuable subagent or list_agents, never a background job_id" },
                "message": { "type": "string", "description": "The message to deliver" },
            },
            "required": ["agent_id", "message"],
        })
    }

    async fn execute_presented(&self, session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let target = crate::required_str(&args, "agent_id")?.to_string();
        let output = self.execute_in(session, args).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(json!({"version":1,"kind":"send_message","agent_id":target,"accepted":true}))))
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("send_message requires a calling session".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        let target = crate::required_str(&args, "agent_id")?.to_string();
        let message = crate::required_str(&args, "message")?.to_string();
        if target.starts_with('j') {
            return Err("message not delivered: agent_id is a background job ID, not an agent session ID. One-shot jobs cannot receive messages; use job_output to read their results. To message a child, spawn it with background_mode='continuable' and use its returned session ID (or list_agents).".into());
        }
        self.runtime
            .send_message(session, &target, message)
            .map_err(|e| format!("message not delivered: {e}"))?;
        Ok(format!("message accepted by {target}"))
    }
}

// -- interrupt_agent ---------------------------------------------------------

pub struct InterruptAgentTool {
    runtime: Arc<SubagentRuntime>,
}

#[async_trait]
impl Tool for InterruptAgentTool {
    fn name(&self) -> &str {
        "interrupt_agent"
    }

    fn description(&self) -> &str {
        "Stop a descendant agent's CURRENT turn only: queued messages \
         stay parked, its own children keep running, and the agent stays \
         available for later send_message. Interrupting an idle agent is \
         an accepted no-op."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": { "type": "string", "description": "Session id of the agent to interrupt" },
            },
            "required": ["agent_id"],
        })
    }

    async fn execute_presented(&self, session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let target = crate::required_str(&args, "agent_id")?.to_string();
        let output = self.execute_in(session, args).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(json!({"version":1,"kind":"interrupt_agent","agent_id":target,"accepted":true}))))
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("interrupt_agent requires a calling session".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        let target = crate::required_str(&args, "agent_id")?.to_string();
        self.runtime
            .interrupt(session, &target)
            .map_err(|e| e.to_string())?;
        Ok(format!("interrupt accepted for {target}"))
    }
}

// -- list_agents -------------------------------------------------------------

pub struct ListAgentsTool {
    runtime: Arc<SubagentRuntime>,
}

#[async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &str {
        "list_agents"
    }

    fn description(&self) -> &str {
        "List your continuable child agents: scope 'children' (default) \
         shows direct children, 'descendants' walks the whole tree in \
         pre-order. Each line: <session id> depth=<n> <running|idle> \
         parent=<id>. One-shot children are absent — they cannot accept \
         send_message. Use this to recall children, not poll for completion: \
         their closing answers arrive automatically as settle notices, at the \
         next step when working or in a new turn when idle."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["children", "descendants"],
                    "description": "Direct children (default) or the full descendant tree",
                },
            },
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, String> {
        Err("list_agents requires a calling session".into())
    }

    async fn execute_in(&self, session: &SessionId, args: Value) -> Result<String, String> {
        self.list_presented(session, args).map(|(output, _)| output)
    }

    async fn execute_presented(&self, session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, metadata) = self.list_presented(session, args)?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(metadata)))
    }
}

impl ListAgentsTool {
    fn list_presented(&self, session: &SessionId, args: Value) -> Result<(String, Value), String> {
        let descendants = args["scope"].as_str() == Some("descendants");
        let children = self
            .runtime
            .list_children(session, descendants)
            .map_err(|e| e.to_string())?;
        let records: Vec<_> = children.iter().take(100).map(|c| json!({"session":c.session,"parent":c.parent,"depth":c.depth,"running":c.running})).collect();
        let metadata = json!({"version":1,"kind":"list_agents","descendants":descendants,"total":children.len(),"truncated":records.len()<children.len(),"agents":records});
        if children.is_empty() {
            return Ok(("No continuable child agents".into(), metadata));
        }
        Ok((children
            .iter()
            .map(|c| {
                format!(
                    "{} depth={} {} parent={}",
                    c.session,
                    c.depth,
                    if c.running { "running" } else { "idle" },
                    c.parent
                )
            })
            .collect::<Vec<_>>()
            .join("\n"), metadata))
    }
}

/// Register the three control tools.
pub fn register_subagent_control(
    registry: &rness_engine::tools::ToolRegistry,
    runtime: Arc<SubagentRuntime>,
) {
    registry.register(Arc::new(SendMessageTool { runtime: Arc::clone(&runtime) }));
    registry.register(Arc::new(InterruptAgentTool { runtime: Arc::clone(&runtime) }));
    registry.register(Arc::new(ListAgentsTool { runtime }));
}
