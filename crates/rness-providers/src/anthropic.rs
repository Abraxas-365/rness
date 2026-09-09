//! Anthropic Messages API adapter (streaming).
//!
//! Implements the engine's [`Provider`] seam over `POST /v1/messages`
//! with `stream: true`. The adapter:
//! - builds the request from the replay-derived [`StepRequest`] (context
//!   turns map 1:1 onto Anthropic messages; tool results become
//!   `tool_result` user messages)
//! - accumulates streamed deltas into final content while recording every
//!   delta as a [`TimedChunk`] (traceability: the committed message embeds
//!   its exact stream)
//! - maps HTTP/stream failures onto retryable/fatal [`ProviderError`]s
//!   (429/5xx/transport → retryable; 4xx → fatal)
//! - honors cancellation between deltas, returning the partial stream

use std::time::Instant;

use async_trait::async_trait;
use rness_engine::turn::provider::{Provider, ProviderError, StepOutcome, StepRequest};
use rness_engine::session::projection::ModelTurn;
use rness_protocol::events::{
    AssistantMessage, ChunkDelta, ContentPart, Reasoning, StopReason, TimedChunk, Usage,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::auth::{AuthError, Credential, CredentialSource};
use crate::sse::{SsePull, SseReader};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;

// OAuth attribution headers (Claude Code — subscription billing
// requires the request to look like Claude Code).
const OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
const OAUTH_USER_AGENT: &str = "claude-cli/2.1.195 (external, sdk-cli)";
const OAUTH_BILLING_SYSTEM: &str =
    "x-anthropic-billing-header: cc_version=2.1.195; cc_entrypoint=cli; cch=00000;";

/// How the provider authenticates each request.
enum Auth {
    /// Fixed API key (tests, explicit key).
    ApiKey(String),
    /// Resolved per request: env > stored key > OAuth with auto-refresh.
    Source(CredentialSource),
}

pub struct AnthropicProvider {
    idle_timeout: Option<std::time::Duration>,
    client: reqwest::Client,
    base_url: String,
    auth: Auth,
    model: String,
    max_tokens: u32,
}

impl AnthropicProvider {
    pub fn with_stream_idle_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }

    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            idle_timeout: crate::sse::DEFAULT_IDLE_TIMEOUT,
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth: Auth::ApiKey(api_key.into()),
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Resolve credentials per request from `source` (API key or OAuth
    /// Pro/Max tokens with transparent refresh).
    pub fn with_credentials(source: CredentialSource, model: impl Into<String>) -> Self {
        Self {
            idle_timeout: crate::sse::DEFAULT_IDLE_TIMEOUT,
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth: Auth::Source(source),
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Point at a different endpoint (tests, proxies).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    async fn credential(&self) -> Result<Credential, AuthError> {
        match &self.auth {
            Auth::ApiKey(key) => Ok(Credential::ApiKey(key.clone())),
            Auth::Source(source) => source.resolve().await,
        }
    }

    fn build_body(&self, request: &StepRequest<'_>, oauth: bool) -> Result<Value, ProviderError> {
        let max_tokens = request.context.config.max_output_tokens.unwrap_or(self.max_tokens);
        if let Some(Reasoning::BudgetTokens { tokens }) = &request.context.config.reasoning {
            if *tokens < 1024 || *tokens >= max_tokens {
                return Err(ProviderError { code: "PROVIDER", retry_after: None,
                    message: format!(
                        "Anthropic reasoning budget must satisfy 1024 <= budget < max_output_tokens ({max_tokens})"
                    ),
                    retryable: false,
                });
            }
        }
        let mut body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "stream": true,
            "messages": messages_from_context(request.context),
        });
        if let Some(reasoning) = &request.context.config.reasoning {
            match reasoning {
                Reasoning::BudgetTokens { tokens } => {
                    body["thinking"] = json!({ "type": "enabled", "budget_tokens": tokens });
                }
                Reasoning::Effort { effort } => {
                    body["thinking"] = json!({ "type": "adaptive" });
                    body["output_config"] = json!({ "effort": effort });
                }
            }
        }
        if let Some(temperature) = request.context.config.temperature {
            body["temperature"] = json!(temperature);
        }
        // OAuth (subscription) requests must lead the system prompt with
        // the billing attribution block.
        let mut system_blocks = Vec::new();
        if oauth {
            system_blocks.push(json!({ "type": "text", "text": OAUTH_BILLING_SYSTEM }));
        }
        if !request.system.is_empty() {
            system_blocks.push(json!({ "type": "text", "text": request.system }));
        }
        if !system_blocks.is_empty() {
            body["system"] = Value::Array(system_blocks);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.input_schema,
                        })
                    })
                    .collect(),
            );
        }
        Ok(body)
    }

    fn request_for(&self, credential: &Credential, body: &Value) -> reqwest::RequestBuilder {
        let base = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", API_VERSION)
            .json(body);
        match credential {
            Credential::ApiKey(key) => base.header("x-api-key", key),
            Credential::OAuth(token) => base
                .bearer_auth(token)
                .header("anthropic-beta", OAUTH_BETA)
                .header("User-Agent", OAUTH_USER_AGENT)
                .header("anthropic-dangerous-direct-browser-access", "true")
                .header("x-app", "cli"),
        }
    }
}

/// Map the replay-derived context onto Anthropic `messages`.
fn messages_from_context(context: &rness_engine::session::projection::ModelContext) -> Vec<Value> {
    let mut messages = Vec::new();
    for turn in &context.turns {
        match turn {
            ModelTurn::User { content } => messages.push(json!({
                "role": "user",
                "content": parts_to_json(content),
            })),
            ModelTurn::Assistant { content } => messages.push(json!({
                "role": "assistant",
                "content": parts_to_json(content),
            })),
            ModelTurn::ToolResults { results } => messages.push(json!({
                "role": "user",
                "content": results
                    .iter()
                    .map(|r| json!({
                        "type": "tool_result",
                        "tool_use_id": r.call,
                        "content": r.output,
                        "is_error": r.is_error,
                    }))
                    .collect::<Vec<_>>(),
            })),
        }
    }
    messages
}

fn parts_to_json(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => json!({ "type": "text", "text": text }),
            ContentPart::Thinking { text, signature } => json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature.as_deref().unwrap_or(""),
            }),
            ContentPart::ToolUse { call, name, args } => json!({
                "type": "tool_use",
                "id": call,
                "name": name,
                "input": args,
            }),
        })
        .collect()
}

// -- stream accumulation ---------------------------------------------------

/// One in-flight content block being assembled from deltas.
enum Block {
    Text(String),
    Thinking { text: String, signature: String },
    ToolUse { call: String, name: String, args_json: String },
}

#[derive(Default)]
struct Accumulator {
    blocks: Vec<Block>,
    chunks: Vec<TimedChunk>,
    stop: Option<StopReason>,
    usage: Usage,
}

impl Accumulator {
    fn finish(self, model: &str) -> AssistantMessage {
        let content = self
            .blocks
            .into_iter()
            .map(|b| match b {
                Block::Text(text) => ContentPart::Text { text },
                Block::Thinking { text, signature } => ContentPart::Thinking {
                    text,
                    signature: (!signature.is_empty()).then_some(signature),
                },
                Block::ToolUse { call, name, args_json } => ContentPart::ToolUse {
                    call,
                    name,
                    // No-arg calls stream no input_json_delta: empty buffer
                    // must replay as {} — the API rejects null input.
                    args: serde_json::from_str(&args_json)
                        .unwrap_or_else(|_| Value::Object(Default::default())),
                },
            })
            .collect();
        AssistantMessage {
            model: model.to_string(),
            content,
            stop: self.stop.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
            chunks: self.chunks,
        }
    }

    /// Apply one SSE event. Returns an error message on malformed data.
    fn apply(&mut self, event: &str, data: &str, at_ms: u64) -> Result<(), String> {
        let v: Value = serde_json::from_str(data).map_err(|e| format!("bad json: {e}"))?;
        match event {
            "message_start" => {
                let usage = &v["message"]["usage"];
                self.usage.input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
                self.usage.cache_read_tokens =
                    usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                self.usage.cache_write_tokens =
                    usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let block = &v["content_block"];
                match block["type"].as_str().unwrap_or("") {
                    "text" => self.blocks.push(Block::Text(String::new())),
                    "thinking" => self
                        .blocks
                        .push(Block::Thinking { text: String::new(), signature: String::new() }),
                    "tool_use" => self.blocks.push(Block::ToolUse {
                        call: block["id"].as_str().unwrap_or("").to_string(),
                        name: block["name"].as_str().unwrap_or("").to_string(),
                        args_json: String::new(),
                    }),
                    other => return Err(format!("unknown content block type '{other}'")),
                }
            }
            "content_block_delta" => {
                let delta = &v["delta"];
                let last = self.blocks.last_mut().ok_or("delta before block_start")?;
                match (delta["type"].as_str().unwrap_or(""), last) {
                    ("text_delta", Block::Text(text)) => {
                        let t = delta["text"].as_str().unwrap_or("");
                        text.push_str(t);
                        self.chunks.push(TimedChunk {
                            ms: at_ms,
                            delta: ChunkDelta::Text { t: t.to_string() },
                        });
                    }
                    ("thinking_delta", Block::Thinking { text, .. }) => {
                        let t = delta["thinking"].as_str().unwrap_or("");
                        text.push_str(t);
                        self.chunks.push(TimedChunk {
                            ms: at_ms,
                            delta: ChunkDelta::Thinking { t: t.to_string() },
                        });
                    }
                    ("input_json_delta", Block::ToolUse { call, args_json, .. }) => {
                        let t = delta["partial_json"].as_str().unwrap_or("");
                        args_json.push_str(t);
                        self.chunks.push(TimedChunk {
                            ms: at_ms,
                            delta: ChunkDelta::ToolArgs { call: call.clone(), t: t.to_string() },
                        });
                    }
                    // The thinking block's integrity signature — required
                    // by the API when the block is replayed.
                    ("signature_delta", Block::Thinking { signature, .. }) => {
                        signature.push_str(delta["signature"].as_str().unwrap_or(""));
                    }
                    // Unknown deltas: ignore, forward-tolerant.
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(match reason {
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        _ => StopReason::EndTurn,
                    });
                }
                if let Some(out) = v["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = out;
                }
            }
            "error" => {
                return Err(v["error"]["message"]
                    .as_str()
                    .unwrap_or("stream error")
                    .to_string());
            }
            // ping, content_block_stop, message_stop: nothing to record.
            _ => {}
        }
        Ok(())
    }
}

// -- the provider ----------------------------------------------------------

#[async_trait]
impl Provider for AnthropicProvider {
    fn model(&self) -> &str {
        &self.model
    }

    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        let started = Instant::now();

        let mut credential = match self.credential().await {
            Ok(c) => c,
            Err(e) => {
                let retryable = !matches!(e, AuthError::NoCredentials);
                return StepOutcome::Failed {
                    error: ProviderError { code: "PROVIDER", retry_after: None, message: e.to_string(), retryable },
                    partial: vec![],
                };
            }
        };
        let mut refreshed_once = false;

        loop {
            let body = match self.build_body(&request, matches!(credential, Credential::OAuth(_))) {
                Ok(body) => body,
                Err(error) => return StepOutcome::Failed { error, partial: vec![] },
            };
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
                _ = crate::sse::idle_deadline(self.idle_timeout) => return StepOutcome::Failed { error: ProviderError { code: "TIMEOUT", retry_after: None, message: "TIMEOUT: waiting for provider response".into(), retryable: true }, partial: vec![] },
                r = self.request_for(&credential, &body).send() => r,
            };

            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    return StepOutcome::Failed {
                        error: ProviderError { code: "PROVIDER", retry_after: None,
                            message: format!("transport: {e}"),
                            retryable: true,
                        },
                        partial: vec![],
                    }
                }
            };

            let status = response.status();
            if !status.is_success() {
                // 401 with OAuth: force-refresh once and retry.
                if status.as_u16() == 401 && !refreshed_once {
                    if let (Auth::Source(source), Credential::OAuth(_)) =
                        (&self.auth, &credential)
                    {
                        if let Ok(new) = source.handle_unauthorized().await {
                            credential = new;
                            refreshed_once = true;
                            continue;
                        }
                    }
                }
                let retry_after = crate::sse::retry_after(response.headers());
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let body = response.text().await.unwrap_or_default();
                let message = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("http {status}"));
                return StepOutcome::Failed {
                    error: ProviderError { code: "HTTP", retry_after, message, retryable },
                    partial: vec![],
                };
            }

            let mut reader = SseReader::new(response.bytes_stream(), self.idle_timeout);
            let mut acc = Accumulator::default();
            let mut emitted = 0usize;
            loop {
                match reader.pull(cancel).await {
                    SsePull::Event { event, data } => {
                        let at_ms = started.elapsed().as_millis() as u64;
                        if let Err(message) = acc.apply(&event, &data, at_ms) {
                            return StepOutcome::Failed {
                                error: ProviderError { code: "PROVIDER", retry_after: None, message, retryable: true },
                                partial: acc.chunks,
                            };
                        }
                        if let Some(sink) = request.on_delta {
                            for chunk in &acc.chunks[emitted..] {
                                sink(&chunk.delta);
                            }
                            emitted = acc.chunks.len();
                        }
                        if event == "message_stop" { return StepOutcome::Committed(acc.finish(&self.model)); }
                    }
                    SsePull::Timeout => return StepOutcome::Failed { error: ProviderError { code: "TIMEOUT", retry_after: None, message: "TIMEOUT: provider stream inactivity timeout".into(), retryable: true }, partial: acc.chunks },
                SsePull::Done => return StepOutcome::Failed {
                        error: ProviderError { code: "PROVIDER", retry_after: None, message: "anthropic: stream ended before message_stop (connection closed or incomplete response)".into(), retryable: true },
                        partial: acc.chunks,
                    },
                    SsePull::Cancelled => {
                        return StepOutcome::Cancelled { partial: acc.chunks }
                    }
                    SsePull::Error(e) => {
                        return StepOutcome::Failed {
                            error: ProviderError { code: "PROVIDER", retry_after: None,
                                message: format!("stream: {e}"),
                                retryable: true,
                            },
                            partial: acc.chunks,
                        }
                    }
                }
            }
        }
    }
}
