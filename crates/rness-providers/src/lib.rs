//! LLM provider adapters, each a leaf plugin registering into the engine's
//! provider seam. We own the provider trait — no LLM abstraction crates
//! (they always lag providers; we need exact streaming/tool_use control).

pub mod anthropic;
pub mod auth;
pub mod ollama;
pub mod openai;
pub mod responses;
pub mod routes;
pub mod sse;
