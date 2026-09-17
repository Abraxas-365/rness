//! "Sign in with ChatGPT" — OpenAI OAuth (PKCE) for using a ChatGPT
//! Plus/Pro subscription through the Codex backend.
//!
//! Endpoints and the public client id match Codex CLI. The account id
//! rides inside the id_token JWT (`https://api.openai.com/auth` claim);
//! we base64-decode the payload without verifying — the server verifies,
//! we only route.

use serde::Deserialize;
use serde_json::Value;

use super::{AuthError, LoginPrompt, TokensError, callback_query, pkce, query_param, respond_html};
use crate::auth::{CredentialStore, Tokens};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const SCOPE: &str = "openid profile email offline_access";
/// The redirect is registered for exactly this port and path.
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_PATH: &str = "/auth/callback";
/// JWT claim namespace where ChatGPT embeds auth metadata.
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

/// The provider key these tokens are stored under.
pub const STORE_KEY: &str = "openai-chatgpt";

/// Refresh when within this many seconds of expiry.
pub const REFRESH_BUFFER_SECS: i64 = 300;

// -- token endpoint client -------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub id_token: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: i64,
}

impl TokenResponse {
    /// Convert to persisted form, extracting account id + email from JWTs.
    pub fn into_tokens(self) -> Tokens {
        let expires_in = if self.expires_in > 0 {
            self.expires_in
        } else {
            3600
        };
        let mut tokens = Tokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: Some(
                (jiff::Timestamp::now() + jiff::SignedDuration::from_secs(expires_in)).to_string(),
            ),
            ..Default::default()
        };
        if let Some(id) =
            extract_account_id(&self.id_token).or_else(|| extract_account_id(&tokens.access_token))
        {
            tokens.extra.insert("accountId".into(), Value::String(id));
        }
        if let Some(email) =
            jwt_claims(&self.id_token).and_then(|c| c["email"].as_str().map(String::from))
        {
            tokens.extra.insert("email".into(), Value::String(email));
        }
        tokens
    }
}

/// The ChatGPT account id a stored token routes to.
pub fn account_id(tokens: &Tokens) -> Option<&str> {
    tokens.extra.get("accountId").and_then(|v| v.as_str())
}

/// Decode a JWT payload (no verification — routing metadata only).
fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn extract_account_id(token: &str) -> Option<String> {
    let claims = jwt_claims(token)?;
    if let Some(id) = claims["chatgpt_account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        return Some(id.to_string());
    }
    if let Some(id) = claims[JWT_CLAIM_PATH]["chatgpt_account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        return Some(id.to_string());
    }
    // Fallback: first organization id.
    claims["organizations"][0]["id"].as_str().map(String::from)
}

/// OAuth client for auth.openai.com. Token requests are form-encoded
/// (unlike Anthropic's JSON endpoint).
pub struct CodexOAuthClient {
    token_url: String,
    authorize_url: String,
    http: reqwest::Client,
}

impl Default for CodexOAuthClient {
    fn default() -> Self {
        Self {
            token_url: TOKEN_URL.into(),
            authorize_url: AUTHORIZE_URL.into(),
            http: reqwest::Client::new(),
        }
    }
}

impl CodexOAuthClient {
    /// Point at a mock server (tests).
    pub fn with_base_url(base: &str) -> Self {
        Self {
            token_url: format!("{base}/oauth/token"),
            authorize_url: format!("{base}/oauth/authorize"),
            http: reqwest::Client::new(),
        }
    }

    pub fn authorize_url(&self, challenge: &str, state: &str, redirect_uri: &str) -> String {
        let mut url = reqwest::Url::parse(&self.authorize_url).expect("static url");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("state", state)
            .append_pair("originator", "rness");
        url.to_string()
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse, AuthError> {
        let response = self.http.post(&self.token_url).form(form).send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthError::OAuth(format!(
                "token endpoint http {status}: {text}"
            )));
        }
        serde_json::from_str(&text)
            .map_err(|e| AuthError::OAuth(format!("bad token response: {e}")))
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<TokenResponse, AuthError> {
        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, AuthError> {
        self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .await
    }
}

// -- login flow ------------------------------------------------------------

/// Run the browser PKCE login on the registered port and persist tokens
/// under [`STORE_KEY`].
pub async fn login(store: &CredentialStore, prompt: &LoginPrompt) -> Result<Tokens, AuthError> {
    login_with(CodexOAuthClient::default(), store, prompt).await
}

pub async fn login_with(
    client: CodexOAuthClient,
    store: &CredentialStore,
    prompt: &LoginPrompt,
) -> Result<Tokens, AuthError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let verifier = pkce::code_verifier();
    let challenge = pkce::code_challenge(&verifier);
    let state = pkce::state();

    // The redirect URI is registered for this exact port — no ephemeral
    // port here (unlike the Anthropic flow).
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| {
            AuthError::OAuth(format!(
                "cannot listen on localhost:{CALLBACK_PORT} (is another login or Codex CLI \
                 running?): {e}"
            ))
        })?;
    let redirect_uri = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");

    let url = client.authorize_url(&challenge, &state, &redirect_uri);
    let _ = open::that(&url);
    (prompt.on_url)(&url);

    let (code, cb_state) = loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| AuthError::OAuth(format!("callback accept: {e}")))?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.map_err(TokensError::Io)?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(query) = callback_query(&request, CALLBACK_PATH) else {
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
            "Signed in with ChatGPT! You can close this tab and return to the terminal.",
        )
        .await;
        break (code, cb_state);
    };

    if cb_state != state {
        return Err(AuthError::OAuth("state mismatch (possible CSRF)".into()));
    }

    let tokens = client
        .exchange_code(&code, &verifier, &redirect_uri)
        .await?
        .into_tokens();
    store.save_tokens(STORE_KEY, &tokens)?;
    Ok(tokens)
}

// -- credential resolution (per-request) -----------------------------------

/// Per-request token source: stored tokens, refreshed near expiry,
/// refresh deduplicated across concurrent steps.
pub struct CodexCredentialSource {
    store: CredentialStore,
    credential: String,
    client: CodexOAuthClient,
    refresh_lock: tokio::sync::Mutex<()>,
}

/// What a Responses request needs to authenticate.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexCredential {
    pub access_token: String,
    pub account_id: Option<String>,
}

impl CodexCredentialSource {
    pub fn new(store: CredentialStore) -> Self {
        Self {
            store,
            credential: STORE_KEY.into(),
            client: CodexOAuthClient::default(),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Override the OAuth client (tests: point at a mock token endpoint).
    pub fn with_oauth_client(mut self, client: CodexOAuthClient) -> Self {
        self.client = client;
        self
    }

    pub fn with_credential(mut self, credential: String) -> Self {
        self.credential = credential;
        self
    }

    pub async fn resolve(&self) -> Result<CodexCredential, AuthError> {
        let tokens = self
            .store
            .tokens(&self.credential)?
            .filter(|t| !t.access_token.is_empty())
            .ok_or(AuthError::NoCredentials)?;
        let tokens = if tokens.is_expired(REFRESH_BUFFER_SECS) && !tokens.refresh_token.is_empty() {
            self.refresh(tokens).await?
        } else {
            tokens
        };
        Ok(credential_of(&tokens))
    }

    /// Force-refresh after a 401.
    pub async fn handle_unauthorized(&self) -> Result<CodexCredential, AuthError> {
        let tokens = self
            .store
            .tokens(&self.credential)?
            .filter(|t| !t.refresh_token.is_empty())
            .ok_or(AuthError::NoCredentials)?;
        Ok(credential_of(&self.refresh(tokens).await?))
    }

    async fn refresh(&self, old: Tokens) -> Result<Tokens, AuthError> {
        let _guard = self.refresh_lock.lock().await;
        // Double-check: another task may have refreshed while we waited.
        if let Some(current) = self.store.tokens(&self.credential)? {
            if current.access_token != old.access_token && !current.is_expired(REFRESH_BUFFER_SECS)
            {
                return Ok(current);
            }
        }
        let mut new = self.client.refresh(&old.refresh_token).await?.into_tokens();
        if new.refresh_token.is_empty() {
            new.refresh_token = old.refresh_token;
        }
        // Preserve fields the refresh response doesn't return (accountId,
        // email live in extra).
        for (k, v) in old.extra {
            new.extra.entry(k).or_insert(v);
        }
        self.store.save_tokens(&self.credential, &new)?;
        Ok(new)
    }
}

fn credential_of(tokens: &Tokens) -> CodexCredential {
    CodexCredential {
        access_token: tokens.access_token.clone(),
        account_id: account_id(tokens).map(String::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jwt(payload: serde_json::Value) -> String {
        use base64::Engine as _;
        let enc = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            enc(b"{}"),
            enc(payload.to_string().as_bytes()),
            enc(b"sig")
        )
    }

    #[test]
    fn account_id_from_nested_claim() {
        let jwt = fake_jwt(serde_json::json!({
            "email": "a@b.c",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-1" },
        }));
        assert_eq!(extract_account_id(&jwt), Some("acct-1".to_string()));
    }

    #[test]
    fn account_id_falls_back_to_first_org() {
        let jwt = fake_jwt(serde_json::json!({
            "organizations": [{"id": "org-9"}],
        }));
        assert_eq!(extract_account_id(&jwt), Some("org-9".to_string()));
    }

    #[test]
    fn token_response_extracts_account_and_email() {
        let jwt = fake_jwt(serde_json::json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-2" },
        }));
        let tokens = TokenResponse {
            id_token: jwt,
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_in: 3600,
        }
        .into_tokens();
        assert_eq!(account_id(&tokens), Some("acct-2"));
        assert_eq!(tokens.extra["email"], "me@example.com");
        assert!(tokens.expires_at.is_some());
    }

    #[test]
    fn authorize_url_has_codex_params() {
        let client = CodexOAuthClient::default();
        let url = client.authorize_url("chal", "st", "http://localhost:1455/auth/callback");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("id_token_add_organizations=true"));
        assert!(url.contains("originator=rness"));
        assert!(url.contains("code_challenge_method=S256"));
    }
}
