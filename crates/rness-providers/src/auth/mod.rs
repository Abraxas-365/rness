//! Anthropic OAuth (PKCE) — the `rness auth` flow.
//!
//! Endpoints, client id, scopes, and header attribution match Claude Code
//! so Pro/Max subscription auth works. Credentials persist through
//! [`store::CredentialStore`] (`~/.rness/credentials.json`).

pub mod github_copilot;
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
    #[error(
        "credentials file {} is corrupt ({source}); fix or delete it, then run `rness auth login`",
        path.display()
    )]
    Corrupt {
        path: std::path::PathBuf,
        source: serde_json::Error,
    },
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
    /// The token endpoint rejected the refresh token (`invalid_grant`):
    /// revoked, expired, or already spent by another client.
    #[error("refresh token revoked or already used: run `rness auth login` ({0})")]
    InvalidGrant(String),
}

/// Does a token-endpoint error response say the grant itself is bad?
/// Standard OAuth `{"error":"invalid_grant"}`, plus the `refresh_token_*`
/// codes some issuers put in `error` or `error.code`.
pub(crate) fn is_invalid_grant(status: reqwest::StatusCode, body: &str) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST && status != reqwest::StatusCode::UNAUTHORIZED {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let code = match &value["error"] {
        serde_json::Value::String(code) => code.as_str(),
        serde_json::Value::Object(error) => error
            .get("code")
            .and_then(|c| c.as_str())
            .unwrap_or_default(),
        _ => "",
    };
    code == "invalid_grant" || code.starts_with("refresh_token_")
}

/// How long a refresh waits for another process's refresh to finish.
pub const REFRESH_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Cross-process OAuth refresh for the tokens under `key`.
///
/// Holds the store's refresh lease (sidecar file lock) across the grant so
/// only one process spends a rotating refresh token. Under the lease it
/// re-reads the store: tokens another process already refreshed are
/// adopted; otherwise `grant` runs with the freshest stored tokens and the
/// result is saved before the lease is released. If the endpoint still
/// says `invalid_grant` (a client that does not take the lease rotated
/// it), tokens rotated in the store meanwhile are adopted.
pub(crate) async fn refresh_shared<F, Fut>(
    store: &CredentialStore,
    key: &str,
    old: Tokens,
    grant: F,
) -> Result<Tokens, AuthError>
where
    F: FnOnce(Tokens) -> Fut,
    Fut: std::future::Future<Output = Result<Tokens, AuthError>>,
{
    let lease = store.lock_for_refresh(REFRESH_LOCK_WAIT).await?;
    let base = match store.tokens(key)? {
        Some(current)
            if current.access_token != old.access_token
                && !current.access_token.is_empty()
                && !current.is_expired(REFRESH_BUFFER_SECS) =>
        {
            return Ok(current);
        }
        // Someone rotated the refresh token but the access token is stale
        // again: spend the newest refresh token, not ours.
        Some(current) if !current.refresh_token.is_empty() => current,
        _ => old,
    };
    let sent = base.refresh_token.clone();
    match grant(base).await {
        Ok(new) => {
            store.save_tokens_locked(&lease, key, &new)?;
            Ok(new)
        }
        Err(AuthError::InvalidGrant(detail)) => match store.tokens(key)? {
            Some(current)
                if current.refresh_token != sent
                    && !current.access_token.is_empty()
                    && !current.is_expired(REFRESH_BUFFER_SECS) =>
            {
                Ok(current)
            }
            _ => Err(AuthError::InvalidGrant(detail)),
        },
        Err(e) => Err(e),
    }
}

// -- config -----------------------------------------------------------------

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// Where the manual (no local callback) login sends the browser: the page
/// shows the code to paste back, as `<code>#<state>`.
pub const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";
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
    pub manual_redirect_url: String,
    pub profile_url: String,
    pub scopes: Vec<String>,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            client_id: CLIENT_ID.into(),
            authorize_url: AUTHORIZE_URL.into(),
            token_url: TOKEN_URL.into(),
            manual_redirect_url: MANUAL_REDIRECT_URL.into(),
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
            http: crate::oauth_http_client(crate::OAUTH_REQUEST_TIMEOUT),
        }
    }

    /// Override the total request timeout (tests).
    #[cfg(test)]
    pub(crate) fn with_request_timeout(mut self, total: std::time::Duration) -> Self {
        self.http = crate::oauth_http_client(total);
        self
    }

    pub fn authorize_url(&self, challenge: &str, state: &str, redirect_uri: &str) -> String {
        self.authorize_url_with(challenge, state, redirect_uri, false)
    }

    /// The authorize URL of the manual login: redirects to
    /// [`OAuthConfig::manual_redirect_url`] and asks the page to show the
    /// code (`code=true`) instead of handing it to a local callback.
    pub fn manual_authorize_url(&self, challenge: &str, state: &str) -> String {
        self.authorize_url_with(challenge, state, &self.config.manual_redirect_url, true)
    }

    fn authorize_url_with(
        &self,
        challenge: &str,
        state: &str,
        redirect_uri: &str,
        show_code: bool,
    ) -> String {
        let mut url = reqwest::Url::parse(&self.config.authorize_url).expect("static url");
        if show_code {
            url.query_pairs_mut().append_pair("code", "true");
        }
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
        if is_invalid_grant(status, &text) {
            return Err(AuthError::InvalidGrant(format!("http {status}: {text}")));
        }
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
    login_as(config, store, prompt, "anthropic").await
}

/// Like [`login`] but saves tokens under an arbitrary credential key
/// (e.g. `"anthropic/work"` for a named account).
pub async fn login_as(
    config: OAuthConfig,
    store: &CredentialStore,
    prompt: &LoginPrompt,
    credential_key: &str,
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

    finish_login(
        &client,
        store,
        credential_key,
        &code,
        &verifier,
        &redirect_uri,
        &state,
    )
    .await
}

/// Where the manual login reports progress (CLI prints these).
pub struct ManualLoginPrompt {
    /// Called once with the authorize URL to open in any browser.
    pub on_url: Box<dyn Fn(&str) + Send + Sync>,
    /// Called when a pasted line isn't this login's code, with why. `true`
    /// reads the next line (a person at a terminal retries); `false` ends
    /// the login with that reason (a relay or script gets a clear failure
    /// instead of a login waiting for another line).
    pub on_invalid: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

/// The PKCE login without a browser or a local callback, for machines
/// where neither works (containers, SSH): the person opens the URL
/// anywhere, signs in, and the page shows `<code>#<state>`, which is read
/// from `input` (one line; for a line that isn't this login's code,
/// `on_invalid` decides between reading another and failing). End of
/// input before a code is an error.
pub async fn login_manual_as<R>(
    config: OAuthConfig,
    store: &CredentialStore,
    prompt: &ManualLoginPrompt,
    input: R,
    credential_key: &str,
) -> Result<Tokens, AuthError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let verifier = pkce::code_verifier();
    let challenge = pkce::code_challenge(&verifier);
    let state = pkce::state();
    let client = OAuthClient::new(config);
    let url = client.manual_authorize_url(&challenge, &state);
    (prompt.on_url)(&url);

    let mut lines = input.lines();
    let code = loop {
        let Some(line) = lines.next_line().await.map_err(TokensError::Io)? else {
            return Err(AuthError::OAuth("no authorization code entered".into()));
        };
        match pasted_code(&line, &state) {
            Ok(Some(code)) => break code,
            Ok(None) => {}
            Err(why) => {
                if !(prompt.on_invalid)(why) {
                    return Err(AuthError::OAuth(why.into()));
                }
            }
        }
    };
    let redirect_uri = client.config.manual_redirect_url.clone();
    finish_login(
        &client,
        store,
        credential_key,
        &code,
        &verifier,
        &redirect_uri,
        &state,
    )
    .await
}

/// The code in a pasted line `<code>#<state>` of the login whose state is
/// `state`: `Ok(None)` for a blank line, `Err` (why, for the person) for
/// anything else that isn't it.
fn pasted_code(line: &str, state: &str) -> Result<Option<String>, &'static str> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let Some((code, got)) = line.split_once('#') else {
        return Err("that isn't the whole code: copy it with the part after #");
    };
    if code.is_empty() || got.is_empty() || code.contains(char::is_whitespace) {
        return Err("that isn't the whole code: copy it with the part after #");
    }
    if got != state {
        return Err("that code is from another sign-in: paste the one this sign-in's page shows");
    }
    Ok(Some(code.to_string()))
}

/// Exchange the code, fetch the profile (best effort) and save the tokens.
async fn finish_login(
    client: &OAuthClient,
    store: &CredentialStore,
    credential_key: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<Tokens, AuthError> {
    let mut tokens = client
        .exchange_code(code, verifier, redirect_uri, state)
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

    store.save_tokens(credential_key, &tokens)?;
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
        // Lock order: in-process mutex, then the cross-process lease.
        let _guard = self.refresh_lock.lock().await;
        refresh_shared(&self.store, self.token_key(), old, |old| async move {
            let mut new = self.client.refresh(&old.refresh_token, &old.scopes).await?;
            // Preserve fields the refresh response does not return. In
            // particular, an OAuth server may not rotate refresh tokens on
            // every grant.
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
            Ok(new)
        })
        .await
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
    fn invalid_grant_detection() {
        use reqwest::StatusCode as S;
        let bad = S::BAD_REQUEST;
        assert!(is_invalid_grant(bad, r#"{"error":"invalid_grant"}"#));
        assert!(is_invalid_grant(
            S::UNAUTHORIZED,
            r#"{"error":{"code":"refresh_token_reused"}}"#
        ));
        assert!(is_invalid_grant(
            bad,
            r#"{"error":"refresh_token_expired"}"#
        ));
        assert!(!is_invalid_grant(bad, r#"{"error":"invalid_request"}"#));
        assert!(!is_invalid_grant(bad, "not json"));
        assert!(!is_invalid_grant(
            S::INTERNAL_SERVER_ERROR,
            r#"{"error":"invalid_grant"}"#
        ));
    }

    /// A token endpoint that accepts the request and never answers.
    pub(crate) async fn stalling_endpoint() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        url
    }

    pub(crate) fn expired(access: &str) -> Tokens {
        Tokens {
            access_token: access.into(),
            refresh_token: "rt0".into(),
            expires_at: Some(
                (jiff::Timestamp::now() - jiff::SignedDuration::from_secs(60)).to_string(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn oauth_requests_have_a_total_timeout_within_the_lease_wait() {
        assert_eq!(crate::OAUTH_REQUEST_TIMEOUT.as_secs(), 30);
        assert!(crate::OAUTH_REQUEST_TIMEOUT <= REFRESH_LOCK_WAIT);
    }

    #[tokio::test]
    async fn stalled_token_endpoint_times_out_and_releases_the_refresh_lease() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = CredentialStore::new(&path);
        store.save_tokens("anthropic", &expired("at0")).unwrap();
        let client = OAuthClient::new(OAuthConfig {
            token_url: format!("{}/token", stalling_endpoint().await),
            ..Default::default()
        })
        .with_request_timeout(std::time::Duration::from_millis(300));
        let source = CredentialSource::new(CredentialStore::new(&path))
            .with_oauth_client(client)
            .oauth_only("anthropic".into());
        let started = std::time::Instant::now();
        let err = tokio::time::timeout(std::time::Duration::from_secs(10), source.resolve())
            .await
            .expect("refresh must not hang on a stalled endpoint")
            .unwrap_err();
        assert!(
            matches!(&err, AuthError::Transport(e) if e.is_timeout()),
            "{err}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // The lease is free again and the stored tokens are untouched.
        store
            .lock_for_refresh(std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(
            store.tokens("anthropic").unwrap().unwrap().access_token,
            "at0"
        );
    }

    #[test]
    fn manual_authorize_url_shows_the_code_on_anthropics_page() {
        let client = OAuthClient::new(OAuthConfig::default());
        let url = client.manual_authorize_url("chal", "st");
        let parsed = reqwest::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert!(url.starts_with("https://claude.com/cai/oauth/authorize?code=true&"));
        assert_eq!(q["redirect_uri"], MANUAL_REDIRECT_URL);
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["state"], "st");
        assert_eq!(q["response_type"], "code");
        // The callback login never asks for the code to be shown.
        assert!(!client
            .authorize_url("chal", "st", "http://localhost:1/callback")
            .contains("code=true"));
    }

    #[test]
    fn pasted_code_needs_this_logins_state() {
        assert_eq!(pasted_code("  abc#st \n", "st"), Ok(Some("abc".into())));
        assert_eq!(pasted_code("   ", "st"), Ok(None));
        assert!(pasted_code("abc", "st").is_err());
        assert!(pasted_code("#st", "st").is_err());
        assert!(pasted_code("abc#", "st").is_err());
        assert!(pasted_code("a bc#st", "st").is_err());
        assert!(pasted_code("abc#other", "st")
            .unwrap_err()
            .contains("another sign-in"));
    }

    /// A token endpoint that answers one exchange with `answer` and hands
    /// the request body it got to the test.
    async fn token_endpoint(
        answer: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<serde_json::Value>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let body = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break body[..len].to_string();
                    }
                }
                if n == 0 {
                    break String::new();
                }
            };
            let _ = tx.send(serde_json::from_str(&body).unwrap_or_default());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                answer.len()
            );
            sock.write_all(response.as_bytes()).await.unwrap();
        });
        (url, rx)
    }

    #[tokio::test]
    async fn manual_login_exchanges_the_pasted_code_and_saves_the_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("credentials.json"));
        let (endpoint, body) = token_endpoint(
            r#"{"access_token":"at1","refresh_token":"rt1","expires_in":3600,"scope":"user:inference user:profile"}"#,
        )
        .await;
        let config = OAuthConfig {
            token_url: format!("{endpoint}/token"),
            // No profile server (a closed port): the fetch fails and is skipped.
            profile_url: {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                format!("http://{}/profile", l.local_addr().unwrap())
            },
            ..Default::default()
        };
        let shown = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let invalid = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (s, i) = (shown.clone(), invalid.clone());
        let prompt = ManualLoginPrompt {
            on_url: Box::new(move |url| *s.lock().unwrap() = url.to_string()),
            on_invalid: Box::new(move |why| {
                i.lock().unwrap().push(why.to_string());
                true
            }),
        };
        // The person pastes after seeing the URL: feed stdin through a pipe
        // once it was shown, with a wrong line first.
        let (mut writer, reader) = tokio::io::duplex(1024);
        let feeder = {
            let shown = shown.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let state = loop {
                    let url = shown.lock().unwrap().clone();
                    if !url.is_empty() {
                        let parsed = reqwest::Url::parse(&url).unwrap();
                        break parsed
                            .query_pairs()
                            .find(|(k, _)| k == "state")
                            .unwrap()
                            .1
                            .into_owned();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                };
                writer.write_all(b"abc\n").await.unwrap();
                writer
                    .write_all(format!("the-code#{state}\n").as_bytes())
                    .await
                    .unwrap();
                state
            })
        };
        let tokens = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            login_manual_as(
                config,
                &store,
                &prompt,
                tokio::io::BufReader::new(reader),
                "anthropic",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let state = feeder.await.unwrap();
        assert_eq!(tokens.access_token, "at1");
        let saved = store.tokens("anthropic").unwrap().unwrap();
        assert_eq!(saved.access_token, "at1");
        assert_eq!(saved.refresh_token, "rt1");
        assert_eq!(invalid.lock().unwrap().len(), 1);
        let body = body.await.unwrap();
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["state"], state.as_str());
        assert_eq!(body["redirect_uri"], MANUAL_REDIRECT_URL);
        assert!(!body["code_verifier"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn manual_login_without_a_code_fails_and_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("credentials.json"));
        let prompt = ManualLoginPrompt {
            on_url: Box::new(|_| {}),
            on_invalid: Box::new(|_| true),
        };
        let err = login_manual_as(
            OAuthConfig::default(),
            &store,
            &prompt,
            tokio::io::BufReader::new(&b"\nnope\n"[..]),
            "anthropic",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no authorization code"), "{err}");
        assert!(store.tokens("anthropic").unwrap().is_none());

        // A caller that doesn't retry fails on the first wrong line, with why.
        let prompt = ManualLoginPrompt {
            on_url: Box::new(|_| {}),
            on_invalid: Box::new(|_| false),
        };
        let err = login_manual_as(
            OAuthConfig::default(),
            &store,
            &prompt,
            tokio::io::BufReader::new(&b"abc#other\nnever-read#x\n"[..]),
            "anthropic",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("another sign-in"), "{err}");
        assert!(store.tokens("anthropic").unwrap().is_none());
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
