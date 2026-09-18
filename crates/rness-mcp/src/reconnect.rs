//! Host-owned reconnect policy. Tool calls are never replayed.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconnectPolicy {
    pub enabled: bool,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub max_attempts: u32,
}
impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            initial_delay_ms: 500,
            max_delay_ms: 30_000,
            max_attempts: 5,
        }
    }
}
impl ReconnectPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.initial_delay_ms == 0
            || self.max_delay_ms < self.initial_delay_ms
            || self.max_delay_ms > 3_600_000
            || self.max_attempts == 0
            || self.max_attempts > 100
        {
            return Err("reconnect requires 1 <= initial_delay_ms <= max_delay_ms <= 3600000 and 1 <= max_attempts <= 100".into());
        }
        Ok(())
    }
}
