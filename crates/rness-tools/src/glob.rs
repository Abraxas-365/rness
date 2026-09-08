//! Glob: gitignore-aware file matching, newest-first.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use globset::GlobBuilder;
use ignore::WalkBuilder;
use rness_engine::tools::Tool;
use serde_json::{json, Value};

use crate::{required_str, Workspace};

const MAX_RESULTS: usize = 100;

pub struct GlobTool {
    ws: Arc<Workspace>,
}

impl GlobTool {
    pub fn new(ws: Arc<Workspace>) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern (e.g. \"src/**/*.rs\"). Respects .gitignore. \
         Results are sorted newest-first."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. \"**/*.rs\"" },
                "path": { "type": "string", "description": "Base directory (default: working directory)" },
            },
            "required": ["pattern"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        let pattern = required_str(&args, "pattern")?.to_string();
        let base = self.ws.resolve(args["path"].as_str().unwrap_or("."));

        // Walking is blocking work; keep it off the async runtime.
        tokio::task::spawn_blocking(move || {
            let matcher = GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("bad pattern: {e}"))?
                .compile_matcher();

            let mut hits: Vec<(SystemTime, String)> = Vec::new();
            for entry in WalkBuilder::new(&base).hidden(false).require_git(false).build().flatten()
            {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
                if matcher.is_match(rel) {
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    hits.push((mtime, entry.path().display().to_string()));
                }
            }
            if hits.is_empty() {
                return Ok(format!("No files match {pattern}"));
            }
            hits.sort_by(|a, b| b.0.cmp(&a.0));
            let total = hits.len();
            let mut out: String = hits
                .into_iter()
                .take(MAX_RESULTS)
                .map(|(_, p)| p + "\n")
                .collect();
            if total > MAX_RESULTS {
                out.push_str(&format!(
                    "… {} more matches (showing the {MAX_RESULTS} newest; narrow the \
                     pattern or path to see the rest)\n",
                    total - MAX_RESULTS
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| format!("glob task: {e}"))?
    }
}
