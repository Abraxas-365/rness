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
    history_images: Vec<rness_protocol::events::ImageRef>,
    history_image_index: usize,
    history_preview: bool,
    history_image_key: Option<crate::keys::Chord>,
    image_preview: bool,
    image_preview_key: Option<crate::keys::Chord>,
    image_close_key: Option<crate::keys::Chord>,
    thumbnails: std::collections::HashMap<String, image::RgbaImage>,
    image_session: Option<String>,
    selected_image: usize,
    next_image_key: Option<crate::keys::Chord>,
    images: Vec<rness_protocol::events::ImageRef>,
    clipboard_key: Option<crate::keys::Chord>,
    remove_image_key: Option<crate::keys::Chord>,
    edit_key: Option<crate::keys::Chord>,
    preview_key: Option<crate::keys::Chord>,
    preview: Option<(usize, String)>,
    preview_scroll: usize,
    preview_max: usize,
    paste_lines: usize,
    paste_chars: usize,
    external_editor: Option<Vec<String>>,
    references: Vec<(String, String)>,
    reference_query: String,
    editor: Editor,
    /// First wrapped row currently shown (scrollport offset).
    scroll_top: usize,
    commands: Vec<(String, String)>,
    candidates: Vec<(String, String)>,
    selected: usize,
    dismissed: bool,
    history: Vec<Editor>,
    history_index: Option<usize>,
    draft: Editor,
}

/// Mount the module (composition-root seam, same as future Lua installs).
pub fn install(slots: &mut Slots) {
    slots.mount(INPUT_FOOTER, 0, Box::new(Input::new()));
}

impl Input {
    pub fn new() -> Self {
        Self { history_images: Vec::new(), history_image_index: 0, history_preview: false, history_image_key: crate::keys::Chord::parse("alt+h"), image_preview: false, image_preview_key: crate::keys::Chord::parse("alt+p"), image_close_key: crate::keys::Chord::parse("esc"), thumbnails: Default::default(), selected_image: 0, next_image_key: crate::keys::Chord::parse("alt+right"), image_session: None, images: Vec::new(), clipboard_key: crate::keys::Chord::parse("ctrl+v"), remove_image_key: crate::keys::Chord::parse("alt+backspace"), edit_key: crate::keys::Chord::parse("ctrl+e"), preview_key: crate::keys::Chord::parse("ctrl+g"), preview: None, preview_scroll: 0, preview_max: 0, paste_lines: 5, paste_chars: 500, external_editor: None, references: Vec::new(), reference_query: String::new(), commands: Vec::new(), editor: Editor::new(), scroll_top: 0, candidates: vec![("unload".into(), "Unload a plugin".into())], selected: 0, dismissed: false, history: Vec::new(), history_index: None, draft: Editor::new() }
    }

    pub fn with_candidates(candidates: Vec<(String, String)>) -> Self {
        Self { candidates, ..Self::new() }
    }

    fn at_token(&self) -> Option<String> {
        if self.editor.selected_paste().is_some() { return None; }
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
        if self.editor.has_pastes() { return Vec::new(); }
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
    fn named_submit_is_not_redirected_by_configured_clipboard_key() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.clipboard_key = crate::keys::Chord::parse("enter");
        input.editor.insert_str("draft");
        let outcome = input.on_binding(&ctx, "submit");
        assert!(matches!(outcome.actions.as_slice(), [Action::Submit(text)] if text == "draft"), "named submit was redirected: {:?}", outcome.actions);
    }

    #[test]
    fn editor_modifier_compatibility_is_preserved() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        for mods in [KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT, KeyModifiers::SHIFT, KeyModifiers::ALT | KeyModifiers::CONTROL] {
            let mut input = Input::new();
            input.remove_image_key = None;
            input.editor.insert_str("ab");
            assert!(input.on_key(&ctx, KeyEvent::new(KeyCode::Left, mods)).handled);
            assert!(input.on_key(&ctx, KeyEvent::new(KeyCode::Backspace, mods)).handled);
            assert_eq!(input.editor.text(), "b");
            let result = input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, mods));
            if mods.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) {
                assert!(result.actions.is_empty());
                assert!(input.editor.text().contains('\n'));
            } else if mods == KeyModifiers::CONTROL {
                assert!(matches!(result.actions.as_slice(), [Action::Steer(text)] if text == "b"));
            } else {
                assert!(matches!(result.actions.as_slice(), [Action::Submit(text)] if text == "b"));
            }
        }
    }

    #[test]
    fn configured_binding_declarations_drive_help_and_dispatch() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.clipboard_key = crate::keys::Chord::parse("f8");
        assert!(input.binding_help().iter().any(|line| line.contains("F(8)") && line.contains("paste_clipboard")));
        assert!(matches!(input.on_key(&ctx, KeyEvent::from(KeyCode::F(8))).actions.as_slice(), [Action::PasteClipboard]));
        input.clipboard_key = None;
        assert!(!input.binding_help().iter().any(|line| line.contains("paste_clipboard")));
        assert!(!input.on_key(&ctx, KeyEvent::from(KeyCode::F(8))).handled);
    }

    #[test]
    fn history_preview_is_read_only_and_preserves_draft() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.editor.insert_str("draft");
        input.on_action(&ctx, "input:history-images", &serde_json::json!([
            {"id":"a","media_type":"image/png","width":1,"height":1,"bytes":4},
            {"id":"b","media_type":"image/png","width":1,"height":1,"bytes":4}
        ]));
        input.on_key(&ctx, KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(input.history_images.len(), 2);
        assert!(input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).actions.is_empty());
        input.on_key(&ctx, KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(input.history_image_index, 0);
        input.on_key(&ctx, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!input.image_preview);
        assert!(!input.history_preview);
        assert!(input.images.is_empty());
        assert_eq!(input.editor.text(), "draft");
    }

    #[test]
    fn unified_clipboard_paste_uses_promptbox_key_and_text_paste_behavior() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.on_action(&ctx, "input:promptbox-config", &serde_json::json!({"keys":{"paste":"ctrl+y"}}));
        assert!(matches!(input.on_key(&ctx, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)).actions.as_slice(), [Action::PasteClipboard]));
        assert!(!input.on_key(&ctx, KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)).actions.iter().any(|a| matches!(a, Action::PasteClipboard)));
        let text = "clipboard text\n".repeat(10);
        input.on_action(&ctx, "input:clipboard-text", &serde_json::json!(text));
        assert_eq!(input.editor.text(), text);
        assert!(input.editor.has_pastes());
        input.on_action(&ctx, "input:promptbox-config", &serde_json::json!({"keys":{"paste":false}}));
        assert!(input.clipboard_key.is_none());
    }

    #[test]
    fn raster_preview_uses_configured_keys_and_renders_pixels() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.on_action(&ctx, "input:promptbox-config", &serde_json::json!({"images":{"keys":{"preview":"ctrl+y","close":"ctrl+x"}}}));
        input.on_action(&ctx, "input:image-added", &serde_json::json!({"id":"a","media_type":"image/png","width":1,"height":2,"bytes":8}));
        input.on_action(&ctx, "input:image-thumbnail", &serde_json::json!({"id":"a","width":1,"height":2,"pixels":[255,0,0,255,0,0,255,255]}));
        input.on_key(&ctx, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        assert!(!input.image_preview);
        input.on_key(&ctx, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(input.image_preview);
        let area = Rect::new(0, 0, 20, 10);
        let mut buf = Buffer::empty(area);
        input.render(&ctx, area, &mut buf);
        assert_eq!(buf[(1,1)].symbol(), "▀");
        assert_eq!(buf[(1,1)].fg, ratatui::style::Color::Rgb(255,0,0));
        assert_eq!(buf[(1,1)].bg, ratatui::style::Color::Rgb(0,0,255));
        assert!(input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).actions.is_empty());
        input.on_key(&ctx, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(!input.image_preview);
        assert_eq!(input.images.len(), 1);
    }

    #[test]
    fn image_draft_survives_editor_and_clears_only_on_success() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        let reference = serde_json::json!({"id":"a".repeat(64),"media_type":"image/png","width":2,"height":2,"bytes":20});
        input.on_action(&ctx, "input:image-added", &reference);
        input.on_action(&ctx, "input:image-added", &reference);
        input.on_action(&ctx, "input:prompt-edited", &serde_json::json!({"text":"describe"}));
        assert_eq!(input.images.len(), 2);
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(&outcome.actions[..], [Action::SubmitImages(text, images)] if text == "describe" && images.len() == 2));
        let outcome = input.on_binding(&ctx, "steer");
        assert!(matches!(&outcome.actions[..], [Action::SteerImages(text, images)] if text == "describe" && images.len() == 2));
        let outcome = input.on_binding(&ctx, "queue");
        assert!(matches!(&outcome.actions[..], [Action::SubmitImages(_, images)] if images.len() == 2));
        assert_eq!(input.editor.text(), "describe");
        assert_eq!(input.images.len(), 2);
        input.on_key(&ctx, KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(input.images.len(), 1);
        input.on_action(&ctx, "input:images-submitted", &serde_json::Value::Null);
        assert!(input.images.is_empty());
        assert!(input.editor.text().is_empty());
    }

    #[test]
    fn paste_preview_keys_are_configurable_and_modifier_sensitive() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.on_action(&ctx, "input:promptbox-config", &serde_json::json!({"keys": {"edit": "ctrl+x"}, "paste": {"keys": {"preview": "alt+p"}}}));
        let first = "first\n".repeat(10);
        let second = "second\n".repeat(10);
        input.on_paste(&ctx, &first);
        input.editor.insert_str("between");
        input.on_paste(&ctx, &second);
        input.on_key(&ctx, KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(input.preview.is_none());
        input.on_key(&ctx, KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(input.preview.as_ref().unwrap().1, second);
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(matches!(&outcome.actions[..], [Action::Custom(name, payload)] if name == "terminal:edit-prompt" && payload["text"] == format!("{first}between{second}")));
        input.on_action(&ctx, "input:prompt-edited", &serde_json::json!({"text": "edited draft"}));
        assert_eq!(input.editor.text(), "edited draft");
        assert!(!input.editor.has_pastes());
        assert!(input.preview.is_none());
        input.on_action(&ctx, "input:promptbox-config", &serde_json::json!({"keys": {"edit": false}, "paste": {"keys": {"preview": false}}}));
        assert!(input.edit_key.is_none());
        assert!(input.preview_key.is_none());
    }

    #[test]
    fn paste_preview_never_submits_and_send_expands_content() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        let content = "  日本語\n".repeat(43);
        assert!(input.on_paste(&ctx, &content).actions.is_empty());
        input.on_action(&ctx, "input:paste-preview", &serde_json::Value::Null);
        for (width, height) in [(80, 18), (40, 8), (12, 4)] {
            let area = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(area);
            input.render(&ctx, area, &mut buf);
            input.on_key(&ctx, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
            input.render(&ctx, area, &mut buf);
            assert_eq!(input.preview_scroll, input.preview_max);
        }
        assert!(input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).actions.is_empty());
        input.on_key(&ctx, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(&outcome.actions[..], [Action::Submit(text)] if text == &content));
    }

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
    fn typing_and_accepting_commands_requests_dynamic_completion() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.editor.insert_str("/model");
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(matches!(outcome.actions.as_slice(), [Action::Complete(text)] if text == "/model "));
        input.on_action(&ctx, "input:completion", &serde_json::json!({"text":"/model ", "values":["chatgpt/test"]}));
        assert_eq!(input.matches()[0].0, "model chatgpt/test");
        let mut input = Input::new();
        input.on_action(&ctx, "input:commands", &serde_json::json!([["model", "Select model"]]));
        input.editor.insert_str("/mod");
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(outcome.actions.as_slice(), [Action::Complete(text)] if text == "/model "));
    }

    #[test]
    fn exact_agent_completions_submit_on_enter_without_dismissing_picker() {
        let model = crate::app::Model::new("s".into(), "m".into());
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        for text in ["/agents", "/agents ", "/agents a1", "/agents a1 "] {
            for named in [false, true] {
                let mut input = Input::new();
                input.on_action(&ctx, "input:commands", &serde_json::json!([["agents", "Monitor"]]));
                input.editor.insert_str(text);
                if text.contains(' ') {
                    input.on_action(&ctx, "input:completion", &serde_json::json!({
                        "text":text, "values":[text.strip_prefix("/agents ").unwrap(), "stop", "a1 steer"]
                    }));
                }
                assert!(!input.matches().is_empty());
                let outcome = if named {
                    let action = input.bindings().into_iter().find(|b| b.chord.code == KeyCode::Enter && b.chord.mods == KeyModifiers::NONE).unwrap().action;
                    assert_eq!(action, "submit");
                    input.on_binding(&ctx, action)
                } else { input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) };
                assert!(matches!(outcome.actions.as_slice(), [Action::Submit(sent)] if sent == text), "{text}: {:?}", outcome.actions);
                assert!(input.editor.is_empty());
                assert_eq!(input.history.last().unwrap().text(), text);
            }
        }
        let mut input = Input::new();
        input.on_action(&ctx, "input:commands", &serde_json::json!([["agents", "Monitor"]]));
        input.editor.insert_str("/agents");
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(outcome.actions.as_slice(), [Action::Complete(text)] if text == "/agents "));
        input.editor.take();
        input.editor.insert_str("/agen");
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(outcome.actions.as_slice(), [Action::Complete(text)] if text == "/agents "));
    }

    #[test]
    fn agents_stop_id_completes_as_a_multiword_argument_while_busy() {
        let mut model = crate::app::Model::new("s".into(), "m".into());
        model.busy = true;
        let theme = crate::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut input = Input::new();
        input.on_action(&ctx, "input:commands", &serde_json::json!([["agents", "Stop children"]]));
        input.editor.insert_str("/agents stop ch");
        input.on_action(&ctx, "input:completion", &serde_json::json!({
            "text":"/agents stop ch", "values":["stop", "stop child-123", "stop other-456"]
        }));
        assert_eq!(input.matches().len(), 1);
        assert_eq!(input.matches()[0].0, "agents stop child-123");
        input.on_key(&ctx, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(input.editor.text().trim(), "/agents stop child-123");
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
        let help = input.binding_help().join("\n");
        assert!(help.contains("completion_accept"));
        assert!(!help.contains("→ submit"));
        let outcome = input.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(outcome.actions.as_slice(), [Action::Complete(text)] if text == "/unload "));
        assert_eq!(input.editor.text(), "/unload ");
        input.editor.take();
        input.dismissed = false;
        input.editor.insert_str("/");
        input.on_key(&ctx, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(input.editor.text(), "/");
        assert!(input.matches().is_empty());
        assert!(input.binding_help().iter().any(|line| line.contains("→ submit")));
        assert!(!input.binding_help().iter().any(|line| line.contains("completion_accept")));
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Input {
    fn on_paste(&mut self, _ctx: &Ctx<'_>, text: &str) -> KeyOutcome {
        if self.preview.is_none() {
            self.editor.paste(text, self.paste_lines, self.paste_chars);
            self.dismissed = true;
        }
        KeyOutcome::consumed()
    }

    fn on_action(&mut self, ctx: &Ctx<'_>, name: &str, payload: &serde_json::Value) {
        if name == "input:history-images" {
            if let Ok(images) = serde_json::from_value::<Vec<rness_protocol::events::ImageRef>>(payload.clone()) {
                self.history_images = images;
                self.history_image_index = self.history_images.len().saturating_sub(1);
                self.history_preview = !self.history_images.is_empty();
                self.image_preview = self.history_preview;
                self.image_session = Some(ctx.model.session.clone());
            }
            return;
        }
        if name == "input:clipboard-text" {
            if let Some(text) = payload.as_str() { self.on_paste(ctx, text); }
            return;
        }
        if name == "input:image-thumbnail" {
            if let (Some(id), Some(width), Some(height), Ok(pixels)) = (payload["id"].as_str(), payload["width"].as_u64(), payload["height"].as_u64(), serde_json::from_value::<Vec<u8>>(payload["pixels"].clone())) {
                if width <= 160 && height <= 80 && self.images.iter().chain(self.history_images.iter()).any(|image| image.id == id) {
                    if let Some(image) = image::RgbaImage::from_raw(width as u32, height as u32, pixels) { self.thumbnails.insert(id.into(), image); }
                }
            }
            return;
        }
        if name == "input:image-added" {
            if self.image_session.as_ref() != Some(&ctx.model.session) { self.images.clear(); }
            self.image_session = Some(ctx.model.session.clone());
            if let Ok(image) = serde_json::from_value(payload.clone()) { self.images.push(image); self.selected_image = self.images.len() - 1; }
            return;
        }
        if name == "input:images-submitted" { self.images.clear(); self.thumbnails.clear(); self.image_preview = false; self.editor.take(); return; }
        if name == "input:promptbox-config" {
            for (value, target) in [(&payload["images"]["keys"]["history"], &mut self.history_image_key), (&payload["images"]["keys"]["preview"], &mut self.image_preview_key), (&payload["images"]["keys"]["close"], &mut self.image_close_key), (&payload["images"]["keys"]["next"], &mut self.next_image_key), (&payload["keys"]["paste"], &mut self.clipboard_key), (&payload["images"]["keys"]["remove"], &mut self.remove_image_key), (&payload["keys"]["edit"], &mut self.edit_key), (&payload["paste"]["keys"]["preview"], &mut self.preview_key)] {
                if value == &serde_json::Value::Bool(false) { *target = None; }
                else if let Some(key) = value.as_str() {
                    if let Some(chord) = crate::keys::Chord::parse(key) { *target = Some(chord); }
                    else { tracing::warn!(key, "invalid promptbox shortcut; retaining default"); }
                }
            }
            if let Some(n) = payload["paste"]["lines"].as_u64() { self.paste_lines = n as usize; }
            if let Some(n) = payload["paste"]["chars"].as_u64() { self.paste_chars = n as usize; }
            if let Ok(argv) = serde_json::from_value::<Vec<String>>(payload["editor"].clone()) { self.external_editor = Some(argv); }
            return;
        }
        if name == "input:paste-preview" {
            self.preview = self.editor.selected_paste().map(|(id, text)| (id, text.to_owned()));
            self.preview_scroll = 0;
            return;
        }
        if name == "input:prompt-edited" {
            if let Some(text) = payload["text"].as_str() {
                self.editor = Editor::new();
                self.editor.insert_str(text);
                self.preview = None;
                self.scroll_top = 0;
                self.history_index = None;
                self.references.clear();
                self.reference_query.clear();
                self.dismissed = true;
            }
            return;
        }
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
        if self.image_preview || self.preview.is_some() { return Some(18); }
        let rows = self.editor.wrapped_rows(Self::wrap_width(width)).len();
        // +1: blank margin row above the box, separating it from the
        // conversation body.
        Some(rows.min(MAX_ROWS) as u16 + 1 + self.picker_height())
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        if self.image_preview && self.image_session.as_ref() == Some(&ctx.model.session) {
            use ratatui::widgets::{Block, Borders};
            let block = Block::default().borders(Borders::ALL).title(" Image preview ");
            let inner = block.inner(area);
            block.render(area, buf);
            let reference = if self.history_preview { self.history_images.get(self.history_image_index) } else { self.images.get(self.selected_image) };
            if let Some(source) = reference.and_then(|r| self.thumbnails.get(&r.id)) {
                if inner.width > 0 && inner.height > 0 {
                    let image = image::DynamicImage::ImageRgba8(source.clone()).thumbnail(u32::from(inner.width).min(source.width()), (u32::from(inner.height) * 2).min(source.height())).to_rgba8();
                    for y in 0..image.height().div_ceil(2) {
                        for x in 0..image.width() {
                            let color = |p: &image::Rgba<u8>| ratatui::style::Color::Rgb(
                                ((u16::from(p[0]) * u16::from(p[3])) / 255) as u8,
                                ((u16::from(p[1]) * u16::from(p[3])) / 255) as u8,
                                ((u16::from(p[2]) * u16::from(p[3])) / 255) as u8,
                            );
                            let top = color(image.get_pixel(x, y * 2));
                            let bottom = if y * 2 + 1 < image.height() { color(image.get_pixel(x, y * 2 + 1)) } else { ratatui::style::Color::Black };
                            buf[(inner.x + x as u16, inner.y + y as u16)].set_symbol("▀").set_fg(top).set_bg(bottom);
                        }
                    }
                }
            } else { Line::raw("Preview unavailable for this attachment").render(inner, buf); }
            return;
        }
        if let Some((id, content)) = &self.preview {
            use ratatui::widgets::{Block, Borders, Paragraph};
            let block = Block::default().borders(Borders::ALL).title(format!(" Paste #{id} ")).title_bottom(" Esc: close · arrows/PageUp/PageDown: scroll ");
            let inner = block.inner(area);
            block.render(area, buf);
            let mut display = Editor::new();
            display.insert_str(&content.replace('\t', "    ").chars().filter(|c| *c == '\n' || !c.is_control()).collect::<String>());
            let rows = display.wrapped_rows(inner.width as usize);
            self.preview_max = rows.len().saturating_sub(inner.height as usize);
            self.preview_scroll = self.preview_scroll.min(self.preview_max);
            let lines: Vec<_> = rows.iter().skip(self.preview_scroll).take(inner.height as usize)
                .map(|r| Line::raw(display.display_lines()[r.line][r.start..r.end].to_owned())).collect();
            Paragraph::new(lines).render(inner, buf);
            return;
        }
        if !self.images.is_empty() && self.image_session.as_ref() == Some(&ctx.model.session) && area.height > 0 {
            let index = self.selected_image.min(self.images.len() - 1);
            let image = &self.images[index];
            Line::styled(format!("Image {}/{} · {} × {} · {} bytes", index + 1, self.images.len(), image.width, image.height, image.bytes), ctx.theme.editor_prompt).render(Rect::new(area.x, area.y, area.width, 1), buf);
        } else if self.editor.selected_paste().is_some() && area.height > 0 {
            Line::styled("Paste selected", ctx.theme.editor_prompt).render(Rect::new(area.x, area.y, area.width, 1), buf);
        }
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

    fn captures_input(&self) -> bool {
        self.preview.is_some() || self.image_preview || !self.matches().is_empty()
    }

    fn bindings(&self) -> Vec<crate::keymaps::ComponentBinding> {
        if self.preview.is_some() {
            return self.edit_key.into_iter().map(|chord| crate::keymaps::ComponentBinding { action: "external_editor", chord }).chain([("esc", "close_preview"), ("up", "preview_up"), ("k", "preview_up"),
                ("down", "preview_down"), ("j", "preview_down"), ("pageup", "preview_page_up"),
                ("pagedown", "preview_page_down"), ("home", "preview_start"), ("end", "preview_end")]
                .into_iter().map(|(key, action)| crate::keymaps::ComponentBinding {
                    action, chord: crate::keys::Chord::parse(key).expect("preview chord"),
                })).collect();
        }
        let mut bindings: Vec<_> = [
            (self.clipboard_key, "paste_clipboard"), (self.edit_key, "external_editor"),
            (self.preview_key, "paste_preview"), (self.history_image_key, "history_images"),
            (self.image_preview_key, "image_preview"), (self.image_close_key, "close_image_preview"),
            (self.next_image_key, "next_image"), (self.remove_image_key, "remove_image"),
        ].into_iter().filter_map(|(chord, action)| chord.map(|chord| crate::keymaps::ComponentBinding { action, chord })).collect();
        let matches = self.matches();
        let keys = if !matches.is_empty() {
            let selected = self.selected.min(matches.len() - 1);
            // Enter on an exact command is submission, not a no-op completion
            // loop. Tab still completes; references retain their own behavior.
            let exact = self.at_token().is_none()
                && self.editor.text().trim_end() == format!("/{}", matches[selected].0).trim_end();
            vec![("up", "completion_previous"), ("ctrl+p", "completion_previous"),
                ("down", "completion_next"), ("ctrl+n", "completion_next"),
                ("tab", "completion_accept"), ("enter", if exact { "submit" } else { "completion_accept" }), ("esc", "completion_dismiss")]
        } else { Vec::new() };
        bindings.extend(keys.into_iter().chain([
            ("enter", "submit"), ("ctrl+enter", "steer"), ("alt+enter", "newline"), ("shift+enter", "newline"),
            ("backspace", "delete_previous"), ("left", "cursor_left"), ("right", "cursor_right"),
            ("up", "cursor_up"), ("down", "cursor_down"), ("home", "cursor_home"), ("end", "cursor_end"),
        ]).map(|(key, action)| crate::keymaps::ComponentBinding { action, chord: crate::keys::Chord::parse(key).expect("editor chord") }));
        bindings
    }

    fn binding_help(&self) -> Vec<String> {
        if self.preview.is_some() {
            let mut lines: Vec<_> = self.bindings().iter().map(|binding| binding.help("Paste preview")).collect();
            lines.push("Paste preview captures all other keys; prompt and global mappings are inactive.".into());
            return lines;
        }
        if self.image_preview && self.history_preview {
            let mut lines: Vec<_> = self.bindings().iter()
                .filter(|binding| matches!(binding.action, "history_images" | "close_image_preview" | "next_image"))
                .map(|binding| binding.help("History image preview")).collect();
            lines.push("History image preview is read-only and captures all other keys.".into());
            return lines;
        }
        let mut lines = vec![
            "Prompt: text inserts; Up/Down navigate history or multiline text".into(),
            "Queue (submit): later turn; Steer: next model step. Both start a turn when idle.".into(),
        ];
        let completion = !self.matches().is_empty();
        let bindings = self.bindings();
        if completion { lines.push("Completion menu: navigation, acceptance and dismissal take precedence over prompt mappings.".into()); }
        lines.extend(bindings.iter().filter(|binding| {
            !completion || binding.action.starts_with("completion_") || !bindings.iter().any(|other| other.action.starts_with("completion_") && other.chord == binding.chord)
        }).map(|binding| binding.help("Prompt")));
        lines
    }

    fn on_key(&mut self, ctx: &Ctx<'_>, key: KeyEvent) -> KeyOutcome {
        self.handle_key(ctx, key, None)
    }

    fn on_binding(&mut self, ctx: &Ctx<'_>, action: &str) -> KeyOutcome {
        let action = if action == "queue" { "submit" } else { action };
        if action == "noop" { return KeyOutcome::consumed(); }
        let Some(binding) = self.bindings().into_iter().find(|binding| binding.action == action) else { return KeyOutcome::pass(); };
        self.handle_key(ctx, KeyEvent::new(binding.chord.code, binding.chord.mods), Some(action))
    }
}

impl Input {
    fn handle_key(&mut self, ctx: &Ctx<'_>, key: KeyEvent, action: Option<&str>) -> KeyOutcome {
        let bindings = self.bindings();
        let matched = |name: &str| action.map_or_else(|| bindings.iter().any(|binding| binding.matches(name, &key)), |action| action == name);
        if self.image_session.as_ref().is_some_and(|session| session != &ctx.model.session) { self.images.clear(); self.thumbnails.clear(); self.history_images.clear(); self.history_preview = false; self.image_preview = false; self.image_session = None; }
        if matched("history_images") { return KeyOutcome::act(vec![Action::PreviewHistoryImage]); }
        if self.image_preview && matched("close_image_preview") { self.image_preview = false; self.history_preview = false; return KeyOutcome::consumed(); }
        if self.image_preview && self.history_preview {
            if matched("next_image") && !self.history_images.is_empty() { self.history_image_index = (self.history_image_index + 1) % self.history_images.len(); }
            return KeyOutcome::consumed();
        }
        if matched("image_preview") && !self.images.is_empty() { self.image_preview = !self.image_preview; return KeyOutcome::consumed(); }
        if matched("paste_clipboard") { return KeyOutcome::act(vec![Action::PasteClipboard]); }
        if matched("next_image") && !self.images.is_empty() {
            self.selected_image = (self.selected_image + 1) % self.images.len();
            return KeyOutcome::consumed();
        }
        if matched("remove_image") {
            if !self.images.is_empty() {
                let removed = self.images.remove(self.selected_image.min(self.images.len() - 1));
                if !self.images.iter().any(|r| r.id == removed.id) { self.thumbnails.remove(&removed.id); }
                if self.images.is_empty() { self.image_preview = false; }
                self.selected_image = self.selected_image.min(self.images.len().saturating_sub(1));
            }
            return KeyOutcome::consumed();
        }
        if self.image_preview { return KeyOutcome::consumed(); }
        if !self.images.is_empty() && self.preview.is_none() && (matched("submit") || matched("steer")) {
            return KeyOutcome::act(vec![if matched("steer") {
                Action::SteerImages(self.editor.text(), self.images.clone())
            } else { Action::SubmitImages(self.editor.text(), self.images.clone()) }]);
        }
        if matched("external_editor") {
            return KeyOutcome::act(vec![Action::Custom("terminal:edit-prompt".into(), serde_json::json!({"text": self.editor.text(), "editor": self.external_editor}))]);
        }
        if matched("paste_preview") {
            self.preview = self.editor.selected_paste().map(|(id, text)| (id, text.to_owned()));
            self.preview_scroll = 0;
            return KeyOutcome::consumed();
        }
        if self.preview.is_some() {
            let action = action.or_else(|| bindings.iter().find(|binding| binding.chord.matches(&key)).map(|binding| binding.action));
            match action {
                Some("close_preview") => self.preview = None,
                Some("preview_up") => self.preview_scroll = self.preview_scroll.saturating_sub(1),
                Some("preview_down") => self.preview_scroll = (self.preview_scroll + 1).min(self.preview_max),
                Some("preview_page_up") => self.preview_scroll = self.preview_scroll.saturating_sub(10),
                Some("preview_page_down") => self.preview_scroll = self.preview_scroll.saturating_add(10).min(self.preview_max),
                Some("preview_start") => self.preview_scroll = 0,
                Some("preview_end") => self.preview_scroll = self.preview_max,
                _ => {}
            }
            return KeyOutcome::consumed();
        }
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
            match action.or_else(|| bindings.iter().find(|binding| binding.action.starts_with("completion_") && binding.matches_key(&key)).map(|binding| binding.action)) {
                Some("completion_previous") => {
                    self.selected = (selected + count - 1) % count;
                    return KeyOutcome::consumed();
                }
                Some("completion_next") => {
                    self.selected = (selected + 1) % count;
                    return KeyOutcome::consumed();
                }
                Some("completion_accept") => {
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
                    self.dismissed = false;
                    return KeyOutcome::act(vec![Action::Complete(replacement)]);
                }
                Some("completion_dismiss") => {
                    self.dismissed = true;
                    return KeyOutcome::consumed();
                }
                _ => {}
            }
        }
        if key.modifiers == KeyModifiers::NONE && (matched("cursor_up") || matched("cursor_down"))
            && (self.editor.line_count() == 1 || self.history_index.is_some())
        {
            if matched("cursor_up") && !self.history.is_empty() {
                let index = match self.history_index {
                    Some(index) => index.saturating_sub(1),
                    None => { self.draft = self.editor.clone(); self.history.len() - 1 }
                };
                self.history_index = Some(index);
                self.editor.take();
                self.editor = self.history[index].clone();
            } else if matched("cursor_down") {
                if let Some(index) = self.history_index {
                    self.editor.take();
                    if index + 1 < self.history.len() {
                        self.history_index = Some(index + 1);
                        self.editor = self.history[index + 1].clone();
                    } else {
                        self.history_index = None;
                        self.editor = self.draft.clone();
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
            _ if matched("newline") =>
            {
                self.editor.insert_newline();
                KeyOutcome::consumed()
            }
            _ if matched("submit") || matched("steer") => {
                if self.editor.is_empty() {
                    return KeyOutcome::consumed();
                }
                // Queue waits for a later turn; Steer reaches the next model step.
                self.scroll_top = 0;
                let snapshot = self.editor.clone();
                let text = self.editor.take();
                if !text.trim().is_empty() && self.history.last().map(Editor::text).as_ref() != Some(&text) {
                    self.history.push(snapshot);
                }
                self.history_index = None;
                self.draft = Editor::new();
                self.dismissed = false;
                KeyOutcome::act(vec![if matched("steer") { Action::Steer(text) } else { Action::Submit(text) }])
            }
            (KeyCode::Char(c), m)
                if action.is_none() && !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
            {
                self.editor.insert_char(c);
                KeyOutcome::consumed()
            }
            _ if matched("delete_previous") => {
                self.editor.backspace();
                KeyOutcome::consumed()
            }
            _ if matched("cursor_left") => {
                self.editor.move_left();
                KeyOutcome::consumed()
            }
            _ if matched("cursor_right") => {
                self.editor.move_right();
                KeyOutcome::consumed()
            }
            _ if matched("cursor_up") && self.editor.line_count() > 1 =>
            {
                self.editor.move_up();
                KeyOutcome::consumed()
            }
            _ if matched("cursor_down") && self.editor.line_count() > 1 =>
            {
                self.editor.move_down();
                KeyOutcome::consumed()
            }
            _ if matched("cursor_home") => {
                self.editor.move_home();
                KeyOutcome::consumed()
            }
            _ if matched("cursor_end") => {
                self.editor.move_end();
                KeyOutcome::consumed()
            }
            _ => KeyOutcome::pass(),
        };
        if self.at_token().is_some() && matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End) {
            return KeyOutcome::act(vec![Action::Complete(self.editor.before_cursor())]);
        }
        let current = self.editor.text();
        if current != text && current.starts_with('/') && current.contains(' ')
            && !current.contains('\n') && !self.editor.has_pastes()
        {
            return KeyOutcome::act(vec![Action::Complete(current)]);
        }
        outcome
    }
}
