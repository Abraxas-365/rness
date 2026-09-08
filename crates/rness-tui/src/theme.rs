//! Colors and styles, centralized so modules stay consistent and a
//! future config layer can swap palettes.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone)]
pub struct Theme {
    pub added: Style,
    pub removed: Style,
    pub user_prefix: Style,
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
    /// Resolve a Lua card STYLE NAME. Unknown names get the default
    /// tool-output style — a typo'd plugin degrades, never breaks.
    pub fn card_style(&self, name: &str) -> Style {
        match name {
            "added" => self.added,
            "removed" => self.removed,
            "title" => self.tool_name,
            "dim" => self.dim,
            "error" => self.error,
            "heading" => self.heading,
            "code" => self.code,
            _ => self.tool_output,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            added: Style::default().fg(Color::Green),
            removed: Style::default().fg(Color::Red),
            user_prefix: Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
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
