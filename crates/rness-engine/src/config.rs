//! User-declared model facts and reusable request preferences. No catalog data.

use std::collections::BTreeMap;

use rness_protocol::events::{CallConfig, ModelSelection, Reasoning};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    /// None leaves support unknown; false explicitly forbids image input.
    pub image_input: Option<bool>,
    pub temperature: Option<bool>,
    pub output_token_limit: Option<bool>,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning: Option<ReasoningCapabilities>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningCapabilities {
    pub efforts: Option<Vec<String>>,
    pub budget_tokens: Option<TokenRange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRange {
    pub min: u32,
    pub max: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestOptions {
    pub reasoning: Option<Reasoning>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Profile {
    Fixed {
        provider: String,
        model: String,
        #[serde(default)]
        options: RequestOptions,
    },
    ByProvider { by_provider: BTreeMap<String, ProfileVariant> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileVariant {
    pub model: String,
    #[serde(default)]
    pub options: RequestOptions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDeclaration {
    pub provider: String,
    pub model: String,
    pub capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinition {
    #[serde(default)]
    pub subagent: bool,
    pub description: String,
    pub instructions: String,
    pub profile: Option<String>,
    /// None adds no restriction; an empty list allows no tools.
    pub tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    models: BTreeMap<(String, String), ModelCapabilities>,
    profiles: BTreeMap<String, Profile>,
}

impl ModelRegistry {
    pub fn declare_model(&mut self, declaration: ModelDeclaration) -> Result<(), String> {
        let caps = &declaration.capabilities;
        if declaration.provider.is_empty() || declaration.model.is_empty() {
            return Err("model declaration requires a provider and model".into());
        }
        if caps.context_window == Some(0) || caps.max_output_tokens == Some(0) {
            return Err("model token limits must be positive".into());
        }
        if let Some(reasoning) = &caps.reasoning {
            if let Some(range) = &reasoning.budget_tokens {
                if range.min == 0 || range.min > range.max {
                    return Err("invalid reasoning token range".into());
                }
            }
            if let Some(efforts) = &reasoning.efforts {
                if efforts.iter().any(|effort| effort.is_empty()) {
                    return Err("reasoning efforts must not be empty strings".into());
                }
            }
        }
        let key = (declaration.provider, declaration.model);
        if self.models.contains_key(&key) {
            return Err(format!("duplicate model declaration: {}/{}", key.0, key.1));
        }
        self.models.insert(key, declaration.capabilities);
        Ok(())
    }

    pub fn declare_profile(&mut self, name: String, profile: Profile) -> Result<(), String> {
        if name.is_empty() { return Err("profile requires a name".into()); }
        let variants: Vec<_> = match &profile {
            Profile::Fixed { provider, model, options } => vec![(provider, model, options)],
            Profile::ByProvider { by_provider } => {
                if by_provider.is_empty() { return Err("by_provider requires at least one connection".into()); }
                by_provider.iter().map(|(provider, variant)| (provider, &variant.model, &variant.options)).collect()
            }
        };
        for (provider, model, options) in variants {
            if provider.trim().is_empty() || model.trim().is_empty() {
                return Err("profile requires nonempty provider and model names".into());
            }
            self.validate(&CallConfig { selection: Some(ModelSelection { route: provider.clone(), model: model.clone() }),
                reasoning: options.reasoning.clone(), max_output_tokens: options.max_output_tokens,
                temperature: options.temperature, ..Default::default() })?;
        }
        if self.profiles.contains_key(&name) {
            return Err(format!("duplicate profile: {name}"));
        }
        self.profiles.insert(name, profile);
        Ok(())
    }

    pub fn profile_names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    pub fn model_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.models.keys().map(|(provider, model)| format!("{provider}/{model}")).collect();
        names.sort();
        names
    }

    pub fn capabilities(&self, selection: &ModelSelection) -> Option<&ModelCapabilities> {
        self.models.get(&(selection.route.clone(), selection.model.clone()))
    }

    pub fn validate_profile(&self, name: &str) -> Result<(), String> {
        match self.profiles.get(name).ok_or_else(|| format!("unknown profile: {name}"))? {
            Profile::Fixed { .. } => { self.resolve_profile(name)?; }
            Profile::ByProvider { by_provider } => {
                for provider in by_provider.keys() { self.resolve_profile_for(name, Some(provider))?; }
            }
        }
        Ok(())
    }

    pub fn resolve_profile(&self, name: &str) -> Result<CallConfig, String> {
        self.resolve_profile_for(name, None)
    }

    pub fn resolve_profile_for(&self, name: &str, provider: Option<&str>) -> Result<CallConfig, String> {
        let profile = self.profiles.get(name).ok_or_else(|| format!("unknown profile: {name}"))?;
        let (provider, model, options) = match profile {
            Profile::Fixed { provider, model, options } => (provider.as_str(), model, options),
            Profile::ByProvider { by_provider } => {
                let provider = provider.ok_or_else(|| format!("profile '{name}' requires a current provider connection"))?;
                let variant = by_provider.get(provider).ok_or_else(|| format!("profile '{name}' has no variant for provider '{provider}'"))?;
                (provider, &variant.model, &variant.options)
            }
        };
        let config = CallConfig {
            tool_ceiling: None,
            agent: None,
            selection: Some(ModelSelection {
                route: provider.into(),
                model: model.clone(),
            }),
            profile: Some(name.into()),
            reasoning: options.reasoning.clone(),
            max_output_tokens: options.max_output_tokens,
            temperature: options.temperature,
        };
        self.validate(&config)?;
        Ok(config)
    }

    pub fn validate(&self, config: &CallConfig) -> Result<(), String> {
        if config.max_output_tokens == Some(0) {
            return Err("max_output_tokens must be positive".into());
        }
        if config.temperature.is_some_and(|value| !value.is_finite()) {
            return Err("temperature must be finite".into());
        }
        let caps = config.selection.as_ref().and_then(|selection| self.capabilities(selection));
        if config.temperature.is_some() && caps.and_then(|caps| caps.temperature) == Some(false) {
            return Err("temperature is not supported by this model".into());
        }
        if config.max_output_tokens.is_some() && caps.and_then(|caps| caps.output_token_limit) == Some(false) {
            return Err("max_output_tokens is not supported by this model".into());
        }
        if let (Some(requested), Some(limit)) = (
            config.max_output_tokens,
            caps.and_then(|caps| caps.max_output_tokens),
        ) {
            if requested > limit {
                return Err(format!("max_output_tokens {requested} exceeds declared limit {limit}"));
            }
        }
        match &config.reasoning {
            Some(Reasoning::Effort { effort }) => {
                if effort.is_empty() {
                    return Err("reasoning effort must not be empty".into());
                }
                if let Some(efforts) = caps.and_then(|caps| caps.reasoning.as_ref())
                    .and_then(|reasoning| reasoning.efforts.as_ref())
                {
                    if !efforts.contains(effort) {
                        return Err(format!("reasoning effort not declared as supported: {effort}"));
                    }
                }
            }
            Some(Reasoning::BudgetTokens { tokens }) => {
                if *tokens == 0 {
                    return Err("reasoning budget must be positive".into());
                }
                if let Some(range) = caps.and_then(|caps| caps.reasoning.as_ref())
                    .and_then(|reasoning| reasoning.budget_tokens.as_ref())
                {
                    if *tokens < range.min || *tokens > range.max {
                        return Err("reasoning budget outside declared range".into());
                    }
                }
            }
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicitly_unsupported_generation_controls_are_rejected() {
        let mut registry = ModelRegistry::default();
        registry.declare_model(ModelDeclaration {
            provider: "p".into(), model: "m".into(),
            capabilities: ModelCapabilities { temperature: Some(false), output_token_limit: Some(false), ..Default::default() },
        }).unwrap();
        let mut config = CallConfig {
            selection: Some(ModelSelection { route: "p".into(), model: "m".into() }),
            ..Default::default()
        };
        assert!(registry.validate(&config).is_ok());
        config.temperature = Some(0.5);
        assert!(registry.validate(&config).is_err());
        config.temperature = None;
        config.max_output_tokens = Some(100);
        assert!(registry.validate(&config).is_err());
    }

    #[test]
    fn provider_profile_rejects_ambiguous_or_invalid_shapes() {
        for value in [
            serde_json::json!({"provider":"a","model":"b","by_provider":{"a":{"model":"c"}}}),
            serde_json::json!({"by_provider":{"a":{"model":"c","provider":"other"}}}),
            serde_json::json!({"by_provider":{"a":{"model":"c"}},"options":{}}),
        ] { assert!(serde_json::from_value::<Profile>(value).is_err()); }
        for value in [
            serde_json::json!({"by_provider":{}}),
            serde_json::json!({"by_provider":{"a":{"model":""}}}),
            serde_json::json!({"by_provider":{"a":{"model":"c","options":{"max_output_tokens":0}}}}),
        ] {
            let profile = serde_json::from_value(value).unwrap();
            assert!(ModelRegistry::default().declare_profile("small".into(), profile).is_err());
        }
        let mut registry = ModelRegistry::default();
        registry.declare_profile("small".into(), serde_json::from_value(serde_json::json!({"by_provider":{"a":{"model":"c"}}})).unwrap()).unwrap();
        assert!(registry.validate_profile("small").is_ok());
        assert!(registry.resolve_profile("small").unwrap_err().contains("current provider"));
    }

    #[test]
    fn profile_resolves_without_catalog_and_unknown_profile_fails() {
        let mut registry = ModelRegistry::default();
        registry.declare_profile("local".into(), Profile::Fixed {
            provider: "ollama".into(), model: "qwen3:14b".into(),
            options: RequestOptions::default(),
        }).unwrap();
        let config = registry.resolve_profile("local").unwrap();
        assert_eq!(config.selection.unwrap().model, "qwen3:14b");
        assert_eq!(config.profile.as_deref(), Some("local"));
        assert_eq!(config.reasoning, None);
        assert!(registry.resolve_profile("missing").is_err());
    }

    #[test]
    fn limits_are_scoped_to_connection_and_do_not_clamp() {
        let mut registry = ModelRegistry::default();
        registry.declare_model(ModelDeclaration {
            provider: "one".into(), model: "same".into(),
            capabilities: ModelCapabilities {
                max_output_tokens: Some(100), ..Default::default()
            },
        }).unwrap();
        let mut config = CallConfig {
            selection: Some(ModelSelection { route: "one".into(), model: "same".into() }),
            max_output_tokens: Some(101), ..Default::default()
        };
        assert!(registry.validate(&config).is_err());
        assert_eq!(config.max_output_tokens, Some(101));
        config.selection.as_mut().unwrap().route = "two".into();
        assert!(registry.validate(&config).is_ok());
    }

    #[test]
    fn malformed_and_duplicate_declarations_fail() {
        let mut registry = ModelRegistry::default();
        let mut declaration = ModelDeclaration {
            provider: "p".into(), model: "m".into(),
            capabilities: ModelCapabilities { context_window: Some(0), ..Default::default() },
        };
        assert!(registry.declare_model(declaration.clone()).is_err());
        declaration.capabilities.context_window = Some(100);
        registry.declare_model(declaration.clone()).unwrap();
        assert!(registry.declare_model(declaration).is_err());
    }
}
