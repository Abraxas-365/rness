//! Presentation-only question settings. Defaults preserve the stock overlay.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuestionUiConfig {
    pub width: Option<u16>,
    pub padding: Padding,
    pub option_spacing: u16,
    pub border: String,
    pub descriptions: String,
    pub show_help: bool,
    pub styles: BTreeMap<String, serde_json::Value>,
    pub symbols: Symbols,
    pub labels: Labels,
    pub keys: BTreeMap<String, String>,
}
impl Default for QuestionUiConfig {
    fn default() -> Self {
        Self {
            width: None,
            padding: Padding::default(),
            option_spacing: 0,
            border: "plain".into(),
            descriptions: "auto".into(),
            show_help: true,
            styles: BTreeMap::new(),
            symbols: Symbols::default(),
            labels: Labels::default(),
            keys: default_keys(),
        }
    }
}
pub fn default_keys() -> BTreeMap<String, String> {
    [
        ("up", "up"),
        ("down", "down"),
        ("select", "space"),
        ("submit", "enter"),
        ("next", "tab"),
        ("previous", "shift+tab"),
        ("dismiss", "esc"),
        ("cancel", "ctrl+c"),
        ("details", "pagedown"),
        ("page_up", "pageup"),
        ("first", "home"),
        ("last", "end"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}
impl QuestionUiConfig {
    /// Fill omitted actions rather than replacing the whole keymap.
    pub fn normalize(&mut self) {
        for (key, value) in default_keys() {
            self.keys.entry(key).or_insert(value);
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.width.is_some_and(|w| w < 30) {
            return Err("questions.ui.width must be >= 30".into());
        }
        if [
            self.padding.left,
            self.padding.right,
            self.padding.top,
            self.padding.bottom,
        ]
        .iter()
        .any(|p| *p > 10)
            || self.option_spacing > 5
        {
            return Err("questions.ui padding must be <= 10 and option_spacing <= 5".into());
        }
        if !["plain", "rounded", "double", "thick", "none"].contains(&self.border.as_str()) {
            return Err("invalid questions.ui.border".into());
        }
        if !["auto", "always", "never"].contains(&self.descriptions.as_str()) {
            return Err("invalid questions.ui.descriptions".into());
        }
        for (name, value) in &self.styles {
            if ![
                "panel",
                "border",
                "title",
                "question",
                "option",
                "selected",
                "description",
                "input",
                "help",
                "error",
            ]
            .contains(&name.as_str())
            {
                return Err(format!("unknown questions.ui.styles field: {name}"));
            }
            validate_style(value).map_err(|e| format!("questions.ui.styles.{name}: {e}"))?;
        }
        for text in [
            &self.symbols.cursor,
            &self.symbols.selected,
            &self.symbols.unselected,
            &self.symbols.custom,
        ] {
            if text.chars().count() > 8 || text.chars().any(char::is_control) {
                return Err("question symbols must be single-line and at most 8 characters".into());
            }
        }
        for text in [
            &self.labels.other,
            &self.labels.feedback,
            &self.labels.custom,
            &self.labels.editing,
        ] {
            if text.trim().is_empty()
                || text.chars().count() > 120
                || text.chars().any(char::is_control)
            {
                return Err(
                    "question labels must be nonempty single-line text of at most 120 characters"
                        .into(),
                );
            }
        }
        let defaults = default_keys();
        let mut seen = std::collections::HashSet::new();
        for (action, key) in &self.keys {
            if !defaults.contains_key(action) {
                return Err(format!("unknown question key action: {action}"));
            }
            let canonical = rness_kernel::presentation::canonical_chord(key)
                .map_err(|e| format!("questions.ui.keys.{action}: {e}"))?;
            if canonical.split('+').any(|part| part == "ctrl")
                && canonical.ends_with("+c")
                && action != "cancel"
            {
                return Err("Ctrl+C is reserved for cancellation".into());
            }
            if !seen.insert(canonical) {
                return Err("duplicate question key bindings".into());
            }
        }
        Ok(())
    }
}
// Validate presentation data without depending on a terminal renderer. Keep the
// supported style fields aligned with Theme::resolve_style in rness-tui.
fn validate_style(value: &serde_json::Value) -> Result<(), String> {
    if let Some(name) = value.as_str() {
        return if [
            "added",
            "removed",
            "user_prefix",
            "user_message",
            "assistant_text",
            "thinking",
            "title",
            "tool_name",
            "tool_output",
            "error",
            "statusline",
            "statusline_accent",
            "editor_prompt",
            "heading",
            "code",
            "code_block",
            "dim",
            "overlay",
            "overlay_border",
        ]
        .contains(&name)
        {
            Ok(())
        } else {
            Err(format!("unknown theme group: {name}"))
        };
    }
    let fields = value
        .as_object()
        .ok_or("style must be a theme group or object")?;
    for (field, value) in fields {
        match field.as_str() {
            "fg" | "bg" => {
                let text = value.as_str().ok_or("color must be a string")?;
                let normalized = text
                    .to_lowercase()
                    .replace([' ', '-', '_'], "")
                    .replace("bright", "light")
                    .replace("grey", "gray")
                    .replace("silver", "gray")
                    .replace("lightblack", "darkgray")
                    .replace("lightwhite", "white")
                    .replace("lightgray", "white");
                let named = text == "default"
                    || [
                        "reset",
                        "black",
                        "red",
                        "green",
                        "yellow",
                        "blue",
                        "magenta",
                        "cyan",
                        "gray",
                        "darkgray",
                        "lightred",
                        "lightgreen",
                        "lightyellow",
                        "lightblue",
                        "lightmagenta",
                        "lightcyan",
                        "white",
                    ]
                    .contains(&normalized.as_str());
                let hex = text.len() == 7
                    && text.starts_with('#')
                    && text[1..].bytes().all(|b| b.is_ascii_hexdigit());
                if !named && !hex && text.parse::<u8>().is_err() {
                    return Err(format!("invalid color: {text}"));
                }
            }
            "bold" | "italic" | "underline" | "reverse" if value.is_boolean() => {}
            _ => return Err(format!("invalid style field or value: {field}")),
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Padding {
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Symbols {
    pub cursor: String,
    pub selected: String,
    pub unselected: String,
    pub custom: String,
}
impl Default for Symbols {
    fn default() -> Self {
        Self {
            cursor: "> ".into(),
            selected: "[x]".into(),
            unselected: "[ ]".into(),
            custom: "[+]".into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Labels {
    pub other: String,
    pub feedback: String,
    pub custom: String,
    pub editing: String,
}
impl Default for Labels {
    fn default() -> Self {
        Self {
            other: "Other / write an answer".into(),
            feedback: "Request changes / feedback".into(),
            custom: "Custom: ".into(),
            editing: "Editing > ".into(),
        }
    }
}
