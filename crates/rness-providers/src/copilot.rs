//! GitHub Copilot transport: one provider that speaks three wire shapes.
//!
//! Copilot serves every model behind one host but not one API: Claude
//! models answer Anthropic Messages (`/v1/messages`), GPT-5-class models
//! answer OpenAI Responses (`/responses`), and the rest (GPT-4.x, Gemini)
//! answer Chat Completions (`/chat/completions`). `GET /models` says which
//! endpoints each model supports; this provider asks once per instance,
//! picks the endpoint, and delegates to the matching adapter with
//! Copilot's authentication attached by a [`RequestSigner`].
//!
//! Headers follow OpenCode's Copilot integration: the GitHub OAuth token as
//! bearer, `Openai-Intent: conversation-edits`, `X-GitHub-Api-Version`,
//! `x-initiator` (`user` only when the request answers a human message, so
//! tool-loop continuations are not billed as new premium requests),
//! `Copilot-Vision-Request` when the body carries images, and the
//! interleaved-thinking beta on the Messages route.

use std::sync::Arc;

use async_trait::async_trait;
use rness_engine::session::projection::ModelTurn;
use rness_engine::turn::provider::{Provider, ProviderError, StepOutcome, StepRequest};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::auth::github_copilot::{self, STORE_KEY};
use crate::auth::CredentialStore;
use crate::RequestSigner;

/// `User-Agent` on every Copilot and GitHub request.
pub const USER_AGENT: &str = concat!("rness/", env!("CARGO_PKG_VERSION"));
/// `X-GitHub-Api-Version` for inference and `/models`.
pub const API_VERSION: &str = "2026-08-01";
const MODELS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Which Copilot API a model is served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Messages,
    Responses,
    Chat,
}

impl Endpoint {
    /// From `/models` `supported_endpoints`, Messages first (Claude keeps
    /// its native thinking and tool semantics there).
    fn from_supported(endpoints: &[String]) -> Option<Self> {
        let has = |e: &str| endpoints.iter().any(|s| s == e);
        if has("/v1/messages") {
            Some(Self::Messages)
        } else if has("/responses") {
            Some(Self::Responses)
        } else if has("/chat/completions") {
            Some(Self::Chat)
        } else {
            None
        }
    }

    /// Without a live model list: GPT-5+ (but not `gpt-5-mini`), Grok and
    /// MAI code models are Responses-only; everything else has Chat.
    pub fn guess(model: &str) -> Self {
        let gpt_major = model
            .strip_prefix("gpt-")
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|digits| digits.parse::<u32>().ok());
        let responses = (gpt_major.is_some_and(|n| n >= 5) && !model.starts_with("gpt-5-mini"))
            || model.starts_with("grok-")
            || model.starts_with("mai-code-");
        if responses {
            Self::Responses
        } else {
            Self::Chat
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<RemoteModel>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteModel {
    id: String,
    #[serde(default)]
    supported_endpoints: Vec<String>,
    #[serde(default)]
    capabilities: RemoteCapabilities,
    #[serde(default)]
    policy: Option<RemotePolicy>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteCapabilities {
    #[serde(default)]
    limits: RemoteLimits,
    #[serde(default)]
    supports: RemoteSupports,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteLimits {
    #[serde(default)]
    max_output_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteSupports {
    #[serde(default)]
    reasoning_effort: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RemotePolicy {
    #[serde(default)]
    state: Option<String>,
}

/// What the model list says about the selected model.
#[derive(Debug, Clone, Copy)]
struct Plan {
    endpoint: Endpoint,
    max_output_tokens: Option<u32>,
    /// Whether to request reasoning summaries on the Responses route.
    reasoning: bool,
}

/// Who started the request, and why (`X-Interaction-Type`).
#[derive(Clone, Copy)]
struct Interaction {
    initiator: &'static str,
    kind: &'static str,
}

tokio::task_local! {
    static INTERACTION: Interaction;
}

pub struct CopilotProvider {
    store: CredentialStore,
    credential: String,
    /// Explicit endpoint override (proxies, tests); `None` = the account's.
    base_url: Option<String>,
    model: String,
    headers: crate::headers::ProviderHeaders,
    idle_timeout: Option<std::time::Duration>,
    images: Option<(
        Arc<rness_engine::images::ImageStore>,
        rness_engine::images::ImagePolicy,
    )>,
    output_limit: Option<u32>,
    inner: tokio::sync::Mutex<Option<Built>>,
}

/// The adapter built for one token, and when its route must be re-checked.
struct Built {
    token: String,
    provider: Arc<dyn Provider>,
    /// `Some` when the route was guessed from the model name.
    replan_at: Option<std::time::Instant>,
}

enum ListError {
    /// 401/403: the stored token no longer works.
    Unauthorized(u16),
    Other(String),
}

const REPLAN_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
const LOGIN_HINT_SHORT: &str = "run `rness auth login --provider github-copilot`";
const LOGIN_HINT: &str =
    "no GitHub Copilot credentials: run `rness auth login --provider github-copilot`";

fn fail(message: String) -> ProviderError {
    ProviderError {
        code: "PROVIDER",
        retry_after: None,
        message,
        retryable: false,
    }
}

impl CopilotProvider {
    pub fn new(store: CredentialStore, model: impl Into<String>) -> Self {
        Self {
            store,
            credential: STORE_KEY.into(),
            base_url: None,
            model: model.into(),
            headers: Default::default(),
            idle_timeout: crate::sse::DEFAULT_IDLE_TIMEOUT,
            images: None,
            output_limit: None,
            inner: tokio::sync::Mutex::new(None),
        }
    }

    /// Read tokens from another store key (named accounts).
    pub fn with_credential(mut self, credential: impl Into<String>) -> Self {
        self.credential = credential.into();
        self
    }

    /// Override the API host. The public default host is not an override:
    /// it yields to the endpoint GitHub assigned the account at login.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let url = base_url.into().trim_end_matches('/').to_string();
        self.base_url = (url != github_copilot::DEFAULT_API_BASE).then_some(url);
        self
    }

    pub fn with_headers(
        mut self,
        headers: crate::headers::ProviderHeaders,
    ) -> Result<Self, reqwest::Error> {
        // Validate once here; adapters rebuild their own clients.
        headers.client(crate::connect_timeout(self.idle_timeout))?;
        self.headers = headers;
        Ok(self)
    }

    pub fn with_stream_idle_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }

    async fn inner(&self) -> Result<Arc<dyn Provider>, ProviderError> {
        // The store is re-read every step so a re-login or logout during a
        // long session takes effect on the next request.
        let tokens = self
            .store
            .tokens(&self.credential)
            .map_err(|e| fail(format!("auth: {e}")))?
            .filter(|t| !t.access_token.is_empty())
            .ok_or_else(|| fail(LOGIN_HINT.into()))?;
        let mut slot = self.inner.lock().await;
        if let Some(built) = slot.as_ref() {
            let fresh_plan = built
                .replan_at
                .is_none_or(|at| std::time::Instant::now() < at);
            if built.token == tokens.access_token && fresh_plan {
                return Ok(built.provider.clone());
            }
        }
        let built = self.build_inner(tokens).await?;
        let provider = built.provider.clone();
        *slot = Some(built);
        Ok(provider)
    }

    async fn build_inner(&self, tokens: crate::auth::Tokens) -> Result<Built, ProviderError> {
        let base = self
            .base_url
            .clone()
            .unwrap_or_else(|| github_copilot::api_base(&tokens));
        let token = tokens.access_token;
        let (plan, authoritative) = self.plan(&base, &token).await?;
        let signer = signer(token.clone(), plan.endpoint);
        let http = |e: reqwest::Error| fail(format!("provider HTTP client: {e}"));
        let mut provider: Box<dyn Provider> = match plan.endpoint {
            Endpoint::Messages => {
                let mut p = crate::anthropic::AnthropicProvider::signed(signer, &base, &self.model)
                    .with_headers(self.headers.clone())
                    .map_err(http)?
                    .with_stream_idle_timeout(self.idle_timeout);
                if let Some(limit) = self.output_limit.or(plan.max_output_tokens) {
                    p.configure_output_limit(limit);
                }
                Box::new(p)
            }
            Endpoint::Responses => Box::new(
                crate::responses::ResponsesProvider::signed(signer, &base, &self.model)
                    .with_reasoning(plan.reasoning)
                    .with_headers(self.headers.clone())
                    .map_err(http)?
                    .with_stream_idle_timeout(self.idle_timeout),
            ),
            Endpoint::Chat => Box::new(
                crate::openai::OpenAiProvider::signed(signer, &base, &self.model)
                    .with_headers(self.headers.clone())
                    .map_err(http)?
                    .with_stream_idle_timeout(self.idle_timeout),
            ),
        };
        if let Some((store, policy)) = &self.images {
            provider.configure_images(store.clone(), policy.clone());
        }
        Ok(Built {
            token,
            provider: Arc::from(provider),
            // A name-guessed route is provisional: ask `/models` again later.
            replan_at: (!authoritative).then(|| std::time::Instant::now() + REPLAN_AFTER),
        })
    }

    /// Ask `/models` which endpoint serves the model; `true` when the
    /// answer came from the list. A failed listing or an unlisted model
    /// falls back to the name heuristic; a rejected token or an explicitly
    /// disabled model fails here with the reason rather than as an opaque
    /// HTTP error later.
    async fn plan(&self, base: &str, token: &str) -> Result<(Plan, bool), ProviderError> {
        let endpoint = Endpoint::guess(&self.model);
        let guess = Plan {
            endpoint,
            max_output_tokens: None,
            // Copilot's Responses-only models are reasoning models.
            reasoning: endpoint == Endpoint::Responses,
        };
        let listed = match self.list_models(base, token).await {
            Ok(list) => list,
            Err(ListError::Unauthorized(status)) => {
                return Err(fail(format!(
                    "GitHub Copilot rejected the stored token (http {status}): {LOGIN_HINT_SHORT}"
                )))
            }
            Err(ListError::Other(e)) => {
                tracing::warn!(error = %e, "github-copilot: model list unavailable; routing by model name");
                return Ok((guess, false));
            }
        };
        let Some(remote) = listed.data.into_iter().find(|m| m.id == self.model) else {
            tracing::warn!(model = %self.model, "github-copilot: model not in /models; routing by model name");
            return Ok((guess, false));
        };
        if remote
            .policy
            .as_ref()
            .and_then(|p| p.state.as_deref())
            .is_some_and(|state| state == "disabled")
        {
            return Err(fail(format!(
                "GitHub Copilot model {} is disabled for this account: enable it in \
                 the Copilot settings on github.com (or ask an organization admin)",
                self.model
            )));
        }
        Ok((
            Plan {
                endpoint: Endpoint::from_supported(&remote.supported_endpoints)
                    .unwrap_or(guess.endpoint),
                max_output_tokens: remote.capabilities.limits.max_output_tokens,
                reasoning: remote
                    .capabilities
                    .supports
                    .reasoning_effort
                    .is_some_and(|efforts| !efforts.is_empty()),
            },
            true,
        ))
    }

    async fn list_models(&self, base: &str, token: &str) -> Result<ModelList, ListError> {
        let other = |e: &dyn std::fmt::Display| ListError::Other(e.to_string());
        let client = self
            .headers
            .client(crate::connect_timeout(Some(MODELS_TIMEOUT)))
            .map_err(|e| other(&e))?;
        let response = client
            .get(format!("{base}/models"))
            .bearer_auth(token)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .timeout(MODELS_TIMEOUT)
            .send()
            .await
            .map_err(|e| other(&e))?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ListError::Unauthorized(status.as_u16()));
        }
        if !status.is_success() {
            return Err(ListError::Other(format!("http {status}")));
        }
        response.json().await.map_err(|e| other(&e))
    }

    async fn run(
        &self,
        request: StepRequest<'_>,
        cancel: &CancellationToken,
        kind: &'static str,
    ) -> StepOutcome {
        let inner = tokio::select! {
            biased;
            _ = cancel.cancelled() => return StepOutcome::Cancelled { partial: vec![] },
            inner = self.inner() => inner,
        };
        let inner = match inner {
            Ok(inner) => inner,
            Err(error) => {
                return StepOutcome::Failed {
                    error,
                    partial: vec![],
                }
            }
        };
        // Billing: a request is user-initiated only when it answers a
        // human message; tool-loop continuations and compaction are agent.
        let initiator = match request.context.turns.last() {
            Some(ModelTurn::User { .. }) if kind == "conversation-agent" => "user",
            _ => "agent",
        };
        INTERACTION
            .scope(Interaction { initiator, kind }, inner.step(request, cancel))
            .await
    }
}

/// The per-request header set for `endpoint`.
fn signer(token: String, endpoint: Endpoint) -> RequestSigner {
    Arc::new(move |request: reqwest::RequestBuilder, body: &Value| {
        let interaction = INTERACTION.try_with(|i| *i).unwrap_or(Interaction {
            initiator: if last_is_user(body) { "user" } else { "agent" },
            kind: "conversation-agent",
        });
        let mut request = request
            .bearer_auth(&token)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header("Openai-Intent", "conversation-edits")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("X-Interaction-Type", interaction.kind)
            .header("x-initiator", interaction.initiator);
        if has_image(body) {
            request = request.header("Copilot-Vision-Request", "true");
        }
        if endpoint == Endpoint::Messages {
            request = request.header("anthropic-beta", INTERLEAVED_THINKING_BETA);
        }
        request
    })
}

/// The conversation array of any of the three wire shapes.
fn conversation(body: &Value) -> &[Value] {
    body["input"]
        .as_array()
        .or_else(|| body["messages"].as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Body-only initiator, used when no step context is in scope. An
/// Anthropic user message holding only tool results is the agent's.
fn last_is_user(body: &Value) -> bool {
    let Some(last) = conversation(body).last() else {
        return false;
    };
    if last["role"] != "user" {
        return false;
    }
    match last["content"].as_array() {
        Some(parts) => parts.iter().any(|p| p["type"] != "tool_result"),
        None => true,
    }
}

/// Whether any message carries an image, in any of the three shapes
/// (including images nested in tool results).
fn has_image(body: &Value) -> bool {
    fn part_is_image(part: &Value) -> bool {
        match part["type"].as_str() {
            Some("image" | "image_url" | "input_image") => true,
            Some("tool_result") => part["content"]
                .as_array()
                .is_some_and(|inner| inner.iter().any(part_is_image)),
            _ => false,
        }
    }
    conversation(body).iter().any(|item| {
        item["content"]
            .as_array()
            .or_else(|| item["output"].as_array())
            .is_some_and(|parts| parts.iter().any(part_is_image))
    })
}

#[async_trait]
impl Provider for CopilotProvider {
    fn model(&self) -> &str {
        &self.model
    }

    fn configure_images(
        &mut self,
        store: Arc<rness_engine::images::ImageStore>,
        policy: rness_engine::images::ImagePolicy,
    ) {
        self.images = Some((store, policy));
    }

    fn configure_output_limit(&mut self, max_output_tokens: u32) {
        self.output_limit = Some(max_output_tokens);
    }

    async fn summarize_step(
        &self,
        request: StepRequest<'_>,
        cancel: &CancellationToken,
    ) -> StepOutcome {
        self.run(request, cancel, "conversation-compaction").await
    }

    async fn step(&self, request: StepRequest<'_>, cancel: &CancellationToken) -> StepOutcome {
        self.run(request, cancel, "conversation-agent").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_heuristic_matches_copilot_routing() {
        for (model, want) in [
            ("gpt-5", Endpoint::Responses),
            ("gpt-5.2-codex", Endpoint::Responses),
            ("gpt-6", Endpoint::Responses),
            ("grok-code-fast-1", Endpoint::Responses),
            ("mai-code-1", Endpoint::Responses),
            ("gpt-5-mini", Endpoint::Chat),
            ("gpt-4.1", Endpoint::Chat),
            ("gpt-4o", Endpoint::Chat),
            ("claude-sonnet-4.5", Endpoint::Chat),
            ("gemini-2.5-pro", Endpoint::Chat),
        ] {
            assert_eq!(Endpoint::guess(model), want, "{model}");
        }
    }

    #[test]
    fn supported_endpoints_prefer_messages_then_responses() {
        let e = |list: &[&str]| {
            Endpoint::from_supported(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(
            e(&["/chat/completions", "/v1/messages"]),
            Some(Endpoint::Messages)
        );
        assert_eq!(
            e(&["/chat/completions", "/responses"]),
            Some(Endpoint::Responses)
        );
        assert_eq!(e(&["/chat/completions"]), Some(Endpoint::Chat));
        assert_eq!(e(&[]), None);
    }

    #[test]
    fn initiator_and_vision_from_each_wire_shape() {
        let chat_user =
            json!({"messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"}]});
        let chat_tool =
            json!({"messages":[{"role":"user","content":"hi"},{"role":"tool","content":"x"}]});
        assert!(last_is_user(&chat_user));
        assert!(!last_is_user(&chat_tool));
        let anthropic_tool = json!({"messages":[{"role":"user","content":[{"type":"tool_result","content":[{"type":"image"}]}]}]});
        assert!(!last_is_user(&anthropic_tool));
        assert!(has_image(&anthropic_tool));
        let responses =
            json!({"input":[{"type":"function_call_output","output":[{"type":"input_image"}]}]});
        assert!(!last_is_user(&responses));
        assert!(has_image(&responses));
        assert!(!has_image(&chat_user));
        let chat_image = json!({"messages":[{"role":"user","content":[{"type":"image_url"}]}]});
        assert!(has_image(&chat_image));
    }
}
