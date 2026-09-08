//! Ollama adapter: a thin constructor over the OpenAI-compatible adapter.
//! Ollama serves Chat Completions (streaming + tools) at `/v1`, so a
//! separate native `/api/chat` client would just duplicate the mapping.

use crate::openai::OpenAiProvider;

pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// A provider for a local Ollama instance at the default port.
pub fn provider(model: impl Into<String>) -> OpenAiProvider {
    provider_at(DEFAULT_BASE_URL, model)
}

/// A provider for an Ollama instance at `base_url` (include `/v1`).
pub fn provider_at(base_url: impl Into<String>, model: impl Into<String>) -> OpenAiProvider {
    // Ollama ignores auth; the key just satisfies the header.
    OpenAiProvider::new("ollama", model).with_base_url(base_url)
}
