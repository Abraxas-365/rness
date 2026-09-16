//! Edit: exact-string replacement with unique-match and freshness
//! guarantees. `replace_all` opts into multi-occurrence replacement.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::tools::Tool;
use serde_json::{json, Value};

use crate::{required_str, Workspace};

pub struct EditTool {
    ws: Arc<Workspace>,
    policy: crate::sandbox::Policy,
}

impl EditTool {
    pub fn new(ws: Arc<Workspace>) -> Self {
        let policy = crate::sandbox::Policy::new(rness_protocol::sandbox::SandboxMode::DangerFullAccess, ws.root());
        Self { ws, policy }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn for_workspace_with_policy(&self, session: &String, workspace: &std::path::Path, mode: rness_protocol::sandbox::SandboxMode) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self { ws: self.ws.for_session(session, workspace), policy: crate::sandbox::Policy { mode, workspace: workspace.to_owned() } }))
    }
    fn for_workspace(&self, session: &String, workspace: &std::path::Path) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::new(self.ws.for_session(session, workspace))))
    }
    fn name(&self) -> &str {
        "Edit"
    }

    fn sensitive(&self) -> bool {
        true // mutates the filesystem
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file. old_string must match exactly once \
         unless replace_all is true. The file must be Read first."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to the working directory)" },
                "old_string": { "type": "string", "description": "Exact text to replace" },
                "new_string": { "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)" },
            },
            "required": ["path", "old_string", "new_string"],
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

impl EditTool {
    async fn apply(&self, args: Value) -> Result<(String, Value), String> {
        let path = self.ws.resolve(required_str(&args, "path")?);
        let old = required_str(&args, "old_string")?;
        let new = args["new_string"]
            .as_str()
            .ok_or_else(|| "missing required argument 'new_string'".to_string())?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        if old == new {
            return Err("old_string and new_string are identical".to_string());
        }

        self.policy.check_write(&path)?;
        self.policy.check_write(&path)?;
        self.ws.ensure_fresh(&path)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))?;

        let count = content.matches(old).count();
        let replaced = match (count, replace_all) {
            (0, _) => {
                return Err(format!(
                    "old_string not found in {} — Read the file again and copy the text \
                     exactly (whitespace included)",
                    path.display()
                ))
            }
            (1, _) => content.replacen(old, new, 1),
            (n, true) => {
                let _ = n;
                content.replace(old, new)
            }
            (n, false) => {
                return Err(format!(
                    "old_string matches {n} times in {} — add surrounding context to make it \
                     unique, or set replace_all",
                    path.display()
                ))
            }
        };

        tokio::fs::write(&path, &replaced)
            .await
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        self.ws.mark_seen(&path);
        let n = if replace_all { count } else { 1 };
        let snapshot_bytes = content.len().saturating_add(replaced.len());
        let presentation = if snapshot_bytes <= 48 * 1024 {
            json!({"version":1,"kind":"edit","path":path,"before":content,"after":replaced,"old_start":1,"new_start":1,"truncated":false})
        } else {
            let mut hunks: Vec<serde_json::Value> = Vec::new();
            let mut last_window: Option<(usize, usize, usize, usize)> = None;
            let mut captured = 0;
            let mut budget = 48 * 1024;
            let mut old_line = 1;
            let mut new_line = 1;
            let mut previous = 0;
            let mut new_previous = 0;
            for (offset, matched) in content.match_indices(old).take(n) {
                if matched.len().saturating_add(new.len()) > budget || hunks.len() >= 100 { break; }
                let unchanged_lines = content[previous..offset].bytes().filter(|b| *b == b'\n').count();
                old_line += unchanged_lines;
                new_line += unchanged_lines;
                let new_offset = new_previous + offset - previous;
                let line_window = |text: &str, start: usize, end: usize| {
                    let start = text[..start].rfind('\n').map_or(0, |i| i + 1);
                    let end = if end > start && text.as_bytes().get(end - 1) == Some(&b'\n') { end }
                        else { text[end..].find('\n').map_or(text.len(), |i| end + i + 1) };
                    (start, end)
                };
                let (old_start, old_end) = line_window(&content, offset, offset + matched.len());
                let (new_start, new_end) = line_window(&replaced, new_offset, new_offset + new.len());
                let complete_lines = (old_end - old_start).saturating_add(new_end - new_start) <= budget / 6;
                let hunk = if complete_lines {
                    json!({"before":&content[old_start..old_end],"after":&replaced[new_start..new_end],"old_start":old_line,"new_start":new_line,"fragment":false})
                } else {
                    json!({"before":matched,"after":new,"old_start":old_line,"new_start":new_line,"fragment":true})
                };
                let mut merged = false;
                if complete_lines {
                    if let Some((a, b, c, d)) = last_window {
                        if old_start <= b && new_start <= d {
                            let last = hunks.last().unwrap();
                            let candidate = json!({"before":&content[a..old_end.max(b)],"after":&replaced[c..new_end.max(d)],
                                "old_start":last["old_start"],"new_start":last["new_start"],"fragment":false});
                            let old_size = serde_json::to_vec(last).unwrap().len();
                            let size = serde_json::to_vec(&candidate).unwrap().len();
                            if size > budget + old_size { break; }
                            budget = budget + old_size - size;
                            *hunks.last_mut().unwrap() = candidate;
                            last_window = Some((a, old_end.max(b), c, new_end.max(d)));
                            merged = true;
                        }
                    }
                }
                if !merged {
                    let size = serde_json::to_vec(&hunk).map(|v| v.len()).unwrap_or(usize::MAX);
                    if size > budget || hunks.len() >= 100 { break; }
                    budget -= size;
                    hunks.push(hunk);
                    last_window = complete_lines.then_some((old_start,old_end,new_start,new_end));
                }
                captured += 1;
                old_line += matched.bytes().filter(|b| *b == b'\n').count();
                new_line += new.bytes().filter(|b| *b == b'\n').count();
                previous = offset + matched.len();
                new_previous = new_offset + new.len();
            }
            json!({"version":1,"kind":"edit","path":path,"replacements":n,"captured_replacements":captured,"changes_complete":captured == n,"hunks":hunks,"truncated":true})
        };
        Ok((format!("Replaced {n} occurrence(s) in {}", path.display()), presentation))
    }
}
