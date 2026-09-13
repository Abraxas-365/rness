//! Immutable filesystem-effect policy selected at startup and enforced by
//! process-tool backends. This module deliberately does not claim network,
//! process-count, or plugin/native-code isolation.

use serde::{Deserialize, Serialize};

pub use rness_protocol::sandbox::SandboxMode;

/// Startup policy declared with `rness.sandbox.setup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// Deliberately preserves existing unrestricted behavior until a user opts in.
    pub default: SandboxMode,
    /// Agent roles may only retain or narrow the global mode.
    pub agent_overrides: AgentOverrides,
    /// Enforced modes never silently execute unrestricted.
    pub unavailable: UnavailableBehavior,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            default: SandboxMode::DangerFullAccess,
            agent_overrides: AgentOverrides::TightenOnly,
            unavailable: UnavailableBehavior::Deny,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentOverrides {
    #[default]
    TightenOnly,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnavailableBehavior {
    #[default]
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_ordered_from_least_to_most_authority() {
        assert!(SandboxMode::ReadOnly.is_at_most(SandboxMode::WorkspaceWrite));
        assert!(SandboxMode::WorkspaceWrite.is_at_most(SandboxMode::DangerFullAccess));
        assert!(!SandboxMode::DangerFullAccess.is_at_most(SandboxMode::WorkspaceWrite));
    }

    #[test]
    fn defaults_preserve_existing_unrestricted_execution() {
        assert_eq!(
            SandboxConfig::default().default,
            SandboxMode::DangerFullAccess
        );
    }
}
