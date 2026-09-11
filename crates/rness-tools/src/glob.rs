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
    fn for_workspace(&self, session: &String, workspace: &std::path::Path) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::new(self.ws.for_session(session, workspace))))
    }
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
        self.glob_presented(args).await.map(|(output, _)| output)
    }

    async fn execute_presented(
        &self, _session: &String, _call: &String, args: Value,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<Value>), String> {
        let (output, presentation) = self.glob_presented(args).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text { text: output }], None, false, Some(presentation)))
    }
}

impl GlobTool {
    async fn glob_presented(&self, args: Value) -> Result<(String, Value), String> {
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
                return Ok((format!("No files match {pattern}"), json!({"version":1,"kind":"glob","path":base,"pattern":pattern,"paths":[],"total":0,"truncated":false})));
            }
            hits.sort_by(|a, b| b.0.cmp(&a.0));
            let total = hits.len();
            let mut bytes = 0;
            let paths: Vec<_> = hits.iter().take(MAX_RESULTS).map(|(_, p)| p).take_while(|p| {
                bytes += p.len();
                bytes <= 24 * 1024
            }).cloned().collect();
            let presentation = json!({"version":1,"kind":"glob","path":base,"pattern":pattern,"truncated":paths.len()<total,"paths":paths,"total":total});
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
            Ok((out, presentation))
        })
        .await
        .map_err(|e| format!("glob task: {e}"))?
    }
}
