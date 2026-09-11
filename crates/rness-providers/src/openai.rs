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
    images: Option<(std::sync::Arc<rness_engine::images::ImageStore>, rness_engine::images::ImagePolicy)>,
    idle_timeout: Option<std::time::Duration>,
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
            images: None,
            idle_timeout: crate::sse::DEFAULT_IDLE_TIMEOUT,
            client: reqwest::Client::new(),
            base_url: OPENAI_BASE_URL.to_string(),
            api_key: Some(api_key.into()),
            model: model.into(),
            extra_body: json!({}),
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

    async fn reuse_image_uploads(&self, body: &mut Value, cancel: &CancellationToken, rejected: &[(String, String)], used: &mut Vec<(String, String)>) -> Result<(), ProviderError> {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        static UPLOADS: std::sync::OnceLock<tokio::sync::Mutex<std::collections::HashMap<String, (String, u64)>>> = std::sync::OnceLock::new();
        let Some((store, policy)) = &self.images else { return Ok(()); };
        if !store.effective_request_policy(policy).deepseek_files { return Ok(()); }
        let url = reqwest::Url::parse(&self.base_url).map_err(|e| crate::image_error(e.to_string()))?;
        if !matches!(url.host_str(), Some("api.deepseek.com" | "127.0.0.1" | "localhost")) { return Ok(()); }
        let key = self.api_key.as_ref().ok_or_else(|| crate::image_error("DeepSeek Files requires API-key authentication".into()))?;
        let mut paths = Vec::new();
        for (mi, message) in body["messages"].as_array().into_iter().flatten().enumerate() {
            for (ci, part) in message["content"].as_array().into_iter().flatten().enumerate() {
                if part["type"] == "image_url" { paths.push(format!("/messages/{mi}/content/{ci}")); }
            }
        }
        let mut protected = Vec::new();
        for path in paths {
            let data_url = body.pointer(&path).and_then(|p| p["image_url"]["url"].as_str()).ok_or_else(|| crate::image_error("missing image URL".into()))?;
            let (prefix, encoded) = data_url.split_once(";base64,").ok_or_else(|| crate::image_error("invalid inline image URL".into()))?;
            let mime = prefix.strip_prefix("data:").ok_or_else(|| crate::image_error("invalid inline image MIME".into()))?;
            let cache_key = format!("{:x}", Sha256::digest(serde_json::to_vec(&(&self.base_url, key, data_url)).map_err(|e| crate::image_error(e.to_string()))?));
            let mut cache = tokio::select! {
                _ = cancel.cancelled() => return Err(crate::image_error("image upload cancelled".into())),
                cache = UPLOADS.get_or_init(Default::default).lock() => cache,
            };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| crate::image_error(e.to_string()))?.as_secs();
            cache.retain(|_, (_, expires)| *expires > now.saturating_add(60));
            let mut cached = cache.get(&cache_key).cloned().or(store.cached_upload(&cache_key, now).map_err(crate::image_error)?);
            if cached.as_ref().is_some_and(|(id, _)| rejected.contains(&(cache_key.clone(), id.clone()))) {
                cache.remove(&cache_key);
                store.cache_upload(&cache_key, "expired", 0).map_err(crate::image_error)?;
                cached = None;
            }
            if let Some((id, _)) = &cached {
                let response = tokio::select! {
                    _ = cancel.cancelled() => return Err(crate::image_error("image lookup cancelled".into())),
                    response = self.client.get(format!("{}/files/{}", self.base_url.trim_end_matches('/'), id)).bearer_auth(key).timeout(std::time::Duration::from_secs(30)).send() => response.map_err(|e| crate::image_error(e.to_string()))?,
                };
                if response.status() == reqwest::StatusCode::NOT_FOUND { cache.remove(&cache_key); store.cache_upload(&cache_key, "expired", 0).map_err(crate::image_error)?; cached = None; }
                else { response.error_for_status().map_err(|e| crate::image_error(e.to_string()))?; }
            }
            let id = if let Some((id, _)) = cached { id } else {
                let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|e| crate::image_error(e.to_string()))?;
                let endpoint = format!("{}/files", self.base_url.trim_end_matches('/'));
                let scope = crate::upload_scope(&endpoint, key);
                let value = crate::upload_with_quota_recovery(store, &scope, &protected, || {
                    let part = reqwest::multipart::Part::bytes(bytes.clone()).file_name("image").mime_str(mime).map_err(|e| crate::image_error(e.to_string()))?;
                    let form = reqwest::multipart::Form::new().part("file", part).text("purpose", "user_data").text("expires_after[anchor]", "created_at").text("expires_after[seconds]", "3600");
                    Ok(self.client.post(&endpoint).bearer_auth(key).multipart(form))
                }, |id| self.client.delete(format!("{endpoint}/{id}")).bearer_auth(key), cancel).await?;
                let id = value["id"].as_str().filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')).ok_or_else(|| crate::image_error("invalid uploaded file ID".into()))?.to_owned();
                let expires = value["expires_at"].as_u64().filter(|expires| *expires > now + 60).ok_or_else(|| crate::image_error("invalid uploaded file expiration".into()))?;
                store.record_owned_upload(&scope, &id, expires).map_err(crate::image_error)?;
                store.cache_upload(&cache_key, &id, expires).map_err(crate::image_error)?;
                cache.insert(cache_key.clone(), (id.clone(), expires));
                id
            };
            used.push((cache_key, id.clone()));
            protected.push(id.clone());
            *body.pointer_mut(&path).expect("existing image path") = json!({"type":"file", "file_id":id});
        }
        Ok(())
    }

    fn build_body(&self, request: &StepRequest<'_>) -> Result<Value, ProviderError> {
        crate::validate_image_roles(request.context, self.images.is_some())?;
        let images = self.images.as_ref().map(|(store, policy)| (store.clone(), store.effective_request_policy(policy)));
        let projected = images.as_ref().map(|(store, policy)| store.project_request(request.context, policy)).transpose().map_err(crate::image_error)?;
        let request = StepRequest { context: projected.as_ref().unwrap_or(request.context), system: request.system, tools: request.tools, on_delta: request.on_delta };
        if matches!(request.context.config.reasoning, Some(Reasoning::BudgetTokens { .. })) {
            return Err(ProviderError { code: "PROVIDER", retry_after: None,
                message: "reasoning budget tokens are unsupported by OpenAI-compatible providers; use effort".into(),
                retryable: false,
            });
        }
        let mut messages = Vec::new();
        if !request.system.is_empty() {
            messages.push(json!({ "role": "system", "content": request.system }));
        }
        let mut conversation = messages_from_context(request.context);
        if let Some((store, policy)) = &images {
            let mut index = 0;
            for turn in &request.context.turns {
                match turn {
                    ModelTurn::User { content } => {
                        if content.iter().any(|p| matches!(p, ContentPart::Image { .. })) {
                            let mut parts = Vec::new();
                            for part in content {
                                match part {
                                    ContentPart::Text { text } => parts.push(json!({"type":"text", "text":text})),
                                    ContentPart::Image { attachment } => {
                                        let (mime, data) = store.request_image(attachment, policy).map_err(crate::image_error)?;
                                        use base64::Engine;
                                        parts.push(json!({"type":"image_url", "image_url":{"url":format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data))}}));
                                    }
                                    _ => {}
                                }
                            }
                            conversation[index]["content"] = json!(parts);
                        }
                        index += 1;
                    }
                    ModelTurn::Assistant { .. } => index += 1,
                    ModelTurn::ToolResults { results } => {
                        index += results.len();
                        // Chat Completions tool messages cannot carry images. Keep all
                        // tool replies together, then associate rich output by call ID.
                        for result in results {
                            if !result.content.iter().any(|p| matches!(p, rness_protocol::events::ToolResultContentPart::Image { .. })) { continue; }
                            let mut parts = vec![json!({"type":"text", "text":format!("Output from tool call {} ({})", result.call, result.name)})];
                            for part in result.effective_content() {
                                match part {
                                    rness_protocol::events::ToolResultContentPart::Text { text } => parts.push(json!({"type":"text", "text":text})),
                                    rness_protocol::events::ToolResultContentPart::Image { attachment } => {
                                        let (mime, data) = store.request_image(&attachment, policy).map_err(crate::image_error)?;
                                        use base64::Engine;
                                        parts.push(json!({"type":"image_url", "image_url":{"url":format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data))}}));
                                    }
                                }
                            }
                            conversation.insert(index, json!({"role":"user", "content":parts}));
                            index += 1;
                        }
                    }
                }
            }
        }
        messages.extend(conversation);

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
    fn configure_images(&mut self, store: std::sync::Arc<rness_engine::images::ImageStore>, policy: rness_engine::images::ImagePolicy) {
        self.images = Some((store, policy));
    }
    fn model(&self) -> &str {
        &self.model
    }

    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        let started = Instant::now();
        let mut rejected = Vec::new();
        let response = loop {
        let mut used = Vec::new();
        let mut body = match self.build_body(&request) {
            Ok(body) => body,
            Err(error) => return StepOutcome::Failed { error, partial: vec![] },
        };

        if let Err(error) = self.reuse_image_uploads(&mut body, cancel, &rejected, &mut used).await {
            if cancel.is_cancelled() { return StepOutcome::Cancelled { partial: vec![] }; }
            return StepOutcome::Failed { error, partial: vec![] };
        }
        if let Err(error) = rness_engine::turn::provider::capture_wire(&body).await {
            return StepOutcome::Failed { error, partial: vec![] };
        }
        let mut http_request = self.client.post(format!("{}/chat/completions", self.base_url)).json(&body);
        if let Some(key) = &self.api_key { http_request = http_request.bearer_auth(key); }
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
            _ = crate::sse::idle_deadline(self.idle_timeout) => return StepOutcome::Failed { error: ProviderError { code: "TIMEOUT", retry_after: None, message: "TIMEOUT: waiting for provider response".into(), retryable: true }, partial: vec![] },
            r = http_request.send() => r,
        };

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                return StepOutcome::Failed {
                    error: ProviderError { code: "PROVIDER", retry_after: None, message: format!("transport: {e}"), retryable: true },
                    partial: vec![],
                }
            }
        };

        let status = response.status();
        if !status.is_success() {
            let retry_after = crate::sse::retry_after(response.headers());
            let retryable = status.as_u16() == 429 || status.is_server_error();
            let body = tokio::select! {
                _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
                _ = crate::sse::idle_deadline(self.idle_timeout) => return StepOutcome::Failed { error: crate::image_error("timeout reading provider error".into()), partial: vec![] },
                text = response.text() => text.unwrap_or_default(),
            };
            if rejected.is_empty() && matches!(status.as_u16(), 400 | 404 | 410 | 422) {
                rejected = crate::rejected_uploads(&body, &used);
                if !rejected.is_empty() { continue; }
            }
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or_else(|| format!("http {status}"));
            return StepOutcome::Failed {
                error: ProviderError { code: crate::request_error_code(status.as_u16(), &body), retry_after, message, retryable },
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
                SsePull::Event { data, .. } => {
                    if let Some(error) = crate::stream_overflow("", &data) {
                        return StepOutcome::Failed { error, partial: acc.chunks };
                    }
                    if data.trim() == "[DONE]" {
                        return StepOutcome::Committed(acc.finish(&self.model));
                    }
                    let at_ms = started.elapsed().as_millis() as u64;
                    if let Err(message) = acc.apply(&data, at_ms) {
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
                }
                SsePull::Timeout => return StepOutcome::Failed { error: ProviderError { code: "TIMEOUT", retry_after: None, message: "TIMEOUT: provider stream inactivity timeout".into(), retryable: true }, partial: acc.chunks },
                SsePull::Done => return StepOutcome::Committed(acc.finish(&self.model)),
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
