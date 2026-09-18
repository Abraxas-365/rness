//! Language-independent, asynchronous text presentation extension.

pub fn core_action_scope(name: &str) -> Option<&'static str> {
    match name {
        "core.scroll_up"
        | "core.scroll_down"
        | "core.scroll_up_page"
        | "core.scroll_down_page"
        | "core.cancel_or_quit"
        | "core.quit" => Some("global"),
        "core.promptbox.submit"
        | "core.promptbox.queue"
        | "core.promptbox.steer"
        | "core.promptbox.newline"
        | "core.promptbox.delete_previous"
        | "core.promptbox.cursor_left"
        | "core.promptbox.cursor_right"
        | "core.promptbox.cursor_up"
        | "core.promptbox.cursor_down"
        | "core.promptbox.cursor_home"
        | "core.promptbox.cursor_end"
        | "core.promptbox.paste_clipboard"
        | "core.promptbox.external_editor"
        | "core.promptbox.noop" => Some("promptbox"),
        "core.messagebox.previous_thinking"
        | "core.messagebox.next_thinking"
        | "core.messagebox.toggle_thinking"
        | "core.messagebox.previous_tool"
        | "core.messagebox.next_tool"
        | "core.messagebox.toggle_tool"
        | "core.messagebox.noop" => Some("messagebox"),
        "core.promptbox.completion_previous"
        | "core.promptbox.completion_next"
        | "core.promptbox.completion_accept"
        | "core.promptbox.completion_dismiss"
        | "core.promptbox.close_preview"
        | "core.promptbox.preview_up"
        | "core.promptbox.preview_down"
        | "core.promptbox.preview_page_up"
        | "core.promptbox.preview_page_down"
        | "core.promptbox.preview_start"
        | "core.promptbox.preview_end" => Some("promptbox"),
        "core.app.close" | "core.app.noop" => Some("app"),
        _ => None,
    }
}

pub fn core_action_matches_scope(name: &str, scope: &str) -> bool {
    match core_action_scope(name) {
        Some("app") => scope
            .strip_prefix("app:")
            .is_some_and(|name| !name.is_empty()),
        Some(expected) => expected == scope,
        None => false,
    }
}

/// Normalize the single-chord language shared by configuration and frontends.
pub fn canonical_chord(input: &str) -> Result<String, String> {
    let input = match (input.strip_prefix('<'), input.strip_suffix('>')) {
        (Some(_), Some(_)) => input[1..input.len() - 1]
            .replace("C-", "ctrl+")
            .replace("A-", "alt+")
            .replace("S-", "shift+"),
        (None, None) => input.to_owned(),
        _ => return Err(format!("invalid key chord: {input}")),
    }
    .to_ascii_lowercase();
    let mut modifiers = [false; 3];
    let mut code = None;
    for part in input.split('+') {
        let modifier = match part {
            "ctrl" => Some(0),
            "alt" => Some(1),
            "shift" => Some(2),
            _ => None,
        };
        if let Some(index) = modifier {
            if modifiers[index] {
                return Err("duplicate key modifier".into());
            }
            modifiers[index] = true;
        } else {
            if code.is_some() {
                return Err("multi-key sequences are not supported".into());
            }
            let valid = matches!(
                part,
                "enter"
                    | "esc"
                    | "tab"
                    | "space"
                    | "up"
                    | "down"
                    | "left"
                    | "right"
                    | "pageup"
                    | "pagedown"
                    | "home"
                    | "end"
                    | "backspace"
                    | "delete"
            ) || (part.len() == 1
                && part.as_bytes()[0].is_ascii_graphic()
                && !matches!(part, "<" | ">"))
                || part
                    .strip_prefix('f')
                    .and_then(|n| n.parse::<u8>().ok())
                    .is_some_and(|n| (1..=24).contains(&n));
            if !valid {
                return Err(format!("invalid key chord: {input}"));
            }
            code = Some(part);
        }
    }
    let code = code.ok_or_else(|| "key chord needs a key".to_owned())?;
    let mut result = String::new();
    for (enabled, modifier) in modifiers.into_iter().zip(["ctrl+", "alt+", "shift+"]) {
        if enabled {
            result.push_str(modifier);
        }
    }
    if let Some(number) = code.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        result.push_str(&format!("f{number}"));
    } else {
        result.push_str(code);
    }
    Ok(result)
}

/// A presentation callback evaluated outside the frontend render loop.
/// `None` declines to render so the frontend can use its fallback.
#[async_trait::async_trait]
pub trait TextProvider: Send + Sync {
    async fn text(&self) -> Option<String>;

    async fn status(&self, _context: serde_json::Value) -> Option<serde_json::Value> {
        self.text().await.map(serde_json::Value::String)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledLine {
    pub text: String,
    pub style: String,
    pub spans: Vec<StyledSpan>,
    pub right: Vec<StyledSpan>,
    pub block: Option<serde_json::Value>,
    pub is_header: bool,
    pub structured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    pub text: String,
    pub style: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct AppSpec {
    pub key_help: Vec<String>,
    pub name: String,
    pub slot: String,
    pub title: String,
    pub keymap: Option<String>,
    /// Optional visible-only refresh interval; absent means event-driven.
    pub refresh_ms: Option<u64>,
    /// Let on_key handle Escape before the host's close fallback.
    pub capture_escape: bool,
    /// Host layout and theme styles; content/navigation remain provider-owned.
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppKeyOutcome {
    Pass,
    Consumed,
    Close,
    Action {
        name: String,
        payload: serde_json::Value,
    },
}

/// Collection of named applications. The host owns mounting and focus.
#[async_trait::async_trait]
pub trait Applications: Send + Sync {
    async fn app_specs(&self) -> Vec<AppSpec>;
    async fn app_view(&self, name: &str, ctx: serde_json::Value) -> Result<Vec<String>, String>;
    async fn app_key(
        &self,
        name: &str,
        key: &str,
        ctx: serde_json::Value,
    ) -> Result<AppKeyOutcome, String>;
}

/// Nonblocking event submission. Implementations must enqueue slow work.
pub trait HookSink: Send + Sync {
    fn fire_hook(&self, event: &str, payload: serde_json::Value);
}
