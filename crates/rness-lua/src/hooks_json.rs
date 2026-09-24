//! hooks.json bridge — declarative command hooks (dsh parity).
//!
//! Users place a `hooks.json` in `.rness/hooks.json` (project) or
//! `~/.rness/hooks.json` (global). Format:
//!
//! ```json
//! {
//!   "hooks": {
//!     "pre_tool": [
//!       { "matcher": "Bash", "hooks": [{ "type": "command", "command": "my-checker" }] }
//!     ]
//!   }
//! }
//! ```
//!
//! Each command hook receives the hook payload as JSON on stdin and must
//! print a JSON decision on stdout. Exit code 0 = success, non-zero =
//! error. The bridge logs durable `hook/invoked` + `hook/result` events.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Top-level hooks.json file.
#[derive(Debug, Deserialize)]
pub struct HooksConfig {
    pub hooks: HashMap<String, Vec<MatcherGroup>>,
}

/// One matcher group: optional pattern + list of command hooks.
#[derive(Debug, Deserialize)]
pub struct MatcherGroup {
    pub matcher: Option<String>,
    pub hooks: Vec<CommandHook>,
}

/// A single command hook.
#[derive(Debug, Deserialize)]
pub struct CommandHook {
    #[serde(rename = "type")]
    pub hook_type: String,
    pub command: String,
    /// Timeout in seconds (default 30).
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_timeout() -> u64 {
    30
}

/// Load and parse `hooks.json` from a path. Returns `None` if the file
/// doesn't exist; errors on parse failure.
pub fn load(path: &Path) -> Result<Option<HooksConfig>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path).map_err(|e| format!("read hooks.json: {e}"))?;
    // Accept either { "hooks": { ... } } or bare { "pre_tool": [...], ... }
    if let Ok(config) = serde_json::from_str::<HooksConfig>(&content) {
        return Ok(Some(config));
    }
    // Try bare format
    let hooks: HashMap<String, Vec<MatcherGroup>> =
        serde_json::from_str(&content).map_err(|e| format!("parse hooks.json: {e}"))?;
    Ok(Some(HooksConfig { hooks }))
}

/// Known hook points for validation.
const KNOWN_HOOK_POINTS: &[&str] = &[
    "pre_tool",
    "guard",
    "post_tool",
    "tool_result",
    "tool_execute",
    "pre_step",
    "request",
    "request_error",
    "turn_stopping",
    "session_start",
    "session_created",
    "session_idle",
    "subagent_start",
    "subagent_stop",
    "turn_start",
    "turn_end",
    "frame",
    "ready",
];

/// Generate Lua code that registers all command hooks from a parsed config.
/// Each hook calls `__rness_run_command_hook(command, timeout, payload_json)`
/// which is a Rust-backed function injected into the Lua VM.
pub fn generate_lua(config: &HooksConfig) -> String {
    let mut lua = String::new();
    for (point, groups) in &config.hooks {
        if !KNOWN_HOOK_POINTS.contains(&point.as_str()) {
            tracing::warn!("hooks.json: unknown hook point '{point}', skipping");
            continue;
        }
        for group in groups {
            for hook in &group.hooks {
                if hook.hook_type != "command" {
                    tracing::warn!(
                        "hooks.json: skipping non-command hook type '{}' for {point}",
                        hook.hook_type
                    );
                    continue;
                }
                let cmd_escaped = hook.command.replace('\\', "\\\\").replace('"', "\\\"");
                let timeout = hook.timeout;
                // `request` is observe-only (return value ignored), so treat
                // it as notification even though it goes through intercept().
                let is_interception = matches!(
                    point.as_str(),
                    "pre_tool" | "guard" | "post_tool" | "tool_execute"
                        | "pre_step" | "request_error" | "turn_stopping"
                );
                if let Some(matcher) = &group.matcher {
                    let matcher_escaped = matcher.replace('\\', "\\\\").replace('"', "\\\"");
                    if is_interception {
                        lua.push_str(&format!(
                            r#"rness.hook.on("{point}", {{match = "{matcher_escaped}"}}, function(ev, next)
  local result = __rness_run_command_hook("{cmd_escaped}", {timeout}, ev)
  if result == nil then return next() end
  return result
end)
"#
                        ));
                    } else {
                        lua.push_str(&format!(
                            r#"rness.hook.on("{point}", {{match = "{matcher_escaped}"}}, function(ev)
  __rness_run_command_hook("{cmd_escaped}", {timeout}, ev)
end)
"#
                        ));
                    }
                } else if is_interception {
                    lua.push_str(&format!(
                        r#"rness.hook.on("{point}", function(ev, next)
  local result = __rness_run_command_hook("{cmd_escaped}", {timeout}, ev)
  if result == nil then return next() end
  return result
end)
"#
                    ));
                } else {
                    lua.push_str(&format!(
                        r#"rness.hook.on("{point}", function(ev)
  __rness_run_command_hook("{cmd_escaped}", {timeout}, ev)
end)
"#
                    ));
                }
            }
        }
    }
    lua
}

/// The maximum stderr we capture from a command hook (bytes).
const STDERR_MAX: usize = 500;

/// Run a command hook synchronously. Called from the Lua VM thread.
///
/// - Payload JSON is written to the child's stdin.
/// - stdout is parsed as JSON for the decision.
/// - stderr is captured (bounded).
/// - Returns `(exit_code, stdout_json_or_nil, stderr_summary)`.
pub fn run_command(command: &str, timeout_secs: u64, payload_json: &str) -> CommandResult {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let start = std::time::Instant::now();

    let child = Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return CommandResult {
                exit_code: -1,
                decision: None,
                stderr_summary: Some(format!("spawn failed: {e}")),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    // Write payload to stdin.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload_json.as_bytes());
        // Drop closes stdin.
    }

    // Wait with timeout.
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let output = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = child.stdout.take().map(|mut r| {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut r, &mut s).ok();
                    s
                }).unwrap_or_default();
                let stderr = child.stderr.take().map(|mut r| {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut r, &mut s).ok();
                    s
                }).unwrap_or_default();
                break Ok((status, stdout, stderr));
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    break Err("timed out".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                break Err(format!("wait failed: {e}"));
            }
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    match output {
        Ok((status, stdout, stderr)) => {
            let exit_code = status.code().unwrap_or(-1);
            let stderr_summary = if stderr.len() > STDERR_MAX {
                // Find a char boundary at or before STDERR_MAX to avoid panic.
                let end = stderr.floor_char_boundary(STDERR_MAX);
                Some(format!("{}…", &stderr[..end]))
            } else if stderr.is_empty() {
                None
            } else {
                Some(stderr)
            };
            let decision = if exit_code == 0 && !stdout.trim().is_empty() {
                serde_json::from_str(stdout.trim()).ok()
            } else {
                None
            };
            CommandResult {
                exit_code,
                decision,
                stderr_summary,
                duration_ms,
            }
        }
        Err(msg) => CommandResult {
            exit_code: -1,
            decision: None,
            stderr_summary: Some(msg),
            duration_ms,
        },
    }
}

/// Result of running a command hook.
pub struct CommandResult {
    pub exit_code: i32,
    pub decision: Option<serde_json::Value>,
    pub stderr_summary: Option<String>,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wrapped_format() {
        let json = r#"{
            "hooks": {
                "pre_tool": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            { "type": "command", "command": "echo ok" }
                        ]
                    }
                ]
            }
        }"#;
        let config: HooksConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks["pre_tool"][0].matcher.as_deref(), Some("Bash"));
        assert_eq!(config.hooks["pre_tool"][0].hooks[0].command, "echo ok");
    }

    #[test]
    fn parse_bare_format() {
        let json = r#"{
            "pre_tool": [
                { "hooks": [{ "type": "command", "command": "true" }] }
            ]
        }"#;
        let config = load_from_str(json).unwrap();
        assert!(config.hooks.contains_key("pre_tool"));
    }

    fn load_from_str(s: &str) -> Result<HooksConfig, String> {
        if let Ok(config) = serde_json::from_str::<HooksConfig>(s) {
            return Ok(config);
        }
        let hooks: HashMap<String, Vec<MatcherGroup>> =
            serde_json::from_str(s).map_err(|e| format!("{e}"))?;
        Ok(HooksConfig { hooks })
    }

    #[test]
    fn generate_lua_produces_registrations() {
        let config = HooksConfig {
            hooks: [(
                "pre_tool".into(),
                vec![MatcherGroup {
                    matcher: Some("Bash".into()),
                    hooks: vec![CommandHook {
                        hook_type: "command".into(),
                        command: "my-checker".into(),
                        timeout: 10,
                    }],
                }],
            )]
            .into(),
        };
        let lua = generate_lua(&config);
        assert!(lua.contains("rness.hook.on(\"pre_tool\""), "{lua}");
        assert!(lua.contains("match = \"Bash\""), "{lua}");
        assert!(lua.contains("my-checker"), "{lua}");
    }

    #[test]
    fn run_command_captures_stdout_json() {
        let result = run_command("echo '{\"kind\":\"allow\"}'", 5, "{}");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.decision.unwrap()["kind"], "allow");
    }

    #[test]
    fn run_command_nonzero_exit_gives_no_decision() {
        let result = run_command("exit 1", 5, "{}");
        assert_eq!(result.exit_code, 1);
        assert!(result.decision.is_none());
    }

    #[test]
    fn run_command_reads_stdin() {
        // The command reads stdin and echoes part of it.
        let result = run_command(
            r#"read input; echo "{\"got\": \"$input\"}" "#,
            5,
            "hello",
        );
        assert_eq!(result.exit_code, 0);
        // Shell read strips trailing newline from stdin, so it should get "hello".
        assert!(result.decision.is_some(), "should parse stdout as JSON");
    }
}
