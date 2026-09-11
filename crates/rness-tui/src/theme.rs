//! Colors and styles, centralized so modules stay consistent and a
//! future config layer can swap palettes.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, PartialEq, Eq)]
pub struct Theme {
    pub added: Style,
    pub removed: Style,
    pub user_prefix: Style,
    pub user_message: Style,
    pub assistant_text: Style,
    pub thinking: Style,
    pub tool_name: Style,
    pub tool_output: Style,
    pub error: Style,
    pub statusline: Style,
    pub statusline_accent: Style,
    pub editor_prompt: Style,
    pub heading: Style,
    pub code: Style,
    pub code_block: Style,
    pub dim: Style,
    pub overlay: Style,
    pub overlay_border: Style,
}

impl Theme {
    pub fn from_overrides(value: &serde_json::Value) -> Result<Self, String> {
        let groups = value.as_object().ok_or("theme must be an object")?;
        let mut theme = Self::default();
        for (name, value) in groups {
            let target = match name.as_str() {
                "added" => &mut theme.added, "removed" => &mut theme.removed,
                "user_message" => &mut theme.user_message,
                "user_prefix" => &mut theme.user_prefix, "assistant_text" => &mut theme.assistant_text,
                "thinking" => &mut theme.thinking, "tool_name" => &mut theme.tool_name,
                "tool_output" => &mut theme.tool_output, "error" => &mut theme.error,
                "statusline" => &mut theme.statusline, "statusline_accent" => &mut theme.statusline_accent,
                "editor_prompt" => &mut theme.editor_prompt, "heading" => &mut theme.heading,
                "code" => &mut theme.code, "code_block" => &mut theme.code_block,
                "dim" => &mut theme.dim, "overlay" => &mut theme.overlay,
                "overlay_border" => &mut theme.overlay_border,
                _ => return Err(format!("unknown theme group: {name}")),
            };
            for (field, value) in value.as_object().ok_or_else(|| format!("{name} must be an object"))? {
                match field.as_str() {
                    "fg" | "bg" => {
                        let text = value.as_str().ok_or("color must be a string")?;
                        let color = if text == "default" { Color::Reset } else { text.parse::<Color>().map_err(|_| format!("invalid color: {text}"))? };
                        *target = if field == "fg" { target.fg(color) } else { target.bg(color) };
                    }
                    "bold" | "italic" | "underline" | "reverse" => {
                        let enabled = value.as_bool().ok_or("style attribute must be boolean")?;
                        let modifier = match field.as_str() {
                            "bold" => Modifier::BOLD, "italic" => Modifier::ITALIC,
                            "underline" => Modifier::UNDERLINED, _ => Modifier::REVERSED,
                        };
                        *target = if enabled { target.add_modifier(modifier) } else { target.remove_modifier(modifier) };
                    }
                    _ => return Err(format!("unknown style field: {field}")),
                }
            }
        }
        Ok(theme)
    }
    pub fn validate_messagebox(&self, value: &serde_json::Value) -> Result<(), String> {
        fn walk(theme: &Theme, value: &serde_json::Value, path: &str) -> Result<(), String> {
            let Some(fields) = value.as_object() else { return Ok(()); };
            for (key, value) in fields {
                let path = format!("{path}.{key}");
                if key == "style" || matches!(key.as_str(), "heading" | "link" | "quote" | "inline_code") {
                    theme.resolve_style(value, Style::default()).map_err(|e| format!("{path}: {e}"))?;
                } else if key == "tools" {
                    if let Some(tools) = value.as_object() {
                        for (name, options) in tools { walk(theme, options, &format!("{path}.{name}"))?; }
                    }
                } else if key == "keys" {
                    if let Some(keys) = value.as_object() {
                        let mut seen = Vec::new();
                        for (name, chord) in keys {
                            if let Some(parsed) = chord.as_str().and_then(crate::keys::Chord::parse) {
                                if seen.contains(&parsed) { return Err(format!("{path}.{name}: duplicate key chord")); }
                                seen.push(parsed);
                            }
                            if chord != false && chord.as_str().and_then(crate::keys::Chord::parse).is_none() {
                                return Err(format!("{path}.{name}: invalid key chord"));
                            }
                        }
                    }
                } else { walk(theme, value, &path)?; }
            }
            Ok(())
        }
        walk(self, value, "ui.messagebox")
    }

    pub fn resolve_style(&self, value: &serde_json::Value, inherited: Style) -> Result<Style, String> {
        if let Some(name) = value.as_str() {
            return self.named_style(name).map(|style| inherited.patch(style))
                .ok_or_else(|| format!("unknown theme group: {name}"));
        }
        let fields = value.as_object().ok_or("style must be a theme group or object")?;
        let mut style = inherited;
        for (field, value) in fields {
            match field.as_str() {
                "fg" | "bg" => {
                    let text = value.as_str().ok_or("color must be a string")?;
                    let color = if text == "default" { Color::Reset } else {
                        text.parse::<Color>().map_err(|_| format!("invalid color: {text}"))?
                    };
                    style = if field == "fg" { style.fg(color) } else { style.bg(color) };
                }
                "bold" | "italic" | "underline" | "reverse" => {
                    let enabled = value.as_bool().ok_or("style attribute must be boolean")?;
                    let modifier = match field.as_str() {
                        "bold" => Modifier::BOLD,
                        "italic" => Modifier::ITALIC,
                        "underline" => Modifier::UNDERLINED,
                        _ => Modifier::REVERSED,
                    };
                    style = if enabled { style.add_modifier(modifier) } else { style.remove_modifier(modifier) };
                }
                _ => return Err(format!("unknown style field: {field}")),
            }
        }
        Ok(style)
    }

    fn named_style(&self, name: &str) -> Option<Style> {
        Some(match name {
            "added" => self.added,
            "removed" => self.removed,
            "user_prefix" => self.user_prefix,
            "user_message" => self.user_message,
            "assistant_text" => self.assistant_text,
            "thinking" => self.thinking,
            "title" | "tool_name" => self.tool_name,
            "tool_output" => self.tool_output,
            "error" => self.error,
            "statusline" => self.statusline,
            "statusline_accent" => self.statusline_accent,
            "editor_prompt" => self.editor_prompt,
            "heading" => self.heading,
            "code" => self.code,
            "code_block" => self.code_block,
            "dim" => self.dim,
            "overlay" => self.overlay,
            "overlay_border" => self.overlay_border,
            _ => return None,
        })
    }

    /// Resolve a Lua card style name, falling back for unknown plugin names.
    pub fn card_style(&self, name: &str) -> Style {
        self.named_style(name).unwrap_or(self.tool_output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn direct_styles_preserve_inherited_fields_and_remove_modifiers() {
        let theme = Theme::default();
        let inherited = Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD);
        let style = theme.resolve_style(&json!({"fg":"#ebdbb2", "bold":false}), inherited).unwrap();
        assert_eq!(style.fg, Some(Color::Rgb(235, 219, 178)));
        assert_eq!(style.bg, Some(Color::Blue));
        assert!(!style.add_modifier.contains(Modifier::BOLD));
        assert!(style.sub_modifier.contains(Modifier::BOLD));
        assert_eq!(theme.resolve_style(&json!({"bg":"default"}), inherited).unwrap().bg, Some(Color::Reset));
    }

    #[test]
    fn references_follow_active_theme_but_literals_do_not() {
        let first = Theme::default();
        let second = Theme::from_overrides(&json!({"assistant_text":{"fg":"#ebdbb2"}})).unwrap();
        let reference = json!("assistant_text");
        assert_ne!(first.resolve_style(&reference, Style::default()).unwrap(), second.resolve_style(&reference, Style::default()).unwrap());
        let literal = json!({"fg":"#fabd2f"});
        assert_eq!(first.resolve_style(&literal, Style::default()).unwrap(), second.resolve_style(&literal, Style::default()).unwrap());
        assert_eq!(second.card_style("assistant_text"), second.assistant_text);
        assert_eq!(second.card_style("title"), second.tool_name);
        assert_eq!(second.card_style("unknown"), second.tool_output);
    }

    #[test]
    fn messagebox_validation_checks_styles_and_chords_without_interpreting_tool_names() {
        let theme = Theme::default();
        assert!(theme.validate_messagebox(&json!({"tools":{"style":{"style":"tool_output"}}})).is_ok());
        assert!(theme.validate_messagebox(&json!({"user":{"style":{"bg":"bad-color"}}})).is_err());
        assert!(theme.validate_messagebox(&json!({"keys":{"toggle_tool":"nonsense"}})).is_err());
        assert!(theme.validate_messagebox(&json!({"keys":{"toggle_tool":"ctrl+o","next_tool":"ctrl+o"}})).is_err());
        assert!(theme.validate_messagebox(&json!({"keys":{"toggle_tool":false}})).is_ok());
    }

    #[test]
    fn invalid_configured_styles_are_rejected() {
        let theme = Theme::default();
        for value in [json!("unknown"), json!({"bg":"not-a-color"}), json!({"fg":3}), json!({"bold":"yes"}), json!({"extra":true}), json!([])] {
            assert!(theme.resolve_style(&value, Style::default()).is_err(), "{value}");
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            added: Style::default().fg(Color::Green),
            removed: Style::default().fg(Color::Red),
            user_prefix: Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            user_message: Style::default(),
            assistant_text: Style::default(),
            thinking: Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            tool_name: Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            tool_output: Style::default().fg(Color::DarkGray),
            error: Style::default().fg(Color::Red),
            statusline: Style::default().fg(Color::Gray).bg(Color::Rgb(30, 30, 40)),
            statusline_accent: Style::default().fg(Color::Cyan).bg(Color::Rgb(30, 30, 40)),
            editor_prompt: Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            heading: Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            code: Style::default().fg(Color::Yellow),
            code_block: Style::default().fg(Color::Green),
            dim: Style::default().fg(Color::DarkGray),
            overlay: Style::default().bg(Color::Rgb(24, 24, 34)),
            overlay_border: Style::default().fg(Color::Cyan).bg(Color::Rgb(24, 24, 34)),
        }
    }
}
