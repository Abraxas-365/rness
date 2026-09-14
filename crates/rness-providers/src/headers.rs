//! Validated, secret-aware provider request headers (never OAuth headers).

use std::collections::BTreeMap;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

#[derive(Clone, Default, Debug)]
pub struct ProviderHeaders(HeaderMap);

impl ProviderHeaders {
    /// Names are case-insensitive; reject duplicates and adapter/transport overrides.
    /// Errors deliberately omit values, which may contain credentials.
    pub fn new(headers: &BTreeMap<String, String>) -> Result<Self, String> {
        let mut parsed = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "invalid provider header name".to_string())?;
            if matches!(name.as_str(),
                "authorization" | "proxy-authorization" | "x-api-key" | "cookie" | "host"
                | "content-length" | "content-type" | "content-encoding" | "transfer-encoding"
                | "connection" | "keep-alive" | "te" | "trailer" | "upgrade" | "expect"
                | "accept" | "accept-encoding" | "user-agent" | "anthropic-version"
                | "anthropic-beta" | "anthropic-dangerous-direct-browser-access" | "x-app"
                | "openai-beta" | "originator" | "chatgpt-account-id"
            ) {
                return Err(format!("provider header '{name}' is reserved"));
            }
            if parsed.contains_key(&name) {
                return Err(format!("duplicate provider header '{name}'"));
            }
            if value.len() > 8192 || value.chars().any(char::is_control) {
                return Err(format!("invalid value for provider header '{name}'"));
            }
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| format!("invalid value for provider header '{name}'"))?;
            value.set_sensitive(true);
            parsed.insert(name, value);
        }
        Ok(Self(parsed))
    }

    pub(crate) fn client(&self) -> Result<reqwest::Client, reqwest::Error> {
        let mut builder = reqwest::Client::builder().default_headers(self.0.clone());
        if self.0.contains_key(reqwest::header::REFERER) {
            // Preserve an explicit Referer rather than replacing it on redirects.
            builder = builder.referer(false);
        }
        if !self.0.is_empty() {
            // reqwest only strips a few standard secrets on redirects. Custom
            // credentials must never follow a cross-origin redirect or downgrade.
            builder = builder.redirect(reqwest::redirect::Policy::custom(|attempt| {
                let same_origin = attempt.previous().first().is_some_and(|first| {
                    first.scheme() == attempt.url().scheme()
                        && first.host_str() == attempt.url().host_str()
                        && first.port_or_known_default() == attempt.url().port_or_known_default()
                });
                if !same_origin { attempt.stop() }
                else if attempt.previous().len() >= 10 { attempt.error("too many redirects") }
                else { attempt.follow() }
            }));
        }
        builder.build()
    }

    /// Partition file caches and quota cleanup by tenant/routing headers too.
    /// Preserve existing cache keys for connections without custom headers.
    pub(crate) fn upload_credential(&self, credential: &str) -> String {
        if self.0.is_empty() { return credential.to_owned(); }
        use sha2::{Digest, Sha256};
        let sorted: BTreeMap<_, _> = self.0.iter().map(|(name, value)| (name.as_str(), value.as_bytes())).collect();
        format!("{:x}", Sha256::digest(serde_json::to_vec(&(credential, sorted)).expect("header tuple")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names_values_duplicates_and_reserved_headers_without_leaking_values() {
        for name in ["Authorization", "X-Api-Key", "HOST", "Content-Length", "Cookie", "Connection", "Anthropic-Beta", "ChatGPT-Account-ID", "bad name", ""] {
            let error = ProviderHeaders::new(&[(name.into(), "secret-value".into())].into()).unwrap_err();
            assert!(!error.contains("secret-value"));
        }
        for value in ["secret\r\ninjected: yes", "secret\0", "secret\t", &"s".repeat(8193)] {
            let error = ProviderHeaders::new(&[("x-test".into(), value.into())].into()).unwrap_err();
            assert!(!error.contains(value));
        }
        assert!(ProviderHeaders::new(&[("X-Test".into(), "one".into()), ("x-test".into(), "two".into())].into()).is_err());
        let headers = ProviderHeaders::new(&[("HTTP-Referer".into(), "secret-value".into()), ("X-Title".into(), "".into())].into()).unwrap();
        assert!(!format!("{headers:?}").contains("secret-value"));
        assert!(headers.0["http-referer"].is_sensitive());
    }

    #[test]
    fn file_cache_identity_includes_headers_and_is_case_insensitive() {
        let headers = |name: &str, value: &str| ProviderHeaders::new(&[(name.into(), value.into())].into()).unwrap();
        assert_eq!(ProviderHeaders::default().upload_credential("key"), "key");
        assert_eq!(headers("X-Tenant", "one").upload_credential("key"), headers("x-tenant", "one").upload_credential("key"));
        assert_ne!(headers("X-Tenant", "one").upload_credential("key"), headers("X-Tenant", "two").upload_credential("key"));
        assert_ne!(headers("X-Tenant", "one").upload_credential("key"), headers("X-Tenant", "one").upload_credential("other"));
    }
}
