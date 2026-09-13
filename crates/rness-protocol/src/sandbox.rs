//! Shared sandbox vocabulary used by durable session configuration and tools.

use serde::{Deserialize, Serialize};

/// Filesystem authority for an agent session's process tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Process tools may read but may not write filesystem state.
    ReadOnly,
    /// Process tools may write only in the immutable session workspace.
    WorkspaceWrite,
    /// Process tools use the host user's ordinary authority.
    #[default]
    DangerFullAccess,
}

impl SandboxMode {
    /// Whether `self` is no broader than `ceiling`.
    pub fn is_at_most(self, ceiling: Self) -> bool {
        self <= ceiling
    }
}
