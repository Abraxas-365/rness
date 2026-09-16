//! Optional model-facing image reader; activated by a Lua plugin, not register_all.
use std::{path::{Path, PathBuf}, sync::{Arc, OnceLock}};
use async_trait::async_trait;
use rness_engine::{images::ImageStore, tools::Tool};
use rness_protocol::events::{SessionId, ToolResultContentPart, TaskSnapshot};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub type ImageCapability = dyn Fn(&SessionId) -> Result<(), String> + Send + Sync;

pub struct ReadImage {
    pub images: Arc<OnceLock<Arc<ImageStore>>>,
    pub capability: Arc<ImageCapability>,
    pub processing: Arc<tokio::sync::Semaphore>,
    pub workspace: Option<PathBuf>,
}

#[async_trait]
impl Tool for ReadImage {
    fn name(&self) -> &str { "read_image" }
    fn description(&self) -> &str {
        "Read a PNG/JPEG/WebP/GIF file and return the image itself. Format is detected from content, including extensionless files. Images are validated and resized using Lua image policy. Requires declared image-input support for the selected model."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"file_path":{"type":"string","description":"Image path; relative paths resolve against the session workspace"}},"required":["file_path"],"additionalProperties":false})
    }
    fn concurrency_safe(&self, _: &Value) -> bool { true }
    fn for_workspace(&self, _: &SessionId, workspace: &Path) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self { images:self.images.clone(), capability:self.capability.clone(), processing:self.processing.clone(), workspace:Some(workspace.into()) }))
    }
    async fn execute(&self, _: Value) -> Result<String, String> { Err("read_image requires session-aware rich tool dispatch".into()) }
    async fn execute_rich(&self, session: &SessionId, _: &str, args: Value, cancel: &CancellationToken)
        -> Result<(Vec<ToolResultContentPart>, Option<TaskSnapshot>, bool), String> {
        if cancel.is_cancelled() { return Err("image read cancelled".into()); }
        (self.capability)(session)?;
        let store = self.images.get().ok_or("image storage is not configured")?.clone();
        let input = crate::required_str(&args, "file_path")?;
        if input.trim().is_empty() { return Err("file_path must not be empty".into()); }
        let path = Path::new(input);
        let path = if path.is_absolute() { path.to_owned() } else {
            self.workspace.as_ref().ok_or("relative image path requires a session workspace")?.join(path)
        };
        let permit = tokio::select! {
            _ = cancel.cancelled() => return Err("image read cancelled".into()),
            permit = self.processing.clone().acquire_owned() => permit.map_err(|e| e.to_string())?,
        };
        let session = session.clone();
        let token = cancel.clone();
        let work = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let _permit = permit;
            if token.is_cancelled() { return Err("image read cancelled".into()); }
            // Refuse special files before open (notably FIFOs). O_NONBLOCK also
            // prevents a raced FIFO replacement from blocking on Unix.
            if !std::fs::metadata(&path).map_err(|e| e.to_string())?.is_file() { return Err("image target must be a regular file".into()); }
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NONBLOCK); }
            let file = options.open(&path).map_err(|e| e.to_string())?;
            let meta = file.metadata().map_err(|e| e.to_string())?;
            if !meta.is_file() { return Err("image target must be a regular file".into()); }
            let limit = store.policy().max_input_bytes as u64;
            if meta.len() > limit { return Err("image exceeds input byte limit".into()); }
            let mut bytes = Vec::new();
            file.take(limit.saturating_add(1)).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            if bytes.len() as u64 > limit { return Err("image exceeds input byte limit".into()); }
            if token.is_cancelled() { return Err("image read cancelled".into()); }
            let (attachment, original) = store.admit_tool_image(&session, &bytes)?;
            if token.is_cancelled() { return Err("image read cancelled".into()); }
            let text = format!("{}: {} image, {}x{} px (source {}x{} px). If resized, map coordinates to the source using x * {:.4}, y * {:.4}.",
                path.display(), attachment.media_type, attachment.width, attachment.height, original.0, original.1,
                f64::from(original.0) / f64::from(attachment.width), f64::from(original.1) / f64::from(attachment.height));
            Ok((vec![ToolResultContentPart::Text { text }, ToolResultContentPart::Image { attachment }], None, false))
        });
        tokio::select! {
            _ = cancel.cancelled() => Err("image read cancelled; in-progress decoding may finish locally".into()),
            result = work => result.map_err(|e| e.to_string())?,
        }
    }
}
