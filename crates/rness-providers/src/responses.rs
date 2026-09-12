//! ChatGPT-backend Responses API adapter — uses a ChatGPT Plus/Pro
//! subscription ("Sign in with ChatGPT") instead of an API key.
//!
//! Wire notes (matching Codex CLI's client, verified against pi and
//! opencode):
//! - endpoint `POST {base}/codex/responses`, SSE stream mandatory
//! - `store: false` is required (the backend rejects stored responses)
//! - system prompt goes in top-level `instructions`, not an input item
//! - assistant history replays as `function_call` / output `message`
//!   items; tool results as `function_call_output`
//! - `response.completed` may carry an EMPTY output array — accumulate
//!   `response.output_item.done` items as the source of truth
//! - 401 → refresh token once and retry

use std::time::Instant;

use async_trait::async_trait;
use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::turn::provider::{Provider, ProviderError, StepOutcome, StepRequest};
use rness_protocol::events::{
    AssistantMessage, ChunkDelta, ContentPart, Reasoning, StopReason, TimedChunk, Usage,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::auth::openai::{CodexCredential, CodexCredentialSource};
use crate::auth::AuthError;
use crate::sse::{SsePull, SseReader};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const ORIGINATOR: &str = "rness";

pub struct ResponsesProvider {
    images: Option<(std::sync::Arc<rness_engine::images::ImageStore>, rness_engine::images::ImagePolicy)>,
    idle_timeout: Option<std::time::Duration>,
    client: reqwest::Client,
    base_url: String,
    source: CodexCredentialSource,
    model: String,
}

impl ResponsesProvider {
    pub fn new(source: CodexCredentialSource, model: impl Into<String>) -> Self {
        Self {
            images: None,
            idle_timeout: crate::sse::DEFAULT_IDLE_TIMEOUT,
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            source,
            model: model.into(),
        }
    }

    pub fn with_images(mut self, store: std::sync::Arc<rness_engine::images::ImageStore>, policy: rness_engine::images::ImagePolicy) -> Self {
        self.images = Some((store, policy));
        self
    }

    pub fn with_stream_idle_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Point at a mock server (tests).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn build_body(&self, request: &StepRequest<'_>) -> Result<Value, ProviderError> {
        crate::validate_image_roles(request.context, self.images.is_some())?;
        let images = self.images.as_ref().map(|(store, policy)| (store.clone(), store.effective_request_policy(policy)));
        let projected = images.as_ref().map(|(store, policy)| store.project_request(request.context, policy)).transpose().map_err(crate::image_error)?;
        let request = StepRequest { context: projected.as_ref().unwrap_or(request.context), system: request.system, tools: request.tools, on_delta: request.on_delta };
        if matches!(request.context.config.reasoning, Some(Reasoning::BudgetTokens { .. })) {
            return Err(ProviderError { code: "PROVIDER", retry_after: None,
                message: "reasoning budget tokens are unsupported by OpenAI Responses; use effort".into(),
                retryable: false,
            });
        }
        let mut body = json!({
            "model": self.model,
            "instructions": request.system,
            "input": input_items(request.context, images.as_ref())?,
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "include": ["reasoning.encrypted_content"],
            "reasoning": { "summary": "auto" },
        });
        if let Some(Reasoning::Effort { effort }) = &request.context.config.reasoning {
            body["reasoning"]["effort"] = json!(effort);
        }
        if let Some(max_output_tokens) = request.context.config.max_output_tokens {
            body["max_output_tokens"] = json!(max_output_tokens);
        }
        if let Some(temperature) = request.context.config.temperature {
            body["temperature"] = json!(temperature);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                            "strict": false,
                        })
                    })
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
        }
        Ok(body)
    }

    fn request_for(&self, credential: &CodexCredential, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(format!("{}/codex/responses", self.base_url))
            .bearer_auth(&credential.access_token)
            .header("openai-beta", "responses=experimental")
            .header("originator", ORIGINATOR)
            .header("accept", "text/event-stream")
            .json(body);
        if let Some(account) = &credential.account_id {
            req = req.header("chatgpt-account-id", account);
        }
        req
    }
}

// -- context → input items -------------------------------------------------

fn input_items(context: &ModelContext, images: Option<&(std::sync::Arc<rness_engine::images::ImageStore>, rness_engine::images::ImagePolicy)>) -> Result<Vec<Value>, ProviderError> {
    let mut items = Vec::new();
    for turn in &context.turns {
        match turn {
            ModelTurn::User { content } => {
                let mut parts = Vec::new();
                for part in content {
                    match part {
                        ContentPart::Text { text } if !text.is_empty() => parts.push(json!({"type":"input_text", "text":text})),
                        ContentPart::Image { attachment } => {
                            let (store, policy) = images.ok_or_else(|| crate::image_error("image store not configured".into()))?;
                            let (mime, data) = store.request_image(attachment, policy).map_err(crate::image_error)?;
                            use base64::Engine;
                            parts.push(json!({"type":"input_image", "image_url":format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data))}));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    items.push(json!({"type": "message", "role": "user", "content": parts}));
                }
            }
            ModelTurn::Assistant { content } => {
                let text: Vec<Value> = content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } if !text.is_empty() => {
                            Some(json!({"type": "output_text", "text": text}))
                        }
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": text,
                    }));
                }
                for p in content {
                    if let ContentPart::ToolUse { call, name, args } = p {
                        let arguments = if args.is_null() { "{}".to_string() } else { args.to_string() };
                        items.push(json!({
                            "type": "function_call",
                            "call_id": call,
                            "name": name,
                            "arguments": arguments,
                        }));
                    }
                }
            }
            ModelTurn::ToolResults { results } => {
                for r in results {
                    let output = if r.output.is_empty() && r.is_error {
                        "(tool reported an error)"
                    } else {
                        &r.output
                    };
                    let output = if r.content.iter().any(|p| matches!(p, rness_protocol::events::ToolResultContentPart::Image { .. })) {
                        let mut parts = Vec::new();
                        for part in r.effective_content() {
                            match part {
                                rness_protocol::events::ToolResultContentPart::Text { text } => parts.push(json!({"type":"input_text", "text":text})),
                                rness_protocol::events::ToolResultContentPart::Image { attachment } => {
                                    let (store, policy) = images.ok_or_else(|| crate::image_error("image store not configured".into()))?;
                                    let (mime, data) = store.request_image(&attachment, policy).map_err(crate::image_error)?;
                                    use base64::Engine;
                                    parts.push(json!({"type":"input_image", "image_url":format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data))}));
                                }
                            }
                        }
                        json!(parts)
                    } else { json!(output) };
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": r.call,
                        "output": output,
                    }));
                }
            }
        }
    }
    Ok(items)
}

// -- stream accumulation ---------------------------------------------------

#[derive(Default)]
struct Accumulator {
    /// Finished output items (`response.output_item.done`), model order.
    items: Vec<Value>,
    chunks: Vec<TimedChunk>,
    usage: Usage,
    status: Option<String>,
    error: Option<String>,
    done: bool,
}

impl Accumulator {
    fn apply(&mut self, event: &str, data: &str, at_ms: u64) {
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return, // tolerate unknown/empty payloads
        };
        match event {
            "response.output_text.delta" => {
                if let Some(t) = v["delta"].as_str().filter(|t| !t.is_empty()) {
                    self.chunks
                        .push(TimedChunk { ms: at_ms, delta: ChunkDelta::Text { t: t.into() } });
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(t) = v["delta"].as_str().filter(|t| !t.is_empty()) {
                    self.chunks
                        .push(TimedChunk { ms: at_ms, delta: ChunkDelta::Thinking { t: t.into() } });
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(t) = v["delta"].as_str().filter(|t| !t.is_empty()) {
                    let call = v["item_id"].as_str().unwrap_or_default().to_string();
                    self.chunks
                        .push(TimedChunk { ms: at_ms, delta: ChunkDelta::ToolArgs { call, t: t.into() } });
                }
            }
            "response.output_item.done" => {
                if !v["item"].is_null() {
                    self.items.push(v["item"].clone());
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = &v["response"];
                self.status = response["status"].as_str().map(String::from).or_else(|| {
                    Some(if event.ends_with("incomplete") { "incomplete" } else { "completed" }.into())
                });
                let usage = &response["usage"];
                if !usage.is_null() {
                    self.usage.input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
                    self.usage.output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
                }
                // The completed payload's output array can be empty; only
                // trust it when we accumulated nothing.
                if self.items.is_empty() {
                    if let Some(output) = response["output"].as_array() {
                        self.items = output.clone();
                    }
                }
                self.done = true;
            }
            "response.failed" => {
                let message = v["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("response failed")
                    .to_string();
                self.error = Some(message);
                self.done = true;
            }
            "error" => {
                self.error =
                    Some(v["message"].as_str().unwrap_or("stream error").to_string());
                self.done = true;
            }
            _ => {}
        }
    }

    fn finish(self, model: &str) -> AssistantMessage {
        let mut content = Vec::new();
        let mut saw_tool_use = false;
        for item in &self.items {
            match item["type"].as_str().unwrap_or_default() {
                "message" => {
                    for c in item["content"].as_array().into_iter().flatten() {
                        match c["type"].as_str().unwrap_or_default() {
                            "output_text" => {
                                if let Some(t) = c["text"].as_str().filter(|t| !t.is_empty()) {
                                    content.push(ContentPart::Text { text: t.into() });
                                }
                            }
                            "refusal" => {
                                if let Some(r) = c["refusal"].as_str().filter(|r| !r.is_empty()) {
                                    content.push(ContentPart::Text {
                                        text: format!("[Refused] {r}"),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "reasoning" => {
                    let text: String = item["summary"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|s| s["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        content.push(ContentPart::Thinking { text, signature: None });
                    }
                }
                "function_call" => {
                    saw_tool_use = true;
                    let args = item["arguments"].as_str().unwrap_or("{}");
                    content.push(ContentPart::ToolUse {
                        call: item["call_id"].as_str().unwrap_or_default().into(),
                        name: item["name"].as_str().unwrap_or_default().into(),
                        args: serde_json::from_str(args).unwrap_or(json!({})),
                    });
                }
                _ => {}
            }
        }
        let stop = if saw_tool_use {
            StopReason::ToolUse
        } else if self.status.as_deref() == Some("incomplete") {
            StopReason::MaxTokens
        } else {
            StopReason::EndTurn
        };
        AssistantMessage {
            model: model.to_string(),
            content,
            stop,
            usage: self.usage,
            chunks: self.chunks,
        }
    }
}

// -- the provider ----------------------------------------------------------

#[async_trait]
impl Provider for ResponsesProvider {
    fn configure_images(&mut self, store: std::sync::Arc<rness_engine::images::ImageStore>, policy: rness_engine::images::ImagePolicy) {
        self.images = Some((store, policy));
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn supports_max_output_tokens(&self) -> bool { false }

    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        let started = Instant::now();
        let body = match self.build_body(&request) {
            Ok(body) => body,
            Err(error) => return StepOutcome::Failed { error, partial: vec![] },
        };

        let mut credential = match self.source.resolve().await {
            Ok(c) => c,
            Err(e) => {
                return StepOutcome::Failed {
                    error: ProviderError { code: "PROVIDER", retry_after: None,
                        message: auth_message(e),
                        retryable: false,
                    },
                    partial: vec![],
                }
            }
        };

        let mut refreshed = false;
        let response = loop {
            if let Err(error) = rness_engine::turn::provider::capture_wire(&body).await {
                return StepOutcome::Failed { error, partial: vec![] };
            }
            let sent = tokio::select! {
                biased;
                _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
                _ = crate::sse::idle_deadline(self.idle_timeout) => return StepOutcome::Failed { error: ProviderError { code: "TIMEOUT", retry_after: None, message: "TIMEOUT: waiting for provider response".into(), retryable: true }, partial: vec![] },
                r = self.request_for(&credential, &body).send() => r,
            };
            let response = match sent {
                Ok(r) => r,
                Err(e) => {
                    return StepOutcome::Failed {
                        error: ProviderError { code: "PROVIDER", retry_after: None, message: format!("transport: {e}"), retryable: true },
                        partial: vec![],
                    }
                }
            };

            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                match self.source.handle_unauthorized().await {
                    Ok(c) => {
                        credential = c;
                        continue;
                    }
                    Err(e) => {
                        return StepOutcome::Failed {
                            error: ProviderError { code: "PROVIDER", retry_after: None, message: auth_message(e), retryable: false },
                            partial: vec![],
                        }
                    }
                }
            }
            if !status.is_success() {
                let retry_after = crate::sse::retry_after(response.headers());
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let text = response.text().await.unwrap_or_default();
                let message = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| {
                        v["error"]["message"]
                            .as_str()
                            .or(v["detail"].as_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| format!("http {status}"));
                return StepOutcome::Failed {
                    error: ProviderError { code: crate::request_error_code(status.as_u16(), &text), retry_after, message, retryable },
                    partial: vec![],
                };
            }
            break response;
        };

        let mut reader = SseReader::new(response.bytes_stream(), self.idle_timeout);
        let mut acc = Accumulator::default();
        let mut emitted = 0usize;
        loop {
            match reader.pull(cancel).await {
                SsePull::Event { event, data } => {
                    if let Some(error) = crate::stream_overflow(&event, &data) {
                        return StepOutcome::Failed { error, partial: acc.chunks };
                    }
                    let at_ms = started.elapsed().as_millis() as u64;
                    acc.apply(&event, &data, at_ms);
                    if let Some(sink) = request.on_delta {
                        for chunk in &acc.chunks[emitted..] {
                            sink(&chunk.delta);
                        }
                        emitted = acc.chunks.len();
                    }
                    if acc.done {
                        if let Some(message) = acc.error {
                            return StepOutcome::Failed {
                                error: ProviderError { code: "PROVIDER", retry_after: None, message, retryable: true },
                                partial: acc.chunks,
                            };
                        }
                        return StepOutcome::Committed(acc.finish(&self.model));
                    }
                }
                SsePull::Timeout => return StepOutcome::Failed { error: ProviderError { code: "TIMEOUT", retry_after: None, message: "TIMEOUT: provider stream inactivity timeout".into(), retryable: true }, partial: acc.chunks },
                SsePull::Done => {
                    if let Some(message) = acc.error {
                        return StepOutcome::Failed {
                            error: ProviderError { code: "PROVIDER", retry_after: None, message, retryable: true },
                            partial: acc.chunks,
                        };
                    }
                    return StepOutcome::Committed(acc.finish(&self.model));
                }
                SsePull::Cancelled => return StepOutcome::Cancelled { partial: acc.chunks },
                SsePull::Error(e) => {
                    return StepOutcome::Failed {
                        error: ProviderError { code: "PROVIDER", retry_after: None, message: format!("stream: {e}"), retryable: true },
                        partial: acc.chunks,
                    }
                }
            }
        }
    }
}

fn auth_message(e: AuthError) -> String {
    match e {
        AuthError::NoCredentials => {
            "no ChatGPT credentials: run `rness auth login --provider openai-chatgpt`".to_string()
        }
        other => format!("auth: {other}"),
    }
}
