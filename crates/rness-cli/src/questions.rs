use std::sync::Arc;
use rness_engine::questions::{Questions, Answers, Answer, Request, QuestionUiConfig};
use rness_tui::keys::Chord;
use ratatui::style::Style;
use ratatui::widgets::BorderType;
use rness_tui::component::{Component, Ctx, KeyOutcome};
use crossterm::event::{KeyEvent, KeyCode, KeyModifiers};
use ratatui::{buffer::Buffer, layout::{Rect, Layout, Constraint}, widgets::{Block, Borders, Paragraph, Widget, Clear, Wrap, List, ListItem, ListState, StatefulWidget}, text::Line};

#[cfg(test)]
mod tests {
    use super::*;
    use rness_engine::tools::{ToolRegistry, ToolCall};
    #[tokio::test]
    async fn session_drafts_survive_switch_and_frontend_drop_resolves_all() {
        let questions = Arc::new(Questions::default());
        let registry = Arc::new(ToolRegistry::default());
        questions.bind_registry(&registry);
        questions.set_available(true);
        let mut events = questions.subscribe();
        let mut tasks = Vec::new();
        for session in ["a", "b"] {
            let registry = registry.clone();
            tasks.push(tokio::spawn(async move {
                registry.dispatch(&session.into(), &[ToolCall { call: "c".into(), name: "AskUser".into(), args: serde_json::json!({"questions":[{"id":"q","question":"Notes?"}]}) }], 1, &tokio_util::sync::CancellationToken::new()).await
            }));
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await.unwrap().unwrap();
        }
        let mut overlay = QuestionOverlay::new(questions.clone());
        let theme = rness_tui::theme::Theme::default();
        let mut model = rness_tui::app::Model::new("a".into(), "m".into());
        for (session, letter) in [("a", 'A'), ("b", 'B')] {
            model.session = session.into();
            let ctx = Ctx { model: &model, theme: &theme };
            overlay.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            overlay.on_key(&ctx, KeyEvent::new(KeyCode::Char(letter), KeyModifiers::NONE));
        }
        model.session = "a".into();
        let ctx = Ctx { model: &model, theme: &theme };
        let area = Rect::new(0, 0, 40, 12);
        overlay.render(&ctx, area, &mut Buffer::empty(area));
        assert_eq!(overlay.answers[0].custom.as_deref(), Some("A"));
        assert!(overlay.editing);
        drop(overlay);
        assert!(!questions.is_available());
        assert!(registry.get("AskUser").is_none());
        assert!(questions.pending().is_empty());
        for task in tasks {
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap();
            assert!(result[0].is_error);
            assert!(result[0].output.contains("disconnected"));
        }
    }

    #[tokio::test]
    async fn markdown_review_scroll_resize_and_explicit_choices() {
        let questions = Arc::new(Questions::default());
        questions.set_available(true);
        let registry = ToolRegistry::default();
        registry.register(Arc::new(rness_engine::questions::AskUser(questions.clone())));
        let markdown = format!("# Review title\n\n## Changes\n\n{}\n```rust\nlet ready = true;\n```\n\nEND-OF-PLAN", "- **Important** 日本語 café details\n".repeat(40));
        let task = tokio::spawn(async move {
            registry.dispatch(&"s".into(), &[ToolCall { call: "review".into(), name: "AskUser".into(), args: serde_json::json!({"questions":[{"id":"review","question":"Approve this plan?","markdown":markdown,"options":[{"label":"Approve"},{"label":"Keep planning"}]}]}) }], 1, &tokio_util::sync::CancellationToken::new()).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async { while questions.pending().is_empty() { tokio::task::yield_now().await; } }).await.unwrap();
        let mut overlay = QuestionOverlay::new(questions.clone());
        let model = rness_tui::app::Model::new("s".into(), "m".into());
        let theme = rness_tui::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        for (width, height) in [(80, 24), (40, 12), (120, 35)] {
            let area = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(area);
            overlay.render(&ctx, area, &mut buf);
            overlay.on_key(&ctx, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
            overlay.render(&ctx, area, &mut buf);
            let snapshot = |buf: &Buffer| (0..height).map(|y| (0..width).map(|x| buf[(x,y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
            assert!(snapshot(&buf).contains("Review title"));
            assert!(!snapshot(&buf).contains("# Review title"));
            overlay.on_key(&ctx, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
            assert_eq!(overlay.scroll, (1 + overlay.page_rows).min(overlay.max_scroll + 1));
            overlay.on_key(&ctx, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
            overlay.render(&ctx, area, &mut buf);
            assert!(snapshot(&buf).contains("END-OF-PLAN"));
        }
        overlay.on_key(&ctx, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!questions.pending().is_empty(), "reading must not approve");
        overlay.on_key(&ctx, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(overlay.scroll > 0);
        overlay.on_key(&ctx, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(questions.pending().is_empty());
        assert!(task.await.unwrap()[0].is_error);
    }

    #[tokio::test]
    async fn visual_selection_custom_text_and_render() {
        let questions = Arc::new(Questions::default()); questions.set_available(true);
        let registry = ToolRegistry::default(); registry.register(Arc::new(rness_engine::questions::AskUser(questions.clone())));
        let task = tokio::spawn(async move { registry.dispatch(&"s".into(), &[ToolCall { call: "c".into(), name: "AskUser".into(), args: serde_json::json!({"questions":[{"id":"db","question":"Choose a database","options":[{"label":"PostgreSQL","description":"Shared production database"},{"label":"SQLite","description":"Local embedded database"}]},{"id":"reason","question":"Anything else?"}]}) }], 1, &tokio_util::sync::CancellationToken::new()).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async { while questions.pending().is_empty() { tokio::task::yield_now().await; } }).await.unwrap();
        let mut overlay = QuestionOverlay::new(questions);
        let model = rness_tui::app::Model::new("s".into(), "m".into()); let theme = rness_tui::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        for (width, height) in [(80,20), (40,12), (120,30)] {
            let area = Rect::new(0,0,width,height); let mut buf = Buffer::empty(area); overlay.render(&ctx, area, &mut buf);
            let snapshot = (0..height).map(|y| (0..width).map(|x| buf[(x,y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
            assert!(snapshot.contains("Choose a database")); assert!(snapshot.contains("PostgreSQL"));
            if height == 12 { assert!(snapshot.contains("SQLite")); assert!(snapshot.contains("Other / write an answer")); }
            if std::env::var_os("RNESS_QUESTION_SNAPSHOT").is_some() { println!("{snapshot}\n"); }
        }
        let area = Rect::new(0, 0, 20, 8);
        let mut buf = Buffer::empty(area);
        overlay.render(&ctx, area, &mut buf);
        let tiny = (0..8).map(|y| (0..20).map(|x| buf[(x,y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(tiny.contains("Resize to 30x10"));
        overlay.editing = true;
        overlay.answers[0].custom = Some(format!("{}日本語 END", "long text ".repeat(40)));
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        overlay.render(&ctx, area, &mut buf);
        let view = (0..12).map(|y| (0..40).map(|x| buf[(x,y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(view.contains("END"));
        overlay.editing = false;
        overlay.answers[0].custom = None;
        overlay.on_key(&ctx, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        overlay.render(&ctx, area, &mut buf);
        let detail = (0..12).map(|y| (0..40).map(|x| buf[(x,y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(detail.contains("Shared production database"));
        overlay.on_key(&ctx, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(overlay.wants(&ctx));
        let apps = rness_tui::modules::ext_apps::AppsState::default();
        apps.set_apps(vec![rness_tui::modules::ext_apps::AppInfo { name: "sessions".into(), slot: "overlay".into(), title: "Sessions".into(), keymap: Some("ctrl+s".into()) }]);
        overlay.apps = Some(apps.clone());
        rness_tui::modules::ext_apps::handle_global_key(&apps, &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(!overlay.wants(&ctx));
        rness_tui::modules::ext_apps::handle_global_key(&apps, &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(overlay.wants(&ctx));
        if std::env::var_os("RNESS_QUESTION_INTERACTIVE").is_some() {
            crossterm::terminal::enable_raw_mode().unwrap();
            let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout())).unwrap();
            terminal.clear().unwrap();
            while overlay.wants(&ctx) {
                terminal.draw(|frame| { let area = frame.area(); overlay.render(&ctx, area, frame.buffer_mut()); }).unwrap();
                if let crossterm::event::Event::Key(key) = crossterm::event::read().unwrap() { overlay.on_key(&ctx, key); }
            }
            crossterm::terminal::disable_raw_mode().unwrap();
            terminal.show_cursor().unwrap();
        } else {
            for code in [KeyCode::Down, KeyCode::Enter, KeyCode::Enter, KeyCode::Char('o'), KeyCode::Char('k'), KeyCode::Enter, KeyCode::Enter] { overlay.on_key(&ctx, KeyEvent::new(code, KeyModifiers::NONE)); }
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap();
        assert!(!result[0].is_error); assert!(result[0].output.contains("SQLite")); assert!(result[0].output.contains("ok"));
    }
}

// Keep rendering and input on the same binding vocabulary. Crossterm reports
// Shift-Tab as BackTab, whereas the shared chord parser uses Shift + Tab.
fn chord_matches(binding: &str, mut key: KeyEvent) -> bool {
    if key.code == KeyCode::BackTab {
        key.code = KeyCode::Tab;
        key.modifiers |= KeyModifiers::SHIFT;
    }
    Chord::parse(binding).is_some_and(|chord| chord.matches(&key))
}

fn binding_matches(ui: &QuestionUiConfig, action: &str, key: KeyEvent) -> bool {
    ui.keys.get(action).is_some_and(|binding| chord_matches(binding, key))
}

fn markdown_alias(ui: &QuestionUiConfig, action: &str, key: KeyEvent) -> bool {
    let alias = if action == "down" { "j" } else { "k" };
    ui.keys.get(action).is_some_and(|binding| binding == action)
        && !ui.keys.values().any(|binding| Chord::parse(binding) == Chord::parse(alias))
        && chord_matches(alias, key)
}

fn editor_key<'a>(ui: &'a QuestionUiConfig, action: &str) -> &'a str {
    let fallback = if action == "submit" { "enter" } else { "esc" };
    ui.keys.get(action).filter(|key| !Chord::parse(key).is_some_and(|chord|
        chord.code == KeyCode::Backspace || (matches!(chord.code, KeyCode::Char(_)) && !chord.mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT))))
        .map(String::as_str).unwrap_or(fallback)
}

fn question_style(ctx: &Ctx<'_>, ui: &QuestionUiConfig, name: &str, mut fallback: Style) -> Style {
    // A panel background is inherited by children unless their own override sets it.
    if name != "panel" {
        if let Some(value) = ui.styles.get("panel") {
            if let Ok(panel) = ctx.theme.resolve_style(value, ctx.theme.overlay) { fallback.bg = panel.bg; }
        }
    }
    ui.styles.get(name).and_then(|value| ctx.theme.resolve_style(value, fallback).ok()).unwrap_or(fallback)
}

fn panel_area(area: Rect, ui: &QuestionUiConfig) -> Rect {
    let width = ui.width.unwrap_or(area.width).min(area.width);
    Rect::new(area.x.saturating_add((area.width - width) / 2), area.y, width, area.height)
}

fn padded_area(area: Rect, ui: &QuestionUiConfig) -> Rect {
    let left = ui.padding.left.min(area.width);
    let top = ui.padding.top.min(area.height);
    Rect::new(area.x.saturating_add(left), area.y.saturating_add(top),
        area.width.saturating_sub(left).saturating_sub(ui.padding.right),
        area.height.saturating_sub(top).saturating_sub(ui.padding.bottom))
}

fn question_help(ui: &QuestionUiConfig, editing: bool, detail: bool, markdown: bool, width: u16) -> String {
    if !ui.show_help { return String::new(); }
    let hint = |action: &str, label: &str| -> String {
        ui.keys.get(action).filter(|key| !editing || !Chord::parse(key).is_some_and(|chord|
            chord.code == KeyCode::Backspace || (matches!(chord.code, KeyCode::Char(_)) && !chord.mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT))))
            .map(|key| format!("{key}:{label}")).unwrap_or_default()
    };
    let join = |hints: Vec<String>| hints.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    let cancel = if ui.keys.get("cancel").is_some_and(|s| s == "ctrl+c") { "ctrl+c:cancel".into() }
        else { join(vec![hint("cancel", "cancel"), "ctrl+c:cancel".into()]) };
    if editing {
        return format!("{}\n{}", join(vec![format!("{}:done", editor_key(ui, "submit")), format!("{}:back", editor_key(ui, "dismiss"))]), cancel);
    }
    if markdown {
        return format!("{}\n{}", join(vec![hint("up", "up"), hint("down", "down"), hint("page_up", "page"), hint("details", "page"), hint("first", "first"), hint("last", "last")]),
            join(vec![hint("dismiss", "dismiss"), hint("submit", "choices"), hint("next", "choices"), cancel]));
    }
    if detail {
        return format!("{}\n{}", join(vec![hint("dismiss", "back"), cancel]), join(vec![hint("page_up", "scroll"), hint("details", "scroll")]));
    }
    if width < 40 {
        format!("{}\n{}", join(vec![hint("select", "pick"), hint("submit", "send"), hint("next", "next")]), join(vec![hint("dismiss", "close"), hint("details", "details")]))
    } else {
        format!("{}\n{}", join(vec![hint("up", "up"), hint("down", "down"), hint("select", "pick"), hint("submit", "send")]),
            join(vec![hint("dismiss", "close"), hint("next", "next"), hint("previous", "prev"), hint("details", "details"), cancel]))
    }
}

pub struct QuestionOverlay {
    pub questions: Arc<Questions>,
    key: Option<(String, String)>,
    page: usize,
    cursor: usize,
    answers: Vec<Answer>,
    editing: bool,
    error: String,
    scroll: u16,
    page_rows: u16,
    max_scroll: u16,
    drafts: std::collections::HashMap<(String, String), (usize, usize, Vec<Answer>, bool)>,
    pub apps: Option<rness_tui::modules::ext_apps::AppsState>,
}
impl QuestionOverlay {
    pub fn new(questions: Arc<Questions>) -> Self { Self { questions, key: None, page: 0, cursor: 0, answers: vec![], editing: false, error: String::new(), scroll: 0, page_rows: 1, max_scroll: 0, drafts: Default::default(), apps: None } }
    fn sync(&mut self, request: &Request) {
        let key = (request.session.clone(), request.call.clone());
        if self.key.as_ref() != Some(&key) {
            if let Some(old) = self.key.take() {
                self.drafts.insert(old, (self.page, self.cursor, std::mem::take(&mut self.answers), self.editing));
            }
            let pending = self.questions.pending();
            self.drafts.retain(|key, _| pending.iter().any(|r| r.session == key.0 && r.call == key.1));
            let saved = self.drafts.remove(&key);
            self.key = Some(key); self.page = 0; self.cursor = 0; self.editing = false; self.error.clear(); self.scroll = if request.questions[0].markdown.is_some() { 1 } else { 0 };
            self.answers = request.questions.iter().map(|q| Answer { id: q.id.clone(), selected: vec![], custom: None }).collect();
            if let Some((page, cursor, answers, editing)) = saved {
                self.page = page; self.cursor = cursor; self.answers = answers; self.editing = editing;
            }
        }
    }
}
impl Drop for QuestionOverlay {
    fn drop(&mut self) { self.questions.set_available(false); }
}
impl Component for QuestionOverlay {
    fn name(&self) -> &str { "user-questions" }
    fn priority(&self) -> Option<i32> { Some(self.questions.overlay_config().priority) }
    fn wants(&self, ctx: &Ctx<'_>) -> bool {
        !self.apps.as_ref().is_some_and(|apps| apps.active_overlay()) && self.questions.pending().iter().any(|p| p.session == ctx.model.session)
    }
    fn height(&self, _: &Ctx<'_>, _: u16) -> Option<u16> { Some(self.questions.overlay_config().height) }
    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let Some(request) = self.questions.pending().into_iter().find(|p| p.session == ctx.model.session) else { return; };
        self.sync(&request);
        let q = &request.questions[self.page];
        let answer = &self.answers[self.page];
        let config = self.questions.overlay_config();
        let ui = &config.ui;
        let area = panel_area(area, ui);
        let style = |name, fallback| question_style(ctx, ui, name, fallback);
        Clear.render(area, buf);
        // Paint only the overlay region, including padding and empty list rows.
        Block::default().style(style("panel", ctx.theme.overlay)).render(area, buf);
        if area.width < 30 || area.height < 10 {
            let help = if ui.show_help { format!("\n{}", question_help(ui, false, true, false, area.width)) } else { String::new() };
            Paragraph::new(format!("Resize to 30x10{help}")).style(style("error", ctx.theme.error)).wrap(Wrap { trim: false }).render(area, buf);
            return;
        }
        let border = match ui.border.as_str() {
            "rounded" => BorderType::Rounded, "double" => BorderType::Double,
            "thick" => BorderType::Thick, _ => BorderType::Plain,
        };
        let block = Block::default().borders(if ui.border == "none" { Borders::NONE } else { Borders::ALL })
            .border_type(border)
            .title(Line::styled(format!(" {}  {}/{}  {} ", config.title, self.page + 1, request.questions.len(), q.header), style("title", ctx.theme.overlay)))
            .style(style("panel", ctx.theme.overlay)).border_style(style("border", ctx.theme.overlay_border));
        let inner = padded_area(block.inner(area), ui);
        block.render(area, buf);
        if inner.width < 10 || inner.height < 6 {
            let notice = if ui.show_help { format!("Resize or reduce padding\n{}", question_help(ui, self.editing, false, false, area.width)) } else { "Resize or reduce padding".into() };
            Paragraph::new(notice).style(style("error", ctx.theme.error)).wrap(Wrap { trim: false }).render(area, buf);
            return;
        }
        if self.scroll > 0 && q.markdown.is_some() {
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(if ui.show_help { 2 } else { 0 })]).split(inner);
            let lines = rness_tui::core::render::render_markdown(q.markdown.as_deref().unwrap(), chunks[0].width, ctx.theme);
            self.page_rows = chunks[0].height.max(1);
            self.max_scroll = lines.len().saturating_sub(chunks[0].height as usize).min((u16::MAX - 1) as usize) as u16;
            self.scroll = self.scroll.min(self.max_scroll + 1);
            Paragraph::new(lines).scroll((self.scroll - 1, 0)).style(style("question", ctx.theme.overlay)).render(chunks[0], buf);
            Paragraph::new(question_help(ui, false, false, true, inner.width)).style(style("help", ctx.theme.dim)).render(chunks[1], buf);
            return;
        }
        let compact = inner.height < 14;
        let footer_rows = if ui.show_help || !self.error.is_empty() { 2 } else { 0 };
        let chunks = Layout::vertical([Constraint::Length(if compact { 2 } else { 3 }), Constraint::Min(1), Constraint::Length(if compact { 1 } else { 2 }), Constraint::Length(footer_rows)]).split(inner);
        Paragraph::new(q.question.as_str()).style(style("question", ctx.theme.heading)).wrap(Wrap { trim: false }).render(chunks[0], buf);
        let detail = self.scroll > 0;
        if detail {
            let text = if let Some(option) = q.options.get(self.cursor) {
                if ui.descriptions == "never" { format!("{}\n{}", q.question, option.label) }
                else { format!("{}\n{}\n{}", q.question, option.label, option.description) }
            } else { q.question.clone() };
            let lines: Vec<Line> = text.lines().flat_map(|line| textwrap::wrap(line, chunks[1].width.max(1) as usize).into_iter().map(|s| Line::raw(s.into_owned())).collect::<Vec<_>>()).collect();
            let offset = (self.scroll - 1).min(lines.len().saturating_sub(chunks[1].height as usize).min(u16::MAX as usize) as u16);
            Paragraph::new(lines).scroll((offset, 0)).style(style("description", ctx.theme.dim)).render(chunks[1], buf);
        } else {
            let cursor_width = Line::raw(ui.symbols.cursor.as_str()).width();
            let mut items: Vec<_> = q.options.iter().map(|o| {
                let mark = if answer.selected.contains(&o.label) { &ui.symbols.selected } else { &ui.symbols.unselected };
                let width = (chunks[1].width as usize).saturating_sub(cursor_width).max(1);
                let mut lines: Vec<Line> = textwrap::wrap(&format!("{mark} {}", o.label), width).into_iter().map(|s| Line::raw(s.into_owned())).collect();
                if (ui.descriptions == "always" || (ui.descriptions != "never" && !compact)) && !o.description.is_empty() {
                    lines.extend(textwrap::wrap(&o.description, width).into_iter().map(|s| Line::styled(s.into_owned(), style("description", ctx.theme.dim))));
                }
                let available = chunks[1].height as usize;
                if lines.len() > available && available > 0 {
                    lines.truncate(available);
                    let hint = if ui.show_help { ui.keys.get("details").map(|key| format!("… {key}: full details")).unwrap_or_else(|| "…".into()) } else { "…".into() };
                    lines[available - 1] = Line::styled(hint, style("help", ctx.theme.dim));
                }
                // Spacing belongs to an item, not a selectable blank option.
                lines.extend((0..ui.option_spacing.min(chunks[1].height.saturating_sub(lines.len() as u16))).map(|_| Line::raw("")));
                ListItem::new(lines)
            }).collect();
            items.push(ListItem::new(format!("{} {}", ui.symbols.custom, if q.markdown.is_some() { &ui.labels.feedback } else { &ui.labels.other })));
            let mut state = ListState::default().with_selected(Some(self.cursor));
            StatefulWidget::render(List::new(items).style(style("option", ctx.theme.overlay)).highlight_symbol(ui.symbols.cursor.as_str()).highlight_style(style("selected", ctx.theme.statusline_accent)), chunks[1], buf, &mut state);
        }
        let text = format!("{}{}", if self.editing { &ui.labels.editing } else { &ui.labels.custom }, answer.custom.as_deref().unwrap_or(""));
        let lines: Vec<Line> = textwrap::wrap(&text, chunks[2].width.max(1) as usize).into_iter().map(|s| Line::raw(s.into_owned())).collect();
        let offset = if self.editing { lines.len().saturating_sub(chunks[2].height as usize).min(u16::MAX as usize) as u16 } else { 0 };
        Paragraph::new(lines).scroll((offset, 0)).style(style("input", ctx.theme.editor_prompt)).render(chunks[2], buf);
        let footer = if self.error.is_empty() { question_help(ui, self.editing, detail, false, inner.width) } else { self.error.clone() };
        Paragraph::new(footer).style(if self.error.is_empty() { style("help", ctx.theme.dim) } else { style("error", ctx.theme.error) }).render(chunks[3], buf);
    }
    fn on_key(&mut self, ctx: &Ctx<'_>, key: KeyEvent) -> KeyOutcome {
        let Some(request) = self.questions.pending().into_iter().find(|p| p.session == ctx.model.session) else { return KeyOutcome::pass(); };
        self.sync(&request);
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) { return KeyOutcome::act(vec![rness_tui::app::Action::Cancel]); }
        let config = self.questions.overlay_config();
        let matches = |action| binding_matches(&config.ui, action, key);
        self.error.clear();
        // Printable bindings must never eat a custom answer, even when remapped
        // to navigation, submit, dismiss, or cancel. Ctrl+C above stays a safety exit.
        if self.editing && matches!(key.code, KeyCode::Char(_)) && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            if let KeyCode::Char(c) = key.code { self.answers[self.page].custom.get_or_insert_default().push(c); }
            return KeyOutcome::consumed();
        }
        // Backspace remains text editing even if configured as a panel action.
        if self.editing && key.code == KeyCode::Backspace {
            self.answers[self.page].custom.get_or_insert_default().pop();
            return KeyOutcome::consumed();
        }
        if matches("cancel") { return KeyOutcome::act(vec![rness_tui::app::Action::Cancel]); }
        if self.editing {
            if ["dismiss", "submit"].iter().any(|action| chord_matches(editor_key(&config.ui, action), key)) { self.editing = false; }
            return KeyOutcome::consumed();
        }
        if self.scroll > 0 && request.questions[self.page].markdown.is_some() {
            if matches("down") || markdown_alias(&config.ui, "down", key) { self.scroll = self.scroll.saturating_add(1).min(self.max_scroll + 1); }
            else if matches("up") || markdown_alias(&config.ui, "up", key) { self.scroll = self.scroll.saturating_sub(1).max(1); }
            else if matches("details") { self.scroll = self.scroll.saturating_add(self.page_rows).min(self.max_scroll + 1); }
            else if matches("page_up") { self.scroll = self.scroll.saturating_sub(self.page_rows).max(1); }
            else if matches("first") { self.scroll = 1; }
            else if matches("last") { self.scroll = self.max_scroll + 1; }
            else if matches("submit") || matches("next") { self.scroll = 0; }
            else if matches("dismiss") { self.questions.dismiss(&request.session, &request.call); }
            return KeyOutcome::consumed();
        }
        if matches("details") { self.scroll = self.scroll.saturating_add(1); return KeyOutcome::consumed(); }
        if matches("page_up") { self.scroll = self.scroll.saturating_sub(1); return KeyOutcome::consumed(); }
        if matches("dismiss") && self.scroll > 0 { self.scroll = 0; return KeyOutcome::consumed(); }
        self.scroll = 0;
        let q = &request.questions[self.page];
        if matches("dismiss") { self.questions.dismiss(&request.session, &request.call); }
        else if matches("up") { self.cursor = self.cursor.saturating_sub(1); }
        else if matches("down") { self.cursor = (self.cursor + 1).min(q.options.len()); }
        else if matches("next") { self.page = (self.page + 1) % request.questions.len(); self.cursor = 0; }
        else if matches("previous") { self.page = (self.page + request.questions.len() - 1) % request.questions.len(); self.cursor = 0; }
        else if matches("select") && self.cursor == q.options.len()
            || matches("submit") && self.cursor == q.options.len() && self.answers[self.page].custom.as_ref().map_or(true, |s| s.trim().is_empty()) {
            self.editing = true;
        }
        else if matches("select") {
            let label = &q.options[self.cursor].label;
            let answer = &mut self.answers[self.page];
            if answer.selected.contains(label) { answer.selected.retain(|s| s != label); }
            else { if !q.multi_select { answer.selected.clear(); } answer.selected.push(label.clone()); }
        }
        else if matches("submit") {
            let answer = &mut self.answers[self.page];
            if answer.selected.is_empty() && answer.custom.as_ref().map_or(true, |s| s.trim().is_empty()) && !q.options.is_empty() { answer.selected.push(q.options[self.cursor].label.clone()); }
            if self.page + 1 < request.questions.len() { self.page += 1; self.cursor = 0; }
            else if let Err(error) = self.questions.resolve(&request.session, &request.call, Answers { answers: self.answers.clone() }) { self.error = error; }
        }
        KeyOutcome::consumed()
    }
}

#[cfg(test)]
mod configurable_tests {
    use super::*;
    use rness_engine::tools::{ToolRegistry, ToolCall};
    use ratatui::style::Color;

    fn snapshot(buf: &Buffer) -> String {
        let area = buf.area;
        (area.y..area.bottom()).map(|y| (area.x..area.right()).map(|x| buf[(x,y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }
    async fn fixture() -> QuestionOverlay {
        let questions = Arc::new(Questions::default());
        questions.set_available(true);
        let registry = ToolRegistry::default();
        registry.register(Arc::new(rness_engine::questions::AskUser(questions.clone())));
        tokio::spawn(async move {
            registry.dispatch(&"s".into(), &[ToolCall { call: "c".into(), name: "AskUser".into(), args: serde_json::json!({"questions":[
                {"id":"one","question":"Choose a database","options":[{"label":"Alpha","description":"Alpha description"},{"label":"Beta","description":"Beta description"}]},
                {"id":"two","question":"Notes?"}
            ]}) }], 1, &tokio_util::sync::CancellationToken::new()).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async { while questions.pending().is_empty() { tokio::task::yield_now().await; } }).await.unwrap();
        QuestionOverlay::new(questions)
    }
    #[test]
    fn help_uses_active_bindings_and_editor_fallback() {
        let mut ui = QuestionUiConfig::default();
        for (action, key) in [("submit","f2"),("dismiss","f3"),("details","f4"),("previous","ctrl+p"),("cancel","f5")] { ui.keys.insert(action.into(), key.into()); }
        let help = question_help(&ui, false, false, false, 120);
        for hint in ["f2:send","f3:close","f4:details","ctrl+p:prev","f5:cancel","ctrl+c:cancel"] { assert!(help.contains(hint), "{help}"); }
        for stale in ["enter", "esc", "pagedown"] { assert!(!help.contains(stale)); }
        let help = question_help(&ui, false, false, true, 120);
        for hint in ["f2:choices", "home:first", "end:last"] { assert!(help.contains(hint)); }
        ui.keys.insert("submit".into(), "x".into());
        ui.keys.insert("dismiss".into(), "q".into());
        let help = question_help(&ui, true, false, false, 120);
        assert!(help.contains("enter:done") && help.contains("esc:back") && !help.contains("x:done"));
        ui.show_help = false;
        for (editing, detail, markdown) in [(false,false,false),(true,false,false),(false,true,false),(false,false,true)] { assert!(question_help(&ui, editing, detail, markdown, 120).is_empty()); }
    }
    #[tokio::test]
    async fn configured_layout_styles_symbols_and_descriptions() {
        let mut overlay = fixture().await;
        let model = rness_tui::app::Model::new("s".into(), "m".into());
        let theme = rness_tui::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut config = overlay.questions.overlay_config();
        config.ui.width = Some(50);
        config.ui.padding.left = 2; config.ui.padding.top = 1;
        config.ui.border = "double".into(); config.ui.option_spacing = 1;
        config.ui.descriptions = "always".into(); config.ui.show_help = false;
        config.ui.symbols.cursor = "→ ".into(); config.ui.symbols.unselected = "( )".into();
        config.ui.symbols.selected = "(*)".into(); config.ui.symbols.custom = "++".into();
        config.ui.labels.other = "Write here".into(); config.ui.labels.custom = "Response: ".into();
        for (name,color) in [("panel","blue"),("border","red"),("title","green"),("question","yellow"),("option","cyan"),("selected","magenta"),("description","white"),("input","red")] {
            config.ui.styles.insert(name.into(), serde_json::json!({"fg":color}));
        }
        overlay.questions.set_overlay_config(config.clone());
        let area = Rect::new(3,2,90,24);
        let mut buf = Buffer::empty(area);
        for cell in &mut buf.content { cell.set_symbol("Z"); }
        overlay.render(&ctx, area, &mut buf);
        let panel = panel_area(area, &config.ui);
        for y in area.y..area.bottom() { for x in area.x..area.right() {
            if x < panel.x || x >= panel.right() { assert_eq!(buf[(x,y)].symbol(), "Z"); }
        } }
        assert_eq!(buf[(panel.x,panel.y)].symbol(), "╔");
        assert_eq!(buf[(panel.x,panel.y)].fg, Color::Red);
        assert_eq!(buf[(panel.x+2,panel.y)].fg, Color::Green);
        assert_eq!(buf[(panel.x+1,panel.y+1)].fg, Color::Blue);
        let view = snapshot(&buf);
        assert!(view.contains("→ ( ) Alpha") && view.contains("++ Write here") && view.contains("Response: "), "{view}");
        assert!(view.contains("Alpha description") && !view.contains("send"));
        let row = |needle: &str| (area.y..area.bottom()).find(|y| (area.x..area.right()).map(|x| buf[(x,*y)].symbol()).collect::<String>().contains(needle)).unwrap();
        let alpha = row("Alpha"); let beta = row("Beta");
        assert_eq!(beta-alpha, 3);
        assert_eq!(buf[(panel.x+3,panel.y+2)].fg, Color::Yellow);
        assert_eq!(buf[(panel.x+5,alpha)].fg, Color::Magenta);
        assert_eq!(buf[(panel.x+5,beta)].fg, Color::Cyan);
        assert_eq!(buf[(panel.x+5,beta+1)].fg, Color::White);
        assert_eq!(buf[(panel.x+3,row("Response:"))].fg, Color::Red);
        overlay.on_key(&ctx, KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        overlay.render(&ctx, area, &mut buf);
        assert!(snapshot(&buf).contains("(*) Alpha"));
        config.ui.descriptions = "never".into();
        overlay.questions.set_overlay_config(config.clone());
        overlay.render(&ctx, area, &mut buf);
        assert!(!snapshot(&buf).contains("description"));
        for (border,corner) in [("plain","┌"),("rounded","╭"),("thick","┏"),("none"," ")] {
            config.ui.border = border.into(); overlay.questions.set_overlay_config(config.clone());
            overlay.render(&ctx, area, &mut buf);
            assert_eq!(buf[(panel.x,panel.y)].symbol(), corner);
        }
        config.ui.styles.insert("question".into(), serde_json::json!("missing"));
        assert_eq!(question_style(&ctx, &config.ui, "question", theme.heading), theme.overlay.patch(theme.heading));
        config.ui.styles.insert("question".into(), serde_json::json!("added"));
        assert_eq!(question_style(&ctx, &config.ui, "question", theme.heading), theme.overlay.patch(theme.heading).patch(theme.added));
        let changed = rness_tui::theme::Theme { added: Style::default().fg(Color::Red), ..theme.clone() };
        assert_eq!(question_style(&Ctx { model: &model, theme: &changed }, &config.ui, "question", changed.heading).fg, Some(Color::Red));
        config.ui.styles.insert("panel".into(), serde_json::json!({"bg":"green"}));
        assert_eq!(question_style(&ctx, &config.ui, "option", theme.overlay).bg, Some(Color::Green));
    }
    #[tokio::test]
    async fn tiny_layouts_clamp_padding_and_hidden_help_keeps_errors() {
        let mut overlay = fixture().await;
        let model = rness_tui::app::Model::new("s".into(), "m".into());
        let theme = rness_tui::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut config = overlay.questions.overlay_config();
        config.ui.width = Some(120); config.ui.show_help = false;
        config.ui.padding.left = u16::MAX; config.ui.padding.top = u16::MAX;
        config.ui.padding.right = u16::MAX; config.ui.padding.bottom = u16::MAX;
        overlay.questions.set_overlay_config(config.clone());
        for (width,height) in [(0,0),(1,1),(20,8),(30,10),(40,12)] {
            let area = Rect::new(2,3,width,height);
            assert_eq!(panel_area(area, &config.ui), area);
            assert!(padded_area(area, &config.ui).is_empty());
            let mut buf = Buffer::empty(area); overlay.render(&ctx, area, &mut buf);
            let view = snapshot(&buf);
            assert!(!view.contains("scroll") && !view.contains("cancel"));
            if width >= 30 { assert!(view.contains("Resize or reduce padding")); }
        }
        config.ui.padding = Default::default();
        config.ui.styles.insert("error".into(), serde_json::json!({"fg":"red"}));
        overlay.questions.set_overlay_config(config);
        overlay.error = "Validation failed".into();
        let area = Rect::new(0,0,80,20); let mut buf = Buffer::empty(area);
        overlay.render(&ctx, area, &mut buf);
        assert!(snapshot(&buf).contains("Validation failed"));
        assert_eq!(buf[(1,17)].fg, Color::Red);
    }
    #[tokio::test]
    async fn remapped_keys_replace_defaults_and_preserve_custom_input() {
        let mut overlay = fixture().await;
        let model = rness_tui::app::Model::new("s".into(), "m".into());
        let theme = rness_tui::theme::Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut config = overlay.questions.overlay_config();
        for (action,key) in [("up","k"),("down","j"),("select","s"),("submit","x"),("dismiss","q"),("next","n"),("previous","p"),("details","d"),("cancel","f6")] { config.ui.keys.insert(action.into(), key.into()); }
        overlay.questions.set_overlay_config(config.clone());
        let press = |overlay: &mut QuestionOverlay, code| overlay.on_key(&ctx, KeyEvent::new(code, KeyModifiers::NONE));
        for code in [KeyCode::Down,KeyCode::Enter,KeyCode::Esc,KeyCode::Tab,KeyCode::Char(' '),KeyCode::PageDown] { press(&mut overlay, code); }
        assert_eq!((overlay.page,overlay.cursor,overlay.scroll), (0,0,0));
        assert!(overlay.answers[0].selected.is_empty() && overlay.wants(&ctx));
        press(&mut overlay, KeyCode::Char('j')); press(&mut overlay, KeyCode::Char('s'));
        assert_eq!(overlay.answers[0].selected, ["Beta"]);
        press(&mut overlay, KeyCode::Char('n')); assert_eq!(overlay.page, 1);
        press(&mut overlay, KeyCode::Char('p')); assert_eq!(overlay.page, 0);
        press(&mut overlay, KeyCode::Char('d')); assert_eq!(overlay.scroll, 1);
        press(&mut overlay, KeyCode::Char('q')); assert_eq!(overlay.scroll, 0);
        overlay.cursor = 2; press(&mut overlay, KeyCode::Char('x')); assert!(overlay.editing);
        for c in "kj sxqnpd日本語".chars() { press(&mut overlay, KeyCode::Char(c)); }
        assert_eq!(overlay.answers[0].custom.as_deref(), Some("kj sxqnpd日本語"));
        assert!(overlay.editing);
        press(&mut overlay, KeyCode::Backspace);
        assert!(overlay.answers[0].custom.as_ref().unwrap().ends_with("日本"));
        assert!(matches!(press(&mut overlay, KeyCode::F(6)).actions.as_slice(), [rness_tui::app::Action::Cancel]));
        assert!(matches!(overlay.on_key(&ctx, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)).actions.as_slice(), [rness_tui::app::Action::Cancel]));
        press(&mut overlay, KeyCode::Enter); assert!(!overlay.editing, "printable submit has safe editor fallback");
        overlay.editing = true;
        config.ui.keys.insert("submit".into(), "ctrl+e".into()); overlay.questions.set_overlay_config(config);
        press(&mut overlay, KeyCode::Enter); assert!(overlay.editing, "nonprintable remap replaces old submit");
        overlay.on_key(&ctx, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)); assert!(!overlay.editing);
        let ui = QuestionUiConfig::default();
        assert!(binding_matches(&ui, "previous", KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)));
        assert!(binding_matches(&ui, "previous", KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)));
        let mut config = overlay.questions.overlay_config();
        config.ui.keys.insert("submit".into(), "shift+tab".into());
        config.ui.keys.insert("previous".into(), "f2".into());
        config.ui.keys.insert("cancel".into(), "backspace".into());
        overlay.questions.set_overlay_config(config);
        overlay.editing = true;
        let before = overlay.answers[0].custom.clone().unwrap();
        assert!(press(&mut overlay, KeyCode::Backspace).actions.is_empty());
        assert_eq!(overlay.answers[0].custom.as_ref().unwrap().chars().count(), before.chars().count() - 1);
        assert!(overlay.editing);
        overlay.on_key(&ctx, KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(!overlay.editing);
        assert!(markdown_alias(&ui, "down", KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        let mut ui = ui;
        ui.keys.insert("dismiss".into(), "j".into());
        assert!(!markdown_alias(&ui, "down", KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        ui.keys.insert("up".into(), "f2".into());
        assert!(!markdown_alias(&ui, "up", KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)));
        let help = question_help(&QuestionUiConfig::default(), true, false, false, 28);
        assert!(help.lines().next().unwrap().contains("enter:done esc:back"));
        let mut config = overlay.questions.overlay_config();
        config.ui.keys.insert("submit".into(), "backspace".into());
        config.ui.keys.insert("cancel".into(), "f6".into());
        assert_eq!(editor_key(&config.ui, "submit"), "enter");
        overlay.questions.set_overlay_config(config);
        overlay.editing = true;
        press(&mut overlay, KeyCode::Backspace); assert!(overlay.editing);
        press(&mut overlay, KeyCode::Enter); assert!(!overlay.editing);
        ui.keys.insert("cancel".into(), "backspace".into());
        assert!(!question_help(&ui, true, false, false, 28).contains("backspace:cancel"));
    }
}
