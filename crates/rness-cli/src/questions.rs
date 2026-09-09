use std::sync::Arc;
use rness_engine::questions::{Questions, Answers, Answer, Request};
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

pub struct QuestionOverlay {
    pub questions: Arc<Questions>,
    key: Option<(String, String)>,
    page: usize,
    cursor: usize,
    answers: Vec<Answer>,
    editing: bool,
    error: String,
    scroll: u16,
    drafts: std::collections::HashMap<(String, String), (usize, usize, Vec<Answer>, bool)>,
    pub apps: Option<rness_tui::modules::ext_apps::AppsState>,
}
impl QuestionOverlay {
    pub fn new(questions: Arc<Questions>) -> Self { Self { questions, key: None, page: 0, cursor: 0, answers: vec![], editing: false, error: String::new(), scroll: 0, drafts: Default::default(), apps: None } }
    fn sync(&mut self, request: &Request) {
        let key = (request.session.clone(), request.call.clone());
        if self.key.as_ref() != Some(&key) {
            if let Some(old) = self.key.take() {
                self.drafts.insert(old, (self.page, self.cursor, std::mem::take(&mut self.answers), self.editing));
            }
            let pending = self.questions.pending();
            self.drafts.retain(|key, _| pending.iter().any(|r| r.session == key.0 && r.call == key.1));
            let saved = self.drafts.remove(&key);
            self.key = Some(key); self.page = 0; self.cursor = 0; self.editing = false; self.error.clear(); self.scroll = 0;
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
        Clear.render(area, buf);
        if area.width < 30 || area.height < 10 {
            Paragraph::new("Resize to 30x10\nEsc: dismiss\nCtrl+C: cancel").style(ctx.theme.error).wrap(Wrap { trim: false }).render(area, buf);
            return;
        }
        let config = self.questions.overlay_config();
        let block = Block::default().borders(Borders::ALL).title(format!(" {}  {}/{}  {} ", config.title, self.page + 1, request.questions.len(), q.header)).style(ctx.theme.overlay).border_style(ctx.theme.overlay_border);
        let inner = block.inner(area); block.render(area, buf);
        let compact = inner.height < 14;
        let chunks = Layout::vertical([Constraint::Length(if compact { 2 } else { 3 }), Constraint::Min(1), Constraint::Length(if compact { 1 } else { 2 }), Constraint::Length(2)]).split(inner);
        Paragraph::new(q.question.as_str()).style(ctx.theme.heading).wrap(Wrap { trim: false }).render(chunks[0], buf);
        let detail = self.scroll > 0;
        if detail {
            let text = if let Some(option) = q.options.get(self.cursor) {
                format!("{}\n{}\n{}", q.question, option.label, option.description)
            } else { q.question.clone() };
            let lines: Vec<Line> = text.lines().flat_map(|line| textwrap::wrap(line, chunks[1].width.max(1) as usize).into_iter().map(|s| Line::raw(s.into_owned())).collect::<Vec<_>>()).collect();
            let offset = (self.scroll - 1).min(lines.len().saturating_sub(chunks[1].height as usize) as u16);
            Paragraph::new(lines).scroll((offset, 0)).render(chunks[1], buf);
        } else {
            let mut items: Vec<_> = q.options.iter().map(|o| {
                let mark = if answer.selected.contains(&o.label) { "[x]" } else { "[ ]" };
                let width = chunks[1].width.saturating_sub(2).max(1) as usize;
                let mut lines: Vec<Line> = textwrap::wrap(&format!("{mark} {}", o.label), width).into_iter().map(|s| Line::raw(s.into_owned())).collect();
                if !compact && !o.description.is_empty() {
                    lines.extend(textwrap::wrap(&o.description, width).into_iter().map(|s| Line::styled(s.into_owned(), ctx.theme.dim)));
                }
                let available = chunks[1].height as usize;
                if lines.len() > available && available > 0 {
                    lines.truncate(available);
                    lines[available - 1] = Line::styled("… PgDn: full details", ctx.theme.dim);
                }
                ListItem::new(lines)
            }).collect();
            items.push(ListItem::new("[+] Other / write an answer"));
            let mut state = ListState::default().with_selected(Some(self.cursor));
            StatefulWidget::render(List::new(items).highlight_symbol("> ").highlight_style(ctx.theme.statusline_accent), chunks[1], buf, &mut state);
        }
        let text = format!("{}{}", if self.editing { "Editing > " } else { "Custom: " }, answer.custom.as_deref().unwrap_or(""));
        let lines: Vec<Line> = textwrap::wrap(&text, chunks[2].width.max(1) as usize).into_iter().map(|s| Line::raw(s.into_owned())).collect();
        let offset = if self.editing { lines.len().saturating_sub(chunks[2].height as usize).min(u16::MAX as usize) as u16 } else { 0 };
        Paragraph::new(lines).scroll((offset, 0)).style(ctx.theme.editor_prompt).render(chunks[2], buf);
        let footer = if self.error.is_empty() {
            if detail { "PgUp/PgDn: scroll  Esc: back" }
            else if self.editing && inner.width < 65 { "Type answer | Enter: done\nEsc: back" }
            else if self.editing { "Type answer | Enter: finish editing | Esc: back" }
            else if inner.width < 40 { "Space:pick Enter:send Tab:next\nEsc:close PgDn:details" }
            else if inner.width < 65 { "Up/Down Space:pick Enter:send\nTab:next Esc:close PgDn:details" }
            else { "Up/Down: move  Space: select  Enter: next/send\nTab/Shift-Tab: question  PgDn: details  Esc: dismiss  Ctrl+C: cancel" }
        } else { &self.error };
        Paragraph::new(footer).style(if self.error.is_empty() { ctx.theme.dim } else { ctx.theme.error }).render(chunks[3], buf);
    }
    fn on_key(&mut self, ctx: &Ctx<'_>, key: KeyEvent) -> KeyOutcome {
        let Some(request) = self.questions.pending().into_iter().find(|p| p.session == ctx.model.session) else { return KeyOutcome::pass(); };
        self.sync(&request);
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) { return KeyOutcome::act(vec![rness_tui::app::Action::Cancel]); }
        self.error.clear();
        if !self.editing {
            match key.code {
                KeyCode::PageDown => { self.scroll = self.scroll.saturating_add(1); return KeyOutcome::consumed(); }
                KeyCode::PageUp => { self.scroll = self.scroll.saturating_sub(1); return KeyOutcome::consumed(); }
                KeyCode::Esc if self.scroll > 0 => { self.scroll = 0; return KeyOutcome::consumed(); }
                _ => self.scroll = 0,
            }
        }
        if self.editing {
            let answer = &mut self.answers[self.page];
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.editing = false,
                KeyCode::Backspace => { answer.custom.get_or_insert_default().pop(); }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => { answer.custom.get_or_insert_default().push(c); }
                _ => {},
            }
            return KeyOutcome::consumed();
        }
        let q = &request.questions[self.page];
        match key.code {
            KeyCode::Esc => { self.questions.dismiss(&request.session, &request.call); }
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(q.options.len()),
            KeyCode::Tab => { self.page = (self.page + 1) % request.questions.len(); self.cursor = 0; }
            KeyCode::BackTab => { self.page = (self.page + request.questions.len() - 1) % request.questions.len(); self.cursor = 0; }
            KeyCode::Char(' ') if self.cursor == q.options.len() => self.editing = true,
            KeyCode::Enter if self.cursor == q.options.len() && self.answers[self.page].custom.as_ref().map_or(true, |s| s.trim().is_empty()) => self.editing = true,
            KeyCode::Char(' ') => {
                let label = &q.options[self.cursor].label;
                let answer = &mut self.answers[self.page];
                if answer.selected.contains(label) { answer.selected.retain(|s| s != label); }
                else { if !q.multi_select { answer.selected.clear(); } answer.selected.push(label.clone()); }
            }
            KeyCode::Enter => {
                let answer = &mut self.answers[self.page];
                if answer.selected.is_empty() && answer.custom.as_ref().map_or(true, |s| s.trim().is_empty()) && !q.options.is_empty() { answer.selected.push(q.options[self.cursor].label.clone()); }
                if self.page + 1 < request.questions.len() { self.page += 1; self.cursor = 0; }
                else if let Err(error) = self.questions.resolve(&request.session, &request.call, Answers { answers: self.answers.clone() }) { self.error = error; }
            }
            _ => {},
        }
        KeyOutcome::consumed()
    }
}
