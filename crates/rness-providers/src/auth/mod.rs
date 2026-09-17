//! Anthropic OAuth (PKCE) — the `rness auth` flow.
//!
//! Endpoints, client id, scopes, and header attribution match Claude Code
//! so Pro/Max subscription auth works. Credentials persist through
//! [`store::CredentialStore`] (`~/.rness/credentials.json`).

pub mod openai;
pub mod pkce;
pub mod store;

use serde::Deserialize;
use serde_json::json;

pub use store::{CredentialStore, Tokens};

#[derive(Debug, thiserror::Error)]
pub enum TokensError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("credentials file: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    Store(#[from] TokensError),
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("oauth: {0}")]
    OAuth(String),
    #[error("no credentials: run `rness auth login` or set ANTHROPIC_API_KEY")]
    NoCredentials,
}

// -- config -----------------------------------------------------------------

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// Refresh when within this many seconds of expiry.
pub const REFRESH_BUFFER_SECS: i64 = 300;

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub profile_url: String,
    pub scopes: Vec<String>,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            client_id: CLIENT_ID.into(),
            authorize_url: AUTHORIZE_URL.into(),
            token_url: TOKEN_URL.into(),
            profile_url: PROFILE_URL.into(),
            scopes: SCOPES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

// -- token endpoint client -------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    account: Option<serde_json::Value>,
}

impl TokenResponse {
    fn into_tokens(self) -> Tokens {
        let mut tokens = Tokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: (self.expires_in > 0).then(|| {
                (jiff::Timestamp::now() + jiff::SignedDuration::from_secs(self.expires_in))
                    .to_string()
            }),
            scopes: self
                .scope
                .split(' ')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            ..Default::default()
        };
        if let Some(account) = self.account {
            // Stored under the camelCase field name.
            tokens.extra.insert(
                "tokenAccount".into(),
                json!({
                    "uuid": account["uuid"],
                    "emailAddress": account["email_address"],
                    "organizationUuid": account["organization_uuid"],
                }),
            );
        }
        tokens
    }
}

pub struct OAuthClient {
    config: OAuthConfig,
    http: reqwest::Client,
}

impl OAuthClient {
    pub fn new(config: OAuthConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }

    pub fn authorize_url(&self, challenge: &str, state: &str, redirect_uri: &str) -> String {
        let mut url = reqwest::Url::parse(&self.config.authorize_url).expect("static url");
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("response_type", "code")
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("scope", &self.config.scopes.join(" "));
        url.to_string()
    }

    async fn token_request(&self, body: serde_json::Value) -> Result<Tokens, AuthError> {
        let response = self
            .http
            .post(&self.config.token_url)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthError::OAuth(format!(
                "token endpoint http {status}: {text}"
            )));
        }
        let parsed: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| AuthError::OAuth(format!("bad token response: {e}")))?;
        Ok(parsed.into_tokens())
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        state: &str,
    ) -> Result<Tokens, AuthError> {
        self.token_request(json!({
            "grant_type": "authorization_code",
            "code": code,
            "code_verifier": verifier,
            "client_id": self.config.client_id,
            "redirect_uri": redirect_uri,
            "state": state,
        }))
        .await
    }

    pub async fn refresh(
        &self,
        refresh_token: &str,
        scopes: &[String],
    ) -> Result<Tokens, AuthError> {
        self.token_request(json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": self.config.client_id,
            "scope": scopes.join(" "),
        }))
        .await
    }

    /// Fetch the user profile (subscription type etc.). Non-fatal helper.
    pub async fn profile(&self, access_token: &str) -> Result<serde_json::Value, AuthError> {
        let response = self
            .http
            .get(&self.config.profile_url)
            .bearer_auth(access_token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(AuthError::OAuth(format!(
                "profile http {}",
                response.status()
            )));
        }
        Ok(response.json().await?)
    }
}

// -- login flow ------------------------------------------------------------

/// Where the interactive login reports progress (CLI prints these).
pub struct LoginPrompt {
    /// Called with the authorize URL (after attempting to open a browser).
    pub on_url: Box<dyn Fn(&str) + Send + Sync>,
}

/// Run the full PKCE login: local callback server → browser → code
/// exchange → profile fetch → persist to `store`.
pub async fn login(
    config: OAuthConfig,
    store: &CredentialStore,
    prompt: &LoginPrompt,
) -> Result<Tokens, AuthError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let verifier = pkce::code_verifier();
    let challenge = pkce::code_challenge(&verifier);
    let state = pkce::state();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| AuthError::OAuth(format!("callback listener: {e}")))?;
    let port = listener.local_addr().map_err(TokensError::Io)?.port();
    let redirect_uri = format!("http://localhost:{port}/callback");

    let client = OAuthClient::new(config);
    let url = client.authorize_url(&challenge, &state, &redirect_uri);
    let _ = open::that(&url);
    (prompt.on_url)(&url);

    // Accept connections until we get the callback (browsers may probe
    // with favicon requests first).
    let (code, cb_state) = loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| AuthError::OAuth(format!("callback accept: {e}")))?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.map_err(TokensError::Io)?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(query) = parse_callback_query(&request) else {
            let _ = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                .await;
            continue;
        };

        if let Some(err) = query_param(&query, "error") {
            let desc = query_param(&query, "error_description").unwrap_or_default();
            let _ = respond_html(&mut stream, "Authentication failed").await;
            return Err(AuthError::OAuth(format!("{err}: {desc}")));
        }
        let code = query_param(&query, "code").unwrap_or_default();
        let cb_state = query_param(&query, "state").unwrap_or_default();
        if code.is_empty() {
            let _ = respond_html(&mut stream, "No authorization code").await;
            return Err(AuthError::OAuth("no authorization code received".into()));
        }
        let _ = respond_html(
            &mut stream,
            "Authentication successful! You can close this tab and return to the terminal.",
        )
        .await;
        break (code, cb_state);
    };

    if cb_state != state {
        return Err(AuthError::OAuth("state mismatch (possible CSRF)".into()));
    }

    let mut tokens = client
        .exchange_code(&code, &verifier, &redirect_uri, &state)
        .await?;

    // Profile fetch is best-effort.
    if let Ok(profile) = client.profile(&tokens.access_token).await {
        if let Some(st) = profile["subscription_type"].as_str() {
            tokens.subscription_type = st.to_string();
        }
        if let Some(rt) = profile["rate_limit_tier"].as_str() {
            tokens.rate_limit_tier = rt.to_string();
        }
        tokens.extra.insert("profile".into(), profile);
    }

    store.save_tokens("anthropic", &tokens)?;
    Ok(tokens)
}

/// Extract the query string from `GET <path>?... HTTP/1.1`.
pub(crate) fn callback_query(request: &str, path: &str) -> Option<String> {
    let line = request.lines().next()?;
    let target = line.split_whitespace().nth(1)?;
    let (route, query) = target.split_once('?')?;
    (route == path).then(|| query.to_string())
}

fn parse_callback_query(request: &str) -> Option<String> {
    callback_query(request, "/callback")
}

pub(crate) fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| url_decode(v))
    })
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) async fn respond_html(
    stream: &mut tokio::net::TcpStream,
    message: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = format!(
        "<!DOCTYPE html><html><body><h2>{message}</h2><script>window.close()</script></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

// -- credential resolution (per-request) -----------------------------------

/// A resolved credential, ready to attach to a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Credential {
    ApiKey(String),
    /// OAuth bearer token (Claude Pro/Max) — requires the Claude Code
    /// spoof headers.
    OAuth(String),
}

/// Resolves credentials per request: env var > stored API key > stored
/// OAuth tokens (refreshing when near expiry).
pub struct CredentialSource {
    store: CredentialStore,
    oauth_key: Option<String>,
    client: OAuthClient,
    refresh_lock: tokio::sync::Mutex<()>,
}

impl CredentialSource {
    pub fn new(store: CredentialStore) -> Self {
        Self {
            store,
            oauth_key: None,
            client: OAuthClient::new(OAuthConfig::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Override the OAuth client (tests: point the token endpoint at a mock).
    pub fn with_oauth_client(mut self, client: OAuthClient) -> Self {
        self.client = client;
        self
    }

    pub fn store(&self) -> &CredentialStore {
        &self.store
    }

    pub fn oauth_only(mut self, credential: String) -> Self {
        self.oauth_key = Some(credential);
        self
    }

    fn token_key(&self) -> &str {
        self.oauth_key.as_deref().unwrap_or("anthropic")
    }

    pub async fn resolve(&self) -> Result<Credential, AuthError> {
        if self.oauth_key.is_none() {
            if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
                if !key.is_empty() {
                    return Ok(Credential::ApiKey(key));
                }
            }
            if let Some(key) = self.store.api_key("anthropic")? {
                return Ok(Credential::ApiKey(key));
            }
        }
        if let Some(tokens) = self.store.tokens(self.token_key())? {
            if !tokens.access_token.is_empty() {
                let tokens =
                    if tokens.is_expired(REFRESH_BUFFER_SECS) && !tokens.refresh_token.is_empty() {
                        self.refresh(tokens).await?
                    } else {
                        tokens
                    };
                return Ok(Credential::OAuth(tokens.access_token));
            }
        }
        Err(AuthError::NoCredentials)
    }

    /// Force-refresh after a 401. Returns the new credential, or the
    /// original error if refresh is impossible.
    pub async fn handle_unauthorized(&self) -> Result<Credential, AuthError> {
        let tokens = self
            .store
            .tokens(self.token_key())?
            .filter(|t| !t.refresh_token.is_empty())
            .ok_or(AuthError::NoCredentials)?;
        let refreshed = self.refresh(tokens).await?;
        Ok(Credential::OAuth(refreshed.access_token))
    }

    async fn refresh(&self, old: Tokens) -> Result<Tokens, AuthError> {
        let _guard = self.refresh_lock.lock().await;
        // Double-check: another task may have refreshed while we waited.
        if let Some(current) = self.store.tokens(self.token_key())? {
            if current.access_token != old.access_token && !current.is_expired(REFRESH_BUFFER_SECS)
            {
                return Ok(current);
            }
        }
        let mut new = self.client.refresh(&old.refresh_token, &old.scopes).await?;
        // Preserve fields the refresh response does not return. In particular,
        // an OAuth server may not rotate refresh tokens on every grant.
        if new.refresh_token.is_empty() {
            new.refresh_token = old.refresh_token.clone();
        }
        if new.subscription_type.is_empty() {
            new.subscription_type = old.subscription_type;
        }
        if new.rate_limit_tier.is_empty() {
            new.rate_limit_tier = old.rate_limit_tier;
        }
        for (k, v) in old.extra {
            new.extra.entry(k).or_insert(v);
        }
        self.store.save_tokens(self.token_key(), &new)?;
        Ok(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_contains_pkce_params() {
        let client = OAuthClient::new(OAuthConfig::default());
        let url = client.authorize_url("chal", "st", "http://localhost:1234/callback");
        assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("scope=user%3Aprofile+user%3Ainference"));
    }

    #[test]
    fn callback_query_parsing() {
        let req = "GET /callback?code=abc%2F1&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let q = parse_callback_query(req).unwrap();
        assert_eq!(query_param(&q, "code").unwrap(), "abc/1");
        assert_eq!(query_param(&q, "state").unwrap(), "xyz");
        assert!(query_param(&q, "error").is_none());
        assert!(parse_callback_query("GET /favicon.ico HTTP/1.1\r\n\r\n").is_none());
    }
}
