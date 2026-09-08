//! OpenAI-compatible adapter (Chat Completions, streaming). One adapter
//! covers OpenAI itself plus every compatible endpoint (DeepSeek, Groq,
//! Together, vLLM, …) — base URL + key + model are the whole difference.
//!
//! Mapping notes:
//! - context turns → `messages`; tool results become `role: "tool"`
//!   messages; assistant tool calls carry `tool_calls`
//! - streamed `delta.content` / `delta.tool_calls[].function.arguments`
//!   accumulate into final content while every delta is recorded as a
//!   [`TimedChunk`]
//! - `finish_reason`: `tool_calls` → ToolUse, `length` → MaxTokens,
//!   else EndTurn
//! - reasoning models' `delta.reasoning_content` (DeepSeek style) maps to
//!   thinking chunks

use std::time::Instant;

use async_trait::async_trait;
use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::turn::provider::{Provider, ProviderError, StepOutcome, StepRequest};
use rness_protocol::events::{
    AssistantMessage, ChunkDelta, ContentPart, Reasoning, StopReason, TimedChunk, Usage,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::sse::{SsePull, SseReader};

pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    /// Extra body fields merged into every request (temperature, etc.).
    extra_body: Value,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: OPENAI_BASE_URL.to_string(),
            api_key: Some(api_key.into()),
            model: model.into(),
            extra_body: json!({}),
        }
    }

    pub fn without_auth(mut self) -> Self {
        self.api_key = None;
        self
    }

    /// Any OpenAI-compatible endpoint (DeepSeek, Groq, vLLM, …).
    /// `base_url` includes the version prefix, e.g. `https://api.deepseek.com/v1`.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Merge extra fields into the request body (e.g. `{"temperature": 0}`).
    pub fn with_extra_body(mut self, extra: Value) -> Self {
        self.extra_body = extra;
        self
    }

    fn build_body(&self, request: &StepRequest<'_>) -> Result<Value, ProviderError> {
        if matches!(request.context.config.reasoning, Some(Reasoning::BudgetTokens { .. })) {
            return Err(ProviderError {
                message: "reasoning budget tokens are unsupported by OpenAI-compatible providers; use effort".into(),
                retryable: false,
            });
        }
        let mut messages = Vec::new();
        if !request.system.is_empty() {
            messages.push(json!({ "role": "system", "content": request.system }));
        }
        messages.extend(messages_from_context(request.context));

        let mut body = json!({
            "model": self.model,
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": messages,
        });
        if let Some(Reasoning::Effort { effort }) = &request.context.config.reasoning {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some(max_output_tokens) = request.context.config.max_output_tokens {
            body["max_tokens"] = json!(max_output_tokens);
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
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.input_schema,
                            },
                        })
                    })
                    .collect(),
            );
        }
        if let Value::Object(extra) = &self.extra_body {
            let obj = body.as_object_mut().expect("body is an object");
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        Ok(body)
    }
}

fn messages_from_context(context: &ModelContext) -> Vec<Value> {
    let mut messages = Vec::new();
    for turn in &context.turns {
        match turn {
            ModelTurn::User { content } => messages.push(json!({
                "role": "user",
                "content": text_of(content),
            })),
            ModelTurn::Assistant { content } => {
                let text = text_of(content);
                let tool_calls: Vec<Value> = content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::ToolUse { call, name, args } => Some(json!({
                            "id": call,
                            "type": "function",
                            "function": { "name": name, "arguments": args.to_string() },
                        })),
                        _ => None,
                    })
                    .collect();
                let mut msg = json!({ "role": "assistant" });
                if !text.is_empty() {
                    msg["content"] = json!(text);
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = Value::Array(tool_calls);
                }
                messages.push(msg);
            }
            ModelTurn::ToolResults { results } => {
                for r in results {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": r.call,
                        "content": r.output,
                    }));
                }
            }
        }
    }
    messages
}

fn text_of(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// -- stream accumulation ---------------------------------------------------

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
struct Accumulator {
    text: String,
    thinking: String,
    calls: Vec<PendingCall>,
    chunks: Vec<TimedChunk>,
    stop: Option<StopReason>,
    usage: Usage,
}

impl Accumulator {
    fn finish(self, model: &str) -> AssistantMessage {
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(ContentPart::Thinking { text: self.thinking, signature: None });
        }
        if !self.text.is_empty() {
            content.push(ContentPart::Text { text: self.text });
        }
        for call in self.calls {
            content.push(ContentPart::ToolUse {
                call: call.id,
                name: call.name,
                // Empty/absent args must replay as {}, never null.
                args: serde_json::from_str(&call.args)
                    .unwrap_or_else(|_| Value::Object(Default::default())),
            });
        }
        AssistantMessage {
            model: model.to_string(),
            content,
            stop: self.stop.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
            chunks: self.chunks,
        }
    }

    fn apply(&mut self, data: &str, at_ms: u64) -> Result<(), String> {
        let v: Value = serde_json::from_str(data).map_err(|e| format!("bad json: {e}"))?;
        if let Some(err) = v.get("error") {
            return Err(err["message"].as_str().unwrap_or("stream error").to_string());
        }
        if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage.input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
            self.usage.output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
        }
        let Some(choice) = v["choices"].get(0) else { return Ok(()) };

        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stop = Some(match reason {
                "tool_calls" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            });
        }

        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            self.chunks.push(TimedChunk { ms: at_ms, delta: ChunkDelta::Text { t: t.into() } });
        }
        if let Some(t) = delta["reasoning_content"].as_str().filter(|t| !t.is_empty()) {
            self.thinking.push_str(t);
            self.chunks
                .push(TimedChunk { ms: at_ms, delta: ChunkDelta::Thinking { t: t.into() } });
        }
        if let Some(tool_calls) = delta["tool_calls"].as_array() {
            for tc in tool_calls {
                let index = tc["index"].as_u64().unwrap_or(0) as usize;
                while self.calls.len() <= index {
                    self.calls.push(PendingCall::default());
                }
                let call = &mut self.calls[index];
                if let Some(id) = tc["id"].as_str() {
                    call.id = id.to_string();
                }
                if let Some(name) = tc["function"]["name"].as_str() {
                    call.name = name.to_string();
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    call.args.push_str(args);
                    self.chunks.push(TimedChunk {
                        ms: at_ms,
                        delta: ChunkDelta::ToolArgs { call: call.id.clone(), t: args.into() },
                    });
                }
            }
        }
        Ok(())
    }
}

// -- the provider ----------------------------------------------------------

#[async_trait]
impl Provider for OpenAiProvider {
    fn model(&self) -> &str {
        &self.model
    }

    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        let started = Instant::now();
        let body = match self.build_body(&request) {
            Ok(body) => body,
            Err(error) => return StepOutcome::Failed { error, partial: vec![] },
        };

        let mut http_request = self.client.post(format!("{}/chat/completions", self.base_url)).json(&body);
        if let Some(key) = &self.api_key { http_request = http_request.bearer_auth(key); }
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
            r = http_request.send() => r,
        };

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                return StepOutcome::Failed {
                    error: ProviderError { message: format!("transport: {e}"), retryable: true },
                    partial: vec![],
                }
            }
        };

        let status = response.status();
        if !status.is_success() {
            let retryable = status.as_u16() == 429 || status.is_server_error();
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or_else(|| format!("http {status}"));
            return StepOutcome::Failed {
                error: ProviderError { message, retryable },
                partial: vec![],
            };
        }

        let mut reader = SseReader::new(response.bytes_stream());
        let mut acc = Accumulator::default();
        let mut emitted = 0usize;
        loop {
            match reader.pull(cancel).await {
                SsePull::Event { data, .. } => {
                    if data.trim() == "[DONE]" {
                        return StepOutcome::Committed(acc.finish(&self.model));
                    }
                    let at_ms = started.elapsed().as_millis() as u64;
                    if let Err(message) = acc.apply(&data, at_ms) {
                        return StepOutcome::Failed {
                            error: ProviderError { message, retryable: true },
                            partial: acc.chunks,
                        };
                    }
                    if let Some(sink) = request.on_delta {
                        for chunk in &acc.chunks[emitted..] {
                            sink(&chunk.delta);
                        }
                        emitted = acc.chunks.len();
                    }
                }
                SsePull::Done => return StepOutcome::Committed(acc.finish(&self.model)),
                SsePull::Cancelled => return StepOutcome::Cancelled { partial: acc.chunks },
                SsePull::Error(e) => {
                    return StepOutcome::Failed {
                        error: ProviderError { message: format!("stream: {e}"), retryable: true },
                        partial: acc.chunks,
                    }
                }
            }
        }
    }
}
