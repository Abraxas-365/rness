//! Grep: regex content search over the tree, gitignore-aware, with
//! files-with-matches / content / count output modes.

use std::sync::Arc;

use async_trait::async_trait;
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use ignore::WalkBuilder;
use rness_engine::tools::Tool;
use serde_json::{json, Value};

use crate::{required_str, Workspace};

const MAX_OUTPUT_LINES: usize = 250;
/// Cap on one matched-line preview; the cut preserves UTF-8 boundaries.
const MAX_LINE_BYTES: usize = 2000;

fn preview(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

pub struct GrepTool {
    ws: Arc<Workspace>,
}

impl GrepTool {
    pub fn new(ws: Arc<Workspace>) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regex. Respects .gitignore. Modes: \
         files_with_matches (default), content (matching lines), count."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for" },
                "path": { "type": "string", "description": "File or directory to search (default: working directory)" },
                "glob": { "type": "string", "description": "Only search files matching this glob (e.g. \"*.rs\")" },
                "output_mode": {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "description": "What to return (default files_with_matches)",
                },
                "case_insensitive": { "type": "boolean", "description": "Case-insensitive search (default false)" },
            },
            "required": ["pattern"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        let pattern = required_str(&args, "pattern")?.to_string();
        let base = self.ws.resolve(args["path"].as_str().unwrap_or("."));
        let mode = args["output_mode"].as_str().unwrap_or("files_with_matches").to_string();
        let case_insensitive = args["case_insensitive"].as_bool().unwrap_or(false);
        let file_glob = match args["glob"].as_str() {
            Some(g) => Some(
                globset::GlobBuilder::new(g)
                    .literal_separator(false)
                    .build()
                    .map_err(|e| format!("bad glob: {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };

        tokio::task::spawn_blocking(move || {
            let matcher = RegexMatcherBuilder::new()
                .case_insensitive(case_insensitive)
                .build(&pattern)
                .map_err(|e| format!("bad regex: {e}"))?;
            let mut searcher = SearcherBuilder::new().line_number(true).build();

            // (file, hits) accumulated per file, walk order.
            let mut per_file: Vec<(String, Vec<(u64, String)>)> = Vec::new();
            for entry in WalkBuilder::new(&base).hidden(false).require_git(false).build().flatten()
            {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                if let Some(g) = &file_glob {
                    let name = entry.file_name().to_string_lossy();
                    if !g.is_match(name.as_ref()) && !g.is_match(entry.path()) {
                        continue;
                    }
                }
                let mut hits: Vec<(u64, String)> = Vec::new();
                let _ = searcher.search_path(
                    &matcher,
                    entry.path(),
                    UTF8(|line_no, line| {
                        if matcher.is_match(line.as_bytes()).unwrap_or(false) {
                            hits.push((line_no, preview(line.trim_end())));
                        }
                        Ok(true)
                    }),
                );
                if !hits.is_empty() {
                    per_file.push((entry.path().display().to_string(), hits));
                }
            }

            if per_file.is_empty() {
                return Ok("No matches".to_string());
            }
            let mut lines: Vec<String> = Vec::new();
            match mode.as_str() {
                "content" => {
                    for (file, hits) in &per_file {
                        for (no, line) in hits {
                            lines.push(format!("{file}:{no}:{line}"));
                        }
                    }
                }
                "count" => {
                    for (file, hits) in &per_file {
                        lines.push(format!("{file}:{}", hits.len()));
                    }
                }
                _ => {
                    for (file, _) in &per_file {
                        lines.push(file.clone());
                    }
                }
            }
            let total = lines.len();
            let mut out = lines
                .into_iter()
                .take(MAX_OUTPUT_LINES)
                .collect::<Vec<_>>()
                .join("\n");
            out.push('\n');
            if total > MAX_OUTPUT_LINES {
                out.push_str(&format!(
                    "… {} more lines (narrow the pattern, path, or glob to see the rest)\n",
                    total - MAX_OUTPUT_LINES
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| format!("grep task: {e}"))?
    }
}
