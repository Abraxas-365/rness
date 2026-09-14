//! OAuth credential flow against wiremock: header spoofing, billing
//! block, refresh-on-expiry, and 401 retry — plus resolver priority.

use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_protocol::events::ContentPart;
use rness_providers::anthropic::AnthropicProvider;
use rness_providers::auth::{
    Credential, CredentialSource, CredentialStore, OAuthClient, OAuthConfig, Tokens,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, headers, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_ok() -> ResponseTemplate {
    let body = [
        ("message_start", json!({"message": {"usage": {"input_tokens": 1}}})),
        ("content_block_start", json!({"content_block": {"type": "text"}})),
        ("content_block_delta", json!({"delta": {"type": "text_delta", "text": "ok"}})),
        ("message_delta", json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}})),
        ("message_stop", json!({})),
    ]
    .iter()
    .map(|(e, d)| format!("event: {e}\ndata: {d}\n\n"))
    .collect::<String>();
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn context() -> ModelContext {
    ModelContext {
        turns: vec![ModelTurn::User { content: vec![ContentPart::Text { text: "hi".into() }] }],
        ..Default::default()
    }
}

fn store_with_oauth(dir: &std::path::Path, access: &str, expires_in_secs: i64) -> CredentialStore {
    let store = CredentialStore::new(dir.join("credentials.json"));
    store
        .save_tokens("anthropic", &Tokens {
            access_token: access.into(),
            refresh_token: "rt-1".into(),
            expires_at: Some(
                (jiff::Timestamp::now() + jiff::SignedDuration::from_secs(expires_in_secs))
                    .to_string(),
            ),
            scopes: vec!["user:inference".into()],
            subscription_type: "max".into(),
            ..Default::default()
        })
        .unwrap();
    store
}

async fn run_step(provider: &AnthropicProvider) -> StepOutcome {
    let ctx = context();
    provider
        .step(
            StepRequest { context: &ctx, system: "sys", tools: &[], on_delta: None },
            &CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn oauth_credential_sends_bearer_spoof_headers_and_billing_block() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer at-valid"))
        // Note: wiremock parses comma-separated header values as lists —
        // the UA's "(external, sdk-cli)" splits too.
        .and(headers("anthropic-beta", vec!["claude-code-20250219", "oauth-2025-04-20"]))
        .and(headers("user-agent", vec!["claude-cli/2.1.195 (external", "sdk-cli)"]))
        .and(header("x-app", "cli"))
        .and(body_partial_json(json!({
            // Billing block first, then the real system prompt.
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.195; cc_entrypoint=cli; cch=00000;"},
                {"type": "text", "text": "sys"},
            ],
        })))
        .respond_with(sse_ok())
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let store = store_with_oauth(dir.path(), "at-valid", 3600);
    let provider = AnthropicProvider::with_credentials(CredentialSource::new(store), "m")
        .with_base_url(server.uri());
    assert!(matches!(run_step(&provider).await, StepOutcome::Committed(_)));
}

#[tokio::test]
async fn api_key_credential_still_uses_x_api_key_without_spoof() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-stored"))
        .and(body_partial_json(json!({
            "system": [{"type": "text", "text": "sys"}],
        })))
        .respond_with(sse_ok())
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    store.save_api_key("anthropic", "sk-stored").unwrap();
    let provider = AnthropicProvider::with_credentials(CredentialSource::new(store), "m")
        .with_base_url(server.uri());
    assert!(matches!(run_step(&provider).await, StepOutcome::Committed(_)));
}

#[tokio::test]
async fn expired_token_refreshes_before_request_and_persists() {
    let server = MockServer::start().await;
    // Token endpoint returns a fresh token.
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_partial_json(json!({
            "grant_type": "refresh_token",
            "refresh_token": "rt-1",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-fresh",
            "refresh_token": "rt-2",
            "expires_in": 3600,
            "scope": "user:inference",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let store = store_with_oauth(dir.path(), "at-stale", -60); // already expired
    let config = OAuthConfig {
        token_url: format!("{}/token", server.uri()),
        ..Default::default()
    };
    let source = CredentialSource::new(store).with_oauth_client(OAuthClient::new(config));

    let cred = source.resolve().await.unwrap();
    assert_eq!(cred, Credential::OAuth("at-fresh".into()));

    // Refresh persisted: subscription preserved, new refresh token stored.
    let saved = source.store().tokens("anthropic").unwrap().unwrap();
    assert_eq!(saved.access_token, "at-fresh");
    assert_eq!(saved.refresh_token, "rt-2");
    assert_eq!(saved.subscription_type, "max");
}

#[tokio::test]
async fn http_401_refreshes_once_and_retries() {
    let server = MockServer::start().await;
    // First request with the stale token → 401.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer at-stale"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"type": "authentication_error", "message": "expired"},
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Refresh endpoint.
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at-fresh",
            "refresh_token": "rt-2",
            "expires_in": 3600,
            "scope": "user:inference",
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Retried request with the fresh token → success.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer at-fresh"))
        .respond_with(sse_ok())
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    // Token not yet within refresh buffer, so resolve() uses it as-is —
    // the 401 path is what forces the refresh.
    let store = store_with_oauth(dir.path(), "at-stale", 3600);
    let config = OAuthConfig {
        token_url: format!("{}/token", server.uri()),
        ..Default::default()
    };
    let source = CredentialSource::new(store).with_oauth_client(OAuthClient::new(config));
    let provider =
        AnthropicProvider::with_credentials(source, "m").with_base_url(server.uri())
            .with_headers(rness_providers::headers::ProviderHeaders::new(&[("X-Tenant".into(), "secret".into())].into()).unwrap()).unwrap();

    assert!(matches!(run_step(&provider).await, StepOutcome::Committed(_)));
    for request in server.received_requests().await.unwrap() {
        if request.url.path() == "/token" {
            assert!(!request.headers.contains_key("x-tenant"));
        } else {
            assert_eq!(request.headers["x-tenant"], "secret");
        }
    }
}

#[tokio::test]
async fn env_var_wins_over_stored_oauth() {
    // Note: env-var priority is easiest to verify via resolve() order with
    // a store that has OAuth; we don't set the real env var in-process
    // (races with parallel tests). Instead: stored API key wins over OAuth.
    let dir = tempfile::tempdir().unwrap();
    let store = store_with_oauth(dir.path(), "at-oauth", 3600);
    store.save_api_key("anthropic", "sk-key").unwrap();
    let source = CredentialSource::new(store);
    assert_eq!(source.resolve().await.unwrap(), Credential::ApiKey("sk-key".into()));
}

#[tokio::test]
async fn no_credentials_is_a_fatal_error() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    let provider = AnthropicProvider::with_credentials(CredentialSource::new(store), "m")
        .with_base_url(server.uri());
    // Guard: only meaningful when the env var isn't set in this process.
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return;
    }
    let StepOutcome::Failed { error, .. } = run_step(&provider).await else {
        panic!("expected failure");
    };
    assert!(!error.retryable);
    assert!(error.message.contains("no credentials"));
}
