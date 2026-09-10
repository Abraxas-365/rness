//! Route table tests: slug parsing (the only selection path — no default
//! provider, no name-sniffing), credential resolution errors, env naming,
//! and unauthenticated routes.

use rness_providers::auth::CredentialStore;
use rness_providers::routes::{build, env_var_for, Kind, Route, RouteError, Selection};
use std::collections::HashMap;
use tempfile::TempDir;

fn store(dir: &TempDir) -> CredentialStore {
    CredentialStore::new(dir.path().join("credentials.json"))
}

fn configured_routes() -> HashMap<String, Route> {
    [
        ("anthropic", Kind::Anthropic, "https://api.anthropic.com", Some("anthropic")),
        ("deepseek", Kind::OpenAiCompatible, "https://api.deepseek.com/v1", Some("deepseek")),
        ("groq", Kind::OpenAiCompatible, "https://api.groq.com/openai/v1", Some("groq")),
        ("ollama", Kind::OpenAiCompatible, "http://localhost:11434/v1", None),
    ].into_iter().map(|(name, kind, url, credential)| {
        (name.into(), Route {
            kind,
            base_url: Some(url.into()),
            credential: credential.map(str::to_string),
            stream_idle_timeout: None,
        })
    }).collect()
}

#[test]
fn empty_configuration_has_no_implicit_providers() {
    for name in ["anthropic", "openai", "openai-chatgpt", "deepseek", "groq", "ollama"] {
        assert!(matches!(
            Selection::parse(&HashMap::new(), Some(&format!("{name}/model"))),
            Err(RouteError::Unknown(_, known)) if known.is_empty()
        ));
    }
}

fn select(slug: &str) -> Result<Selection, RouteError> {
    Selection::parse(&configured_routes(), Some(slug))
}

// -- Selection::parse ------------------------------------------------------

#[test]
fn slug_with_slash_selects_route_and_model() {
    let s = select("anthropic/claude-opus-4-6").unwrap();
    assert_eq!(s.route, "anthropic");
    assert_eq!(s.model, "claude-opus-4-6");
}

#[test]
fn bare_route_without_model_is_an_error() {
    // dsh purism: the pair is indivisible — no per-route default model.
    let Err(err) = select("deepseek") else {
        panic!("expected error");
    };
    let msg = err.to_string();
    assert!(msg.contains("-m deepseek/<model>"), "{msg}");

    // Trailing slash is the same omission.
    let Err(err) = select("ollama/") else {
        panic!("expected error");
    };
    assert!(err.to_string().contains("-m ollama/<model>"));
}

#[test]
fn no_selection_is_an_error_listing_routes() {
    let Err(err) = Selection::parse(&configured_routes(), None) else {
        panic!("expected error");
    };
    let msg = err.to_string();
    assert!(msg.contains("-m <provider>/<model>"), "{msg}");
    for name in ["anthropic", "deepseek", "groq", "ollama"] {
        assert!(msg.contains(name), "{msg}");
    }
}

#[test]
fn model_names_are_never_sniffed() {
    // A bare model name is not a route — no guessing which provider
    // serves it.
    let Err(err) = select("gpt-5.5") else {
        panic!("expected error");
    };
    assert!(err.to_string().contains("unknown provider 'gpt-5.5'"));
}

#[test]
fn unknown_route_in_slug_lists_known_names() {
    let Err(err) = select("nope/some-model") else {
        panic!("expected error");
    };
    let msg = err.to_string();
    assert!(msg.contains("unknown provider 'nope'"), "{msg}");
    assert!(msg.contains("anthropic"), "{msg}");
}

#[test]
fn model_may_contain_slashes() {
    // Only the FIRST slash splits: ollama model tags like qwen3:8b or
    // registry paths keep the rest intact.
    let s = select("ollama/library/qwen3:8b").unwrap();
    assert_eq!(s.route, "ollama");
    assert_eq!(s.model, "library/qwen3:8b");
}

// -- build -----------------------------------------------------------------

#[test]
fn missing_credential_names_env_and_command() {
    let dir = TempDir::new().unwrap();
    let err = match build(&configured_routes(), &select("groq/llama-3.3-70b").unwrap(), store(&dir)) {
        Err(e @ RouteError::NoCredentials { .. }) => e,
        Err(other) => panic!("expected NoCredentials, got {other}"),
        Ok(_) => panic!("expected NoCredentials, got a provider"),
    };
    let msg = err.to_string();
    assert!(msg.contains("GROQ_API_KEY"), "{msg}");
    assert!(msg.contains("auth set-key --provider groq"), "{msg}");
}

#[test]
fn stored_key_builds_openai_compatible_route() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    s.save_api_key("deepseek", "dk-1").unwrap();
    let provider = build(&configured_routes(), &select("deepseek/deepseek-reasoner").unwrap(), s)
        .unwrap();
    assert_eq!(provider.model(), "deepseek-reasoner");
}

#[test]
fn ollama_needs_no_credentials() {
    let dir = TempDir::new().unwrap();
    let provider =
        build(&configured_routes(), &select("ollama/qwen3").unwrap(), store(&dir)).unwrap();
    assert_eq!(provider.model(), "qwen3");
}

#[test]
fn anthropic_route_builds_without_stored_credentials() {
    // Credential resolution is per request for Anthropic (OAuth refresh),
    // so building succeeds even with an empty store.
    let dir = TempDir::new().unwrap();
    let provider =
        build(&configured_routes(), &select("anthropic/claude-sonnet-4-5").unwrap(), store(&dir))
            .unwrap();
    assert_eq!(provider.model(), "claude-sonnet-4-5");
}

#[test]
fn env_var_naming_uppercases_and_underscores() {
    assert_eq!(env_var_for("deepseek"), "DEEPSEEK_API_KEY");
    assert_eq!(env_var_for("my-proxy"), "MY_PROXY_API_KEY");
}
