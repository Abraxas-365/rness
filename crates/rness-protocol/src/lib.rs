//! Wire types only. serde structs/enums — no logic, no async, no I/O.
//!
//! Frontends (TUI, server clients, SDK) depend on THIS crate, never on
//! engine internals. This boundary is what makes multi-client possible.
//!
//! - [`events`]: versioned durable session-log events (start at v1)
//! - [`frames`]: live streaming frames (ephemeral, never persisted)
//! - [`branch`]: fork points, lineage, active-path types
//! - [`api`]: SessionService request/response types

pub mod api;
pub mod branch;
pub mod events;
pub mod frames;
pub mod sandbox;
