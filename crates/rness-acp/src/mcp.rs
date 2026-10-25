//! ACP `mcpServers` → session-scoped MCP connections (dsh `acp/src/mcp.ts`).
//!
//! The client is a trusted controller: a stdio entry authorizes its absolute
//! command and environment, an HTTP entry its absolute URL and headers. Every
//! entry is validated before anything is spawned; all servers connect and
//! publish their tools before the session is handed to the client, and a
//! failure rolls back the ones already started. Tools land in a registry of
//! their own that only this session's turns see.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rness_engine::tools::ToolRegistry;
use rness_mcp::http::{HttpServer, SsePolicy};
use rness_mcp::{McpConnection, StdioServer};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::rpc::RpcError;

pub(crate) enum Spec {
    Stdio(StdioServer),
    Http(HttpServer),
}

impl Spec {
    fn name(&self) -> &str {
        match self {
            Spec::Stdio(spec) => &spec.name,
            Spec::Http(spec) => &spec.name,
        }
    }
}

/// Validate a session's complete `mcpServers` list. `cwd` is the session
/// workspace, used as the working directory of stdio servers.
pub(crate) fn parse(
    servers: Option<&Value>,
    cwd: &Path,
    timeout: Duration,
) -> Result<Vec<Spec>, RpcError> {
    let servers = match servers {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(servers)) => servers,
        Some(_) => return Err(RpcError::invalid_params("mcpServers must be an array")),
    };
    let mut specs: Vec<Spec> = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        let invalid = |detail: &str| {
            RpcError::invalid_params(format!("mcpServers[{index}] is invalid: {detail}"))
        };
        let raw_name = server
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("name must be a string"))?;
        let name = normalize_server_name(raw_name)?;
        if specs.iter().any(|spec| spec.name() == name) {
            return Err(RpcError::invalid_params(format!(
                "mcpServers contains duplicate normalized name: {name}"
            )));
        }
        let spec = match server.get("type").and_then(Value::as_str) {
            None | Some("stdio") => {
                let command = server
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("a stdio server needs a command string"))?;
                if !Path::new(command).is_absolute() {
                    return Err(RpcError::invalid_params(format!(
                        "mcpServers[{index}].command must be an absolute path"
                    )));
                }
                let args = strings(server.get("args"))
                    .ok_or_else(|| invalid("args must be an array of strings"))?;
                let env = entries(
                    server.get("env"),
                    &format!("mcpServers[{index}].env"),
                    Kind::Environment,
                )?;
                Spec::Stdio(StdioServer {
                    name,
                    command: command.into(),
                    args,
                    env,
                    cwd: Some(cwd.to_path_buf()),
                    timeout,
                })
            }
            Some("http") => {
                let url = server.get("url").and_then(Value::as_str).unwrap_or("");
                let parsed = reqwest::Url::parse(url)
                    .ok()
                    .filter(|url| matches!(url.scheme(), "http" | "https"))
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!(
                            "mcpServers[{index}].url must be an absolute HTTP(S) URL"
                        ))
                    })?;
                // Same transport policy rness-mcp enforces at connect time,
                // checked here so it is a parameter error and nothing starts.
                if !secure_url(&parsed) {
                    return Err(RpcError::invalid_params(format!(
                        "mcpServers[{index}].url must use HTTPS (HTTP only on loopback), \
                         without credentials or fragment"
                    )));
                }
                let headers = entries(
                    server.get("headers"),
                    &format!("mcpServers[{index}].headers"),
                    Kind::Header,
                )?;
                Spec::Http(HttpServer {
                    name,
                    url: url.into(),
                    headers,
                    timeout,
                    sse: SsePolicy::default(),
                })
            }
            Some(other) => {
                return Err(RpcError::invalid_params(format!(
                    "mcpServers[{index}] transport {other} is not supported"
                )))
            }
        };
        specs.push(spec);
    }
    Ok(specs)
}

fn secure_url(url: &reqwest::Url) -> bool {
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Some(_) => None,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Environment,
    Header,
}

/// Ordered ACP `{name, value}` entries, never silently accepting duplicates.
fn entries(
    value: Option<&Value>,
    field: &str,
    kind: Kind,
) -> Result<Vec<(String, String)>, RpcError> {
    let label = match kind {
        Kind::Environment => "environment",
        Kind::Header => "header",
    };
    let invalid = || RpcError::invalid_params(format!("{field} contains an invalid {label} entry"));
    let items = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid()),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for item in items {
        let (Some(name), Some(value)) = (
            item.get("name").and_then(Value::as_str),
            item.get("value").and_then(Value::as_str),
        ) else {
            return Err(invalid());
        };
        let valid = match kind {
            Kind::Environment => {
                !name.is_empty() && !name.contains(['=', '\0']) && !value.contains('\0')
            }
            Kind::Header => {
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok()
                    && reqwest::header::HeaderValue::from_str(value).is_ok()
            }
        };
        if !valid {
            return Err(invalid());
        }
        let same = |known: &str| match kind {
            Kind::Environment => known == name,
            Kind::Header => known.eq_ignore_ascii_case(name),
        };
        if out.iter().any(|(known, _)| same(known)) {
            return Err(RpcError::invalid_params(format!(
                "{field} contains duplicate name: {name}"
            )));
        }
        out.push((name.into(), value.into()));
    }
    Ok(out)
}

/// A stable tool namespace from ACP's human-readable server name: valid
/// names pass through; others become a slug plus a digest of the original.
pub(crate) fn normalize_server_name(name: &str) -> Result<String, RpcError> {
    if name.trim().is_empty() || name.chars().any(|c| c.is_control()) {
        return Err(RpcError::invalid_params(
            "mcpServers contains an invalid server name",
        ));
    }
    let valid = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if name.len() <= 32 && name.chars().all(valid) {
        return Ok(name.into());
    }
    let mut slug = String::new();
    for c in name.chars() {
        if valid(c) {
            slug.push(c);
        } else if !slug.ends_with('_') {
            slug.push('_');
        }
    }
    let slug: String = slug.trim_matches('_').chars().take(20).collect();
    let slug = if slug.is_empty() {
        "server".into()
    } else {
        slug
    };
    let digest = Sha256::digest(name.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    Ok(format!("{slug}_{hex}").chars().take(32).collect())
}

/// One session's live MCP connections and the registry they publish into.
pub(crate) struct Mounted {
    pub(crate) registry: Arc<ToolRegistry>,
    connections: Vec<Arc<McpConnection>>,
}

impl Mounted {
    /// Connect every server and publish its tools; any failure closes the
    /// connections already made.
    pub(crate) async fn connect(specs: Vec<Spec>, base: &ToolRegistry) -> Result<Self, RpcError> {
        let mounted = Self {
            registry: Arc::new(base.scoped_empty()),
            connections: Vec::new(),
        };
        let mut mounted = mounted;
        for (index, spec) in specs.into_iter().enumerate() {
            let connected = match spec {
                Spec::Stdio(spec) => McpConnection::connect(spec).await,
                Spec::Http(spec) => McpConnection::connect_http(spec).await,
            };
            let bridged = match connected {
                Ok(connection) => {
                    let bridged = connection.bridge_tools(&mounted.registry).await;
                    mounted.connections.push(Arc::clone(&connection));
                    bridged.map(|_| connection)
                }
                Err(error) => Err(error),
            };
            match bridged {
                Ok(connection) => connection.watch(&mounted.registry, false).await,
                Err(error) => {
                    mounted.close().await;
                    return Err(RpcError::internal(format!(
                        "mcpServers[{index}] failed to start: {error}"
                    )));
                }
            }
        }
        Ok(mounted)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Unregister the tools and stop the servers (stdio children are killed).
    pub(crate) async fn close(&self) {
        for connection in &self.connections {
            connection.disconnect(&self.registry).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_one(server: Value) -> Result<Vec<Spec>, String> {
        parse(
            Some(&json!([server])),
            Path::new("/w"),
            Duration::from_secs(1),
        )
        .map_err(|e| e.message)
    }

    #[test]
    fn stdio_and_http_entries_parse() {
        let specs = parse(
            Some(&json!([
                {"name": "fs", "command": "/bin/fs", "args": ["-v"], "env": [{"name": "K", "value": "v"}]},
                {"type": "http", "name": "web", "url": "https://x.test/mcp", "headers": [{"name": "Authorization", "value": "Bearer t"}]},
            ])),
            Path::new("/w"),
            Duration::from_secs(1),
        )
        .unwrap();
        let Spec::Stdio(stdio) = &specs[0] else {
            panic!()
        };
        assert_eq!(stdio.args, ["-v"]);
        assert_eq!(stdio.env, [("K".to_string(), "v".to_string())]);
        assert_eq!(stdio.cwd.as_deref(), Some(Path::new("/w")));
        let Spec::Http(http) = &specs[1] else {
            panic!()
        };
        assert_eq!(http.headers[0].0, "Authorization");
        assert!(parse(None, Path::new("/w"), Duration::from_secs(1))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn invalid_entries_are_rejected_like_dsh() {
        let cases = [
            (
                json!({"name": "a", "command": "fs"}),
                "Invalid params: mcpServers[0].command must be an absolute path",
            ),
            (
                json!({"name": "a", "command": "/fs", "env": [{"name": "A=B", "value": ""}]}),
                "Invalid params: mcpServers[0].env contains an invalid environment entry",
            ),
            (
                json!({"name": "a", "command": "/fs", "env": [{"name": "A", "value": "1"}, {"name": "A", "value": "2"}]}),
                "Invalid params: mcpServers[0].env contains duplicate name: A",
            ),
            (
                json!({"type": "http", "name": "a", "url": "ftp://x"}),
                "Invalid params: mcpServers[0].url must be an absolute HTTP(S) URL",
            ),
            (
                json!({"type": "http", "name": "a", "url": "/relative"}),
                "Invalid params: mcpServers[0].url must be an absolute HTTP(S) URL",
            ),
            (
                json!({"type": "http", "name": "a", "url": "http://127.0.0.1:9", "headers": [{"name": "bad header", "value": "v"}]}),
                "Invalid params: mcpServers[0].headers contains an invalid header entry",
            ),
            (
                json!({"type": "http", "name": "a", "url": "https://x", "headers": [{"name": "X-A", "value": "1"}, {"name": "x-a", "value": "2"}]}),
                "Invalid params: mcpServers[0].headers contains duplicate name: x-a",
            ),
            (
                json!({"type": "sse", "name": "a", "url": "http://x"}),
                "Invalid params: mcpServers[0] transport sse is not supported",
            ),
            (
                json!({"type": "http", "name": "a", "url": "http://example.com/mcp"}),
                "Invalid params: mcpServers[0].url must use HTTPS (HTTP only on loopback), without credentials or fragment",
            ),
            (
                json!({"type": "http", "name": "a", "url": "https://u:p@x/mcp"}),
                "Invalid params: mcpServers[0].url must use HTTPS (HTTP only on loopback), without credentials or fragment",
            ),
            (
                json!({"type": "stdio", "name": "a"}),
                "Invalid params: mcpServers[0] is invalid: a stdio server needs a command string",
            ),
            (
                json!({"name": " ", "command": "/fs"}),
                "Invalid params: mcpServers contains an invalid server name",
            ),
        ];
        for (server, expected) in cases {
            assert_eq!(
                parse_one(server.clone()).err().as_deref(),
                Some(expected),
                "{server}"
            );
        }
        let duplicate = parse(
            Some(&json!([{"name": "a", "command": "/x"}, {"name": "a", "command": "/y"}])),
            Path::new("/w"),
            Duration::from_secs(1),
        );
        assert_eq!(
            duplicate.err().map(|e| e.message).as_deref(),
            Some("Invalid params: mcpServers contains duplicate normalized name: a")
        );
    }

    #[test]
    fn server_names_normalize_stably() {
        assert_eq!(normalize_server_name("github").unwrap(), "github");
        let slug = normalize_server_name("My Files (local)").unwrap();
        assert!(slug.starts_with("My_Files_local_"), "{slug}");
        assert_eq!(slug, normalize_server_name("My Files (local)").unwrap());
        assert_ne!(slug, normalize_server_name("My Files [local]").unwrap());
        let long = normalize_server_name(&"x".repeat(40)).unwrap();
        assert!(
            long.len() <= 32 && long.starts_with("xxxxxxxxxxxxxxxxxxxx_"),
            "{long}"
        );
        assert!(normalize_server_name("a\u{7}b").is_err());
    }
}
