//! Extensible session commands, shared by all service clients.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use rness_protocol::events::SessionId;
use crate::service::{ServiceError, SessionService};

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

pub struct CommandInvocation<'a> {
    pub session: &'a SessionId,
    /// Original arguments, including separator whitespace.
    pub raw_input: &'a str,
}

pub trait Command: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn execute(&self, service: &SessionService, invocation: CommandInvocation<'_>) -> Result<CommandResult, ServiceError>;
}

#[derive(Default)]
pub struct CommandRegistry {
    commands: RwLock<BTreeMap<String, Arc<dyn Command>>>,
}

impl CommandRegistry {
    pub fn register(&self, command: Arc<dyn Command>) -> Result<(), String> {
        let name = command.name();
        if !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-') {
            return Err("command names must start with a lowercase letter and contain lowercase letters, digits, underscores or hyphens".into());
        }
        let mut commands = self.commands.write().expect("command registry lock");
        if commands.contains_key(name) {
            return Err(format!("command '{name}' is already registered"));
        }
        commands.insert(name.to_owned(), command);
        Ok(())
    }

    pub fn unregister_if_current(&self, command: &Arc<dyn Command>) -> bool {
        let mut commands = self.commands.write().expect("command registry lock");
        if commands.get(command.name()).is_some_and(|current| Arc::ptr_eq(current, command)) {
            commands.remove(command.name());
            true
        } else {
            false
        }
    }

    pub fn catalog(&self) -> Vec<(String, String)> {
        self.commands.read().expect("command registry lock").values()
            .map(|c| (c.name().to_owned(), c.description().to_owned())).collect()
    }

    pub fn resolve(&self, text: &str) -> Option<(Arc<dyn Command>, usize)> {
        let input = text.strip_prefix('/')?;
        let end = input.find(char::is_whitespace).unwrap_or(input.len());
        let command = self.commands.read().expect("command registry lock").get(&input[..end]).cloned()?;
        Some((command, end + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_resolution_and_identity_safe_removal() {
        let registry = CommandRegistry::default();
        let original: Arc<dyn Command> = Arc::new(AgentCommand);
        let other: Arc<dyn Command> = Arc::new(AgentCommand);
        registry.register(original.clone()).unwrap();
        assert!(registry.register(other.clone()).is_err());
        let text = "/agent  reviewer\n extra";
        let (_, offset) = registry.resolve(text).unwrap();
        assert_eq!(&text[offset..], "  reviewer\n extra");
        assert!(registry.resolve("/agent-extra").is_none());
        assert!(registry.resolve("ordinary text").is_none());
        assert_eq!(registry.catalog(), vec![("agent".into(), "Choose an agent".into())]);
        assert!(!registry.unregister_if_current(&other));
        assert!(registry.unregister_if_current(&original));
        assert!(registry.resolve("/agent").is_none());
    }
}

pub struct AgentCommand;

impl Command for AgentCommand {
    fn name(&self) -> &str { "agent" }
    fn description(&self) -> &str { "Choose an agent" }
    fn execute(&self, service: &SessionService, invocation: CommandInvocation<'_>) -> Result<CommandResult, ServiceError> {
        let mut args = invocation.raw_input.split_whitespace();
        let name = args.next().ok_or_else(|| ServiceError::InvalidConfig("Usage: /agent <name>".into()))?;
        if args.next().is_some() {
            return Err(ServiceError::InvalidConfig("Usage: /agent <name>".into()));
        }
        service.select_agent(invocation.session, name)?;
        Ok(CommandResult { message: format!("Agent selected: {name}"), data: serde_json::json!({"agent": name}) })
    }
}
