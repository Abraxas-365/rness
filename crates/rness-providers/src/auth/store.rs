//! Credential storage — `~/.rness/credentials.json`.
//!
//! ```json
//! { "providers": { "anthropic": { "oauthTokens": {...}, "apiKey": "..." } } }
//! ```
//!
//! Plaintext JSON at mode 0600. Every save is a read-modify-write under an
//! exclusive lock on the sidecar `credentials.json.lock` and replaces the
//! file by atomic rename (temp file + fsync), so concurrent rness
//! processes neither lose each other's updates nor see a torn file.
//! Keychain backends can layer on later behind this same interface. Unknown fields are preserved on rewrite,
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

/// Guard for the store's sidecar lock (`<file>.lock`); unlocks on drop.
/// `file` is `None` where the filesystem has no locking. Hold it across a
/// token refresh with [`CredentialStore::lock_for_refresh`] and save with
/// [`CredentialStore::save_tokens_locked`] (never a locking method).
pub struct StoreLock {
    file: Option<std::fs::File>,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            let _ = file.unlock();
        }
    }
}

/// `rename` over an existing file. Windows can refuse while another
/// process has the target open without delete sharing: retry briefly.
fn rename_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    for attempt in 1..=5u64 {
        match std::fs::rename(from, to) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                std::thread::sleep(std::time::Duration::from_millis(10 * attempt));
            }
            other => return other,
        }
    }
    std::fs::rename(from, to)
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

    /// Lock-free read. Writes replace the file by atomic rename, so a
    /// reader sees either the old or the new complete file.
    fn read(&self) -> Result<FileData, TokensError> {
        let raw = match std::fs::read(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileData::default()),
            Err(e) => return Err(TokensError::Io(e)),
        };
        // A zero-length file (torn by an older binary) is corrupt too.
        let mut data: FileData =
            serde_json::from_slice(&raw).map_err(|source| TokensError::Corrupt {
                path: self.path.clone(),
                source,
            })?;
        data.migrate();
        Ok(data)
    }

    /// The sidecar lock file. Never renamed or deleted: the data file is
    /// replaced by rename, so a lock on it would not exclude anything.
    fn lock_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "credentials.json".into());
        name.push(".lock");
        self.path.with_file_name(name)
    }

    fn open_lock_file(&self) -> Result<std::fs::File, TokensError> {
        let path = self.lock_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(&path)?)
    }

    /// Network filesystems without flock: still atomic, not exclusive.
    fn unlocked_fallback(&self) -> StoreLock {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                path = %self.lock_path().display(),
                "file locking unsupported; credential saves are atomic but not exclusive"
            )
        });
        StoreLock { file: None }
    }

    /// Exclusive cross-process lock on the store (blocking). Held only
    /// around read-modify-write; released on drop.
    fn lock_exclusive(&self) -> Result<StoreLock, TokensError> {
        let file = self.open_lock_file()?;
        match file.lock() {
            Ok(()) => Ok(StoreLock { file: Some(file) }),
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => Ok(self.unlocked_fallback()),
            Err(e) => Err(e.into()),
        }
    }

    /// The same exclusive lock for holding across an OAuth refresh (one
    /// network round trip), so only one process spends a rotating refresh
    /// token. Polls without blocking the runtime; gives up after `wait`
    /// with a clear error instead of hanging behind a stuck process.
    pub async fn lock_for_refresh(
        &self,
        wait: std::time::Duration,
    ) -> Result<StoreLock, TokensError> {
        let file = self.open_lock_file()?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(StoreLock { file: Some(file) }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e))
                    if e.kind() == std::io::ErrorKind::Unsupported =>
                {
                    return Ok(self.unlocked_fallback())
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "timed out after {}s waiting for {} (another rness process is refreshing a token)",
                        wait.as_secs(),
                        self.lock_path().display()
                    ),
                )
                .into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Read-modify-write under the store lock. A corrupt file is backed up
    /// (`<file>.corrupt-<unix-ts>`) and replaced, so saving self-heals;
    /// read-only paths keep returning [`TokensError::Corrupt`].
    fn update<R>(&self, f: impl FnOnce(&mut FileData) -> R) -> Result<R, TokensError> {
        let lock = self.lock_exclusive()?;
        self.update_locked(&lock, f)
    }

    /// [`Self::update`] for a caller already holding the store lock.
    fn update_locked<R>(
        &self,
        _lock: &StoreLock,
        f: impl FnOnce(&mut FileData) -> R,
    ) -> Result<R, TokensError> {
        let mut data = match self.read() {
            Ok(data) => data,
            Err(TokensError::Corrupt { source, .. }) => {
                let backup = self.quarantine_corrupt()?;
                tracing::warn!(
                    path = %self.path.display(),
                    backup = %backup.display(),
                    "credentials file was corrupt ({source}); moved it aside and started a new one"
                );
                FileData::default()
            }
            Err(e) => return Err(e),
        };
        let out = f(&mut data);
        self.write_atomic(&data)?;
        Ok(out)
    }

    /// Copy the corrupt file to `<file>.corrupt-<unix-ts>` (0600).
    fn quarantine_corrupt(&self) -> Result<PathBuf, TokensError> {
        use std::io::Write as _;
        let raw = std::fs::read(&self.path)?;
        let target = self.resolved();
        let ts = jiff::Timestamp::now().as_second();
        for n in 0u32.. {
            let mut name = target.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".corrupt-{ts}"));
            if n > 0 {
                name.push(format!("-{n}"));
            }
            let backup = target.with_file_name(name);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&backup) {
                Ok(mut f) => {
                    f.write_all(&raw)?;
                    f.sync_all()?;
                    return Ok(backup);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!("unbounded backup name search")
    }

    /// The file a write must replace: a symlinked `credentials.json`
    /// (dotfile managers) keeps its link; the target is rewritten.
    fn resolved(&self) -> PathBuf {
        if let Ok(path) = std::fs::canonicalize(&self.path) {
            return path;
        }
        match std::fs::read_link(&self.path) {
            // Dangling link: write where it points.
            Ok(link) => match self.path.parent() {
                Some(parent) => parent.join(link),
                None => link,
            },
            Err(_) => self.path.clone(),
        }
    }

    /// Write a temp file next to the target (0600 from creation), fsync,
    /// rename over the target, fsync the directory. Never truncates in
    /// place. The temp file is removed on any failure.
    fn write_atomic(&self, data: &FileData) -> Result<(), TokensError> {
        use std::io::Write as _;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let raw = serde_json::to_vec_pretty(data)?;
        let target = self.resolved();
        let dir = match target.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir)?;
        let mut name = target.file_name().unwrap_or_default().to_os_string();
        name.push(format!(
            ".tmp.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let tmp = dir.join(name);
        let result = (|| -> Result<(), TokensError> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&tmp)?;
            // Keep a mode the user chose (e.g. 0640); default 0600.
            #[cfg(unix)]
            if let Ok(meta) = std::fs::metadata(&target) {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() & 0o7777;
                if mode != 0o600 {
                    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
                }
            }
            f.write_all(&raw)?;
            f.sync_all()?;
            drop(f);
            rename_replace(&tmp, &target)?;
            #[cfg(unix)]
            if let Ok(d) = std::fs::File::open(&dir) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
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
        self.update(|data| {
            data.providers
                .entry(provider.into())
                .or_default()
                .oauth_tokens = Some(tokens.clone());
        })
    }

    pub fn api_key(&self, provider: &str) -> Result<Option<String>, TokensError> {
        Ok(self
            .read()?
            .providers
            .get(provider)
            .map(|p| p.api_key.clone())
            .filter(|k| !k.is_empty()))
    }

    /// [`Self::save_tokens`] while holding the store lock (refresh lease).
    pub fn save_tokens_locked(
        &self,
        lock: &StoreLock,
        provider: &str,
        tokens: &Tokens,
    ) -> Result<(), TokensError> {
        self.update_locked(lock, |data| {
            data.providers
                .entry(provider.into())
                .or_default()
                .oauth_tokens = Some(tokens.clone());
        })
    }

    pub fn save_api_key(&self, provider: &str, key: &str) -> Result<(), TokensError> {
        self.update(|data| {
            data.providers.entry(provider.into()).or_default().api_key = key.to_string();
        })
    }

    pub fn delete(&self, provider: &str) -> Result<(), TokensError> {
        self.update(|data| {
            data.providers.remove(provider);
        })
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

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn saves_leave_no_temp_file_even_when_the_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("credentials.json"));
        store.save_api_key("a", "k1").unwrap();
        store.save_api_key("b", "k2").unwrap();
        store.delete("a").unwrap();
        assert!(leftovers(dir.path()).is_empty());
        assert_eq!(store.list().unwrap(), vec!["b".to_string()]);

        // Injected failure: the target is a non-empty directory, so the
        // final rename fails after the temp file was written.
        let blocked = dir.path().join("blocked.json");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("x"), "x").unwrap();
        let store = CredentialStore::new(&blocked);
        let err = store.write_atomic(&FileData::default()).unwrap_err();
        assert!(matches!(err, TokensError::Io(_)), "{err}");
        assert!(
            leftovers(dir.path()).is_empty(),
            "{:?}",
            leftovers(dir.path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn mode_stays_0600_and_a_user_chosen_mode_is_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("credentials.json"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        store.save_api_key("a", "k").unwrap();
        store.save_api_key("a", "k2").unwrap();
        assert_eq!(mode(store.path()), 0o600);
        assert_eq!(mode(&store.lock_path()), 0o600);
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o640)).unwrap();
        store.save_api_key("b", "k").unwrap();
        assert_eq!(mode(store.path()), 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_file_keeps_its_link_and_the_target_is_updated() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles").join("credentials.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, r#"{"providers":{"x":{"apiKey":"old"}}}"#).unwrap();
        let link = dir.path().join("credentials.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let store = CredentialStore::new(&link);
        store.save_api_key("y", "new").unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(raw["providers"]["x"]["apiKey"], "old");
        assert_eq!(raw["providers"]["y"]["apiKey"], "new");
        assert!(leftovers(real.parent().unwrap()).is_empty());
    }

    #[test]
    fn corrupt_file_reads_fail_clearly_and_a_save_quarantines_it() {
        for garbage in [&b"{\"providers\": {}}}"[..], &b""[..]] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("credentials.json");
            std::fs::write(&path, garbage).unwrap();
            let store = CredentialStore::new(&path);
            let err = store.tokens("anthropic").unwrap_err();
            assert!(matches!(&err, TokensError::Corrupt { path: p, .. } if p == &path));
            let msg = err.to_string();
            assert!(
                msg.contains(&path.display().to_string()) && msg.contains("rness auth login"),
                "{msg}"
            );
            // Reads never clear it.
            assert_eq!(std::fs::read(&path).unwrap(), garbage);

            store.save_api_key("anthropic", "k").unwrap();
            assert_eq!(store.api_key("anthropic").unwrap().as_deref(), Some("k"));
            let backups: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.to_string_lossy().contains(".corrupt-"))
                .collect();
            assert_eq!(backups.len(), 1);
            assert_eq!(std::fs::read(&backups[0]).unwrap(), garbage);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&backups[0]).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }
    }

    #[tokio::test]
    async fn refresh_lease_excludes_other_stores_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let a = CredentialStore::new(&path);
        let b = CredentialStore::new(&path);
        let lease = a
            .lock_for_refresh(std::time::Duration::from_secs(1))
            .await
            .unwrap();
        a.save_tokens_locked(&lease, "x", &Tokens::default())
            .unwrap();
        let err = b
            .lock_for_refresh(std::time::Duration::from_millis(50))
            .await
            .err()
            .expect("lease is exclusive");
        assert!(err.to_string().contains("timed out"), "{err}");
        drop(lease);
        b.lock_for_refresh(std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert!(a.tokens("x").unwrap().is_some());
    }

    #[test]
    fn credential_key_helper() {
        assert_eq!(
            CredentialStore::credential_key("anthropic", None),
            "anthropic"
        );
        assert_eq!(
            CredentialStore::credential_key("anthropic", Some("")),
            "anthropic"
        );
        assert_eq!(
            CredentialStore::credential_key("anthropic", Some("work")),
            "anthropic/work"
        );
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
