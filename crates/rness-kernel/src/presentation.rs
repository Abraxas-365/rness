//! Language-independent, asynchronous text presentation extension.

/// A presentation callback evaluated outside the frontend render loop.
/// `None` declines to render so the frontend can use its fallback.
#[async_trait::async_trait]
pub trait TextProvider: Send + Sync {
    async fn text(&self) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledLine {
    pub text: String,
    pub style: String,
    pub spans: Vec<StyledSpan>,
    pub right: Vec<StyledSpan>,
    pub block: Option<serde_json::Value>,
    pub is_header: bool,
    pub structured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    pub text: String,
    pub style: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct AppSpec {
    pub name: String,
    pub slot: String,
    pub title: String,
    pub keymap: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppKeyOutcome {
    Pass,
    Consumed,
    Close,
    Action { name: String, payload: serde_json::Value },
}

/// Collection of named applications. The host owns mounting and focus.
#[async_trait::async_trait]
pub trait Applications: Send + Sync {
    async fn app_specs(&self) -> Vec<AppSpec>;
    async fn app_view(&self, name: &str, ctx: serde_json::Value) -> Result<Vec<String>, String>;
    async fn app_key(&self, name: &str, key: &str, ctx: serde_json::Value) -> Result<AppKeyOutcome, String>;
}

/// Nonblocking event submission. Implementations must enqueue slow work.
pub trait HookSink: Send + Sync {
    fn fire_hook(&self, event: &str, payload: serde_json::Value);
}
