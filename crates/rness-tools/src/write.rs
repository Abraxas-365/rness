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
    fn for_workspace(&self, session: &String, workspace: &std::path::Path) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::new(self.ws.for_session(session, workspace))))
    }
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
        self.apply(args).await.map(|(output, _)| output)
    }

    async fn execute_presented(&self, _session: &String, _call: &String, args: Value, _cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, presentation) = self.apply(args).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text {text:output}], None, false, Some(presentation)))
    }
}

impl WriteTool {
    async fn apply(&self, args: Value) -> Result<(String, Value), String> {
        let path = self.ws.resolve(required_str(&args, "path")?);
        let content = args["content"]
            .as_str()
            .ok_or_else(|| "missing required argument 'content'".to_string())?;

        self.ws.ensure_fresh(&path)?;
        let existed = path.exists();
        let before = if existed {
            use tokio::io::AsyncReadExt;
            match tokio::fs::File::open(&path).await {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    match file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes).await {
                        Ok(_) if bytes.len() <= 2 * 1024 * 1024 => String::from_utf8(bytes).ok(),
                        _ => None,
                    }
                }
                Err(_) => None,
            }
        } else { Some(String::new()) };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        self.ws.mark_seen(&path);
        let mut presentation = json!({"version":1,"kind":"write","path":path,"created":!existed,"bytes":content.len(),"truncated":true});
        if let Some(before) = before {
            if before.len().saturating_add(content.len()) <= 48 * 1024 {
                presentation["before"] = json!(before);
                presentation["after"] = json!(content);
                presentation["old_start"] = json!(1);
                presentation["new_start"] = json!(1);
                presentation["truncated"] = json!(false);
            } else if before == content {
                presentation["hunks"] = json!([]);
                presentation["changes_complete"] = json!(true);
            } else {
                let mut prefix = before.bytes().zip(content.bytes()).take_while(|(a,b)| a == b).count();
                while !before.is_char_boundary(prefix) || !content.is_char_boundary(prefix) { prefix -= 1; }
                let start = before[..prefix].rfind('\n').map_or(0, |i| i + 1);
                let suffix = before.as_bytes()[prefix..].iter().rev().zip(content.as_bytes()[prefix..].iter().rev()).take_while(|(a,b)| a == b).count();
                let mut old_end = before.len() - suffix;
                let mut new_end = content.len() - suffix;
                while !before.is_char_boundary(old_end) { old_end += 1; }
                while !content.is_char_boundary(new_end) { new_end += 1; }
                old_end = before[old_end..].find('\n').map_or(before.len(), |i| old_end + i + 1);
                new_end = content[new_end..].find('\n').map_or(content.len(), |i| new_end + i + 1);
                let changed_bytes = (old_end - start).saturating_add(new_end - start);
                if changed_bytes <= 48 * 1024 {
                    let line = before[..start].bytes().filter(|b| *b == b'\n').count() + 1;
                    let hunk = json!({"before":&before[start..old_end],"after":&content[start..new_end],"old_start":line,"new_start":line,"fragment":false});
                    if serde_json::to_vec(&hunk).is_ok_and(|bytes| bytes.len() <= 48 * 1024) {
                        presentation["hunks"] = json!([hunk]);
                        presentation["changes_complete"] = json!(true);
                    }
                }
                if presentation.get("hunks").is_none() {
                    let diff = similar::TextDiff::configure()
                        .timeout(std::time::Duration::from_millis(20))
                        .diff_lines(before.as_str(), content);
                    let mut budget = 48 * 1024usize;
                    let mut hunks = Vec::new();
                    let mut complete = true;
                    for group in diff.grouped_ops(3) {
                        let first = &group[0];
                        let last = &group[group.len() - 1];
                        let old_range = first.old_range().start..last.old_range().end;
                        let new_range = first.new_range().start..last.new_range().end;
                        let old_bytes: usize = diff.old_slices()[old_range.clone()].iter().map(|s| s.len()).sum();
                        let new_bytes: usize = diff.new_slices()[new_range.clone()].iter().map(|s| s.len()).sum();
                        if old_bytes.saturating_add(new_bytes) > budget || hunks.len() >= 100 {
                            complete = false;
                            break;
                        }
                        let hunk = json!({"before":diff.old_slices()[old_range.clone()].concat(),
                            "after":diff.new_slices()[new_range.clone()].concat(),
                            "old_start":old_range.start + 1,"new_start":new_range.start + 1,"fragment":false});
                        let size = serde_json::to_vec(&hunk).map(|b| b.len()).unwrap_or(usize::MAX);
                        if size > budget { complete = false; break; }
                        budget -= size;
                        hunks.push(hunk);
                    }
                    if !hunks.is_empty() || complete { presentation["hunks"] = json!(hunks); }
                    presentation["changes_complete"] = json!(complete);
                    if !complete { presentation["capture_reason"] = json!("changed_ranges_exceed_limit"); }
                }
            }
        }
        if presentation["truncated"] == true && presentation.get("hunks").is_none() {
            presentation["changes_complete"] = json!(false);
            presentation["capture_reason"] = json!("snapshot_or_changed_range_exceeds_limit_or_unreadable");
        }
        Ok((format!("Wrote {} bytes to {}", content.len(), path.display()), presentation))
    }
}
