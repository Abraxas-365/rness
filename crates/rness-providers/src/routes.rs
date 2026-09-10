//! Provider routes: name → how to build a [`Provider`].
//!
//! A route is plain data (kind + endpoint + credential key), so a new
//! OpenAI-compatible gateway is configuration, not code. The composition
//! root supplies explicitly configured routes; none are registered here.

use std::collections::HashMap;
use std::sync::Arc;

use rness_engine::turn::provider::Provider;

use crate::anthropic::AnthropicProvider;
use crate::auth::{CredentialSource, CredentialStore};
use crate::openai::OpenAiProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Anthropic,
    OpenAiCompatible,
    /// ChatGPT-backend Responses API (subscription OAuth, no API key).
    ChatGptResponses,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub kind: Kind,
    /// Endpoint override; `None` uses the kind's default. For
    /// OpenAI-compatible routes this includes the version prefix (`…/v1`).
    pub base_url: Option<String>,
    /// Credential key: names the store record and (uppercased) the
    /// `<KEY>_API_KEY` env var. `None` = unauthenticated (local servers).
    pub credential: Option<String>,
    pub stream_idle_timeout: Option<std::time::Duration>,
}

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("unknown provider '{0}' (known: {1})")]
    Unknown(String, String),
    #[error(
        "bad route spec '{0}': expected <name>=<url>[,<credential>|,none] \
         (e.g. openrouter=https://openrouter.ai/api/v1)"
    )]
    BadSpec(String),
    #[error(
        "no model selected: pass -m <provider>/<model> \
         (e.g. -m anthropic/claude-sonnet-4-5). Known providers: {0}"
    )]
    NoSelection(String),
    #[error(
        "'{0}' names a provider but no model: pass -m {0}/<model> \
         (e.g. -m anthropic/claude-sonnet-4-5)"
    )]
    NoModel(String),
    #[error(
        "no credentials for '{route}': set {env} or run `rness auth set-key --provider {credential}`"
    )]
    NoCredentials { route: String, env: String, credential: String },
    #[error("credential store: {0}")]
    Store(#[from] crate::auth::TokensError),
}

/// Parse a `--route` spec into a named OpenAI-compatible route:
/// `<name>=<url>[,<credential>|,none]`. New gateways are overwhelmingly
/// OpenAI-compatible, so that is the only kind a spec can declare
/// (anthropic-wire or responses-wire gateways would be new kinds — code,
/// not configuration). Omitted credential defaults to the route name
/// (`<NAME>_API_KEY` env or the store); `none` = unauthenticated
/// (local servers). Inserting over an existing name replaces it.
pub fn validate_base_url(value: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value).map_err(|_| "invalid provider URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some()
        || url.query().is_some() || url.fragment().is_some()
    {
        return Err("provider URL must be HTTP(S), with a host and without credentials, query or fragment".into());
    }
    Ok(())
}

/// Explicit API-key authentication, independent of the route's store lookup.
pub fn build_with_api_key(route: &Route, model: &str, key: String) -> Result<Arc<dyn Provider>, String> {
    if let Some(url) = &route.base_url { validate_base_url(url)?; }
    match route.kind {
        Kind::OpenAiCompatible => {
            let mut provider = OpenAiProvider::new(key, model);
            if let Some(url) = &route.base_url { provider = provider.with_base_url(url.trim_end_matches('/')); }
            Ok(Arc::new(provider.with_stream_idle_timeout(route.stream_idle_timeout)))
        }
        Kind::Anthropic => {
            let mut provider = AnthropicProvider::new(key, model);
            if let Some(url) = &route.base_url { provider = provider.with_base_url(url.trim_end_matches('/')); }
            Ok(Arc::new(provider.with_stream_idle_timeout(route.stream_idle_timeout)))
        }
        Kind::ChatGptResponses => Err("ChatGPT subscription transport requires OAuth, not an API key".into()),
    }
}

pub fn build_with_oauth(route: &Route, model: &str, store: CredentialStore, credential: String) -> Result<Arc<dyn Provider>, String> {
    if credential.is_empty() { return Err("OAuth credential name must not be empty".into()); }
    if let Some(url) = &route.base_url { validate_base_url(url)?; }
    match route.kind {
        Kind::Anthropic => {
            let source = CredentialSource::new(store).oauth_only(credential);
            let mut provider = AnthropicProvider::with_credentials(source, model);
            if let Some(url) = &route.base_url { provider = provider.with_base_url(url.trim_end_matches('/')); }
            Ok(Arc::new(provider.with_stream_idle_timeout(route.stream_idle_timeout)))
        }
        Kind::ChatGptResponses => {
            let source = crate::auth::openai::CodexCredentialSource::new(store).with_credential(credential);
            let mut provider = crate::responses::ResponsesProvider::new(source, model);
            if let Some(url) = &route.base_url { provider = provider.with_base_url(url.trim_end_matches('/')); }
            Ok(Arc::new(provider.with_stream_idle_timeout(route.stream_idle_timeout)))
        }
        Kind::OpenAiCompatible => Err("OAuth is not implemented for generic OpenAI chat connections".into()),
    }
}

pub fn parse_route_spec(spec: &str) -> Result<(String, Route), RouteError> {
    let bad = || RouteError::BadSpec(spec.to_string());
    let (name, rest) = spec.split_once('=').ok_or_else(bad)?;
    let name = name.trim();
    let (url, credential) = match rest.split_once(',') {
        Some((url, cred)) => {
            let cred = cred.trim();
            let credential = match cred {
                "none" => None,
                "" => return Err(bad()),
                c => Some(c.to_string()),
            };
            (url.trim(), credential)
        }
        None => (rest.trim(), Some(name.to_string())),
    };
    if name.is_empty() || name.contains('/') || validate_base_url(url).is_err() {
        return Err(bad());
    }
    Ok((
        name.to_string(),
        Route { kind: Kind::OpenAiCompatible, base_url: Some(url.to_string()), credential, stream_idle_timeout: crate::sse::DEFAULT_IDLE_TIMEOUT },
    ))
}

/// An explicit (provider route, model) pair — the only way a model is
/// ever selected. No detection, no name-sniffing, no per-route default
/// model: the full pair is always stated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub route: String,
    pub model: String,
}

fn known(routes: &HashMap<String, Route>) -> String {
    let mut names: Vec<_> = routes.keys().cloned().collect();
    names.sort();
    names.join(", ")
}

impl Selection {
    /// Resolve a `provider/model` slug against the route table. Both parts
    /// are required: rness has no default provider and no default model.
    pub fn parse(
        routes: &HashMap<String, Route>,
        slug: Option<&str>,
    ) -> Result<Selection, RouteError> {
        let Some(slug) = slug.filter(|s| !s.is_empty()) else {
            return Err(RouteError::NoSelection(known(routes)));
        };
        let (name, model) = match slug.split_once('/') {
            Some((route, model)) if !model.is_empty() => (route, model),
            Some((route, _)) => (route, ""),
            None => (slug, ""),
        };
        if !routes.contains_key(name) {
            return Err(RouteError::Unknown(name.to_string(), known(routes)));
        }
        if model.is_empty() {
            return Err(RouteError::NoModel(name.to_string()));
        }
        Ok(Selection { route: name.to_string(), model: model.to_string() })
    }
}

/// Resolve an API key for `credential`: env var (`<CREDENTIAL>_API_KEY`,
/// uppercased, `-` → `_`) > stored key.
fn api_key_for(store: &CredentialStore, credential: &str) -> Result<Option<String>, RouteError> {
    let env = env_var_for(credential);
    if let Ok(key) = std::env::var(&env) {
        if !key.is_empty() {
            return Ok(Some(key));
        }
    }
    Ok(store.api_key(credential)?)
}

pub fn env_var_for(credential: &str) -> String {
    format!("{}_API_KEY", credential.to_uppercase().replace('-', "_"))
}

/// Build a live provider for an explicit `selection`, resolving
/// credentials against `store`.
pub fn build(
    routes: &HashMap<String, Route>,
    selection: &Selection,
    store: CredentialStore,
) -> Result<Arc<dyn Provider>, RouteError> {
    let name = selection.route.as_str();
    let route = routes
        .get(name)
        .ok_or_else(|| RouteError::Unknown(name.to_string(), known(routes)))?;
    let model = selection.model.clone();

    match route.kind {
        Kind::Anthropic => {
            // Full credential stack: env > stored key > OAuth with refresh,
            // resolved per request inside the provider.
            let mut provider =
                AnthropicProvider::with_credentials(CredentialSource::new(store), model);
            if let Some(url) = &route.base_url {
                provider = provider.with_base_url(url.clone());
            }
            Ok(Arc::new(provider.with_stream_idle_timeout(route.stream_idle_timeout)))
        }
        Kind::ChatGptResponses => {
            // Subscription OAuth only — tokens resolve per request with
            // refresh; no API key path.
            let source = crate::auth::openai::CodexCredentialSource::new(store);
            let mut provider = crate::responses::ResponsesProvider::new(source, model);
            if let Some(url) = &route.base_url {
                provider = provider.with_base_url(url.clone());
            }
            Ok(Arc::new(provider.with_stream_idle_timeout(route.stream_idle_timeout)))
        }
        Kind::OpenAiCompatible => {
            let key = match &route.credential {
                Some(credential) => api_key_for(&store, credential)?.ok_or_else(|| {
                    RouteError::NoCredentials {
                        route: name.to_string(),
                        env: env_var_for(credential),
                        credential: credential.clone(),
                    }
                })?,
                None => "unauthenticated".to_string(),
            };
            let mut provider = OpenAiProvider::new(key, model);
            if route.credential.is_none() { provider = provider.without_auth(); }
            if let Some(url) = &route.base_url {
                provider = provider.with_base_url(url.trim_end_matches('/'));
            }
            Ok(Arc::new(provider.with_stream_idle_timeout(route.stream_idle_timeout)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_validation_rejects_ambiguous_and_credential_urls() {
        for url in ["httpgarbage", "ftp://host/v1", "https://user:secret@host/v1", "https://host/v1?key=secret", "https://host/v1#frag"] {
            assert!(validate_base_url(url).is_err());
        }
        for url in ["http://localhost:11434/v1", "https://api.example/v1", "http://[::1]:8000/v1"] {
            assert!(validate_base_url(url).is_ok());
        }
    }

    #[test]
    fn route_spec_url_only_defaults_credential_to_name() {
        let (name, route) = parse_route_spec("openrouter=https://openrouter.ai/api/v1").unwrap();
        assert_eq!(name, "openrouter");
        assert_eq!(route.kind, Kind::OpenAiCompatible);
        assert_eq!(route.base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(route.credential.as_deref(), Some("openrouter"));
    }

    #[test]
    fn route_spec_explicit_credential_and_none() {
        let (_, route) = parse_route_spec("gw=https://gw.example/v1,shared-key").unwrap();
        assert_eq!(route.credential.as_deref(), Some("shared-key"));
        let (_, route) = parse_route_spec("lan=http://192.168.1.50:8000/v1,none").unwrap();
        assert_eq!(route.credential, None);
    }

    #[test]
    fn route_spec_rejects_malformed() {
        for bad in [
            "nourl",                       // no '='
            "=https://x.example/v1",       // empty name
            "a/b=https://x.example/v1",    // '/' collides with -m parsing
            "gw=ftp://x.example",          // not http(s)
            "gw=https://x.example/v1,",    // trailing comma, empty credential
        ] {
            assert!(parse_route_spec(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn selection_keeps_slashes_in_model_names() {
        // OpenRouter-style model ids contain '/': the FIRST segment is
        // the route, the rest is the model verbatim.
        let mut table = HashMap::new();
        table.insert(
            "openrouter".into(),
            parse_route_spec("openrouter=https://openrouter.ai/api/v1").unwrap().1,
        );
        let sel = Selection::parse(&table, Some("openrouter/anthropic/claude-sonnet-5")).unwrap();
        assert_eq!(sel.route, "openrouter");
        assert_eq!(sel.model, "anthropic/claude-sonnet-5");
    }
}
