//! The terminal UI. One crate, plugin seams inside (NOT crate-per-widget).
//!
//! Architecture: slot-based component registry (deepseek's ui-* pattern,
//! Neovim's extension-point philosophy):
//! - core owns: frame loop, focus, event routing, slot layout
//! - features mount Components into named slots (statusline, sidebar,
//!   overlay, message_body, input_footer, picker) with disposers
//! - Lua components mount through the SAME seam -> vendor and user have
//!   equal power; UI features hot-reload
//! - core/ widgets (editor, viewport, markdown render) are themable and
//!   hookable but NOT swappable day 1 (perf-critical)
//! - a slow component cannot block the frame loop (render budget, cached
//!   buffers, async refresh)
//!
//! This crate talks rness-protocol ONLY. Multi-client (web, nvim) later
//! is additive, not a rewrite.

pub mod app;
pub mod component;
pub mod core;
pub mod keymaps;
pub mod keys;
pub mod modules;
pub mod slots;
pub mod theme;
