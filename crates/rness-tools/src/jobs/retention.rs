//! Output storage policy shared by persistent and temporary job artifacts.
use serde::{Deserialize, Serialize};

/// Defaults bound every host (7 days, 2 GiB total, 256 MiB per job, 200
/// foreground captures); each limit can be set to zero to disable it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    /// Zero disables this limit.
    pub max_job_bytes: u64,
    /// Total output bytes owned by this registry; zero disables the limit.
    /// Reaching it evicts the oldest settled, delivered artifacts first; a
    /// producer is cancelled only when nothing is evictable.
    pub max_total_bytes: u64,
    /// Age after settlement; zero disables automatic deletion.
    pub max_age_secs: u64,
    /// Settled foreground command captures (`bash-output`) kept; the oldest
    /// beyond this are evicted. Zero disables the cap.
    pub max_bash_captures: u64,
    pub cleanup_interval_secs: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_job_bytes: 256 * 1024 * 1024,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            max_age_secs: 7 * 24 * 60 * 60,
            max_bash_captures: 200,
            cleanup_interval_secs: 60,
        }
    }
}

impl Retention {
    pub fn validate(&self) -> Result<(), String> {
        if self.cleanup_interval_secs == 0 || self.cleanup_interval_secs > 86400 {
            return Err("jobs.retention.cleanup_interval_secs must be between 1 and 86400".into());
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Budget {
    pub policy: Retention,
    pub used: u64,
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
