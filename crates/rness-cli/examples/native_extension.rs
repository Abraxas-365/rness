//! Run: cargo run -p rness-cli --example native_extension
//! Composition example, not an automatically loaded binary plugin.
use std::sync::Arc;
use rness_engine::tools::{Tool, ToolRegistry};
use rness_kernel::presentation::TextProvider;
use rness_tui::modules::ext_statusline::StatusText;

struct NativeExtension;

#[async_trait::async_trait]
impl Tool for NativeExtension {
    fn name(&self) -> &str { "native_greeting" }
    fn description(&self) -> &str { "Return a deterministic greeting without side effects." }
    async fn execute(&self, _: serde_json::Value) -> Result<String, String> {
        Ok("Hello from a native extension".into())
    }
}

#[async_trait::async_trait]
impl TextProvider for NativeExtension {
    async fn text(&self) -> Option<String> {
        Some("rness | native extension active".into())
    }
}

#[tokio::main]
async fn main() {
    let extension = Arc::new(NativeExtension);
    let tools = ToolRegistry::default();
    tools.register(extension.clone());
    let status = StatusText::default();
    status.refresh(extension.as_ref()).await;
    // In a frontend, use the handle returned by ext_statusline::install instead.
    let output = tools.get("native_greeting").unwrap()
        .execute(serde_json::json!({})).await.unwrap();
    println!("{output}");
}
