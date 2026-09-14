//! Plan review is collaboration state, never a permission policy.
use rness_protocol::events::{PlanReview, PlanState};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanConfig { pub guidance: String, pub review: PlanReviewConfig }
#[path = "plan_ui.rs"]
mod ui;
pub use ui::PlanReviewConfig;

pub fn validate_plan(plan: &str) -> Result<(), String> {
    if !plan.trim().strip_prefix("# ").is_some_and(|s| !s.lines().next().unwrap_or("").trim().is_empty()) {
        return Err("provide a complete Markdown plan starting with a # heading".into());
    }
    Ok(())
}
impl Default for PlanConfig {
    fn default() -> Self { Self { guidance: "Plan mode is active. Investigate and discuss the work with the user. Present the complete implementation plan using exit_plan_mode before starting implementation. Approval allows execution from your next step. This guidance does not change tool permissions.".into(), review: PlanReviewConfig::default() } }
}

#[derive(Default)]
pub struct PlanSelections(Mutex<std::collections::HashMap<String, bool>>);
impl PlanSelections {
    pub fn select(&self, session: &str, active: bool) { self.0.lock().unwrap().insert(session.into(), active); }
    pub fn pending(&self, session: &str) -> Option<bool> { self.0.lock().unwrap().get(session).copied() }
    pub fn accept(&self, session: &str, commit: impl FnOnce(bool) -> Result<(), crate::session::log::LogError>) -> Result<(), crate::session::log::LogError> {
        let mut pending = self.0.lock().unwrap();
        if let Some(active) = pending.get(session).copied() { commit(active)?; pending.remove(session); }
        Ok(())
    }
}

pub struct ExitPlan {
    pub config: PlanConfig,
    pub store: Arc<crate::session::branch::SessionStore>,
    pub questions: Arc<crate::questions::Questions>,
    pub alive: tokio_util::sync::CancellationToken,
}
impl std::fmt::Debug for ExitPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("ExitPlan").field("config", &self.config).finish_non_exhaustive() }
}
#[async_trait::async_trait]
impl crate::tools::Tool for ExitPlan {
    fn name(&self) -> &str { "exit_plan_mode" }
    fn plan_config(&self) -> Option<PlanConfig> { (!self.alive.is_cancelled()).then(|| self.config.clone()) }
    async fn execute(&self, _: serde_json::Value) -> Result<String, String> { Err("plan review requires agent dispatch".into()) }
    async fn review_plan(&self, session: &str, call: &str, args: serde_json::Value, cancel: &tokio_util::sync::CancellationToken) -> Result<(String, PlanReview), String> { self.review(session, call, args, cancel).await }
}

impl ExitPlan {
    pub async fn review(&self, session: &str, call: &str, args: serde_json::Value, cancel: &tokio_util::sync::CancellationToken) -> Result<(String, PlanReview), String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input { plan: String }
        let input: Input = serde_json::from_value(args).map_err(|e| e.to_string())?;
        validate_plan(&input.plan)?;
        if !PlanState::from_history(&self.store.history(&session.into()).map_err(|e| e.to_string())?).active { return Err("exit_plan_mode requires active plan mode".into()); }
        if !self.questions.is_available() { return Err("no Questions frontend is available; plan mode remains active".into()); }
        use crate::questions::{Question, OptionItem, Request, Answers};
        let request = Request { session: session.into(), call: call.into(), questions: vec![Question {
            plan_review: Some(self.config.review.clone()),
            id: "plan-review".into(), header: "Plan review".into(), question: "Approve this plan and leave plan mode?".into(), markdown: Some(input.plan), multi_select: false,
            options: vec![OptionItem { label: "Approve".into(), description: "Execute the plan from the next step".into() }, OptionItem { label: "Keep planning".into(), description: "Revise the plan using your feedback".into() }],
        }] };
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("plan review cancelled".into()),
            _ = self.alive.cancelled() => return Err("plan plugin unloaded; present the plan again".into()),
            response = self.questions.ask(request, cancel) => response,
        };
        if cancel.is_cancelled() || self.alive.is_cancelled() { return Err("plan review cancelled or plugin unloaded".into()); }
        let response = match response {
            Ok(response) => response,
            Err(_) => return Ok(("Plan review closed. Stay in plan mode and wait for the user's next message.".into(), PlanReview::Dismissed)),
        };
        let answers: Answers = serde_json::from_str(&response).map_err(|e| e.to_string())?;
        let answer = answers.answers.iter().find(|answer| answer.id == "plan-review").ok_or("missing plan review answer")?;
        if answer.selected == ["Approve"] && answer.custom.is_none() {
            let output = match &answer.edited_markdown {
                Some(plan) => {
                    validate_plan(plan)?;
                    format!("The user edited and approved the following plan. It replaces the original plan. Leave plan mode at the next step and carry out this version:\n\n{plan}")
                }
                None => "Plan approved. Leave plan mode at the next step and carry out the plan.".into(),
            };
            Ok((output, PlanReview::Approved))
        } else {
            Ok((format!("Keep planning. Revise and present the plan again. Feedback: {}", answer.custom.as_deref().unwrap_or("")), PlanReview::KeepPlanning))
        }
    }
}
