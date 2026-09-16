//! Streamable HTTP MCP transport. No redirects, ambient proxies or call replay.
use std::{sync::{Arc, Mutex}, time::Duration};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::{McpError, MAX_FRAME_BYTES};

/// Optional server notification stream and stream-only recovery. No POST replay.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsePolicy {
    pub notifications: bool,
    pub resume: bool,
    pub max_attempts: u32,
    pub retry_delay_ms: u64,
    pub idle_timeout_ms: u64,
}
impl Default for SsePolicy {
    fn default() -> Self {
        Self { notifications: false, resume: false, max_attempts: 5, retry_delay_ms: 500, idle_timeout_ms: 60_000 }
    }
}
impl SsePolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_attempts > 100 || !(1..=60_000).contains(&self.retry_delay_ms)
            || !(1..=86_400_000).contains(&self.idle_timeout_ms) {
            return Err("MCP sse requires max_attempts <= 100, retry_delay_ms 1..60000 and idle_timeout_ms 1..86400000".into());
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct HttpServer {
    pub name: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub sse: SsePolicy,
}

pub(crate) struct HttpTransport {
    pub spec: HttpServer,
    client: reqwest::Client,
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
}

/// Incremental SSE framing, bounded per event (not per long-lived connection).
#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    id: Option<String>,
    size: usize,
    after_cr: bool,
    started: bool,
}
struct Event { data: Vec<u8>, id: Option<String> }
impl Decoder {
    fn byte(&mut self, byte: u8) -> Result<Option<Event>, &'static str> {
        if self.after_cr && byte == b'\n' { self.after_cr = false; return Ok(None); }
        self.after_cr = byte == b'\r';
        self.size += 1;
        if self.size > MAX_FRAME_BYTES { return Err("MCP SSE event exceeds 16 MiB"); }
        if byte != b'\r' && byte != b'\n' { self.line.push(byte); return Ok(None); }
        let line = std::mem::take(&mut self.line);
        let line = if !self.started {
            self.started = true;
            line.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&line)
        } else { &line };
        if line.is_empty() {
            self.size = 0;
            if self.data.last() == Some(&b'\n') { self.data.pop(); }
            return Ok(Some(Event { data: std::mem::take(&mut self.data), id: self.id.take() }));
        }
        let end = line.iter().position(|b| *b == b':').unwrap_or(line.len());
        let field = &line[..end];
        let value = line.get(end + 1..).unwrap_or_default();
        let value = value.strip_prefix(b" ").unwrap_or(value);
        if field == b"data" { self.data.extend_from_slice(value); self.data.push(b'\n'); }
        if field == b"id" && !value.contains(&0) {
            let id = std::str::from_utf8(value).map_err(|_| "invalid MCP SSE event ID")?;
            if id.len() > 1024 || HeaderValue::from_str(id).is_err() { return Err("invalid MCP SSE event ID"); }
            self.id = Some(id.into());
        }
        Ok(None)
    }
}

impl HttpTransport {
    pub fn new(spec: HttpServer) -> Result<Arc<Self>, McpError> {
        let error = |message: &str| McpError::Protocol { server: spec.name.clone(), message: message.into() };
        spec.sse.validate().map_err(|e| error(&e))?;
        let url = reqwest::Url::parse(&spec.url).map_err(|_| error("invalid MCP URL"))?;
        let loopback = url.host_str().is_some_and(|host| host == "localhost" || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()));
        if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(error("MCP requires HTTPS (HTTP allowed only on loopback), without URL credentials or fragments"));
        }
        let mut headers = HeaderMap::new();
        for (key, value) in &spec.headers {
            let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| error("invalid MCP header name"))?;
            if matches!(name.as_str(), "host" | "content-length" | "content-type" | "accept" | "mcp-session-id" | "mcp-protocol-version" | "last-event-id") {
                return Err(error("MCP transport header cannot be overridden"));
            }
            headers.insert(name, HeaderValue::from_str(value).map_err(|_| error("invalid MCP header value"))?);
        }
        // GET streams must not inherit a whole-response timeout. Bound header
        // acquisition and stream idle time separately; POST calls remain bounded.
        let client = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
            .default_headers(headers).connect_timeout(spec.timeout).build().map_err(|_| error("MCP HTTP client setup failed"))?;
        Ok(Arc::new(Self { spec, client, session: Mutex::new(None), version: Mutex::new(None) }))
    }
    fn error(&self, message: impl Into<String>) -> McpError {
        McpError::Protocol { server: self.spec.name.clone(), message: message.into() }
    }
    fn request(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
        let accept = if method == reqwest::Method::GET { "text/event-stream" } else { "application/json, text/event-stream" };
        let mut request = self.client.request(method, &self.spec.url).header("Accept", accept);
        if let Some(session) = &*self.session.lock().unwrap() { request = request.header("Mcp-Session-Id", session); }
        if let Some(version) = &*self.version.lock().unwrap() { request = request.header("MCP-Protocol-Version", version); }
        request
    }
    pub fn set_version(&self, version: &str) -> Result<(), McpError> {
        if !["2024-11-05", "2025-03-26", "2025-06-18"].contains(&version) { return Err(self.error("unsupported MCP protocol version")); }
        *self.version.lock().unwrap() = Some(version.into());
        Ok(())
    }
    async fn server_message(&self, msg: &Value, changed: &tokio::sync::Notify) -> Result<bool, McpError> {
        if msg["jsonrpc"] != "2.0" { return Err(self.error("invalid JSON-RPC version")); }
        let Some(method) = msg.get("method") else { return Ok(false); };
        if !method.is_string() { return Err(self.error("invalid MCP method")); }
        if let Some(id) = msg.get("id") {
            let reply = if method == "ping" {
                serde_json::json!({"jsonrpc":"2.0", "id":id, "result":{}})
            } else {
                serde_json::json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601,"message":"Method not supported"}})
            };
            let accepted = self.request(reqwest::Method::POST).timeout(self.spec.timeout).json(&reply).send().await.map_err(|_| self.error("MCP server-request reply failed"))?;
            if accepted.status() != reqwest::StatusCode::ACCEPTED { return Err(self.error("MCP server-request reply expected HTTP 202")); }
        } else if method == "notifications/tools/list_changed" { changed.notify_one(); }
        Ok(true)
    }
    /// Cursor belongs only to this stream. Advance only after successful dispatch.
    /// Partial events on a broken connection are discarded and fetched again.
    async fn consume(&self, response: reqwest::Response, expected: Option<&Value>, changed: &tokio::sync::Notify, cursor: &mut Option<String>) -> Result<Option<Value>, McpError> {
        let content_type = response.headers().get("content-type").and_then(|h| h.to_str().ok()).unwrap_or("").split(';').next().unwrap_or("").trim();
        if content_type != "text/event-stream" { return Err(self.error("expected MCP event stream")); }
        let mut stream = response.bytes_stream();
        let mut decoder = Decoder::default();
        let previous = cursor.clone();
        loop {
            let chunk = match tokio::time::timeout(Duration::from_millis(self.spec.sse.idle_timeout_ms), stream.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                _ => return Ok(None), // EOF, network failure or idle timeout: GET-only recovery.
            };
            for byte in chunk {
                let Some(event) = decoder.byte(byte).map_err(|e| self.error(e))? else { continue; };
                if event.id.is_some() && event.id == previous { continue; }
                if !event.data.is_empty() {
                    let msg: Value = serde_json::from_slice(&event.data).map_err(|_| self.error("invalid MCP SSE JSON"))?;
                    if !self.server_message(&msg, changed).await? {
                        if expected.is_none() || msg.get("id") != expected { return Err(self.error("MCP stream response ID mismatch")); }
                        return self.result(msg).map(Some);
                    }
                }
                if let Some(id) = event.id { *cursor = if id.is_empty() { None } else { Some(id) }; }
            }
        }
    }
    async fn get(&self, cursor: Option<&str>) -> Result<reqwest::Response, McpError> {
        let mut request = self.request(reqwest::Method::GET);
        if let Some(id) = cursor { request = request.header("Last-Event-ID", id); }
        tokio::time::timeout(self.spec.timeout, request.send()).await
            .map_err(|_| self.error("MCP GET header timeout"))?
            .map_err(|_| self.error("MCP GET failed"))
    }
    /// Lifetime retry budget for the standalone stream; 405 is optional-feature refusal.
    pub async fn notifications(&self, changed: &tokio::sync::Notify) -> Result<(), McpError> {
        let mut cursor = None;
        let mut attempts = 0;
        loop {
            match self.get(cursor.as_deref()).await {
                Ok(response) if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED && cursor.is_none() => return Ok(()),
                Ok(response) if response.status().is_success() => { self.consume(response, None, changed, &mut cursor).await?; },
                Ok(response) if !response.status().is_server_error() => return Err(self.error(format!("MCP GET status {}", response.status().as_u16()))),
                _ => {},
            }
            if !self.spec.sse.resume || attempts >= self.spec.sse.max_attempts { return Err(self.error("MCP notification stream ended; recovery disabled or exhausted")); }
            attempts += 1;
            tokio::time::sleep(Duration::from_millis(self.spec.sse.retry_delay_ms)).await;
        }
    }
    pub async fn post(&self, payload: &Value, changed: &tokio::sync::Notify) -> Result<Option<Value>, McpError> {
        let body = serde_json::to_vec(payload).map_err(|_| self.error("invalid MCP payload"))?;
        if body.len() > MAX_FRAME_BYTES { return Err(self.error("outgoing MCP frame exceeds 16 MiB")); }
        let response = self.request(reqwest::Method::POST).timeout(self.spec.timeout).header("Content-Type", "application/json").body(body)
            .send().await.map_err(|_| self.error("MCP HTTP request failed (outcome may be unknown; not replayed)"))?;
        if !response.status().is_success() { return Err(self.error(format!("MCP HTTP status {}", response.status().as_u16()))); }
        if payload["method"] == "initialize" {
            if let Some(session) = response.headers().get("mcp-session-id") {
                let session = session.to_str().map_err(|_| self.error("invalid MCP session ID"))?;
                if session.is_empty() || session.len() > 1024 || !session.bytes().all(|b| b.is_ascii_graphic()) { return Err(self.error("invalid MCP session ID")); }
                *self.session.lock().unwrap() = Some(session.into());
            }
        }
        if payload.get("id").is_none() {
            return if response.status() == reqwest::StatusCode::ACCEPTED { Ok(None) } else { Err(self.error("MCP notification expected HTTP 202")) };
        }
        let content_type = response.headers().get("content-type").and_then(|h| h.to_str().ok()).unwrap_or("").split(';').next().unwrap_or("").trim();
        if content_type == "text/event-stream" {
            let mut cursor = None;
            if let Some(result) = self.consume(response, payload.get("id"), changed, &mut cursor).await? { return Ok(Some(result)); }
            if self.spec.sse.resume {
                for _ in 0..self.spec.sse.max_attempts {
                    let Some(id) = cursor.as_deref() else { break; };
                    tokio::time::sleep(Duration::from_millis(self.spec.sse.retry_delay_ms)).await;
                    let response = match self.get(Some(id)).await { Ok(response) => response, Err(_) => continue };
                    if response.status().is_server_error() { continue; }
                    if !response.status().is_success() { return Err(self.error(format!("MCP resume status {}", response.status().as_u16()))); }
                    if let Some(result) = self.consume(response, payload.get("id"), changed, &mut cursor).await? { return Ok(Some(result)); }
                }
            }
            return Err(self.error("MCP SSE ended without matching response; POST not replayed"));
        }
        if content_type != "application/json" { return Err(self.error("unsupported MCP response content type")); }
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| self.error("MCP HTTP stream failed"))?;
            if buffer.len().saturating_add(chunk.len()) > MAX_FRAME_BYTES { return Err(self.error("MCP response exceeds 16 MiB")); }
            buffer.extend_from_slice(&chunk);
        }
        let msg: Value = serde_json::from_slice(&buffer).map_err(|_| self.error("invalid MCP response JSON"))?;
        if msg.get("id") != payload.get("id") { return Err(self.error("MCP response ID mismatch")); }
        self.result(msg).map(Some)
    }
    fn result(&self, msg: Value) -> Result<Value, McpError> {
        if msg["jsonrpc"] != "2.0" { return Err(self.error("invalid JSON-RPC version")); }
        if msg.get("error").is_some() { return Err(McpError::Rpc { server: self.spec.name.clone(), message: msg["error"]["message"].as_str().unwrap_or("MCP remote error").into() }); }
        msg.get("result").cloned().ok_or_else(|| self.error("MCP response missing result"))
    }
    pub async fn close(&self) {
        if self.session.lock().unwrap().is_some() { let _ = self.request(reqwest::Method::DELETE).timeout(self.spec.timeout).send().await; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sse_framing_bom_multiline_crlf_and_limits() {
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for byte in b"\xef\xbb\xbfid: first\r\ndata: {\r\ndata: }\r\n\r\nid:\n\n" {
            if let Some(event) = decoder.byte(*byte).unwrap() { events.push(event); }
        }
        assert_eq!(events[0].id.as_deref(), Some("first"));
        assert_eq!(events[0].data, b"{\n}");
        assert_eq!(events[1].id.as_deref(), Some(""));
        let mut decoder = Decoder::default();
        let mut event = None;
        for byte in b"\xef\xbb\xbfdata: {}\n\n" { event = decoder.byte(*byte).unwrap().or(event); }
        assert_eq!(event.unwrap().data, b"{}");
        let mut decoder = Decoder { size: MAX_FRAME_BYTES, ..Default::default() };
        assert!(decoder.byte(b'x').is_err());
        assert!(SsePolicy { max_attempts: 101, ..Default::default() }.validate().is_err());
    }
}
