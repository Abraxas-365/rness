//! System-prompt sections: named, ordered guidance. Sources:
//! - config: `rness.system_prompt.section{...}` in init.lua (`TurnConfig.sections`);
//! - tools: `Tool::prompt_section` (Lua: `rness.tool.register{ prompt = ... }`),
//!   named `tool:<name>` and tied to that tool, so the guidance ships, reloads
//!   and unloads with the tool.
//!
//! A section tied to tools is included only on steps where the agent can
//! actually use one of them — the dsh convention that tool guidance ships
//! with the tool, not in the persona.

use serde::{Deserialize, Serialize};

/// Largest accepted section text, in bytes (after trimming).
pub const MAX_TEXT_BYTES: usize = 4096;

/// Guidance a tool contributes about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPrompt {
    pub order: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSection {
    /// Unique name; ties in `order` are broken by name.
    pub name: String,
    /// Ascending position among sections (all follow the role instructions).
    #[serde(default)]
    pub order: i64,
    /// Include only while one of these tools is usable. Empty: always.
    #[serde(default)]
    pub tools: Vec<String>,
    pub text: String,
}

/// Applicable sections, sorted by (order, name), joined with blank lines.
/// Empty when none apply. Callers merge sources with [`merge`] first.
pub fn render(sections: &[PromptSection], usable: impl Fn(&str) -> bool) -> String {
    let mut applicable: Vec<_> = sections
        .iter()
        .filter(|section| section.tools.is_empty() || section.tools.iter().any(|t| usable(t)))
        .collect();
    applicable.sort_by(|a, b| (a.order, &a.name).cmp(&(b.order, &b.name)));
    applicable
        .iter()
        .map(|section| section.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Config sections plus tool sections. A config section with the same name
/// as a tool section (`tool:<name>`) replaces it, so init.lua can reword a
/// plugin's or built-in tool's guidance without editing the plugin
/// (as in dsh, where same-named sections shadow rather than duplicate).
pub fn merge(config: &[PromptSection], tools: Vec<PromptSection>) -> Vec<PromptSection> {
    let mut merged = config.to_vec();
    merged.extend(
        tools
            .into_iter()
            .filter(|tool| !config.iter().any(|c| c.name == tool.name)),
    );
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(name: &str, order: i64, tools: &[&str]) -> PromptSection {
        PromptSection {
            name: name.into(),
            order,
            tools: tools.iter().map(|t| t.to_string()).collect(),
            text: name.to_uppercase(),
        }
    }

    #[test]
    fn filters_by_usable_tools_and_sorts_by_order_then_name() {
        let sections = [
            section("b", 10, &[]),
            section("a", 10, &[]),
            section("first", -5, &["Bash"]),
            section("hidden", 0, &["workflow"]),
            section("any", 20, &["missing", "Read"]),
        ];
        let usable = |name: &str| matches!(name, "Bash" | "Read");
        assert_eq!(render(&sections, usable), "FIRST\n\nA\n\nB\n\nANY");
        assert_eq!(render(&[section("x", 0, &["gone"])], usable), "");
    }

    #[test]
    fn config_sections_replace_same_named_tool_sections() {
        let config = [section("tool:Bash", 5, &["Bash"])];
        let tools = vec![
            section("tool:Bash", 1, &["Bash"]),
            section("tool:Read", 2, &["Read"]),
        ];
        let merged = merge(&config, tools);
        assert_eq!(
            merged
                .iter()
                .map(|s| (s.name.as_str(), s.order))
                .collect::<Vec<_>>(),
            [("tool:Bash", 5), ("tool:Read", 2)]
        );
    }
}
