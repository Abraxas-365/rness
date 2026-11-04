//! "Login with GitHub Copilot" — OAuth device flow against github.com or a
//! GitHub Enterprise host, then the GitHub token itself is the bearer for
//! the Copilot API (no `copilot_internal/v2/token` exchange).
//!
//! Mirrors OpenCode's Copilot integration (`plugin/provider/github-copilot.ts`):
//! same public OAuth client id, `read:user` scope, the
//! `copilot_internal/user` entitlement probe (OAuth succeeds for any GitHub
//! account; only this endpoint says whether the account can chat), and the
//! account-assigned API endpoint (`endpoints.api`: individual, business and
//! enterprise plans are served from different hosts).
//!
//! The token does not expire and has no refresh token; a revoked token
//! surfaces as a 401 and needs a new login.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::AuthError;
use crate::auth::{CredentialStore, Tokens};

/// OpenCode's GitHub OAuth app (the client GitHub approved for Copilot).
pub const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
pub const SCOPE: &str = "read:user";
/// The provider key these tokens are stored under.
pub const STORE_KEY: &str = "github-copilot";
/// `X-GitHub-Api-Version` for `copilot_internal/user`.
pub const USER_API_VERSION: &str = "2025-04-01";
/// Copilot's default (individual-plan) API host.
pub const DEFAULT_API_BASE: &str = "https://api.githubcopilot.com";
/// Extra wait on top of the server-requested polling interval, so clock
/// skew never earns a `slow_down`.
const POLL_SAFETY_MARGIN: Duration = Duration::from_secs(3);
/// Fallback when the device response carries no `expires_in`.
const DEFAULT_DEVICE_TTL_SECS: u64 = 900;

/// Stored-token metadata keys (camelCase like the rest of the file).
const ENTERPRISE_URL: &str = "enterpriseUrl";
const API_ENDPOINT: &str = "apiEndpoint";

/// Receives the verification URL and the code to enter there.
pub type OnDeviceCode = Box<dyn Fn(&str, &str) + Send + Sync>;

/// Where the device flow reports progress (the CLI prints these).
pub struct DevicePrompt {
    /// Called once with the verification URL and the code to enter there.
    pub on_code: OnDeviceCode,
}

#[derive(Debug, Deserialize)]
struct DeviceCode {
    verification_uri: String,
    user_code: String,
    device_code: String,
    #[serde(default)]
    interval: u64,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct TokenPoll {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

/// The `copilot_internal/user` fields login cares about.
#[derive(Debug, Default, Deserialize)]
pub struct CopilotUser {
    #[serde(default)]
    pub chat_enabled: Option<bool>,
    #[serde(default)]
    pub can_signup_for_limited: Option<bool>,
    #[serde(default)]
    pub copilot_plan: Option<String>,
    #[serde(default)]
    pub endpoints: Option<CopilotEndpoints>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CopilotEndpoints {
    #[serde(default)]
    pub api: Option<String>,
}

/// Normalize `company.ghe.com`, `https://company.ghe.com/` → `company.ghe.com`.
/// Rejects anything that is not a bare host (paths, credentials, ports are
/// allowed only as `host:port`).
pub fn normalize_domain(input: &str) -> Result<String, AuthError> {
    let trimmed = input.trim();
    let host = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let bad = || AuthError::OAuth(format!("invalid GitHub Enterprise domain: {input:?}"));
    if host.is_empty() || host.contains(['/', '@', '?', '#', ' ', '\\', '%']) {
        return Err(bad());
    }
    let url = reqwest::Url::parse(&format!("https://{host}")).map_err(|_| bad())?;
    // Rebuild from the parsed URL so later `format!`s see exactly what the
    // parser accepted (lowercased, IDNA-encoded, numeric port only).
    let parsed = url.host_str().ok_or_else(bad)?;
    Ok(match url.port() {
        Some(port) => format!("{parsed}:{port}"),
        None => parsed.to_string(),
    })
}

/// Why login must stop, when `copilot_internal/user` explicitly says the
/// account cannot use Copilot chat.
pub fn entitlement_error(user: &CopilotUser) -> Option<String> {
    if user.chat_enabled != Some(false) {
        return None;
    }
    Some(if user.can_signup_for_limited == Some(true) {
        "this GitHub account is not signed up for GitHub Copilot; sign up for Copilot Free at \
         https://github.com/features/copilot/plans and log in again"
            .into()
    } else {
        "this GitHub account has no GitHub Copilot access: it needs an active Copilot \
         subscription or a seat assigned by an organization"
            .into()
    })
}

/// The Copilot API base a stored token routes to: the account-assigned
/// endpoint from login, else the enterprise host's `copilot-api.` alias,
/// else the public default.
pub fn api_base(tokens: &Tokens) -> String {
    if let Some(endpoint) = tokens
        .extra
        .get(API_ENDPOINT)
        .and_then(Value::as_str)
        .filter(|e| e.starts_with("https://") && crate::routes::validate_base_url(e).is_ok())
    {
        return endpoint.trim_end_matches('/').to_string();
    }
    if let Some(domain) = tokens.extra.get(ENTERPRISE_URL).and_then(Value::as_str) {
        if let Ok(domain) = normalize_domain(domain) {
            return format!("https://copilot-api.{domain}");
        }
    }
    DEFAULT_API_BASE.to_string()
}

/// Device-flow client for one GitHub deployment.
pub struct DeviceFlowClient {
    /// `https://github.com` or `https://<enterprise-host>`.
    web_base: String,
    /// `https://api.github.com` or `https://api.<enterprise-host>`.
    api_base: String,
    /// Recorded on the stored token so the API host can be derived later.
    enterprise: Option<String>,
    http: reqwest::Client,
    margin: Duration,
}

impl Default for DeviceFlowClient {
    fn default() -> Self {
        Self {
            web_base: "https://github.com".into(),
            api_base: "https://api.github.com".into(),
            enterprise: None,
            http: crate::oauth_http_client(crate::OAUTH_REQUEST_TIMEOUT),
            margin: POLL_SAFETY_MARGIN,
        }
    }
}

impl DeviceFlowClient {
    /// GitHub Enterprise (data residency `*.ghe.com` or self-hosted).
    pub fn enterprise(domain: &str) -> Result<Self, AuthError> {
        let domain = normalize_domain(domain)?;
        Ok(Self {
            web_base: format!("https://{domain}"),
            api_base: format!("https://api.{domain}"),
            enterprise: Some(domain),
            ..Self::default()
        })
    }

    /// Point both hosts at a mock server and poll without the safety
    /// margin (tests).
    pub fn with_base_url(base: &str) -> Self {
        Self {
            web_base: base.trim_end_matches('/').into(),
            api_base: base.trim_end_matches('/').into(),
            margin: Duration::ZERO,
            ..Self::default()
        }
    }

    async fn post_json(&self, url: &str, body: &Value) -> Result<Value, AuthError> {
        let response = self
            .http
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, crate::copilot::USER_AGENT)
            .json(body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthError::OAuth(format!("{url}: http {status}: {text}")));
        }
        serde_json::from_str(&text).map_err(|e| AuthError::OAuth(format!("{url}: bad json: {e}")))
    }

    /// `copilot_internal/user` for `token`. `None` when the lookup itself
    /// fails: a GitHub hiccup must not turn into a login denial.
    pub async fn copilot_user(&self, token: &str) -> Option<CopilotUser> {
        let response = self
            .http
            .get(format!("{}/copilot_internal/user", self.api_base))
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, crate::copilot::USER_AGENT)
            .header("X-GitHub-Api-Version", USER_API_VERSION)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json().await.ok()
    }

    /// Run the device flow to completion and return the tokens to store.
    pub async fn login(&self, prompt: &DevicePrompt) -> Result<Tokens, AuthError> {
        let device: DeviceCode = serde_json::from_value(
            self.post_json(
                &format!("{}/login/device/code", self.web_base),
                &serde_json::json!({ "client_id": CLIENT_ID, "scope": SCOPE }),
            )
            .await?,
        )
        .map_err(|e| AuthError::OAuth(format!("bad device code response: {e}")))?;
        (prompt.on_code)(&device.verification_uri, &device.user_code);

        let ttl = if device.expires_in > 0 {
            device.expires_in
        } else {
            DEFAULT_DEVICE_TTL_SECS
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(ttl);
        let mut wait = Duration::from_secs(device.interval.max(1));
        let access = loop {
            tokio::time::sleep(wait + self.margin).await;
            if tokio::time::Instant::now() >= deadline {
                return Err(AuthError::OAuth(
                    "device code expired before it was approved; run the login again".into(),
                ));
            }
            let poll: TokenPoll = serde_json::from_value(
                self.post_json(
                    &format!("{}/login/oauth/access_token", self.web_base),
                    &serde_json::json!({
                        "client_id": CLIENT_ID,
                        "device_code": device.device_code,
                        "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                    }),
                )
                .await?,
            )
            .map_err(|e| AuthError::OAuth(format!("bad token response: {e}")))?;
            if let Some(token) = poll.access_token.filter(|t| !t.is_empty()) {
                break token;
            }
            match poll.error.as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    wait = match poll.interval {
                        Some(seconds) if seconds > 0 => Duration::from_secs(seconds),
                        _ => wait + Duration::from_secs(5),
                    };
                }
                other => {
                    let detail = poll.error_description.unwrap_or_default();
                    return Err(AuthError::OAuth(format!(
                        "device authorization failed: {} {detail}",
                        other.unwrap_or("no token")
                    )));
                }
            }
        };

        let user = self.copilot_user(&access).await;
        if let Some(denied) = user.as_ref().and_then(entitlement_error) {
            return Err(AuthError::OAuth(denied));
        }
        let mut tokens = Tokens {
            access_token: access,
            ..Default::default()
        };
        if let Some(domain) = &self.enterprise {
            tokens
                .extra
                .insert(ENTERPRISE_URL.into(), Value::String(domain.clone()));
        }
        if let Some(user) = user {
            if let Some(endpoint) = user
                .endpoints
                .and_then(|e| e.api)
                .map(|e| e.trim_end_matches('/').to_string())
                // The GitHub token is sent there: HTTPS only.
                .filter(|e| {
                    e.starts_with("https://") && crate::routes::validate_base_url(e).is_ok()
                })
            {
                tokens
                    .extra
                    .insert(API_ENDPOINT.into(), Value::String(endpoint));
            }
            if let Some(plan) = user.copilot_plan.filter(|p| !p.is_empty()) {
                tokens.subscription_type = plan;
            }
        }
        Ok(tokens)
    }
}

/// Log in on github.com (or `enterprise`) and persist under `credential_key`.
pub async fn login_as(
    store: &CredentialStore,
    prompt: &DevicePrompt,
    credential_key: &str,
    enterprise: Option<&str>,
) -> Result<Tokens, AuthError> {
    let client = match enterprise {
        Some(domain) => DeviceFlowClient::enterprise(domain)?,
        None => DeviceFlowClient::default(),
    };
    login_with(&client, store, prompt, credential_key).await
}

pub async fn login_with(
    client: &DeviceFlowClient,
    store: &CredentialStore,
    prompt: &DevicePrompt,
    credential_key: &str,
) -> Result<Tokens, AuthError> {
    let tokens = client.login(prompt).await?;
    store.save_tokens(credential_key, &tokens)?;
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_normalize_and_reject_non_hosts() {
        for (input, want) in [
            ("company.ghe.com", "company.ghe.com"),
            ("https://Company.ghe.com/", "company.ghe.com"),
            ("http://git.corp:8443", "git.corp:8443"),
        ] {
            assert_eq!(normalize_domain(input).unwrap(), want);
        }
        for bad in [
            "",
            "https://",
            "a/b",
            "user@host",
            "host?x",
            "a b",
            "a\\b",
            "h%2fx",
            "host:abc",
        ] {
            assert!(normalize_domain(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_an_explicit_denial_blocks_login() {
        assert!(entitlement_error(&CopilotUser::default()).is_none());
        let enabled = CopilotUser {
            chat_enabled: Some(true),
            ..Default::default()
        };
        assert!(entitlement_error(&enabled).is_none());
        let signup = CopilotUser {
            chat_enabled: Some(false),
            can_signup_for_limited: Some(true),
            ..Default::default()
        };
        assert!(entitlement_error(&signup).unwrap().contains("Copilot Free"));
        let none = CopilotUser {
            chat_enabled: Some(false),
            ..Default::default()
        };
        assert!(entitlement_error(&none).unwrap().contains("seat"));
    }

    #[test]
    fn api_base_prefers_account_endpoint_then_enterprise_then_default() {
        let mut tokens = Tokens::default();
        assert_eq!(api_base(&tokens), DEFAULT_API_BASE);
        tokens
            .extra
            .insert(ENTERPRISE_URL.into(), "corp.ghe.com".into());
        assert_eq!(api_base(&tokens), "https://copilot-api.corp.ghe.com");
        tokens.extra.insert(
            API_ENDPOINT.into(),
            "https://api.enterprise.githubcopilot.com/".into(),
        );
        assert_eq!(
            api_base(&tokens),
            "https://api.enterprise.githubcopilot.com"
        );
    }
}
