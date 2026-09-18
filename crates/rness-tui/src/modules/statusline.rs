//! Statusline: session, model, phase — one line at the very bottom.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::component::{Component, Ctx};
use crate::slots::{Slots, STATUSLINE};

pub struct Statusline;

/// Mount the module (composition-root seam, same as future Lua installs).
pub fn install(slots: &mut Slots) {
    slots.mount(STATUSLINE, 0, Box::new(Statusline));
}

impl Component for Statusline {
    fn name(&self) -> &str {
        "statusline"
    }

    fn height(&self, _ctx: &Ctx<'_>, _width: u16) -> Option<u16> {
        Some(1)
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let model = ctx.model;
        let theme = ctx.theme;
        let phase = match &model.compaction {
            Some(compaction) => format!(
                "compacting {} events · ~{}k tok",
                compaction.events,
                compaction.estimated_tokens / 1000
            ),
            None if model.busy => "streaming".into(),
            None => "idle".into(),
        };
        let scroll = if model.scroll_from_bottom > 0 {
            format!("  ↑{}", model.scroll_from_bottom)
        } else {
            String::new()
        };
        // Fill the whole line with the bar style first.
        for x in area.left()..area.right() {
            buf[(x, area.y)].set_style(theme.statusline).set_symbol(" ");
        }
        Line::from(vec![
            Span::styled(format!(" {} ", model.model_name), theme.statusline_accent),
            Span::styled(format!("· {phase}{scroll}"), theme.statusline),
            Span::styled(
                format!(
                    "  ({})",
                    &model.session.as_str()[..8.min(model.session.as_str().len())]
                ),
                theme.statusline,
            ),
        ])
        .render(area, buf);
    }
}
