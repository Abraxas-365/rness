//! Input module: the editor mounted in input_footer. Enter submits,
//! Alt+Enter / Shift+Enter inserts a newline.
//!
//! Rendering follows the dsh composer contract, translated to cells:
//! the draft soft-wraps to the available width, the box grows with
//! content up to a cap, and past the cap it becomes a scrollport that
//! moves the MINIMUM needed to keep the caret visible (dsh InputBar:
//! `scrollTop += rect.bottom - box.bottom`). Typing never pushes the
//! caret out of view.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::app::Action;
use crate::component::{Component, Ctx, KeyOutcome};
use crate::core::editor::Editor;
use crate::slots::{Slots, INPUT_FOOTER};

/// Max rows the input claims before it starts scrolling (the dsh
/// composer caps height the same way; the conversation keeps the rest).
const MAX_ROWS: usize = 8;
/// Cells taken by the "❯ " / "  " prefix.
const PREFIX_CELLS: u16 = 2;

pub struct Input {
    references: Vec<(String, String)>,
    reference_query: String,
    editor: Editor,
    /// First wrapped row currently shown (scrollport offset).
    scroll_top: usize,
    commands: Vec<(String, String)>,
    candidates: Vec<(String, String)>,
    selected: usize,
    dismissed: bool,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
}

/// Mount the module (composition-root seam, same as future Lua installs).
pub fn install(slots: &mut Slots) {
    slots.mount(INPUT_FOOTER, 0, Box::new(Input::new()));
}

impl Input {
    pub fn new() -> Self {
        Self { references: Vec::new(), reference_query: String::new(), commands: Vec::new(), editor: Editor::new(), scroll_top: 0, candidates: vec![("unload".into(), "Unload a plugin".into())], selected: 0, dismissed: false, history: Vec::new(), history_index: None, draft: String::new() }
    }

    pub fn with_candidates(candidates: Vec<(String, String)>) -> Self {
        Self { candidates, ..Self::new() }
    }

    fn at_token(&self) -> Option<String> {
        let prefix = self.editor.before_cursor();
        if prefix.starts_with('/') { return None; }
        let start = prefix.rfind('@')?;
        if start > 0 && !prefix[..start].ends_with(char::is_whitespace) { return None; }
        let token = &prefix[start..];
        let query = &token[1..];
        if let Some(quoted) = query.strip_prefix('"') {
            if quoted.contains(['"', '\n']) { return None; }
        } else if query.contains(char::is_whitespace) { return None; }
        Some(token.to_owned())
    }

    fn matches(&self) -> Vec<&(String, String)> {
        if !self.dismissed && self.at_token().as_deref() == Some(self.reference_query.as_str()) { return self.references.iter().collect(); }
        let text = self.editor.text();
        if self.dismissed || !text.starts_with('/') || text.contains('\n') { return Vec::new(); }
        let query = text[1..].to_lowercase();
        self.commands.iter().chain(self.candidates.iter().filter(|(name, _)| !self.commands.iter().any(|(command, _)| command == name))).filter(|(name, _)| {
            if query.contains(' ') { name.contains(' ') && name.to_lowercase().starts_with(&query) }
            else { !name.contains(' ') && name.to_lowercase().starts_with(&query) }
        }).collect()
    }

    fn picker_height(&self) -> u16 {
        let count = self.matches().len();
        if count == 0 { 0 } else { count.min(6) as u16 + 2 }
    }

    fn wrap_width(width: u16) -> usize {
        width.saturating_sub(PREFIX_CELLS).max(1) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_picker_quotes_paths_preserves_suffix_and_drills() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.editor.insert_str("Read @日 later");
        for _ in 0..6 { input.editor.move_left(); }
        input.on_action(&ctx, "input:completion", &serde_json::json!({"text":"Read @old", "values":["wrong"]}));
        assert!(input.matches().is_empty());
        input.on_action(&ctx, "input:completion", &serde_json::json!({"text":"Read @日", "values":["日本語/a b.txt"]}));
        let result = input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(result.actions.is_empty());
        assert_eq!(input.editor.text(), "Read @\"日本語/a b.txt\"  later");
        input.editor.take();
        input.editor.insert_str("@");
        input.on_action(&ctx, "input:completion", &serde_json::json!({"text":"@", "values":["docs/"]}));
        let result = input.on_key(&ctx, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(result.actions.as_slice(), [Action::Complete(text)] if text == "@docs/"));
        input.editor.take(); input.editor.insert_str("mail@host");
        assert!(input.at_token().is_none());
    }

    #[test]
    fn dynamic_completion_ignores_stale_queries() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.editor.insert_str("/project pa");
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL));
        assert!(matches!(outcome.actions.as_slice(), [Action::Complete(text)] if text == "/project pa"));
        input.on_action(&ctx, "input:completion", &serde_json::json!({"text":"/project old", "values":["wrong"]}));
        assert!(input.matches().is_empty());
        input.on_action(&ctx, "input:completion", &serde_json::json!({"text":"/project pa", "values":["path"]}));
        assert_eq!(input.matches()[0].0, "project path");
    }

    #[test]
    fn command_catalog_shadows_skills_and_removes_unloaded_commands() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::with_candidates(vec![("hello".into(), "Skill".into())]);
        input.editor.insert_str("/he");
        input.on_action(&ctx, "input:commands", &serde_json::json!([["hello", "Command"]]));
        assert_eq!(input.matches().len(), 1);
        assert_eq!(input.matches()[0].1, "Command");
        input.on_action(&ctx, "input:commands", &serde_json::json!([]));
        assert_eq!(input.matches()[0].1, "Skill");
    }

    #[test]
    fn plugin_catalog_removes_stale_candidates() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.editor.insert_str("/unload ");
        input.on_action(&ctx, "input:plugins", &serde_json::json!(["one", "two"]));
        assert_eq!(input.matches().len(), 2);
        input.on_action(&ctx, "input:plugins", &serde_json::json!(["two"]));
        assert_eq!(input.matches()[0].0, "unload two");
        input.on_action(&ctx, "input:plugins", &serde_json::json!([]));
        assert!(input.matches().is_empty());
    }

    #[test]
    fn history_walk_restores_draft_and_does_not_submit() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        for text in ["primero", "segundo", "segundo"] {
            input.editor.insert_str(text);
            input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        assert_eq!(input.history.len(), 2);
        input.editor.insert_str("borrador ñ");
        for (key, expected) in [(KeyCode::Up, "segundo"), (KeyCode::Up, "primero"), (KeyCode::Up, "primero"), (KeyCode::Down, "segundo"), (KeyCode::Down, "borrador ñ"), (KeyCode::Down, "borrador ñ")] {
            assert!(input.on_key(&ctx, KeyEvent::new(key, KeyModifiers::NONE)).actions.is_empty());
            assert_eq!(input.editor.text(), expected);
        }
        input.editor.take();
        input.editor.insert_str("/");
        input.dismissed = false;
        input.on_key(&ctx, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(input.editor.text(), "/");
        assert_eq!(input.history_index, None);
    }

    #[test]
    fn picker_accepts_without_submitting_and_escape_preserves_draft() {
        let model = crate::app::Model::new("session".into(), "model".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.editor.insert_str("/un");
        assert_eq!(input.matches().len(), 1);
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(outcome.actions.is_empty());
        assert_eq!(input.editor.text(), "/unload ");
        input.editor.take();
        input.dismissed = false;
        input.editor.insert_str("/");
        input.on_key(&ctx, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(input.editor.text(), "/");
        assert!(input.matches().is_empty());
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Input {
    fn on_action(&mut self, _ctx: &Ctx<'_>, name: &str, payload: &serde_json::Value) {
        if name == "input:references" {
            self.references.clear(); self.reference_query.clear(); self.dismissed = true;
            return;
        }
        if name == "input:completion" {
            if let Some(token) = self.at_token() {
                if payload["text"].as_str() == Some(self.editor.before_cursor().as_str()) {
                    if let Ok(values) = serde_json::from_value::<Vec<String>>(payload["values"].clone()) {
                        self.reference_query = token;
                        self.references = values.into_iter().map(|p| (p, String::new())).collect();
                    }
                }
                return;
            }
            if payload["text"].as_str() != Some(self.editor.text().as_str()) { return; }
            if let (Some(text), Ok(values)) = (payload["text"].as_str(), serde_json::from_value::<Vec<String>>(payload["values"].clone())) {
                let command = text.trim_start_matches('/').split_whitespace().next().unwrap_or_default();
                let prefix = format!("{command} ");
                self.commands.retain(|(name, _)| !name.starts_with(&prefix));
                self.commands.extend(values.into_iter().map(|value| (format!("{prefix}{value}"), String::new())));
                self.dismissed = false;
                self.selected = 0;
            }
            return;
        }
        if name == "input:commands" {
            if let Ok(commands) = serde_json::from_value(payload.clone()) {
                self.commands = commands;
                self.selected = 0;
            }
            return;
        }
        if name == "input:skills" {
            if let Ok(skills) = serde_json::from_value::<Vec<(String, String)>>(payload.clone()) {
                let old_names: Vec<_> = self.candidates.iter().filter_map(|(name, _)| name.strip_prefix("skill ").map(str::to_owned)).collect();
                self.candidates.retain(|(name, _)| !name.starts_with("skill ") && (!old_names.contains(name) || ["agent", "skill", "unload", "colorscheme"].contains(&name.as_str())));
                for (name, description) in skills {
                    self.candidates.push((format!("skill {name}"), description.clone()));
                    if !["agent", "skill", "unload", "colorscheme"].contains(&name.as_str()) {
                        self.candidates.push((name, format!("Skill: {description}")));
                    }
                }
                self.selected = 0;
            }
            return;
        }
        if name != "input:plugins" { return; }
        if let Ok(names) = serde_json::from_value::<Vec<String>>(payload.clone()) {
            self.candidates.retain(|(name, _)| !name.starts_with("unload "));
            self.candidates.extend(names.into_iter().map(|name| (format!("unload {name}"), "Loaded plugin".into())));
            self.selected = 0;
        }
    }
    fn name(&self) -> &str {
        "input"
    }

    fn height(&self, _ctx: &Ctx<'_>, width: u16) -> Option<u16> {
        let rows = self.editor.wrapped_rows(Self::wrap_width(width)).len();
        // +1: blank margin row above the box, separating it from the
        // conversation body.
        Some(rows.min(MAX_ROWS) as u16 + 1 + self.picker_height())
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let picker_height = self.picker_height().min(area.height.saturating_sub(2));
        if picker_height > 0 {
            use ratatui::widgets::{Block, Borders, Paragraph};
            let popup = Rect::new(area.x, area.y, area.width, picker_height);
            let block = Block::default().borders(Borders::ALL);
            let inner = block.inner(popup);
            block.render(popup, buf);
            let matches = self.matches();
            let selected = self.selected.min(matches.len().saturating_sub(1));
            let start = selected.saturating_sub(inner.height.saturating_sub(1) as usize);
            let lines = matches.iter().enumerate().skip(start).take(inner.height as usize).map(|(index, (name, description))| {
                let style = if index == selected { ctx.theme.editor_prompt.add_modifier(ratatui::style::Modifier::REVERSED) } else { ratatui::style::Style::default() };
                Line::styled(format!("{name:<16} {description}"), style)
            }).collect::<Vec<_>>();
            Paragraph::new(lines).render(inner, buf);
        }
        // First row is the blank margin; the box renders below it.
        let area = Rect::new(
            area.x,
            area.y + picker_height + 1,
            area.width,
            area.height.saturating_sub(picker_height + 1),
        );
        let wrap = Self::wrap_width(area.width);
        let rows = self.editor.wrapped_rows(wrap);
        let (cursor_row, cursor_cells) = self.editor.cursor_wrapped(wrap);
        let visible = area.height as usize;

        // Minimum scroll that keeps the caret row inside the viewport.
        if cursor_row < self.scroll_top {
            self.scroll_top = cursor_row;
        } else if cursor_row >= self.scroll_top + visible {
            self.scroll_top = cursor_row + 1 - visible;
        }
        // Content shrank (backspace, submit): don't leave a blank port.
        self.scroll_top = self.scroll_top.min(rows.len().saturating_sub(visible));

        let lines = self.editor.display_lines();
        for (i, row) in rows.iter().skip(self.scroll_top).take(visible).enumerate() {
            let abs = self.scroll_top + i;
            // "❯ " on the very first row of the draft, continuation
            // otherwise ("… " when rows above are scrolled out).
            let prefix = if abs == 0 {
                "❯ "
            } else if i == 0 {
                "… "
            } else {
                "  "
            };
            let text = &lines[row.line][row.start..row.end];
            let rect = Rect::new(area.x, area.y + i as u16, area.width, 1);
            Line::from(vec![
                Span::styled(prefix.to_string(), ctx.theme.editor_prompt),
                Span::raw(text.to_string()),
            ])
            .render(rect, buf);
            // Visible cursor: reverse-video the cell.
            if abs == cursor_row {
                let x = area.x + PREFIX_CELLS + cursor_cells as u16;
                if x < area.right() {
                    buf[(x, rect.y)].set_style(
                        ratatui::style::Style::default()
                            .add_modifier(ratatui::style::Modifier::REVERSED),
                    );
                }
            }
        }
    }

    fn on_key(&mut self, _ctx: &Ctx<'_>, key: KeyEvent) -> KeyOutcome {
        let text = self.editor.text();
        if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::CONTROL
            && text.starts_with('/') && text.contains(' ')
        {
            return KeyOutcome::act(vec![Action::Complete(text)]);
        }
        let matches = self.matches();
        if !matches.is_empty() {
            let count = matches.len();
            let selected = self.selected.min(count - 1);
            match (key.code, key.modifiers) {
                (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                    self.selected = (selected + count - 1) % count;
                    return KeyOutcome::consumed();
                }
                (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                    self.selected = (selected + 1) % count;
                    return KeyOutcome::consumed();
                }
                (KeyCode::Tab | KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(token) = self.at_token() {
                        let path = matches[selected].0.clone();
                        let drill = key.code == KeyCode::Tab && path.ends_with('/');
                        let quoted = path.contains(char::is_whitespace);
                        let replacement = if quoted { format!("@\"{path}{}", if drill { "" } else { "\" " }) } else { format!("@{path}{}", if drill { "" } else { " " }) };
                        self.editor.replace_before_cursor(token.len(), &replacement);
                        self.references.clear();
                        self.selected = 0;
                        return if drill { KeyOutcome::act(vec![Action::Complete(self.editor.before_cursor())]) } else { KeyOutcome::consumed() };
                    }
                    let replacement = format!("/{} ", matches[selected].0);
                    self.editor.take();
                    self.editor.insert_str(&replacement);
                    self.dismissed = !self.candidates.iter().any(|(name, _)| name.starts_with(replacement.trim_start_matches('/')));
                    return KeyOutcome::consumed();
                }
                (KeyCode::Esc, _) => {
                    self.dismissed = true;
                    return KeyOutcome::consumed();
                }
                _ => {}
            }
        }
        if key.modifiers == KeyModifiers::NONE && matches!(key.code, KeyCode::Up | KeyCode::Down)
            && (self.editor.line_count() == 1 || self.history_index.is_some())
        {
            if key.code == KeyCode::Up && !self.history.is_empty() {
                let index = match self.history_index {
                    Some(index) => index.saturating_sub(1),
                    None => { self.draft = self.editor.text(); self.history.len() - 1 }
                };
                self.history_index = Some(index);
                self.editor.take();
                self.editor.insert_str(&self.history[index]);
            } else if key.code == KeyCode::Down {
                if let Some(index) = self.history_index {
                    self.editor.take();
                    if index + 1 < self.history.len() {
                        self.history_index = Some(index + 1);
                        self.editor.insert_str(&self.history[index + 1]);
                    } else {
                        self.history_index = None;
                        self.editor.insert_str(&self.draft);
                    }
                }
            }
            self.dismissed = true;
            self.scroll_top = 0;
            return KeyOutcome::consumed();
        }
        if matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace) {
            self.dismissed = false;
            self.selected = 0;
        }
        let outcome = match (key.code, key.modifiers) {
            (KeyCode::Enter, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::SHIFT) =>
            {
                self.editor.insert_newline();
                KeyOutcome::consumed()
            }
            (KeyCode::Enter, _) => {
                if self.editor.is_empty() {
                    return KeyOutcome::consumed();
                }
                // Submitting while busy queues a followup (inbox semantics).
                self.scroll_top = 0;
                let text = self.editor.take();
                if !text.trim().is_empty() && self.history.last() != Some(&text) {
                    self.history.push(text.clone());
                }
                self.history_index = None;
                self.draft.clear();
                self.dismissed = false;
                KeyOutcome::act(vec![Action::Submit(text)])
            }
            (KeyCode::Char(c), m)
                if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
            {
                self.editor.insert_char(c);
                KeyOutcome::consumed()
            }
            (KeyCode::Backspace, _) => {
                self.editor.backspace();
                KeyOutcome::consumed()
            }
            (KeyCode::Left, _) => {
                self.editor.move_left();
                KeyOutcome::consumed()
            }
            (KeyCode::Right, _) => {
                self.editor.move_right();
                KeyOutcome::consumed()
            }
            (KeyCode::Up, m)
                if !m.contains(KeyModifiers::SHIFT) && self.editor.line_count() > 1 =>
            {
                self.editor.move_up();
                KeyOutcome::consumed()
            }
            (KeyCode::Down, m)
                if !m.contains(KeyModifiers::SHIFT) && self.editor.line_count() > 1 =>
            {
                self.editor.move_down();
                KeyOutcome::consumed()
            }
            (KeyCode::Home, _) => {
                self.editor.move_home();
                KeyOutcome::consumed()
            }
            (KeyCode::End, _) => {
                self.editor.move_end();
                KeyOutcome::consumed()
            }
            _ => KeyOutcome::pass(),
        };
        if self.at_token().is_some() && matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End) {
            return KeyOutcome::act(vec![Action::Complete(self.editor.before_cursor())]);
        }
        outcome
    }
}
