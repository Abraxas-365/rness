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
use rness_engine::session::projection::ModelTurn;
use rness_engine::turn::provider::{Provider, ProviderError, StepOutcome, StepRequest};
use rness_protocol::events::{
    AssistantMessage, ChunkDelta, ContentPart, Reasoning, StopReason, TimedChunk, Usage,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::auth::{AuthError, Credential, CredentialSource};
use crate::sse::{SsePull, SseReader};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 32_000;

// OAuth attribution headers (Claude Code — subscription billing
// requires the request to look like Claude Code).
const OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
const OAUTH_USER_AGENT: &str = "claude-cli/2.1.195 (external, sdk-cli)";
const OAUTH_BILLING_SYSTEM: &str =
    "x-anthropic-billing-header: cc_version=2.1.195; cc_entrypoint=cli; cch=00000;";

/// Beta header fragments for prompt caching features.
const BETA_PROMPT_CACHING_SCOPE: &str = "prompt-caching-scope-2026-01-05";
const BETA_EXTENDED_CACHE_TTL: &str = "extended-cache-ttl-2025-04-11";
const BETA_CACHE_DIAGNOSIS: &str = "cache-diagnosis-2026-04-07";

/// How the provider authenticates each request.
enum Auth {
    /// Fixed API key (tests, explicit key).
    ApiKey(String),
    /// Resolved per request: env > stored key > OAuth with auto-refresh.
    Source(CredentialSource),
}

pub struct AnthropicProvider {
    images: Option<(
        std::sync::Arc<rness_engine::images::ImageStore>,
        rness_engine::images::ImagePolicy,
    )>,
    idle_timeout: Option<std::time::Duration>,
    client: reqwest::Client,
    headers: crate::headers::ProviderHeaders,
    base_url: String,
    auth: Auth,
    model: String,
    max_tokens: u32,
    /// When true, inject cache_control breakpoints in requests.
    prompt_caching: bool,
    /// Resolved TTL state. `true` = 1-hour, `false` = 5-minute (default).
    /// Atomic because TTL downgrade may flip it concurrently.
    cache_ttl_1h: std::sync::atomic::AtomicBool,
    /// Whether TTL was auto-downgraded from 1h to 5m after API rejection.
    cache_ttl_downgraded: std::sync::atomic::AtomicBool,
    /// Original configured TTL preference (latched once on first OAuth resolution).
    cache_ttl_setting: crate::routes::CacheTtl,
    /// Whether the cache TTL has been resolved (latched once).
    cache_ttl_resolved: std::sync::atomic::AtomicBool,
}

impl AnthropicProvider {
    pub fn with_images(
        mut self,
        store: std::sync::Arc<rness_engine::images::ImageStore>,
        policy: rness_engine::images::ImagePolicy,
    ) -> Self {
        self.images = Some((store, policy));
        self
    }

    /// Apply validated headers to inference and file requests, never OAuth.
    pub fn with_headers(
        mut self,
        headers: crate::headers::ProviderHeaders,
    ) -> Result<Self, reqwest::Error> {
        self.client = headers.client()?;
        self.headers = headers;
        Ok(self)
    }

    pub fn with_stream_idle_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }

    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            images: None,
            idle_timeout: crate::sse::DEFAULT_IDLE_TIMEOUT,
            client: reqwest::Client::new(),
            headers: Default::default(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth: Auth::ApiKey(api_key.into()),
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            prompt_caching: false,
            cache_ttl_1h: std::sync::atomic::AtomicBool::new(false),
            cache_ttl_downgraded: std::sync::atomic::AtomicBool::new(false),
            cache_ttl_setting: crate::routes::CacheTtl::Auto,
            cache_ttl_resolved: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Resolve credentials per request from `source` (API key or OAuth
    /// Pro/Max tokens with transparent refresh).
    pub fn with_credentials(source: CredentialSource, model: impl Into<String>) -> Self {
        Self {
            images: None,
            idle_timeout: crate::sse::DEFAULT_IDLE_TIMEOUT,
            client: reqwest::Client::new(),
            headers: Default::default(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth: Auth::Source(source),
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            // Credential-based providers (interactive sessions) always enable caching.
            prompt_caching: true,
            cache_ttl_1h: std::sync::atomic::AtomicBool::new(false),
            cache_ttl_downgraded: std::sync::atomic::AtomicBool::new(false),
            cache_ttl_setting: crate::routes::CacheTtl::Auto,
            cache_ttl_resolved: std::sync::atomic::AtomicBool::new(false),
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

    /// Enable prompt caching with the specified TTL preference.
    pub fn with_prompt_caching(mut self, ttl: crate::routes::CacheTtl) -> Self {
        self.prompt_caching = true;
        self.cache_ttl_setting = ttl;
        // Pre-resolve non-auto settings immediately.
        match ttl {
            crate::routes::CacheTtl::OneHour => {
                self.cache_ttl_1h = std::sync::atomic::AtomicBool::new(true);
                self.cache_ttl_resolved = std::sync::atomic::AtomicBool::new(true);
            }
            crate::routes::CacheTtl::FiveMinutes => {
                self.cache_ttl_resolved = std::sync::atomic::AtomicBool::new(true);
            }
            crate::routes::CacheTtl::Auto => {} // Resolved on first request based on auth type.
        }
        self
    }

    /// Latch the cache TTL on first use when set to Auto.
    /// OAuth → 1h (free on subscription), API key → 5m.
    fn resolve_cache_ttl(&self, is_oauth: bool) {
        use std::sync::atomic::Ordering;
        if self
            .cache_ttl_resolved
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            if matches!(self.cache_ttl_setting, crate::routes::CacheTtl::Auto) {
                self.cache_ttl_1h.store(is_oauth, Ordering::Release);
            }
        }
    }

    /// Build the cache_control JSON value based on resolved TTL.
    fn cache_control_value(&self) -> Value {
        use std::sync::atomic::Ordering;
        if self.cache_ttl_1h.load(Ordering::Acquire) {
            json!({"type": "ephemeral", "ttl": "1h"})
        } else {
            json!({"type": "ephemeral"})
        }
    }

    /// Check if a 400 error is a TTL rejection (API doesn't support 1h for this account).
    fn is_ttl_rejection(&self, body: &str) -> bool {
        use std::sync::atomic::Ordering;
        if !self.cache_ttl_1h.load(Ordering::Acquire) {
            return false;
        }
        let lower = body.to_lowercase();
        lower.contains("ttl") || lower.contains("cache_control")
    }

    /// Downgrade from 1h to 5m TTL after API rejection. Permanent for the session.
    fn downgrade_cache_ttl(&self) {
        use std::sync::atomic::Ordering;
        self.cache_ttl_1h.store(false, Ordering::Release);
        self.cache_ttl_downgraded.store(true, Ordering::Release);
    }

    async fn credential(&self) -> Result<Credential, AuthError> {
        match &self.auth {
            Auth::ApiKey(key) => Ok(Credential::ApiKey(key.clone())),
            Auth::Source(source) => source.resolve().await,
        }
    }

    fn build_body(&self, request: &StepRequest<'_>, oauth: bool) -> Result<Value, ProviderError> {
        crate::validate_image_roles(request.context, self.images.is_some())?;
        let images = self
            .images
            .as_ref()
            .map(|(store, policy)| (store.clone(), store.effective_request_policy(policy)));
        let projected = images
            .as_ref()
            .map(|(store, policy)| store.project_request(request.context, policy))
            .transpose()
            .map_err(crate::image_error)?;
        let request = StepRequest {
            context: projected.as_ref().unwrap_or(request.context),
            system: request.system,
            tools: request.tools,
            on_delta: request.on_delta,
        };
        let max_tokens = request
            .context
            .config
            .max_output_tokens
            .unwrap_or(self.max_tokens);
        if let Some(Reasoning::BudgetTokens { tokens }) = &request.context.config.reasoning {
            if *tokens < 1024 || *tokens >= max_tokens {
                return Err(ProviderError {
                    code: "PROVIDER",
                    retry_after: None,
                    message: format!(
                        "Anthropic reasoning budget must satisfy 1024 <= budget < max_output_tokens ({max_tokens})"
                    ),
                    retryable: false,
                });
            }
        }

        // Resolve cache TTL on first use (env > config > auto: OAuth→1h, API-key→5m).
        if self.prompt_caching {
            self.resolve_cache_ttl(oauth);
        }

        let mut body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "stream": true,
            "messages": messages_from_context(request.context),
        });

        // Use explicit breakpoints below rather than top-level automatic
        // caching: Anthropic permits at most four cache_control blocks per
        // request, and system/tools plus the two strategic message points
        // already occupy that full budget.

        if let Some((store, policy)) = &images {
            let mut message_index = 0;
            for turn in &request.context.turns {
                if !has_serialized_message(turn) {
                    continue;
                }
                if let ModelTurn::ToolResults { results } = turn {
                    for (result_index, result) in results.iter().enumerate() {
                        if !result.content.iter().any(|p| {
                            matches!(
                                p,
                                rness_protocol::events::ToolResultContentPart::Image { .. }
                            )
                        }) {
                            continue;
                        }
                        let mut blocks = Vec::new();
                        for part in result.effective_content() {
                            match part {
                                rness_protocol::events::ToolResultContentPart::Text { text }
                                    if !text.is_empty() =>
                                {
                                    blocks.push(json!({"type":"text", "text":text}))
                                }
                                rness_protocol::events::ToolResultContentPart::Text { .. } => {}
                                rness_protocol::events::ToolResultContentPart::Image {
                                    attachment,
                                } => {
                                    let (mime, data) = store
                                        .request_image(&attachment, policy)
                                        .map_err(crate::image_error)?;
                                    use base64::Engine;
                                    blocks.push(json!({"type":"image", "source":{"type":"base64", "media_type":mime, "data":base64::engine::general_purpose::STANDARD.encode(data)}}));
                                }
                            }
                        }
                        body["messages"][message_index]["content"][result_index]["content"] =
                            json!(blocks);
                    }
                }
                if let ModelTurn::User { content } = turn {
                    for (part_index, part) in content
                        .iter()
                        .filter(|part| is_serialized_part(part))
                        .enumerate()
                    {
                        if let ContentPart::Image { attachment } = part {
                            let (mime, data) = store
                                .request_image(attachment, policy)
                                .map_err(crate::image_error)?;
                            use base64::Engine;
                            body["messages"][message_index]["content"][part_index] = json!({"type":"image", "source":{
                                "type":"base64", "media_type":mime, "data":base64::engine::general_purpose::STANDARD.encode(data)
                            }});
                        }
                    }
                }
                message_index += 1;
            }
        }
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
            // Place cache_control on the last system block so the entire
            // system prompt prefix is cached across turns.
            if self.prompt_caching {
                if let Some(last) = system_blocks.last_mut() {
                    last["cache_control"] = self.cache_control_value();
                }
            }
            body["system"] = Value::Array(system_blocks);
        }
        if !request.tools.is_empty() {
            let mut tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            // Place cache_control on the last tool definition so the entire
            // tool definitions prefix is cached.
            if self.prompt_caching {
                if let Some(last) = tools.last_mut() {
                    last["cache_control"] = self.cache_control_value();
                }
            }
            body["tools"] = Value::Array(tools);
        }

        // Explicit message breakpoints for multi-turn caching:
        // 1. Second-to-last message (most recent context, highest value).
        // 2. For long histories (≥ 10 messages), a midpoint breakpoint at len/3.
        if self.prompt_caching {
            if let Some(messages) = body["messages"].as_array_mut() {
                let len = messages.len();
                if len >= 2 {
                    mark_message_cache_control(&mut messages[len - 2], &self.cache_control_value());
                }
                if len >= 10 {
                    let mid = len / 3;
                    if mid > 0 {
                        mark_message_cache_control(&mut messages[mid], &self.cache_control_value());
                    }
                }
            }
        }

        Ok(body)
    }

    async fn reuse_image_uploads(
        &self,
        body: &mut Value,
        credential: &Credential,
        cancel: &CancellationToken,
        rejected: &[(String, String)],
        used: &mut Vec<(String, String)>,
    ) -> Result<(), ProviderError> {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        static UPLOADS: std::sync::OnceLock<
            tokio::sync::Mutex<std::collections::HashMap<String, (String, u64)>>,
        > = std::sync::OnceLock::new();
        let Some((store, policy)) = &self.images else {
            return Ok(());
        };
        if !store.effective_request_policy(policy).anthropic_files {
            return Ok(());
        }
        let Credential::ApiKey(key) = credential else {
            return Err(crate::image_error("Anthropic file reuse requires API-key authentication; disable anthropic_files for OAuth".into()));
        };
        let mut paths = Vec::new();
        for (mi, message) in body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            for (ci, part) in message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if part["type"] == "image" {
                    paths.push(format!("/messages/{mi}/content/{ci}/source"));
                }
                if part["type"] == "tool_result" {
                    for (ti, inner) in part["content"].as_array().into_iter().flatten().enumerate()
                    {
                        if inner["type"] == "image" {
                            paths.push(format!("/messages/{mi}/content/{ci}/content/{ti}/source"));
                        }
                    }
                }
            }
        }
        let mut protected = Vec::new();
        for path in paths {
            let source = body
                .pointer(&path)
                .ok_or_else(|| crate::image_error("missing image source".into()))?;
            let data = source["data"]
                .as_str()
                .ok_or_else(|| crate::image_error("missing inline image bytes".into()))?;
            let mime = source["media_type"]
                .as_str()
                .ok_or_else(|| crate::image_error("missing image media type".into()))?;
            let upload_credential = self.headers.upload_credential(key);
            let cache_key = format!(
                "{:x}",
                Sha256::digest(
                    serde_json::to_vec(&(&self.base_url, &upload_credential, mime, data))
                        .map_err(|e| crate::image_error(e.to_string()))?
                )
            );
            let mut cache = tokio::select! {
                _ = cancel.cancelled() => return Err(crate::image_error("image upload cancelled".into())),
                guard = UPLOADS.get_or_init(Default::default).lock() => guard,
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| crate::image_error(e.to_string()))?
                .as_secs();
            cache.retain(|_, (_, expires)| *expires > now.saturating_add(60));
            let mut cached = cache.get(&cache_key).cloned().or(store
                .cached_upload(&cache_key, now)
                .map_err(crate::image_error)?);
            if cached
                .as_ref()
                .is_some_and(|(id, _)| rejected.contains(&(cache_key.clone(), id.clone())))
            {
                cache.remove(&cache_key);
                store
                    .cache_upload(&cache_key, "expired", 0)
                    .map_err(crate::image_error)?;
                cached = None;
            }
            if let Some((id, _)) = &cached {
                let metadata = tokio::select! {
                    _ = cancel.cancelled() => return Err(crate::image_error("image lookup cancelled".into())),
                    result = self.client.get(format!("{}/v1/files/{}", self.base_url, id)).header("x-api-key", key).header("anthropic-version", API_VERSION).timeout(std::time::Duration::from_secs(30)).send() => result.map_err(|e| crate::image_error(e.to_string()))?,
                };
                if metadata.status() == reqwest::StatusCode::NOT_FOUND {
                    cache.remove(&cache_key);
                    store
                        .cache_upload(&cache_key, "expired", 0)
                        .map_err(crate::image_error)?;
                    cached = None;
                } else {
                    metadata
                        .error_for_status()
                        .map_err(|e| crate::image_error(format!("image lookup failed: {e}")))?;
                }
            }
            let file_id = if let Some((id, _)) = cached {
                id
            } else {
                let expires = now.saturating_add(3500);
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| crate::image_error(e.to_string()))?;
                let endpoint = format!("{}/v1/files", self.base_url);
                let scope = crate::upload_scope(&endpoint, &upload_credential);
                let value = crate::upload_with_quota_recovery(
                    store,
                    &scope,
                    &protected,
                    || {
                        let part = reqwest::multipart::Part::bytes(bytes.clone())
                            .file_name("image")
                            .mime_str(mime)
                            .map_err(|e| crate::image_error(e.to_string()))?;
                        let form = reqwest::multipart::Form::new()
                            .part("file", part)
                            .text("expires_in_seconds", "3600");
                        Ok(self
                            .client
                            .post(&endpoint)
                            .header("x-api-key", key)
                            .header("anthropic-version", API_VERSION)
                            .multipart(form))
                    },
                    |id| {
                        self.client
                            .delete(format!("{endpoint}/{id}"))
                            .header("x-api-key", key)
                            .header("anthropic-version", API_VERSION)
                    },
                    cancel,
                )
                .await?;
                let id = value["id"]
                    .as_str()
                    .filter(|id| {
                        id.starts_with("file_")
                            && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    })
                    .ok_or_else(|| crate::image_error("image upload returned no file ID".into()))?
                    .to_owned();
                store
                    .record_owned_upload(&scope, &id, expires)
                    .map_err(crate::image_error)?;
                store
                    .cache_upload(&cache_key, &id, expires)
                    .map_err(crate::image_error)?;
                cache.insert(cache_key.clone(), (id.clone(), expires));
                id
            };
            used.push((cache_key, file_id.clone()));
            protected.push(file_id.clone());
            *body.pointer_mut(&path).expect("existing image source") =
                json!({"type":"file", "file_id":file_id});
        }
        Ok(())
    }

    fn request_for(&self, credential: &Credential, body: &Value) -> reqwest::RequestBuilder {
        let base = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", API_VERSION)
            .json(body);

        // Build caching beta headers when prompt caching is active.
        let cache_betas = if self.prompt_caching {
            use std::sync::atomic::Ordering;
            let mut parts = vec![BETA_PROMPT_CACHING_SCOPE, BETA_CACHE_DIAGNOSIS];
            if self.cache_ttl_1h.load(Ordering::Acquire) {
                parts.push(BETA_EXTENDED_CACHE_TTL);
            }
            Some(parts.join(","))
        } else {
            None
        };

        match credential {
            Credential::ApiKey(key) => {
                let req = base.header("x-api-key", key);
                if let Some(betas) = cache_betas {
                    req.header("anthropic-beta", betas)
                } else {
                    req
                }
            }
            Credential::OAuth(token) => {
                // Merge cache betas into the OAuth beta header string.
                let beta_str = if let Some(betas) = cache_betas {
                    format!("{OAUTH_BETA},{betas},mid-conversation-system-2026-04-07,structured-outputs-2025-12-15")
                } else {
                    OAUTH_BETA.to_string()
                };
                base.bearer_auth(token)
                    .header("anthropic-beta", beta_str)
                    .header("User-Agent", OAUTH_USER_AGENT)
                    .header("anthropic-dangerous-direct-browser-access", "true")
                    .header("x-app", "cli")
            }
        }
    }
}

fn is_serialized_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text } => !text.is_empty(),
        // Thinking blocks contain provider-specific opaque continuation
        // tokens. They cannot safely survive a provider/model switch,
        // compaction, or replay, so never include them in Anthropic input.
        ContentPart::Thinking { .. } => false,
        ContentPart::Image { .. } | ContentPart::ToolUse { .. } => true,
    }
}

fn has_serialized_message(turn: &ModelTurn) -> bool {
    match turn {
        ModelTurn::User { content } | ModelTurn::Assistant { content } => {
            !parts_to_json(content).is_empty()
        }
        ModelTurn::ToolResults { .. } => true,
    }
}

/// Map the replay-derived context onto Anthropic `messages`.
fn messages_from_context(context: &rness_engine::session::projection::ModelContext) -> Vec<Value> {
    let mut messages = Vec::new();
    for turn in &context.turns {
        match turn {
            ModelTurn::User { content } => {
                let content = parts_to_json(content);
                if !content.is_empty() {
                    messages.push(json!({ "role": "user", "content": content }));
                }
            }
            ModelTurn::Assistant { content } => {
                let content = parts_to_json(content);
                if !content.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": content }));
                }
            }
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

/// Place `cache_control` on the last cacheable content block of a message.
/// Skips thinking blocks (API rejects cache_control there) and empty text blocks.
fn mark_message_cache_control(msg: &mut Value, cc: &Value) {
    let Some(content) = msg["content"].as_array_mut() else {
        return;
    };
    for block in content.iter_mut().rev() {
        let block_type = block["type"].as_str().unwrap_or("");
        // Thinking blocks cannot have cache_control; skip empty text.
        if block_type == "thinking" {
            continue;
        }
        if block_type == "text" && block["text"].as_str().unwrap_or("").is_empty() {
            continue;
        }
        block["cache_control"] = cc.clone();
        return;
    }
}

fn parts_to_json(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Image { .. } => Some(Value::Null), // Filled from the validated attachment store before sending.
            ContentPart::Text { text } if !text.is_empty() => {
                Some(json!({ "type": "text", "text": text }))
            }
            ContentPart::Thinking { .. } => None,
            ContentPart::ToolUse { call, name, args } => Some(json!({
                "type": "tool_use",
                "id": call,
                "name": name,
                "input": args,
            })),
            ContentPart::Text { .. } => None,
        })
        .collect()
}

// -- stream accumulation ---------------------------------------------------

/// One in-flight content block being assembled from deltas.
enum Block {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    ToolUse {
        call: String,
        name: String,
        args_json: String,
    },
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
            .filter_map(|b| match b {
                // Providers may start a text block and finish without deltas;
                // do not persist a block Anthropic will later reject on replay.
                Block::Text(text) if text.is_empty() => None,
                Block::Text(text) => Some(ContentPart::Text { text }),
                // Anthropic starts thinking blocks before it necessarily
                // emits a delta. Empty blocks have no useful transcript
                // content and otherwise render as a ghost "Thinking" panel.
                Block::Thinking { text, .. } if text.is_empty() => None,
                Block::Thinking { text, signature } => Some(ContentPart::Thinking {
                    text,
                    signature: (!signature.is_empty()).then_some(signature),
                }),
                Block::ToolUse {
                    call,
                    name,
                    args_json,
                } if args_json.is_empty() => Some(ContentPart::ToolUse {
                    call,
                    name,
                    // No-arg calls stream no input_json_delta; Anthropic
                    // requires an object rather than null when replayed.
                    args: Value::Object(Default::default()),
                }),
                // A nonempty partial_json which does not parse means the
                // output limit cut a tool call mid-arguments. Never turn it
                // into `{}`: that can execute a different, unsafe command.
                // Dropping it also keeps the next replay valid (there is no
                // unmatched tool_use requiring a tool_result).
                Block::ToolUse { args_json, .. }
                    if serde_json::from_str::<Value>(&args_json).is_err() =>
                {
                    None
                }
                Block::ToolUse {
                    call,
                    name,
                    args_json,
                } => Some(ContentPart::ToolUse {
                    call,
                    name,
                    args: serde_json::from_str(&args_json).expect("validated above"),
                }),
            })
            .collect();
        AssistantMessage {
            model: model.to_string(),
            content,
            stop: self.stop.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
            estimated_input: 0,
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
                    "thinking" => self.blocks.push(Block::Thinking {
                        text: String::new(),
                        signature: String::new(),
                    }),
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
                    (
                        "input_json_delta",
                        Block::ToolUse {
                            call, args_json, ..
                        },
                    ) => {
                        let t = delta["partial_json"].as_str().unwrap_or("");
                        args_json.push_str(t);
                        self.chunks.push(TimedChunk {
                            ms: at_ms,
                            delta: ChunkDelta::ToolArgs {
                                call: call.clone(),
                                t: t.to_string(),
                            },
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
    fn configure_images(
        &mut self,
        store: std::sync::Arc<rness_engine::images::ImageStore>,
        policy: rness_engine::images::ImagePolicy,
    ) {
        self.images = Some((store, policy));
    }
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
                    error: ProviderError {
                        code: "PROVIDER",
                        retry_after: None,
                        message: e.to_string(),
                        retryable,
                    },
                    partial: vec![],
                };
            }
        };
        let mut refreshed_once = false;
        let mut rejected = Vec::new();

        loop {
            let mut used = Vec::new();
            let mut body =
                match self.build_body(&request, matches!(credential, Credential::OAuth(_))) {
                    Ok(body) => body,
                    Err(error) => {
                        return StepOutcome::Failed {
                            error,
                            partial: vec![],
                        };
                    }
                };
            if let Err(error) = self
                .reuse_image_uploads(&mut body, &credential, cancel, &rejected, &mut used)
                .await
            {
                if cancel.is_cancelled() {
                    return StepOutcome::Cancelled { partial: vec![] };
                }
                return StepOutcome::Failed {
                    error,
                    partial: vec![],
                };
            }
            if let Err(error) = rness_engine::turn::provider::capture_wire(&body).await {
                return StepOutcome::Failed {
                    error,
                    partial: vec![],
                };
            }
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
                        error: ProviderError {
                            code: "PROVIDER",
                            retry_after: None,
                            message: format!("transport: {e}"),
                            retryable: true,
                        },
                        partial: vec![],
                    };
                }
            };

            let status = response.status();
            if !status.is_success() {
                // 401 with OAuth: force-refresh once and retry.
                if status.as_u16() == 401 && !refreshed_once {
                    if let (Auth::Source(source), Credential::OAuth(_)) = (&self.auth, &credential)
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
                let body = tokio::select! {
                    _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
                    _ = crate::sse::idle_deadline(self.idle_timeout) => return StepOutcome::Failed { error: crate::image_error("timeout reading provider error".into()), partial: vec![] },
                    text = response.text() => text.unwrap_or_default(),
                };
                if rejected.is_empty() && matches!(status.as_u16(), 400 | 404 | 410 | 422) {
                    rejected = crate::rejected_uploads(&body, &used);
                    if !rejected.is_empty() {
                        continue;
                    }
                }
                // TTL downgrade: if the API rejected our 1h TTL with a 400,
                // switch to 5m and rebuild+retry the request once.
                if status.as_u16() == 400 && self.prompt_caching && self.is_ttl_rejection(&body) {
                    self.downgrade_cache_ttl();
                    continue;
                }
                let message = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("http {status}"));
                return StepOutcome::Failed {
                    error: ProviderError {
                        code: crate::request_error_code(status.as_u16(), &body),
                        retry_after,
                        message,
                        retryable,
                    },
                    partial: vec![],
                };
            }

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
