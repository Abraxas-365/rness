//! Exercise configured authentication against real local HTTP sockets.
use rness_engine::{session::projection::ModelContext, turn::provider::{Provider, StepRequest}};
use rness_providers::{routes::{self, Kind, Route, Selection}, auth::{CredentialStore, Tokens}};
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, ResponseTemplate};
use wiremock::matchers::method;

async fn send(provider: &dyn Provider) {
    let context = ModelContext::default();
    let _ = provider.step(StepRequest { context: &context, system: "", tools: &[], on_delta: None }, &CancellationToken::new()).await;
}

#[tokio::test]
async fn api_keys_use_protocol_specific_headers() {
    for (kind, expected) in [(Kind::Anthropic, "x-api-key"), (Kind::OpenAiCompatible, "authorization")] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(400)).expect(1).mount(&server).await;
        let route = Route { kind, base_url: Some(server.uri()), credential: None };
        let provider = routes::build_with_api_key(&route, "test", "fake-key".into()).unwrap();
        send(provider.as_ref()).await;
        let requests = server.received_requests().await.unwrap();
        let headers = &requests[0].headers;
        assert_eq!(headers.get(expected).unwrap().to_str().unwrap(), if expected == "x-api-key" { "fake-key" } else { "Bearer fake-key" });
        assert!(!headers.contains_key(if expected == "x-api-key" { "authorization" } else { "x-api-key" }));
    }
}

#[tokio::test]
async fn no_auth_means_no_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(400)).expect(1).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::new(dir.path().join("credentials.json"));
    let routes = [("local".into(), Route { kind: Kind::OpenAiCompatible, base_url: Some(server.uri()), credential: None })].into();
    let provider = routes::build(&routes, &Selection { route: "local".into(), model: "test".into() }, store).unwrap();
    send(provider.as_ref()).await;
    assert!(!server.received_requests().await.unwrap()[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn oauth_uses_named_tokens_not_stored_api_keys() {
    for kind in [Kind::Anthropic, Kind::ChatGptResponses] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(400)).expect(1).mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("credentials.json"));
        let mut tokens = Tokens { access_token: "named-token".into(), ..Default::default() };
        tokens.extra.insert("accountId".into(), serde_json::json!("named-account"));
        store.save_tokens("custom-login", &tokens).unwrap();
        let route = Route { kind, base_url: Some(server.uri()), credential: None };
        let provider = routes::build_with_oauth(&route, "test", store, "custom-login".into()).unwrap();
        send(provider.as_ref()).await;
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests[0].headers.get("authorization").unwrap(), "Bearer named-token");
        assert!(!requests[0].headers.contains_key("x-api-key"));
        if matches!(route.kind, Kind::ChatGptResponses) {
            assert_eq!(requests[0].headers.get("chatgpt-account-id").unwrap(), "named-account");
        }
    }
}
