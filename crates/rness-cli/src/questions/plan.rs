use super::*;
use rness_engine::plan::PlanReviewConfig;
use rness_tui::app::Action;

#[cfg(test)]
mod tests {
    use super::*;
    use rness_engine::{
        plan::{ExitPlan, PlanConfig},
        session::branch::SessionStore,
        tools::{ToolCall, ToolRegistry},
    };
    use rness_protocol::events::{PlanReview, SessionEvent};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn review_configuration_rejects_text_controls_and_unusable_dimensions() {
        for value in [
            serde_json::json!({"keys":{"cancel":"enter"}}),
            serde_json::json!({"keys":{"feedback_submit":"space"}}),
            serde_json::json!({"keys":{"feedback_submit":"<a>"}}),
            serde_json::json!({"keys":{"feedback_submit":"shift+a"}}),
            serde_json::json!({"keys":{"edit":"ctrl+alt+c"}}),
            serde_json::json!({"height":10,"padding":{"top":2,"bottom":2}}),
            serde_json::json!({"width":30,"padding":{"left":3,"right":3}}),
        ] {
            let mut config: PlanReviewConfig = serde_json::from_value(value.clone()).unwrap();
            config.normalize();
            assert!(config.validate().is_err(), "{value}");
        }
    }

    #[tokio::test]
    async fn plan_editor_roundtrip_and_configurable_review() {
        for auto_approve in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(SessionStore::new(dir.path()));
            let mut log = store.create(None).unwrap();
            let session = log.session().clone();
            log.append(&SessionEvent::PlanMode { active: true })
                .unwrap();
            let questions = Arc::new(Questions::default());
            questions.set_available(true);
            let mut config = PlanConfig::default();
            config.review.approve_after_edit = auto_approve;
            config.review.title = "Implementation review".into();
            config.review.keys.insert("edit".into(), "ctrl+e".into());
            config.review.keys.insert("cancel".into(), "f6".into());
            config.review.editor = Some(vec!["my-editor".into(), "--wait".into()]);
            let tools = ToolRegistry::default();
            tools.register(Arc::new(ExitPlan {
                config,
                store,
                questions: questions.clone(),
                alive: CancellationToken::new(),
            }));
            let s = session.clone();
            let mut events = questions.subscribe();
            let task = tokio::spawn(async move {
                tools.dispatch(&s, &[ToolCall { call: "review".into(), name: "exit_plan_mode".into(), args: serde_json::json!({"plan":format!("# Initial plan\n{}\nEND-OF-PLAN", "Some details\n".repeat(60))}) }], 1, &CancellationToken::new()).await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            let mut overlay = QuestionOverlay::new(questions.clone());
            let model = rness_tui::app::Model::new(session.clone(), "m".into());
            let theme = rness_tui::theme::Theme::default();
            let ctx = Ctx {
                model: &model,
                theme: &theme,
            };
            for (width, height) in [(100, 30), (40, 12), (160, 40)] {
                let area = Rect::new(0, 0, width, height);
                let mut buf = Buffer::empty(area);
                overlay.render(&ctx, area, &mut buf);
                let snapshot = |buf: &Buffer| {
                    (0..height)
                        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let text = snapshot(&buf);
                assert!(text.contains("Implementation review"));
                assert!(!text.contains("AskUser"));
                assert!(text.contains("a: Approve"));
                assert!(text.contains("ctrl+e:"));
                overlay.on_key(&ctx, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
                overlay.render(&ctx, area, &mut buf);
                assert!(snapshot(&buf).contains("END-OF-PLAN"));
            }
            overlay.on_key(&ctx, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
            overlay.on_paste(&ctx, "User feedback");
            assert_eq!(overlay.answers[0].custom.as_deref(), Some("User feedback"));
            assert!(matches!(
                overlay
                    .on_key(&ctx, KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
                    .actions
                    .as_slice(),
                [Action::Cancel]
            ));
            overlay.on_key(&ctx, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(overlay
                .on_key(&ctx, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
                .actions
                .is_empty());
            let outcome = overlay.on_key(
                &ctx,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            );
            let Action::Custom(name, mut payload) = outcome.actions.into_iter().next().unwrap()
            else {
                panic!("editor action")
            };
            assert_eq!(name, "terminal:edit-plan");
            assert_eq!(payload["editor"][1], "--wait");
            payload["error"] = serde_json::json!("exit 7");
            overlay.on_action(&ctx, "questions:plan-edited", &payload);
            assert!(overlay.error.contains("exit 7"));
            payload.as_object_mut().unwrap().remove("error");
            payload["text"] = serde_json::json!("invalid");
            overlay.on_action(&ctx, "questions:plan-edited", &payload);
            assert!(overlay.error.contains("# heading"));
            assert!(!questions.pending().is_empty());
            payload["text"] = serde_json::json!("# Edited plan\nUse the user's version");
            let mut stale = payload.clone();
            stale["call"] = serde_json::json!("old-review");
            overlay.on_action(&ctx, "questions:plan-edited", &stale);
            assert!(!questions.pending().is_empty());
            overlay.on_action(&ctx, "questions:plan-edited", &payload);
            if !auto_approve {
                assert!(!questions.pending().is_empty());
                assert_eq!(
                    overlay.answers[0].edited_markdown.as_deref(),
                    payload["text"].as_str()
                );
                overlay.on_key(&ctx, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
            }
            assert!(questions.pending().is_empty());
            let results = task.await.unwrap();
            assert_eq!(results[0].plan_review, Some(PlanReview::Approved));
            assert!(results[0]
                .output
                .contains("# Edited plan\nUse the user's version"));
            overlay.on_action(&ctx, "questions:plan-edited", &payload);
            assert!(questions.pending().is_empty());
        }
    }
}

impl QuestionOverlay {
    pub(super) fn render_plan(
        &mut self,
        ctx: &Ctx<'_>,
        area: Rect,
        buf: &mut Buffer,
        request: &Request,
        config: &PlanReviewConfig,
    ) {
        let ui = config.ui();
        let area = panel_area(area, &ui);
        let style = |name, fallback| question_style(ctx, &ui, name, fallback);
        Clear.render(area, buf);
        let border = match config.border.as_str() {
            "rounded" => BorderType::Rounded,
            "double" => BorderType::Double,
            "thick" => BorderType::Thick,
            _ => BorderType::Plain,
        };
        let block = Block::default()
            .borders(if config.border == "none" {
                Borders::NONE
            } else {
                Borders::ALL
            })
            .border_type(border)
            .title(Line::styled(
                format!(" {} ", config.title),
                style("title", ctx.theme.heading),
            ))
            .style(style("panel", ctx.theme.overlay))
            .border_style(style("border", ctx.theme.overlay_border));
        let inner = padded_area(block.inner(area), &ui);
        block.render(area, buf);
        if inner.height < 6 || inner.width < 24 {
            Paragraph::new("Resize to review plan")
                .style(style("error", ctx.theme.error))
                .render(inner, buf);
            return;
        }
        let hint = |action: &str, label: &str| format!("{}: {}", config.keys[action], label);
        let mut actions = vec![
            hint("approve", &config.labels.approve),
            hint("feedback", &config.labels.feedback),
        ];
        if config.edit_enabled {
            actions.push(hint(
                "edit",
                if config.approve_after_edit {
                    &config.labels.edit_and_approve
                } else {
                    &config.labels.edit
                },
            ));
        }
        let combined = actions.join("  ·  ");
        let wrapped = textwrap::wrap(&combined, inner.width as usize);
        // Compact long custom labels to one row per action, never hide a later action.
        let action_lines = if wrapped.len() > 3 {
            actions
                .into_iter()
                .map(|text| {
                    let mut text = text
                        .chars()
                        .take(inner.width.saturating_sub(1) as usize)
                        .collect::<String>();
                    text.push('…');
                    Line::raw(text)
                })
                .collect::<Vec<_>>()
        } else {
            wrapped
                .into_iter()
                .map(|s| Line::raw(s.into_owned()))
                .collect::<Vec<_>>()
        };
        let footer = if self.editing {
            format!(
                "{} ({}: send, {}: back): {}",
                config.labels.feedback_prompt,
                config.keys["feedback_submit"],
                config.keys["feedback_back"],
                self.answers[self.page].custom.as_deref().unwrap_or("")
            )
        } else if !self.error.is_empty() {
            self.error.clone()
        } else if config.show_help {
            format!(
                "{}/{}: scroll  {}/{}: page  {}: close",
                config.keys["up"],
                config.keys["down"],
                config.keys["page_up"],
                config.keys["page_down"],
                config.keys["dismiss"]
            )
        } else {
            String::new()
        };
        let footer_lines = textwrap::wrap(&footer, inner.width as usize)
            .into_iter()
            .map(|s| Line::raw(s.into_owned()))
            .collect::<Vec<_>>();
        let footer_height = footer_lines.len().min(2) as u16;
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(action_lines.len().min(3) as u16),
            Constraint::Length(footer_height),
        ])
        .split(inner);
        let answer = &self.answers[self.page];
        let markdown = answer
            .edited_markdown
            .as_deref()
            .or(request.questions[self.page].markdown.as_deref())
            .unwrap_or("");
        let lines = rness_tui::core::render::render_markdown(markdown, chunks[0].width, ctx.theme);
        self.page_rows = chunks[0].height.max(1);
        self.max_scroll = lines
            .len()
            .saturating_sub(chunks[0].height as usize)
            .min((u16::MAX - 1) as usize) as u16;
        self.scroll = self.scroll.max(1).min(self.max_scroll + 1);
        Paragraph::new(lines)
            .scroll((self.scroll - 1, 0))
            .style(style("question", ctx.theme.overlay))
            .render(chunks[0], buf);
        Paragraph::new(action_lines)
            .style(style("option", ctx.theme.heading))
            .render(chunks[1], buf);
        let offset = footer_lines.len().saturating_sub(footer_height as usize) as u16;
        Paragraph::new(footer_lines)
            .scroll((offset, 0))
            .style(style(
                if self.error.is_empty() {
                    "help"
                } else {
                    "error"
                },
                if self.error.is_empty() {
                    ctx.theme.dim
                } else {
                    ctx.theme.error
                },
            ))
            .render(chunks[2], buf);
    }

    fn submit_plan(&mut self, request: &Request, approve: bool) {
        let mut answer = self.answers[self.page].clone();
        answer.selected = vec![if approve { "Approve" } else { "Keep planning" }.into()];
        if approve {
            answer.custom = None;
        } else {
            answer.edited_markdown = None;
        }
        if let Err(error) = self.questions.resolve(
            &request.session,
            &request.call,
            Answers {
                answers: vec![answer],
            },
        ) {
            self.error = error;
        }
    }

    pub(super) fn plan_key(
        &mut self,
        key: KeyEvent,
        request: &Request,
        config: &PlanReviewConfig,
    ) -> KeyOutcome {
        let matches = |action: &str| {
            config
                .keys
                .get(action)
                .is_some_and(|binding| chord_matches(binding, key))
        };
        self.error.clear();
        if self.editing {
            if matches("cancel")
                && !matches!(key.code, KeyCode::Backspace)
                && (!matches!(key.code, KeyCode::Char(_))
                    || key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT))
            {
                return KeyOutcome::act(vec![Action::Cancel]);
            }
            if matches("feedback_back") {
                self.editing = false;
                return KeyOutcome::consumed();
            }
            if matches("feedback_submit") {
                self.submit_plan(request, false);
                return KeyOutcome::consumed();
            }
            match key.code {
                KeyCode::Backspace => {
                    self.answers[self.page].custom.get_or_insert_default().pop();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.answers[self.page]
                        .custom
                        .get_or_insert_default()
                        .push(c)
                }
                _ => {}
            }
            return KeyOutcome::consumed();
        }
        if matches("cancel") {
            return KeyOutcome::act(vec![Action::Cancel]);
        }
        if matches("dismiss") {
            self.questions.dismiss(&request.session, &request.call);
        } else if matches("approve") {
            self.submit_plan(request, true);
        } else if matches("feedback") {
            self.editing = true;
        } else if matches("edit") && config.edit_enabled {
            let text = self.answers[self.page]
                .edited_markdown
                .as_deref()
                .or(request.questions[self.page].markdown.as_deref())
                .unwrap_or("");
            return KeyOutcome::act(vec![Action::Custom(
                "terminal:edit-plan".into(),
                serde_json::json!({
                    "text":text, "editor":config.editor, "session":request.session, "call":request.call,
                    "question":request.questions[self.page].id,
                }),
            )]);
        } else if matches("down") {
            self.scroll = self.scroll.saturating_add(1).min(self.max_scroll + 1);
        } else if matches("up") {
            self.scroll = self.scroll.saturating_sub(1).max(1);
        } else if matches("page_down") {
            self.scroll = self
                .scroll
                .saturating_add(self.page_rows)
                .min(self.max_scroll + 1);
        } else if matches("page_up") {
            self.scroll = self.scroll.saturating_sub(self.page_rows).max(1);
        } else if matches("first") {
            self.scroll = 1;
        } else if matches("last") {
            self.scroll = self.max_scroll + 1;
        }
        KeyOutcome::consumed()
    }

    pub(super) fn plan_edited(&mut self, ctx: &Ctx<'_>, payload: &serde_json::Value) {
        // An editor completion is tied to the original review, never the current question alone.
        let Some(request) = self.questions.pending().into_iter().find(|r| {
            payload["session"] == r.session
                && payload["call"] == r.call
                && r.session == ctx.model.session
        }) else {
            return;
        };
        let Some(question) = request
            .questions
            .first()
            .filter(|q| payload["question"] == q.id)
        else {
            return;
        };
        let Some(config) = question.plan_review.as_ref().filter(|c| c.edit_enabled) else {
            return;
        };
        self.sync(&request);
        if let Some(error) = payload["error"].as_str() {
            self.error = format!("Editor failed; plan unchanged: {error}");
            return;
        }
        let Some(text) = payload["text"].as_str() else {
            return;
        };
        if let Err(error) = rness_engine::plan::validate_plan(text) {
            self.error = error;
            return;
        }
        self.error.clear();
        self.answers[0].edited_markdown = Some(text.into());
        self.scroll = 1;
        if config.approve_after_edit {
            self.submit_plan(&request, true);
        }
    }
}
