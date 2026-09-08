//! The plugin kernel — rness's Cordis equivalent.
//!
//! RULE: this crate knows NOTHING about AI, sessions, or LLMs.
//! If a type in here mentions "session" or "model", it is in the wrong crate.
//!
//! Building blocks:
//! - [`context`]: the plugin-facing API (provide/get services, listen, effects)
//! - [`services`]: named service slots, type-erased registry
//! - [`events`]: typed bus with emit / bail / waterfall / serial dispatch
//! - [`effects`]: disposers — every side effect is reversible (hot reload)
//! - [`plugin`]: the Plugin trait and the Kernel (activation by injected
//!   services, derived boot order, cascade deactivation, reload)

pub mod context;
pub mod effects;
pub mod events;
pub mod plugin;
pub mod presentation;
pub mod services;

pub use context::Context;
pub use effects::{Disposer, EffectBag};
pub use events::{BailEvent, Event, EventBus, Next};
pub use plugin::{Kernel, Plugin, PluginStatus};
pub use services::ServiceRegistry;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KernelError {
    #[error("service '{0}' already has a provider")]
    DuplicateService(String),
    #[error("plugin '{0}' is already registered")]
    DuplicatePlugin(String),
    #[error("unknown plugin '{0}'")]
    UnknownPlugin(String),
    #[error("plugin '{plugin}' failed to apply: {reason}")]
    ApplyFailed { plugin: String, reason: String },
}
