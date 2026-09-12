//! Branch/lineage types. A branch is a fork point in the log: a forked
//! session's header carries a [`ForkRef`]; its history is the parent's
//! committed prefix up to (and including) `at`, replayed — never copied —
//! plus its own events. Lineage is acyclic by construction (invariant #4):
//! a fork can only reference an already-committed parent event.

use serde::{Deserialize, Serialize};

use crate::events::{EventId, SessionId};

/// Reference from a forked session to its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRef {
    pub session: SessionId,
    /// Last parent event included in this fork's history.
    pub at: EventId,
}

/// One hop in a session's ancestry, root first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AncestryHop {
    pub session: SessionId,
    /// Fork point into the NEXT hop (None for the queried session itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_at: Option<EventId>,
}

/// A child fork of a session, as reported by lineage queries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChildRef {
    pub session: SessionId,
    /// Parent event the child forked from.
    pub at: EventId,
}

/// Delegation lineage: stamped on sessions created BY an agent (subagent
/// runs), fork and spawn alike. Distinct from [`ForkRef`]: ForkRef seeds
/// history (replay); Delegation records who delegated and how deep.
/// Depth is parent's depth + 1 — enforced against a max at start time so
/// runaway recursive delegation cannot happen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    pub parent: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    pub depth: u32,
    /// Child shape (dsh): one-shot settles once; continuable keeps a
    /// durable session that accepts later messages and interrupts.
    #[serde(default)]
    pub mode: DelegationMode,
}

/// How a delegated child runs. Stamped durably so discovery
/// (`list_agents`) can tell shapes apart without loading the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMode {
    #[default]
    OneShot,
    Continuable,
}
