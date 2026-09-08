//! Extension statusline: renders text pushed by an external provider
//! (a Lua plugin via the host bridge, a remote frontend, anything).
//!
//! Protocol purity: the TUI knows nothing about Lua — it renders a
//! shared string slot. Mounted above the built-in statusline; `wants()`
//! selects it only while a provider has published text (chain pattern),
//! so with no plugins the built-in renders untouched.

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
}

impl StatusText {
    /// Evaluate a native or adapted provider outside the synchronous renderer.
    pub async fn refresh(&self, provider: &dyn rness_kernel::presentation::TextProvider) {
        let generation = self.0.read().expect("status text lock").generation;
        let text = provider.text().await;
        let mut state = self.0.write().expect("status text lock");
        if state.generation == generation {
            state.text = text;
        }
    }

    pub fn invalidate(&self) {
        self.set(None);
    }

    pub fn set(&self, text: Option<String>) {
        let mut state = self.0.write().expect("status text lock");
        state.generation = state.generation.checked_add(1).expect("status generation exhausted");
        state.text = text;
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

    fn wants(&self, _ctx: &Ctx<'_>) -> bool {
        self.text.get().is_some()
    }

    fn height(&self, _ctx: &Ctx<'_>, _width: u16) -> Option<u16> {
        Some(1)
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let theme = ctx.theme;
        let model = ctx.model;
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
