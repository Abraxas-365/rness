//! rness.* namespaces.
//!
//! `tool`, `hook`, `events`, `ui.statusline`, `log`, `json` are built
//! directly in [`crate::runtime`] (they park registrations for the
//! drain). The modules here are the larger bridges: engine services and
//! host conveniences. Richer `ui` components land in M5.

pub mod config;
pub mod fs;
pub mod http;
pub mod jobs;
pub mod mcp;
pub mod process;
pub mod session;
pub mod subagents;
pub mod tools;
