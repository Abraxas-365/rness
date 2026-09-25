//! The production [`ChildRunner`]: workflow members are ordinary one-shot
//! subagents of the calling session (durable, depth-limited, visible in
//! `/agents`), started through [`SubagentRuntime::start_with`].

use std::sync::Arc;

use async_trait::async_trait;
use rness_protocol::events::SessionId;
use tokio_util::sync::CancellationToken;

use super::{ChildOutcome, ChildRequest, ChildRunner};
use crate::subagent::{RunOptions, StopReason, SubagentError, SubagentRequest, SubagentRuntime};

pub struct SubagentChildren {
    runtime: Arc<SubagentRuntime>,
    parent: SessionId,
    call: Option<String>,
}

impl SubagentChildren {
    pub fn new(runtime: Arc<SubagentRuntime>, parent: SessionId) -> Self {
        Self {
            runtime,
            parent,
            call: None,
        }
    }

    /// Attribute members to the parent's `workflow` tool call (the
    /// delegation link `/agents` shows).
    pub fn with_call(mut self, call: impl Into<String>) -> Self {
        self.call = Some(call.into());
        self
    }
}

#[async_trait]
impl ChildRunner for SubagentChildren {
    fn check_role(&self, role: Option<&str>) -> Result<(), String> {
        self.runtime.validate_agent(role).map_err(|e| e.to_string())
    }

    async fn run(
        &self,
        request: ChildRequest,
        cancel: CancellationToken,
        started: Box<dyn FnOnce(SessionId) + Send>,
    ) -> ChildOutcome {
        // An admit that raced cancellation must not create a durable session.
        if cancel.is_cancelled() {
            return ChildOutcome::Cancelled;
        }
        let schema = request.schema.is_some();
        let options = RunOptions {
            output_schema: request.schema,
            cancel: Some(cancel.clone()),
            on_start: Some(Box::new(move |child: &SessionId| started(child.clone()))),
            call: self.call.clone(),
            // No nested workflows (dsh defers them): members and their own
            // delegations never see the tool.
            withhold_tools: vec![super::TOOL.into()],
            ..RunOptions::default()
        };
        let delegation = SubagentRequest {
            agent: request.role,
            parent: self.parent.clone(),
            prompt: request.prompt,
        };
        match self
            .runtime
            .start_with(&request.provider, delegation, options)
            .await
        {
            Ok(run) => match run.stop {
                StopReason::Completed => ChildOutcome::Completed {
                    text: run.output,
                    structured: run.structured,
                },
                StopReason::Aborted => ChildOutcome::Cancelled,
                StopReason::Error if schema && run.structured.is_none() => ChildOutcome::Failed(
                    format!("child {} ended without structured output", run.session),
                ),
                StopReason::Error => {
                    ChildOutcome::Failed(format!("child {} ended in error", run.session))
                }
            },
            // A refusal racing our own cancellation is that cancellation.
            Err(_) if cancel.is_cancelled() => ChildOutcome::Cancelled,
            // Storage trouble / a busy session is transient, not the script's
            // fault: that item becomes nil like any other failed member.
            Err(error) if transient(&error) => {
                ChildOutcome::Failed(format!("agent() could not start a child: {error}"))
            }
            // Refused starts (role, profile, depth, provider, schema) are the
            // script's bug or a broken config: fatal, so the reason surfaces.
            Err(error) => ChildOutcome::Fatal(format!("agent() could not start a child: {error}")),
        }
    }
}

fn transient(error: &SubagentError) -> bool {
    use crate::service::ServiceError as S;
    use crate::session::branch::BranchError as B;
    matches!(
        error,
        SubagentError::Service(S::Log(_) | S::Replay(_) | S::Busy | S::Branch(B::Log(_)))
            | SubagentError::Branch(B::Log(_))
    )
}
