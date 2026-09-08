//! Chat module: renders the conversation (durable entries + live stream)
//! into the message_body slot, tool results as inline cards.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use rness_protocol::events::ContentPart;

use crate::app::Entry;
use crate::component::{Component, Ctx};
use crate::core::render::render_markdown;
use crate::core::viewport::render_bottom_anchored;
use crate::modules::tool_cards::CardCache;
use crate::slots::{Slots, MESSAGE_BODY};

pub struct Chat {
    /// Lua-rendered cards, host-published. Hit = plugin card; miss =
    /// built-in card. Default cache is empty forever when no Lua host
    /// feeds it — zero magic, zero cost.
    cards: CardCache,
}

/// Mount the module (composition-root seam, same as future Lua installs).
/// Returns the card cache handle the host publishes Lua cards into.
pub fn install(slots: &mut Slots) -> CardCache {
    let cards = CardCache::default();
    slots.mount(MESSAGE_BODY, 0, Box::new(Chat { cards: cards.clone() }));
    cards
}

const TOOL_CARD_MAX_LINES: usize = 6;

/// Terminal-safe cell text: expand tabs (8-col stops) and drop control
/// chars / ANSI escapes. Raw tool output otherwise desyncs ratatui's
/// cell accounting — overlapping glyphs and stale-frame ghosting.
fn sanitize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut col = 0usize;
    let mut in_escape = false;
    for ch in line.chars() {
        if in_escape {
            // CSI/OSC terminators: a letter ends the sequence.
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }
        match ch {
            '\u{1b}' => in_escape = true,
            '\t' => {
                let next = (col / 8 + 1) * 8;
                for _ in col..next {
                    out.push(' ');
                }
                col = next;
            }
            c if c.is_control() => {}
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

impl Component for Chat {
    fn name(&self) -> &str {
        "chat"
    }

    fn height(&self, _ctx: &Ctx<'_>, _width: u16) -> Option<u16> {
        None // fill
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let width = area.width;
        let theme = ctx.theme;
        let mut lines: Vec<Line<'static>> = Vec::new();

        for entry in &ctx.model.entries {
            match entry {
                Entry::User { content } => {
                    lines.push(Line::default());
                    for part in content {
                        if let ContentPart::Text { text } = part {
                            for (i, l) in text.lines().enumerate() {
                                let prefix = if i == 0 { "> " } else { "  " };
                                lines.push(Line::from(vec![
                                    Span::styled(prefix.to_string(), theme.user_prefix),
                                    Span::raw(l.to_string()),
                                ]));
                            }
                        }
                    }
                }
                Entry::Assistant { content, .. } => {
                    lines.push(Line::default());
                    for part in content {
                        match part {
                            ContentPart::Text { text } => {
                                lines.extend(render_markdown(text, width, theme));
                            }
                            ContentPart::Thinking { text, .. } => {
                                for l in text.lines().take(3) {
                                    lines.push(Line::styled(format!("· {l}"), theme.thinking));
                                }
                            }
                            ContentPart::ToolUse { call, name, .. } => {
                                // A Lua card owns the whole render, its
                                // own title line included — skip the
                                // built-in header.
                                if self.cards.contains(call) {
                                    continue;
                                }
                                lines.push(Line::from(vec![
                                    Span::styled("⚙ ".to_string(), theme.tool_name),
                                    Span::styled(name.clone(), theme.tool_name),
                                ]));
                            }
                        }
                    }
                }
                Entry::ToolResult { call, name, output, is_error } => {
                    // Lua card (host-published) shadows the built-in.
                    // The card owns its lines verbatim — no forced
                    // indent, no header; plugins format everything.
                    if let Some(card) = self.cards.get(call) {
                        for row in card {
                            lines.push(Line::styled(
                                sanitize(&row.text),
                                theme.card_style(&row.style),
                            ));
                        }
                        continue;
                    }
                    let style = if *is_error { theme.error } else { theme.tool_output };
                    let total = output.lines().count();
                    for l in output.lines().take(TOOL_CARD_MAX_LINES) {
                        lines.push(Line::styled(format!("  │ {}", sanitize(l)), style));
                    }
                    if total > TOOL_CARD_MAX_LINES {
                        lines.push(Line::styled(
                            format!("  │ … {} more lines ({name})", total - TOOL_CARD_MAX_LINES),
                            theme.dim,
                        ));
                    }
                }
                Entry::Notice(text) => {
                    lines.push(Line::styled(text.clone(), theme.dim));
                }
            }
        }

        // Live stream below the durable entries.
        if let Some(live) = &ctx.model.live {
            lines.push(Line::default());
            if !live.thinking.is_empty() {
                if let Some(l) = live.thinking.lines().last() {
                    lines.push(Line::styled(format!("· {l}"), theme.thinking));
                }
            }
            if !live.text.is_empty() {
                lines.extend(render_markdown(&live.text, width, theme));
            }
            for (_, name) in &live.running_tools {
                lines.push(Line::from(vec![
                    Span::styled("⚙ ".to_string(), theme.tool_name),
                    Span::styled(format!("{name}…"), theme.tool_name),
                ]));
            }
        }

        render_bottom_anchored(&lines, ctx.model.scroll_from_bottom, area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn tabs_expand_to_8_col_stops() {
        assert_eq!(sanitize("1\tuse std;"), "1       use std;");
        assert_eq!(sanitize("12345678\tx"), "12345678        x");
    }

    #[test]
    fn ansi_escapes_and_control_chars_are_stripped() {
        assert_eq!(sanitize("\u{1b}[31mred\u{1b}[0m ok"), "red ok");
        assert_eq!(sanitize("bell\u{7}cr\r"), "bellcr");
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(sanitize("  │ hola único"), "  │ hola único");
    }
}
