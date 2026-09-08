//! Write: create or overwrite a file. Overwriting an existing file
//! requires freshness (Read since last modification).

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::tools::Tool;
use serde_json::{json, Value};

use crate::{required_str, Workspace};

pub struct WriteTool {
    ws: Arc<Workspace>,
}

impl WriteTool {
    pub fn new(ws: Arc<Workspace>) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn sensitive(&self) -> bool {
        true // mutates the filesystem
    }

    fn description(&self) -> &str {
        "Write content to a file, creating parent directories as needed. \
         Overwrites the file if it exists — existing files must be Read first."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to the working directory)" },
                "content": { "type": "string", "description": "Full content to write" },
            },
            "required": ["path", "content"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        let path = self.ws.resolve(required_str(&args, "path")?);
        let content = args["content"]
            .as_str()
            .ok_or_else(|| "missing required argument 'content'".to_string())?;

        self.ws.ensure_fresh(&path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        self.ws.mark_seen(&path);
        Ok(format!("Wrote {} bytes to {}", content.len(), path.display()))
    }
}
