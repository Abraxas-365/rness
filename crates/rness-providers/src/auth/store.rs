//! Credential storage — `~/.rness/credentials.json`.
//!
//! ```json
//! { "providers": { "anthropic": { "oauthTokens": {...}, "apiKey": "..." } } }
//! ```
//!
//! Plaintext JSON at mode 0600. Keychain backends can layer on later
//! behind this same interface. Unknown fields are preserved on rewrite,
//! and a legacy top-level `oauthTokens`/`apiKey` layout is migrated on
//! read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::TokensError;

/// Anthropic-style OAuth tokens (camelCase on disk).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    /// RFC 3339; empty means "no expiry recorded".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subscription_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rate_limit_tier: String,
    /// Extra fields (account, profile, apiKey…) preserved on rewrite.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Tokens {
    /// Expired (or expiring within `buffer_secs`)?
    pub fn is_expired(&self, buffer_secs: i64) -> bool {
        let Some(at) = &self.expires_at else {
            return false;
        };
        let Ok(ts) = at.parse::<jiff::Timestamp>() else {
            return false;
        };
        jiff::Timestamp::now() + jiff::SignedDuration::from_secs(buffer_secs) > ts
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProviderCreds {
    #[serde(
        rename = "oauthTokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    oauth_tokens: Option<Tokens>,
    #[serde(rename = "apiKey", default, skip_serializing_if = "String::is_empty")]
    api_key: String,
    /// Unknown provider fields (codexTokens…) preserved.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileData {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    providers: HashMap<String, ProviderCreds>,
    // Legacy top-level fields, migrated on read, never written.
    #[serde(rename = "oauthTokens", default, skip_serializing)]
    legacy_oauth: Option<Tokens>,
    #[serde(rename = "apiKey", default, skip_serializing)]
    legacy_api_key: String,
}

impl FileData {
    fn migrate(&mut self) {
        if self.legacy_oauth.is_some() || !self.legacy_api_key.is_empty() {
            let p = self.providers.entry("anthropic".into()).or_default();
            if let Some(t) = self.legacy_oauth.take() {
                p.oauth_tokens = Some(t);
            }
            if !self.legacy_api_key.is_empty() {
                p.api_key = std::mem::take(&mut self.legacy_api_key);
            }
        }
    }
}

/// File-backed credential store.
#[derive(Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Default location: `$RNESS_HOME/credentials.json` or
    /// `~/.rness/credentials.json`.
    pub fn default_path() -> PathBuf {
        let home = std::env::var("RNESS_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".rness")
            });
        home.join("credentials.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<FileData, TokensError> {
        let raw = match std::fs::read(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileData::default()),
            Err(e) => return Err(TokensError::Io(e)),
        };
        let mut data: FileData = serde_json::from_slice(&raw)?;
        data.migrate();
        Ok(data)
    }

    fn write(&self, data: &FileData) -> Result<(), TokensError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_vec_pretty(data)?;
        // 0600: owner-only.
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)?;
            f.write_all(&raw)?;
        }
        #[cfg(not(unix))]
        std::fs::write(&self.path, &raw)?;
        Ok(())
    }

    // -- account-aware key helpers ----------------------------------------

    /// Build the credential key for a provider/account pair.
    /// `None` or empty account → bare provider name (default account).
    /// Named account → `"provider/account"`.
    pub fn credential_key(provider: &str, account: Option<&str>) -> String {
        match account {
            Some(a) if !a.is_empty() => format!("{provider}/{a}"),
            _ => provider.to_string(),
        }
    }

    /// List named accounts stored for `provider`. Returns account names
    /// (not credential keys). The default (unnamed) account is represented
    /// as `None` in the output when it has a stored credential.
    pub fn accounts(&self, provider: &str) -> Result<Vec<Option<String>>, TokensError> {
        let data = self.read()?;
        let prefix = format!("{provider}/");
        let mut out = Vec::new();
        for key in data.providers.keys() {
            if key == provider {
                out.push(None);
            } else if let Some(name) = key.strip_prefix(&prefix) {
                out.push(Some(name.to_string()));
            }
        }
        out.sort();
        Ok(out)
    }

    // -- provider-keyed records (any provider) -----------------------------

    pub fn tokens(&self, provider: &str) -> Result<Option<Tokens>, TokensError> {
        Ok(self
            .read()?
            .providers
            .get(provider)
            .and_then(|p| p.oauth_tokens.clone()))
    }

    pub fn save_tokens(&self, provider: &str, tokens: &Tokens) -> Result<(), TokensError> {
        let mut data = self.read()?;
        data.providers
            .entry(provider.into())
            .or_default()
            .oauth_tokens = Some(tokens.clone());
        self.write(&data)
    }

    pub fn api_key(&self, provider: &str) -> Result<Option<String>, TokensError> {
        Ok(self
            .read()?
            .providers
            .get(provider)
            .map(|p| p.api_key.clone())
            .filter(|k| !k.is_empty()))
    }

    pub fn save_api_key(&self, provider: &str, key: &str) -> Result<(), TokensError> {
        let mut data = self.read()?;
        data.providers.entry(provider.into()).or_default().api_key = key.to_string();
        self.write(&data)
    }

    pub fn delete(&self, provider: &str) -> Result<(), TokensError> {
        let mut data = self.read()?;
        data.providers.remove(provider);
        self.write(&data)
    }

    /// Providers with any stored credential, sorted.
    pub fn list(&self) -> Result<Vec<String>, TokensError> {
        let mut v: Vec<_> = self.read()?.providers.keys().cloned().collect();
        v.sort();
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "providers": {
                    "anthropic": {
                        "oauthTokens": {
                            "accessToken": "at",
                            "refreshToken": "rt",
                            "subscriptionType": "max",
                            "tokenAccount": {"uuid": "u1", "emailAddress": "a@b.c"}
                        }
                    },
                    "codex": {"codexTokens": {"accessToken": "cx"}}
                }
            })
            .to_string(),
        )
        .unwrap();

        let store = CredentialStore::new(&path);
        let tokens = store.tokens("anthropic").unwrap().unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.subscription_type, "max");
        // Extra field survives read.
        assert!(tokens.extra.contains_key("tokenAccount"));

        // Rewrite; codex entry must survive.
        store.save_api_key("anthropic", "sk-test").unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            raw["providers"]["codex"]["codexTokens"]["accessToken"],
            "cx"
        );
        assert_eq!(raw["providers"]["anthropic"]["apiKey"], "sk-test");
        assert_eq!(
            raw["providers"]["anthropic"]["oauthTokens"]["tokenAccount"]["uuid"],
            "u1"
        );
    }

    #[test]
    fn migrates_legacy_top_level_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(
            &path,
            serde_json::json!({"oauthTokens": {"accessToken": "legacy"}, "apiKey": "old-key"})
                .to_string(),
        )
        .unwrap();

        let store = CredentialStore::new(&path);
        assert_eq!(
            store.tokens("anthropic").unwrap().unwrap().access_token,
            "legacy"
        );
        assert_eq!(store.api_key("anthropic").unwrap().unwrap(), "old-key");
    }

    #[test]
    fn expiry_buffer() {
        let mut t = Tokens {
            access_token: "x".into(),
            ..Default::default()
        };
        assert!(!t.is_expired(300), "no expiry set");
        t.expires_at =
            Some((jiff::Timestamp::now() + jiff::SignedDuration::from_secs(600)).to_string());
        assert!(!t.is_expired(300));
        assert!(t.is_expired(900), "inside the buffer");
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("credentials.json"));
        store.save_api_key("anthropic", "k").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn credential_key_helper() {
        assert_eq!(CredentialStore::credential_key("anthropic", None), "anthropic");
        assert_eq!(CredentialStore::credential_key("anthropic", Some("")), "anthropic");
        assert_eq!(CredentialStore::credential_key("anthropic", Some("work")), "anthropic/work");
    }

    #[test]
    fn accounts_lists_named_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = CredentialStore::new(&path);
        // Default account
        store.save_api_key("anthropic", "k1").unwrap();
        // Named accounts
        store.save_api_key("anthropic/work", "k2").unwrap();
        store.save_api_key("anthropic/personal", "k3").unwrap();
        // Different provider entirely
        store.save_api_key("openrouter", "k4").unwrap();

        let accounts = store.accounts("anthropic").unwrap();
        assert_eq!(accounts.len(), 3);
        assert!(accounts.contains(&None)); // default
        assert!(accounts.contains(&Some("work".into())));
        assert!(accounts.contains(&Some("personal".into())));

        let or_accounts = store.accounts("openrouter").unwrap();
        assert_eq!(or_accounts, vec![None]);
    }
}
