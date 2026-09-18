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
    pub process: ProcessConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            default: SandboxMode::DangerFullAccess,
            agent_overrides: AgentOverrides::TightenOnly,
            unavailable: UnavailableBehavior::Deny,
            process: Default::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessConfig {
    pub unix_shell: std::path::PathBuf,
    pub macos_runner: std::path::PathBuf,
    pub linux_runner: std::path::PathBuf,
    pub windows_shell: std::path::PathBuf,
    pub windows_container_runner: std::path::PathBuf,
    /// Explicit, locally available Linux image; None denies restricted commands.
    pub windows_container_image: Option<String>,
    pub windows_container_shell: String,
    pub windows_container_pids: u32,
    pub temp_parent: Option<std::path::PathBuf>,
}
impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            unix_shell: "/bin/sh".into(),
            macos_runner: "/usr/bin/sandbox-exec".into(),
            linux_runner: "/usr/bin/bwrap".into(),
            windows_shell: "powershell.exe".into(),
            windows_container_runner: "docker.exe".into(),
            windows_container_image: None,
            windows_container_shell: "/bin/sh".into(),
            windows_container_pids: 256,
            temp_parent: None,
        }
    }
}

impl ProcessConfig {
    pub fn validate(&self) -> Result<(), String> {
        for path in [
            &self.unix_shell,
            &self.macos_runner,
            &self.linux_runner,
            &self.windows_shell,
            &self.windows_container_runner,
        ] {
            if path.as_os_str().is_empty() || path.to_string_lossy().contains('\0') {
                return Err(
                    "sandbox process executable must be nonempty and contain no NUL".into(),
                );
            }
        }
        if self.windows_container_pids == 0
            || self.windows_container_shell.is_empty()
            || self.windows_container_shell.contains('\0')
        {
            return Err("sandbox container requires a shell and positive process limit".into());
        }
        if self.windows_container_image.as_ref().is_some_and(|image| {
            image.is_empty()
                || image.starts_with('-')
                || image.chars().any(char::is_whitespace)
                || image.contains('\0')
        }) {
            return Err("sandbox container image must be a nonempty image reference".into());
        }
        Ok(())
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
