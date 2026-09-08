//! Edit: exact-string replacement with unique-match and freshness
//! guarantees. `replace_all` opts into multi-occurrence replacement.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::tools::Tool;
use serde_json::{json, Value};

use crate::{required_str, Workspace};

pub struct EditTool {
    ws: Arc<Workspace>,
}

impl EditTool {
    pub fn new(ws: Arc<Workspace>) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for EditTool {
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
        let path = self.ws.resolve(required_str(&args, "path")?);
        let old = required_str(&args, "old_string")?;
        let new = args["new_string"]
            .as_str()
            .ok_or_else(|| "missing required argument 'new_string'".to_string())?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        if old == new {
            return Err("old_string and new_string are identical".to_string());
        }

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
        Ok(format!("Replaced {n} occurrence(s) in {}", path.display()))
    }
}
