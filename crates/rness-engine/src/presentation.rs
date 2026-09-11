//! Host-side tool presentation, independent of frontend and implementation language.
#[async_trait::async_trait]
pub trait ToolCards: Send + Sync {
    /// None declines to render; the frontend uses its built-in fallback.
    async fn tool_card(&self, name: &str, args: serde_json::Value, output: &str, is_error: bool)
        -> Option<Vec<rness_kernel::presentation::StyledLine>>;

    async fn tool_card_result(&self, args: serde_json::Value, result: &rness_protocol::events::ToolResult)
        -> Option<Vec<rness_kernel::presentation::StyledLine>> {
        let mut presentation = result.presentation.clone().filter(serde_json::Value::is_object).unwrap_or_else(|| serde_json::json!({"version":1,"kind":"tool_result"}));
        if let Some(object) = presentation.as_object_mut() {
            object.insert("content".into(), serde_json::to_value(&result.content).ok()?);
            object.insert("duration_ms".into(), serde_json::json!(result.duration_ms));
            if let Some(tasks) = &result.tasks { object.insert("tasks".into(), serde_json::to_value(tasks).ok()?); }
            if let Some(review) = &result.plan_review { object.insert("plan_review".into(), serde_json::to_value(review).ok()?); }
        }
        self.tool_card_presented(&result.name, args, &result.output, result.is_error, Some(presentation)).await
    }

    async fn tool_card_presented(&self, name: &str, args: serde_json::Value, output: &str, is_error: bool, presentation: Option<serde_json::Value>)
        -> Option<Vec<rness_kernel::presentation::StyledLine>> {
        let _ = presentation;
        self.tool_card(name, args, output, is_error).await
    }
}
