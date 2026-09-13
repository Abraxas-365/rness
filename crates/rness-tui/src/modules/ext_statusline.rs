//! Extension statusline: renders text pushed by an external provider
//! (a Lua plugin via the host bridge, a remote frontend, anything).
//!
//! Protocol purity: the TUI knows nothing about Lua — it renders a
//! shared string slot. Mounted above the built-in statusline; `wants()`
//! selects it only while a provider has published text (chain pattern).
//! With no plugins the built-in renders untouched.

use std::sync::{Arc, RwLock};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::component::{Component, Ctx};
use crate::slots::{Slots, STATUSLINE};

/// Shared cell the host writes into and the component renders from.
#[derive(Clone, Default)]
pub struct StatusText(Arc<RwLock<StatusState>>);

#[derive(Default)]
struct StatusState {
    generation: u64,
    text: Option<String>,
    view: Option<serde_json::Value>,
    context: serde_json::Value,
    context_revision: u64,
    busy_since: Option<std::time::Instant>,
}

impl StatusText {
    /// Evaluate and publish the initial provider view before the first TUI frame.
    pub async fn prime(
        &self,
        provider: &dyn rness_kernel::presentation::TextProvider,
        context: serde_json::Value,
    ) {
        self.update_context(context);
        self.refresh(provider).await;
    }

    /// Evaluate a native or adapted provider outside the synchronous renderer.
    pub async fn refresh(&self, provider: &dyn rness_kernel::presentation::TextProvider) {
        let (generation, revision, context) = {
            let state = self.0.read().expect("status text lock");
            let mut context = state.context.clone();
            if let Some(object) = context.as_object_mut() {
                let millis = state.busy_since.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
                object.insert("elapsed_ms".into(), millis.into());
                object.insert("elapsed".into(), format!("{}s", millis / 1000).into());
            }
            (state.generation, state.context_revision, context)
        };
        let view = provider.status(context).await;
        let mut state = self.0.write().expect("status text lock");
        if state.generation == generation && state.context_revision == revision {
            state.text = view.as_ref().and_then(|v| v.as_str()).map(str::to_owned);
            state.view = view;
        }
    }

    pub fn invalidate(&self) {
        self.set(None);
    }

    pub fn set(&self, text: Option<String>) {
        let mut state = self.0.write().expect("status text lock");
        state.generation = state.generation.checked_add(1).expect("status generation exhausted");
        state.view = text.clone().map(serde_json::Value::String);
        state.text = text;
    }

    fn context(&self, ctx: &Ctx<'_>) {
        self.update_context(serde_json::json!({
            "session": ctx.model.session,
            "model": ctx.model.model_name,
            "profile": ctx.model.profile_name,
            "agent": ctx.model.agent_name,
            "busy": ctx.model.busy,
            "activity": if ctx.model.busy { "working" } else { "idle" },
        }));
    }

    fn update_context(&self, context: serde_json::Value) {
        let mut state = self.0.write().expect("status text lock");
        if state.context != context {
            if !context["busy"].as_bool().unwrap_or(false) { state.busy_since = None; }
            else if state.busy_since.is_none() || state.context["session"] != context["session"] {
                state.busy_since = Some(std::time::Instant::now());
            }
            state.context = context;
            state.context_revision = state.context_revision.wrapping_add(1);
        }
    }

    fn get(&self) -> Option<String> {
        self.0.read().expect("status text lock").text.clone()
    }
}

pub struct ExtStatusline {
    text: StatusText,
}

/// Mount above the built-in statusline (priority 10 > 0). Returns the
/// handle the host uses to publish text.
pub fn install(slots: &mut Slots) -> StatusText {
    let text = StatusText::default();
    slots.mount(STATUSLINE, 10, Box::new(ExtStatusline { text: text.clone() }));
    text
}

impl Component for ExtStatusline {
    fn name(&self) -> &str {
        "ext_statusline"
    }

    fn wants(&self, ctx: &Ctx<'_>) -> bool {
        self.text.context(ctx);
        self.text.0.read().expect("status text lock").view.is_some()
    }

    fn height(&self, _ctx: &Ctx<'_>, _width: u16) -> Option<u16> {
        let state = self.text.0.read().expect("status text lock");
        if state.view.as_ref().and_then(|v| v.get("visible")).and_then(|v| v.as_bool()) == Some(false) { Some(0) } else { Some(1) }
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let theme = ctx.theme;
        let model = ctx.model;
        if area.width == 0 || area.height == 0 { return; }
        let view = self.text.0.read().expect("status text lock").view.clone();
        if let Some(view) = view.filter(|v| v.is_object()) {
            let base = theme.resolve_style(&view["style"], theme.statusline).unwrap_or(theme.statusline);
            for x in area.left()..area.right() { buf[(x, area.y)].set_style(base).set_symbol(" "); }
            let pad = |side| view["padding"][side].as_u64().unwrap_or(0).min(u16::MAX as u64) as u16;
            let left_pad = pad("left").min(area.width);
            let width = area.width.saturating_sub(left_pad).saturating_sub(pad("right"));
            let inner = Rect::new(area.x + left_pad, area.y, width, 1);
            let section = |value: &serde_json::Value| -> Line<'static> {
                let values = value.as_array().cloned().unwrap_or_else(|| vec![value.clone()]);
                let mut spans = Vec::new();
                for value in values {
                    let text = value.as_str().or_else(|| value["text"].as_str()).unwrap_or("");
                    if text.is_empty() { continue; }
                    if !spans.is_empty() { spans.push(Span::styled(view["separator"].as_str().unwrap_or(" · ").to_owned(), base)); }
                    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
                    spans.push(Span::styled(text, theme.resolve_style(&value["style"], base).unwrap_or(base)));
                }
                Line::from(spans)
            };
            let left = section(&view["left"]);
            let right = section(&view["right"]);
            let right_width = right.width().min(width as usize) as u16;
            let left_width = width.saturating_sub(right_width).saturating_sub(u16::from(right_width > 0));
            left.render(Rect::new(inner.x, inner.y, left_width, 1), buf);
            right.render(Rect::new(inner.right() - right_width, inner.y, right_width, 1), buf);
            return;
        }
        let Some(text) = self.text.get() else { return };
        for x in area.left()..area.right() {
            buf[(x, area.y)].set_style(theme.statusline).set_symbol(" ");
        }
        let phase = if model.busy { "streaming" } else { "idle" };
        Line::from(vec![
            Span::styled(format!(" {} ", model.model_name), theme.statusline_accent),
            Span::styled(format!("· {phase}  "), theme.statusline),
            Span::styled(text, theme.statusline_accent),
        ])
        .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Model;
    use crate::theme::Theme;

    #[test]
    fn structured_sections_align_and_clip_at_every_width() {
        let cell = StatusText::default();
        cell.0.write().unwrap().view = Some(serde_json::json!({
            "left":[{"text":"working", "style":{"fg":"#83a598"}}],
            "right":"model", "padding":{"left":1,"right":1},
            "style":{"bg":"#282828"}
        }));
        let mut component = ExtStatusline { text: cell.clone() };
        let model = Model::new("s".into(), "m".into());
        let theme = Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        for width in 0..32 {
            let area = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(area);
            component.render(&ctx, area, &mut buf);
            if width == 25 {
                let text: String = (0..width).map(|x| buf[(x,0)].symbol()).collect();
                assert!(text.starts_with(" working"));
                assert!(text.ends_with("model "));
                assert_eq!(buf[(1,0)].fg, ratatui::style::Color::Rgb(131,165,152));
            }
        }
        cell.0.write().unwrap().view = Some(serde_json::json!({"visible":false}));
        assert_eq!(component.height(&ctx, 20), Some(0));
    }

    #[test]
    fn status_context_includes_profile_and_agent() {
        let cell = StatusText::default();
        let mut model = Model::new("s".into(), "provider/model".into());
        model.profile_name = Some("fast".into());
        model.agent_name = Some("coder".into());
        let theme = Theme::default();
        cell.context(&Ctx { model: &model, theme: &theme });

        let state = cell.0.read().unwrap();
        assert_eq!(state.context["model"], "provider/model");
        assert_eq!(state.context["profile"], "fast");
        assert_eq!(state.context["agent"], "coder");
    }

    #[test]
    fn context_change_keeps_published_view_until_refresh_replaces_it() {
        let cell = StatusText::default();
        let mut model = Model::new("s".into(), "provider/model".into());
        let theme = Theme::default();
        cell.context(&Ctx { model: &model, theme: &theme });
        cell.set(Some("published".into()));

        model.busy = true;
        cell.context(&Ctx { model: &model, theme: &theme });

        assert_eq!(cell.get().as_deref(), Some("published"));
    }

    #[tokio::test]
    async fn prime_publishes_initial_provider_view() {
        struct Initial;
        #[async_trait::async_trait]
        impl rness_kernel::presentation::TextProvider for Initial {
            async fn text(&self) -> Option<String> { Some("initial".into()) }
        }

        let cell = StatusText::default();
        cell.prime(&Initial, serde_json::json!({"session":"s", "busy":false})).await;

        assert_eq!(cell.get().as_deref(), Some("initial"));
    }

    #[tokio::test]
    async fn invalidation_during_refresh_discards_old_provider_text() {
        struct Delayed {
            started: tokio::sync::Notify,
            release: tokio::sync::Notify,
        }
        #[async_trait::async_trait]
        impl rness_kernel::presentation::TextProvider for Delayed {
            async fn text(&self) -> Option<String> {
                self.started.notify_one();
                self.release.notified().await;
                Some("stale".into())
            }
        }
        let cell = StatusText::default();
        let provider = Delayed {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        };
        tokio::join!(cell.refresh(&provider), async {
            provider.started.notified().await;
            cell.invalidate();
            provider.release.notify_one();
        });
        assert_eq!(cell.get(), None);
        tokio::join!(cell.refresh(&provider), async {
            provider.started.notified().await;
            cell.set(Some("replacement".into()));
            provider.release.notify_one();
        });
        assert_eq!(cell.get().as_deref(), Some("replacement"));
    }

    #[test]
    fn provider_can_animate_during_compaction() {
        use rness_protocol::frames::Frame;

        let mut slots = Slots::default();
        crate::modules::statusline::install(&mut slots);
        let handle = install(&mut slots);
        let mut model = Model::new("s".into(), "m".into());
        let theme = Theme::default();
        model.apply_frame(&Frame::CompactionStarted {
            session: "s".into(), events: 42, estimated_tokens: 73000,
        });
        let render = |slots: &mut Slots| {
            let area = Rect::new(0, 0, 80, 1);
            let mut buf = Buffer::empty(area);
            slots.render(&Ctx { model: &model, theme: &theme }, area, &mut buf);
            (0..area.width).map(|x| buf[(x, 0)].symbol()).collect::<String>()
        };
        for frame in ["⠋", "⠙"] {
            let text = format!("{frame} compacting context");
            handle.set(Some(text.clone()));
            assert!(render(&mut slots).contains(&text));
        }
        handle.set(None);
        assert!(render(&mut slots).contains("compacting 42 events · ~73k tok"));
    }

    #[test]
    fn shadows_builtin_only_while_text_is_published() {
        let mut slots = Slots::default();
        crate::modules::statusline::install(&mut slots);
        let handle = install(&mut slots);

        let model = Model::new("s".into(), "m".into());
        let theme = Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };

        let render_winner = |slots: &mut Slots, ctx: &Ctx<'_>| -> String {
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 1));
            slots.render(ctx, Rect::new(0, 0, 40, 1), &mut buf);
            (0..40).map(|x| buf[(x, 0)].symbol().to_string()).collect()
        };

        // No text published: the built-in renders (session id visible).
        let line = render_winner(&mut slots, &ctx);
        assert!(line.contains("(s)"), "builtin statusline: {line}");

        handle.set(Some("⚡ from lua".into()));
        let line = render_winner(&mut slots, &ctx);
        assert!(line.contains("from lua"), "ext statusline: {line}");

        handle.set(None);
        let line = render_winner(&mut slots, &ctx);
        assert!(line.contains("(s)"), "builtin again: {line}");
    }
}
