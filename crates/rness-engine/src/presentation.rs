//! Host-side tool presentation, independent of frontend and implementation language.
#[async_trait::async_trait]
pub trait ToolCards: Send + Sync {
    /// None declines to render; the frontend uses its built-in fallback.
    async fn tool_card(&self, name: &str, args: serde_json::Value, output: &str, is_error: bool)
        -> Option<Vec<rness_kernel::presentation::StyledLine>>;
}
