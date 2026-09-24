//! The Lua integration — rness's soul.
//!
//! Architecture decisions:
//! - ONE mlua VM (real Lua 5.4), not luacfg-stubs + runtime + worker replay
//! - async Lua: tools/hooks await Rust futures directly (mlua `async` feature)
//! - capability injection per context instead of neutralization-by-replay
//! - Lua plugins register into the SAME kernel seams as Rust plugins (peers)
//! - NO sandbox: plugins run with the user's full permissions and the
//!   whole Lua stdlib (`os`, `io`, `require`). Like Neovim, trust is the
//!   user's call — installing a plugin means trusting its author. The
//!   `rness.*` API exists for integration (approval seam, workspace,
//!   logging), not confinement.
//!
//! The rness.* API surface is stable; the
//! implementation is not.

pub mod api;
pub mod hooks_json;
pub mod loader;
pub mod packages;
pub mod plugin_host;
pub mod reload;
pub mod runtime;
