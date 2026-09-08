//! Read: file contents with line numbers (cat -n style), offset/limit
//! windows, and freshness ledger updates.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::tools::Tool;
use serde_json::{json, Value};

use crate::{required_str, Workspace};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
/// Cap on total bytes returned by one read: line windows alone don't
/// bound the payload when lines are long.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Parse an optional positive-integer argument with a clear error.
fn positive_arg(args: &Value, key: &str, default: usize) -> Result<usize, String> {
    match &args[key] {
        Value::Null => Ok(default),
        v => match v.as_u64() {
            Some(n) if n >= 1 => Ok(n as usize),
            _ => Err(format!("'{key}' must be a positive integer, got {v}")),
        },
    }
}

pub struct ReadTool {
    ws: Arc<Workspace>,
}

impl ReadTool {
    pub fn new(ws: Arc<Workspace>) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn for_workspace(&self, session: &String, workspace: &std::path::Path) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::new(self.ws.for_session(session, workspace))))
    }
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file from the filesystem. Returns numbered lines (cat -n style). \
         Use offset (1-based line) and limit to window large files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to the working directory)" },
                "offset": { "type": "integer", "description": "1-based line to start from (default 1)" },
                "limit": { "type": "integer", "description": "Max lines to return (default 2000)" },
            },
            "required": ["path"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, String> {
        let path = self.ws.resolve(required_str(&args, "path")?);
        let offset = positive_arg(&args, "offset", 1)?;
        let limit = positive_arg(&args, "limit", DEFAULT_LIMIT)?;

        let content = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let content = String::from_utf8(content)
            .map_err(|_| format!("{} is not valid UTF-8 (binary file?)", path.display()))?;
        self.ws.mark_seen(&path);

        let total = content.lines().count();
        if total == 0 {
            return Ok("(empty file)".to_string());
        }
        if offset > total {
            return Err(format!("offset {offset} is past the end of the file ({total} lines)"));
        }

        let mut out = String::new();
        let mut shown_end = offset - 1;
        for (i, line) in content.lines().enumerate().skip(offset - 1).take(limit) {
            let line = if line.len() > MAX_LINE_LEN {
                let mut end = MAX_LINE_LEN;
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                &line[..end]
            } else {
                line
            };
            // Byte cap: stop before this line would push past it, so the
            // model gets whole numbered lines plus an accurate footer.
            if !out.is_empty() && out.len() + line.len() + 8 > MAX_OUTPUT_BYTES {
                break;
            }
            out.push_str(&format!("{:>6}\t{line}\n", i + 1));
            shown_end = i + 1;
        }
        if shown_end < total {
            out.push_str(&format!(
                "… {} more lines (file has {total} lines; continue with offset={})\n",
                total - shown_end,
                shown_end + 1,
            ));
        }
        Ok(out)
    }
}
