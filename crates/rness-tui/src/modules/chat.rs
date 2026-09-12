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

#[derive(Default)]
pub struct Chat {
    /// Lua-rendered cards, host-published. Hit = plugin card; miss =
    /// built-in card. Default cache is empty forever when no Lua host
    /// feeds it — zero magic, zero cost.
    cards: CardCache,
    config: serde_json::Value,
    focused_call: Option<String>,
    expanded: std::collections::HashMap<String, bool>,
    session: Option<String>,
    thinking_expanded: std::collections::HashMap<(usize, usize), bool>,
    card_rows: std::collections::HashMap<String, usize>,
    total_rows: usize,
    viewport_height: usize,
    focused_thinking: Option<(usize, usize)>,
    last_area: Rect,
    entry_cache: Vec<(u64, String, Vec<Line<'static>>)>,
    evicted: std::collections::HashSet<usize>,
    refill: Vec<usize>,
    resident_bytes: std::collections::HashMap<usize, usize>,
    retained_bytes: usize,
    cached_ids: Vec<String>,
    cached_positions: std::collections::HashMap<String, usize>,
    scroll_anchor: Option<(String, usize)>,
    live_scroll_anchor: Option<(Option<usize>, usize)>,
    last_scroll: u16,
    durable_stamp: Option<String>,
    cache_epoch: u64,
    cache_view: String,
    cache_card_revision: u64,
    cache_history_revision: u64,
    cache_interaction: String,
    cached_calls: std::collections::HashMap<String, usize>,
    cached_assistants: std::collections::HashMap<usize, usize>,
    cache_focused_call: Option<String>,
    cache_expanded: std::collections::HashMap<String, bool>,
    cache_focused_thinking: Option<(usize, usize)>,
    cache_thinking_expanded: std::collections::HashMap<(usize, usize), bool>,
    cache_args: std::collections::HashMap<String, serde_json::Value>,
    #[cfg(test)]
    entry_visits: usize,
    row_ends: Vec<usize>,
    cache_theme: Option<crate::theme::Theme>,
    cache_width: u16,
    cache_config: serde_json::Value,
}

/// Mount the module (composition-root seam, same as future Lua installs).
/// Returns the card cache handle the host publishes Lua cards into.
pub fn install(slots: &mut Slots) -> CardCache {
    let cards = CardCache::default();
    slots.mount(MESSAGE_BODY, 0, Box::new(Chat { cards: cards.clone(), config: serde_json::Value::Null, focused_call: None, expanded: Default::default(), session: None, thinking_expanded: Default::default(), card_rows: Default::default(), total_rows: 0, viewport_height: 0, focused_thinking: None, last_area: Rect::default(), ..Default::default() }));
    cards
}

fn entry_fingerprint(entry: &Entry) -> u64 {
    use std::{fmt::Write, hash::Hasher};
    struct Digest(std::collections::hash_map::DefaultHasher);
    impl std::fmt::Write for Digest {
        fn write_str(&mut self, text: &str) -> std::fmt::Result { self.0.write(text.as_bytes()); Ok(()) }
    }
    let mut digest = Digest(Default::default());
    write!(&mut digest, "{entry:?}").expect("hash formatting");
    digest.0.finish()
}

fn row_bytes(rows: &Vec<Line<'_>>) -> usize {
    rows.capacity() * std::mem::size_of::<Line<'_>>() + rows.iter().map(|row| {
        row.spans.capacity() * std::mem::size_of::<ratatui::text::Span<'_>>()
            + row.spans.iter().map(|span| match &span.content { std::borrow::Cow::Owned(text) => text.capacity(), std::borrow::Cow::Borrowed(_) => 0 }).sum::<usize>()
    }).sum::<usize>()
}

const TOOL_CARD_MAX_LINES: usize = 6;

/// Terminal-safe cell text: expand tabs (8-col stops) and drop control
/// chars / ANSI escapes. Raw tool output otherwise desyncs ratatui's
/// cell accounting — overlapping glyphs and stale-frame ghosting.
fn sanitize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut col = 0usize;
    let mut escape = 0u8;
    for ch in line.chars() {
        match escape {
            1 => { escape = match ch { '[' => 2, ']' => 3, _ => 0 }; continue; }
            2 => { if ('\u{40}'..='\u{7e}').contains(&ch) { escape = 0; } continue; }
            3 => { if ch == '\u{7}' { escape = 0; } else if ch == '\u{1b}' { escape = 4; } continue; }
            4 => { escape = if ch == '\\' { 0 } else { 3 }; continue; }
            _ => {}
        }
        match ch {
            '\u{1b}' => escape = 1,
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
                col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
    }
    out
}

fn card_rows(row: crate::modules::tool_cards::CardLine, width: u16, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
    let Some(block) = &row.block else { return vec![card_line(row, width, theme)]; };
    let source_bytes = ["before", "after", "text"].iter().map(|key| block[*key].as_str().map_or(0, str::len)).fold(0usize, usize::saturating_add);
    if source_bytes > 256 * 1024 {
        return vec![Line::styled("Block omitted: snapshot exceeds rendering limit", theme.dim)];
    }
    let language = if block["syntax_highlight"] == false { "" } else { block["language"].as_str().unwrap_or("") };
    let before_colors = crate::core::highlight::highlight_code(block["before"].as_str().unwrap_or(""), language);
    let after_colors = crate::core::highlight::highlight_code(block["after"].as_str().or_else(|| block["text"].as_str()).unwrap_or(""), language);
    let mut result = Vec::new();
    let mut push = |text: &str, number: usize, sign: &str, style: ratatui::style::Style, colored: Option<&Line<'static>>| {
        let highlighted = vec![colored.cloned().unwrap_or_else(|| Line::raw(text.to_owned()))];
        for code in highlighted {
            let gutter = if block["line_numbers"] == false { sign.to_owned() } else { format!("{number:>4} {sign}") };
            let gutter_style = theme.resolve_style(&block["styles"]["gutter"], theme.dim).unwrap_or(theme.dim);
            let mut spans = vec![Span::styled(gutter, gutter_style)];
            spans.extend(code.spans.into_iter().map(|span| {
                let mut combined = style.patch(span.style);
                combined.bg = style.bg;
                span.style(combined)
            }));
            let used = Line::from(spans.clone()).width();
            spans.push(Span::styled(" ".repeat(usize::from(width).saturating_sub(used)), style));
            result.push(Line::from(spans).style(style));
        }
    };
    if block["kind"] == "diff" {
        let before = block["before"].as_str().unwrap_or("");
        let after = block["after"].as_str().unwrap_or("");
        let old_start = block["old_start"].as_u64().unwrap_or(1) as usize;
        let new_start = block["new_start"].as_u64().unwrap_or(1) as usize;
        if before.len().saturating_add(after.len()) > 256 * 1024 {
            return vec![Line::styled("Diff omitted: snapshot exceeds rendering limit", theme.dim)];
        }
        let diff = similar::TextDiff::configure()
            .timeout(std::time::Duration::from_millis(20))
            .diff_lines(before, after);
        let context = block["context_lines"].as_u64().unwrap_or(3).min(1000) as usize;
        let mut added = 0;
        let mut removed = 0;
        for change in diff.iter_all_changes() {
            match change.tag() { similar::ChangeTag::Insert => added += 1, similar::ChangeTag::Delete => removed += 1, _ => {} }
        }
        for (group_index, group) in diff.grouped_ops(context).iter().enumerate() {
            if group_index > 0 { push("…", 0, "  ", theme.dim, None); }
            for op in group {
            for change in diff.iter_changes(op) {
            let (name, sign, fallback, number) = match change.tag() {
                similar::ChangeTag::Delete => ("removed", "- ", theme.removed, old_start + change.old_index().unwrap_or(0)),
                similar::ChangeTag::Insert => ("added", "+ ", theme.added, new_start + change.new_index().unwrap_or(0)),
                similar::ChangeTag::Equal => ("context", "  ", theme.tool_output, new_start + change.new_index().unwrap_or(0)),
            };
            let style = theme.resolve_style(&block["styles"][name], fallback).unwrap_or(fallback);
            let colored = if change.tag() == similar::ChangeTag::Delete {
                before_colors.as_ref().and_then(|lines| change.old_index().and_then(|i| lines.get(i)))
            } else {
                after_colors.as_ref().and_then(|lines| change.new_index().and_then(|i| lines.get(i)))
            };
            push(change.value().trim_end_matches('\n'), number, sign, style, colored);
            if change.missing_newline() && block["fragment"] != true {
                push("\\ No newline at end of file", number, "  ", theme.dim, None);
            }
            }
            }
        }
        if block["summary"] == true {
            result.insert(0, Line::styled(format!("Added {added} lines, removed {removed} lines"), theme.dim));
        }
    } else {
        let start = block["start_line"].as_u64().unwrap_or(1) as usize;
        for (index, text) in block["text"].as_str().unwrap_or("").lines().enumerate() { push(text, start + index, "", theme.code_block, after_colors.as_ref().and_then(|lines| lines.get(index))); }
    }
    result
}

fn card_line(row: crate::modules::tool_cards::CardLine, width: u16, theme: &crate::theme::Theme) -> Line<'static> {
    let base = theme.card_style(&row.style);
    let decode = |span: rness_kernel::presentation::StyledSpan| Span::styled(sanitize(&span.text), theme.resolve_style(&span.style, base).unwrap_or(base));
    let mut left = if row.spans.is_empty() { vec![Span::styled(sanitize(&row.text), base)] } else { row.spans.into_iter().map(decode).collect() };
    let right: Vec<_> = row.right.into_iter().map(decode).collect();
    if !right.is_empty() {
        let right = visual_rows(Line::from(right), usize::from(width), false).remove(0);
        let reserve = right.width();
        let available = usize::from(width).saturating_sub(reserve + 1);
        let clipped = visual_rows(Line::from(left), available, false).remove(0);
        let used = clipped.width();
        let mut clipped = clipped.spans;
        clipped.push(Span::styled(" ".repeat(usize::from(width).saturating_sub(used + reserve)), base));
        clipped.extend(right.spans);
        left = clipped;
    }
    Line::from(left)
}

fn merge_options(base: &serde_json::Value, specific: &serde_json::Value) -> serde_json::Value {
    let mut result = if base.is_object() { base.clone() } else { serde_json::json!({}) };
    if let Some(fields) = specific.as_object() {
        if !result.is_object() { result = serde_json::json!({}); }
        for (key, value) in fields {
            result[key] = if value.is_object() { merge_options(&result[key], value) } else { value.clone() };
        }
    }
    result
}

fn panel_inner_width(options: &serde_json::Value, width: u16) -> u16 {
    let width = if options["border"]["kind"].as_str().unwrap_or("none") != "none" && width >= 2 { width - 2 } else { width };
    let padding = |side: &str| options["padding"][side].as_u64().unwrap_or(0).min(64) as usize;
    let left = padding("left").min(usize::from(width).saturating_sub(1));
    let right = padding("right").min(usize::from(width).saturating_sub(left + 1));
    let marker = options["marker"]["text"].as_str().unwrap_or("");
    let marker_width = Line::raw(marker).width();
    let gutter = if marker.is_empty() || marker_width + 1 >= usize::from(width).saturating_sub(left + right) { 0 } else { marker_width + 1 };
    usize::from(width).saturating_sub(left + right + gutter).max(1) as u16
}

fn panel(content: Vec<Line<'static>>, options: &serde_json::Value, width: u16, theme: &crate::theme::Theme, inherited: ratatui::style::Style) -> Vec<Line<'static>> {
    let kind = options["border"]["kind"].as_str().unwrap_or("none");
    if kind == "none" || width < 2 { return panel_body(content, options, width, theme, inherited); }
    let style = theme.resolve_style(&options["style"], inherited).unwrap_or(inherited);
    let border = theme.resolve_style(&options["border"]["style"], style).unwrap_or(style);
    let (tl, tr, bl, br, h, v) = match kind {
        "rounded" => ("╭", "╮", "╰", "╯", "─", "│"),
        "double" => ("╔", "╗", "╚", "╝", "═", "║"),
        _ => ("┌", "┐", "└", "┘", "─", "│"),
    };
    let mut out = vec![Line::styled(format!("{tl}{}{tr}", h.repeat(usize::from(width - 2))), border)];
    for line in panel_body(content, options, width - 2, theme, style) {
        let line_style = line.style;
        let mut spans = vec![Span::styled(v, border)];
        spans.extend(line.spans.into_iter().map(|span| {
            let combined = line_style.patch(span.style);
            span.style(combined)
        }));
        spans.push(Span::styled(v, border));
        out.push(Line::from(spans));
    }
    out.push(Line::styled(format!("{bl}{}{br}", h.repeat(usize::from(width - 2))), border));
    out
}

fn visual_rows(line: Line<'static>, width: usize, wrap: bool) -> Vec<Line<'static>> {
    visual_rows_limited(line, width, wrap, usize::MAX)
}

fn visual_rows_limited(line: Line<'static>, width: usize, wrap: bool, limit: usize) -> Vec<Line<'static>> {
    if limit == 0 { return Vec::new(); }
    let mut rows = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for span in line.spans {
        // Ratatui's grapheme iterator keeps combining marks and joined glyphs together.
        for grapheme in span.styled_graphemes(line.style) {
            let cells = unicode_width::UnicodeWidthStr::width(grapheme.symbol);
            if used + cells > width {
                if !wrap { rows.push(Line::from(spans)); return rows; }
                if used > 0 {
                    rows.push(Line::from(std::mem::take(&mut spans)));
                    if rows.len() == limit { return rows; }
                    used = 0;
                }
            }
            if cells > width { continue; }
            spans.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
            used += cells;
        }
    }
    rows.push(Line::from(spans));
    rows
}

fn message_rows(content: Vec<Line<'static>>, options: &serde_json::Value, width: u16) -> Vec<Line<'static>> {
    let limit = match options["display"].as_str() {
        Some("collapsed") => 0,
        Some("preview") => options["preview_lines"].as_u64().unwrap_or(3).min(10000) as usize,
        _ => return content,
    };
    let inner = usize::from(panel_inner_width(options, width));
    let mut rows = Vec::new();
    for line in content {
        let remaining = limit.saturating_sub(rows.len());
        if remaining == 0 { break; }
        rows.extend(visual_rows_limited(line, inner, true, remaining));
    }
    rows
}

fn panel_body(mut content: Vec<Line<'static>>, options: &serde_json::Value, width: u16, theme: &crate::theme::Theme, inherited: ratatui::style::Style) -> Vec<Line<'static>> {
    use ratatui::style::Style;
    let style = if options["style"].is_null() { inherited } else { theme.resolve_style(&options["style"], inherited).unwrap_or(inherited) };
    let padding = |side: &str| options["padding"][side].as_u64().unwrap_or(0).min(64) as usize;
    if width == 0 { return Vec::new(); }
    let left = padding("left").min(usize::from(width).saturating_sub(1));
    let right = padding("right").min(usize::from(width).saturating_sub(left + 1));
    let marker = options["marker"]["text"].as_str().unwrap_or("");
    let marker_width = Line::raw(marker).width();
    let gutter = if marker.is_empty() || marker_width + 1 >= usize::from(width).saturating_sub(left + right) { 0 } else { marker_width + 1 };
    let inner = usize::from(width).saturating_sub(left + right + gutter).max(1);
    let marker_style = theme.resolve_style(&options["marker"]["style"], style).unwrap_or(style);
    if let Some(label) = options["label"]["text"].as_str() {
        let label_style = theme.resolve_style(&options["label"]["style"], style).unwrap_or(style);
        content.insert(0, Line::styled(label.to_owned(), label_style));
    }
    let mut output = Vec::new();
    for _ in 0..padding("top") { output.push(Line::styled(" ".repeat(width.into()), style)); }
    let mut row_index = 0;
    for line in content {
        for row in visual_rows(line, inner, true) {
            let row = row.spans.into_iter().map(|span| { let patched = style.patch(span.style); span.style(patched) });
            let show_marker = row_index == 0 || options["marker"]["mode"] == "bar";
            let mut spans = vec![Span::styled(" ".repeat(left), style)];
            if gutter > 0 {
                spans.push(Span::styled(if show_marker { format!("{marker} ") } else { " ".repeat(gutter) }, marker_style));
            }
            spans.extend(row);
            let used = Line::from(spans.clone()).width();
            spans.push(Span::styled(" ".repeat(usize::from(width).saturating_sub(used)), style));
            output.push(Line::from(spans).style(Style::default()));
            row_index += 1;
        }
    }
    for _ in 0..padding("bottom") { output.push(Line::styled(" ".repeat(width.into()), style)); }
    output
}

impl Component for Chat {
    fn bindings(&self) -> Vec<crate::keymaps::ComponentBinding> {
        if !self.config.is_object() { return Vec::new(); }
        [("previous_thinking", "alt+p"), ("next_thinking", "alt+n"), ("toggle_thinking", "alt+t"),
         ("previous_tool", "alt+up"), ("next_tool", "alt+down"), ("toggle_tool", "ctrl+o")]
            .into_iter().filter_map(|(action, default)| {
                let value = &self.config["keys"][action];
                if value == false { return None; }
                crate::keys::Chord::parse(value.as_str().unwrap_or(default))
                    .map(|chord| crate::keymaps::ComponentBinding { action, chord })
            }).collect()
    }

    fn on_key(&mut self, ctx: &Ctx<'_>, key: crossterm::event::KeyEvent) -> crate::component::KeyOutcome {
        let action = self.bindings().into_iter().find(|binding| binding.matches_key(&key)).map(|binding| binding.action);
        action.map_or_else(crate::component::KeyOutcome::pass, |action| self.on_binding(ctx, action))
    }

    fn on_binding(&mut self, ctx: &Ctx<'_>, action: &str) -> crate::component::KeyOutcome {
        use crate::component::KeyOutcome;
        if action == "noop" { return KeyOutcome::consumed(); }
        if !self.config.is_object() { return KeyOutcome::pass(); }
        if self.session.as_ref() != Some(&ctx.model.session) {
            self.session = Some(ctx.model.session.clone());
            self.scroll_anchor = None;
            self.live_scroll_anchor = None;
            self.durable_stamp = None;
            self.entry_cache.clear();
            self.evicted.clear();
            self.resident_bytes.clear();
            self.retained_bytes = 0;
            self.focused_call = None;
            self.expanded.clear();
            self.thinking_expanded.clear();
            self.focused_thinking = None;
            self.card_rows.clear();
        }
        let matched = |name: &str| action == name;
        let previous_thinking = matched("previous_thinking");
        let next_thinking = matched("next_thinking");
        if matched("toggle_thinking") || previous_thinking || next_thinking {
            let mut targets: Vec<_> = ctx.model.entries.iter().enumerate().filter_map(|(entry_index, entry)| {
                match entry { Entry::Assistant { content, .. } => Some((entry_index, content)), _ => None }
            }).enumerate().flat_map(|(ordinal, (entry_index, content))| {
                let assistant_index = ctx.model.assistant_ids.get(&entry_index).copied().unwrap_or(ordinal);
                content.iter().filter(|part| matches!(part, ContentPart::Thinking { .. })).enumerate().map(move |(thinking_index, _)| (assistant_index, thinking_index))
            }).collect();
            if ctx.model.live.as_ref().is_some_and(|live| !live.thinking.is_empty()) {
                targets.push((ctx.model.live_assistant_id.unwrap_or_else(|| ctx.model.next_assistant_id.max(ctx.model.entries.iter().filter(|entry| matches!(entry, Entry::Assistant { .. })).count())), 0));
            }
            if targets.is_empty() || self.config["thinking"]["visible"] == false { return KeyOutcome::pass(); }
            let current = targets.iter().position(|target| Some(*target) == self.focused_thinking);
            let index = match current {
                None => targets.len() - 1,
                Some(i) if previous_thinking => i.saturating_sub(1),
                Some(i) if next_thinking => (i+1).min(targets.len()-1),
                Some(i) => i,
            };
            let target = Some(targets[index]);
            self.focused_thinking = target;
            if !previous_thinking && !next_thinking {
                let expanded = self.thinking_expanded.entry(targets[index]).or_insert(self.config["thinking"]["display"] == "expanded");
                *expanded = !*expanded;
            }
            return KeyOutcome::consumed();
        }
        let previous = matched("previous_tool");
        let next = matched("next_tool");
        let toggle = matched("toggle_tool");
        if !previous && !next && !toggle { return KeyOutcome::pass(); }
        let mut calls: Vec<_> = ctx.model.entries.iter().filter_map(|entry| match entry {
            Entry::ToolResult { call, name, is_error, .. } => Some((call, name, *is_error)), _ => None,
        }).collect();
        if let Some(live) = &ctx.model.live {
            calls.extend(live.running_tools.iter().map(|(call, name)| (call, name, false)));
        }
        if calls.is_empty() { return KeyOutcome::pass(); }
        let current = calls.iter().position(|(call, _, _)| self.focused_call.as_ref() == Some(call));
        let index = match current {
            None => calls.len() - 1,
            Some(i) if previous => i.saturating_sub(1),
            Some(i) if next => (i + 1).min(calls.len() - 1),
            Some(i) => i,
        };
        let (call, name, is_error) = calls[index];
        self.focused_call = Some(call.clone());
        if toggle {
            let options = merge_options(&self.config["tool"], &self.config["tools"][name]);
            let options = merge_options(&options, &options["states"][if ctx.model.cancelled_tools.contains(call) { "cancelled" } else if is_error { "error" } else { "success" }]);
            let default = options["display"] == "expanded";
            let expanded = self.expanded.entry(call.clone()).or_insert(default);
            *expanded = !*expanded;
        }
        if let Some(row) = self.card_rows.get(call) {
            let target = if toggle {
                let mut probe = Chat {
                    cards:self.cards.clone(), config:self.config.clone(), focused_call:self.focused_call.clone(),
                    expanded:self.expanded.clone(), session:self.session.clone(), thinking_expanded:self.thinking_expanded.clone(),
                    card_rows:Default::default(), total_rows:0, viewport_height:0, focused_thinking:self.focused_thinking, last_area:self.last_area, ..Default::default()
                };
                // Measure changed layout before choosing an offset; old row counts
                // cannot anchor a card whose expanded height just changed.
                let area = self.last_area;
                let mut buffer = Buffer::empty(area);
                probe.render(ctx, area, &mut buffer);
                probe.total_rows.saturating_sub(probe.card_rows.get(call).copied().unwrap_or(*row).saturating_add(self.viewport_height))
            } else { self.total_rows.saturating_sub(row.saturating_add(self.viewport_height)) };
            let target = target.min(u16::MAX as usize) as u16;
            let current = ctx.model.scroll_from_bottom;
            let action = if target >= current { crate::app::Action::ScrollUp(target-current) } else { crate::app::Action::ScrollDown(current-target) };
            return KeyOutcome::act(vec![action]);
        }
        KeyOutcome::consumed()
    }

    fn on_action(&mut self, _ctx: &Ctx<'_>, name: &str, payload: &serde_json::Value) {
        if name == "chat:messagebox-config" { self.config = payload.clone(); }
    }

    fn name(&self) -> &str {
        "chat"
    }

    fn height(&self, _ctx: &Ctx<'_>, _width: u16) -> Option<u16> {
        None // fill
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        if self.session.as_ref() != Some(&ctx.model.session) {
            self.session = Some(ctx.model.session.clone());
            self.scroll_anchor = None;
            self.live_scroll_anchor = None;
            self.durable_stamp = None;
            self.entry_cache.clear();
            self.evicted.clear();
            self.resident_bytes.clear();
            self.retained_bytes = 0;
            self.focused_call = None;
            self.expanded.clear();
            self.thinking_expanded.clear();
            self.focused_thinking = None;
            self.card_rows.clear();
        }
        self.last_area = area;
        let mut render_area = area;
        if self.config.is_object() {
            use ratatui::widgets::{Block, BorderType, Borders, Widget};
            let style = ctx.theme.resolve_style(&self.config["style"], ratatui::style::Style::default()).unwrap_or_default();
            buf.set_style(area, style);
            let kind = self.config["border"]["kind"].as_str().unwrap_or("none");
            if kind != "none" {
                let border_style = ctx.theme.resolve_style(&self.config["border"]["style"], style).unwrap_or(style);
                let block = Block::default().borders(Borders::ALL).border_style(border_style).border_type(match kind { "rounded" => BorderType::Rounded, "double" => BorderType::Double, _ => BorderType::Plain });
                render_area = block.inner(area);
                block.render(area, buf);
            }
            let pad = |side: &str| self.config["padding"][side].as_u64().unwrap_or(0).min(64) as u16;
            let left = pad("left").min(render_area.width);
            let top = pad("top").min(render_area.height);
            render_area.x += left; render_area.y += top;
            render_area.width = render_area.width.saturating_sub(left).saturating_sub(pad("right"));
            render_area.height = render_area.height.saturating_sub(top).saturating_sub(pad("bottom"));
        }
        let area = render_area;
        let width = area.width;
        let theme = ctx.theme;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let configured = self.config.as_object().is_some_and(|o| !o.is_empty());
        let background = theme.resolve_style(&self.config["style"], ratatui::style::Style::default()).unwrap_or_default();
        if configured { buf.set_style(area, background); }

        let card_revision = self.cards.revision();
        let stamp = format!("{}:{}:{}:{}:{:?}:{:?}:{:?}:{:?}", ctx.model.session, ctx.model.history_revision, ctx.model.entries.len(), card_revision, self.focused_call, self.expanded, self.focused_thinking, self.thinking_expanded);
        let view = format!("{}:{:?}:{:?}:{:?}:{:?}", self.cards.revision(), self.focused_call, self.expanded, self.focused_thinking, self.thinking_expanded);
        let append_only = self.durable_stamp.is_some() && ctx.model.history_epoch != 0
            && self.cache_epoch == ctx.model.history_epoch && self.cache_view == view
            && self.entry_cache.len() == self.cached_ids.len()
            && self.entry_cache.len() <= ctx.model.entry_ids.len()
            && self.cache_width == width && self.cache_theme.as_ref() == Some(theme) && self.cache_config == self.config;
        let unchanged = self.refill.is_empty() && ctx.model.history_revision != 0
            && self.cache_epoch == ctx.model.history_epoch
            && self.durable_stamp.as_ref() == Some(&stamp)
            && self.cache_width == width && self.cache_theme.as_ref() == Some(theme) && self.cache_config == self.config;
        let interaction = format!("{:?}:{:?}:{:?}:{:?}", self.focused_call, self.expanded, self.focused_thinking, self.thinking_expanded);
        let selective = self.durable_stamp.is_some() && ctx.model.history_revision != 0
            && self.cache_history_revision == ctx.model.history_revision
            && self.cache_epoch == ctx.model.history_epoch
            && self.entry_cache.len() == ctx.model.entries.len()
            && self.cache_width == width
            && self.cache_theme.as_ref() == Some(theme) && self.cache_config == self.config;
        let dirty = if selective { self.cards.changes_since(self.cache_card_revision) } else { None };
        let mut durable_rows = self.row_ends.last().copied().unwrap_or(0);
        let mut assistant_count = ctx.model.next_assistant_id;
        if !unchanged {
        let partial = dirty.is_some();
        let start_entry = if append_only || partial { self.entry_cache.len() } else { 0 };
        if !append_only && !partial { self.cache_args.clear(); }
        for entry in &ctx.model.entries[start_entry..] {
            if let Entry::Assistant { content, .. } = entry {
                for part in content {
                    if let ContentPart::ToolUse { call, args, .. } = part { self.cache_args.insert(call.clone(), args.clone()); }
                }
            }
        }
        let tool_args = &self.cache_args;
        if self.cache_width != width || self.cache_theme.as_ref() != Some(theme) || self.cache_config != self.config {
            self.entry_cache.clear();
            self.evicted.clear();
            self.resident_bytes.clear();
            self.retained_bytes = 0;
            self.cache_width = width;
            self.cache_theme = Some(theme.clone());
            self.cache_config = self.config.clone();
        }
        if append_only {
            for (index, id) in ctx.model.entry_ids.iter().enumerate().skip(start_entry) {
                self.cached_positions.insert(id.clone(), index);
            }
            self.cached_ids.extend_from_slice(&ctx.model.entry_ids[start_entry..]);
        } else if self.cached_ids != ctx.model.entry_ids {
            // Reordered slots cannot retain eviction indexes from the old order.
            if !self.evicted.is_empty() { self.entry_cache.clear(); self.evicted.clear(); }
            let old = std::mem::take(&mut self.entry_cache);
            let mut by_id: std::collections::HashMap<_, _> = self.cached_ids.iter().cloned().zip(old).collect();
            self.entry_cache = ctx.model.entry_ids.iter().enumerate().map(|(index, id)| {
                by_id.remove(id).unwrap_or_else(|| (entry_fingerprint(&ctx.model.entries[index]), String::new(), Vec::new()))
            }).collect();
            self.cached_ids = ctx.model.entry_ids.clone();
        }
        if !append_only && !partial {
            self.cached_positions = self.cached_ids.iter().enumerate().map(|(i,id)| (id.clone(),i)).collect();
            self.cached_calls.clear();
            self.cached_assistants.clear();
        }
        self.entry_cache.truncate(ctx.model.entries.len());
        if !append_only && !partial {
            self.resident_bytes.clear();
            self.retained_bytes = 0;
            self.row_ends.clear();
            self.card_rows.clear();
            durable_rows = 0;
        }
        assistant_count = 0;
        let mut indices: Vec<usize> = if let Some(calls) = dirty {
            let mut indices: Vec<_> = calls.iter().filter_map(|call| self.cached_calls.get(call).copied()).collect();
            for call in self.expanded.keys().chain(self.cache_expanded.keys()) {
                if self.expanded.get(call) != self.cache_expanded.get(call) {
                    indices.extend(self.cached_calls.get(call).copied());
                }
            }
            if self.focused_call != self.cache_focused_call {
                for call in self.focused_call.iter().chain(self.cache_focused_call.iter()) { indices.extend(self.cached_calls.get(call).copied()); }
            }
            for key in self.thinking_expanded.keys().chain(self.cache_thinking_expanded.keys()) {
                if self.thinking_expanded.get(key) != self.cache_thinking_expanded.get(key) { indices.extend(self.cached_assistants.get(&key.0).copied()); }
            }
            if self.focused_thinking != self.cache_focused_thinking {
                for key in self.focused_thinking.iter().chain(self.cache_focused_thinking.iter()) { indices.extend(self.cached_assistants.get(&key.0).copied()); }
            }
            indices
        } else { (start_entry..ctx.model.entries.len()).collect() };
        indices.append(&mut self.refill);
        indices.sort_unstable();
        indices.dedup();
        for entry_index in indices {
            let entry = &ctx.model.entries[entry_index];
            let old_height = if partial { self.row_ends[entry_index] - entry_index.checked_sub(1).map_or(0, |i| self.row_ends[i]) } else { 0 };
            if partial { durable_rows = entry_index.checked_sub(1).map_or(0, |i| self.row_ends[i]); }
            #[cfg(test)]
            { self.entry_visits += 1; }
            let assistant_index = ctx.model.assistant_ids.get(&entry_index).copied().unwrap_or(assistant_count);
            if matches!(entry, Entry::Assistant { .. }) {
                self.cached_assistants.insert(assistant_index, entry_index);
                assistant_count += 1;
            }
            let state = match entry {
                Entry::ToolResult {call, ..} => format!("{:?}:{:?}:{}:{:?}:{:?}", self.expanded.get(call), self.focused_call.as_ref() == Some(call), self.cards.call_revision(call), ctx.model.tool_durations.get(call), tool_args.get(call)),
                Entry::Assistant {..} => {
                    let mut expanded:Vec<_> = self.thinking_expanded.iter().filter(|(key, _)| key.0 == assistant_index).collect();
                    expanded.sort_by_key(|(key, _)| **key);
                    format!("{}:{:?}:{:?}", assistant_index, self.focused_thinking.filter(|key| key.0 == assistant_index), expanded)
                },
                _ => format!("{}", ctx.model.error_entries.contains(&entry_index)),
            };
            if let Entry::ToolResult {call, ..} = entry {
                self.cached_calls.insert(call.clone(), entry_index);
                self.card_rows.insert(call.clone(), durable_rows);
            }
            let fingerprint = entry_fingerprint(entry);
            let hit = !self.evicted.remove(&entry_index) && self.entry_cache.get(entry_index).is_some_and(|(cached, key, _)| *cached == fingerprint && key == &state);
            if !hit {
            lines.clear();
            (|| {
            let mut thinking_index = 0;
            if configured {
                let role = match entry { Entry::User { .. } => Some("user"), Entry::Assistant { .. } => Some("assistant"), Entry::Notice(_) => Some(if ctx.model.error_entries.contains(&entry_index) { "error" } else { "notice" }), _ => None };
                if let Some(role) = role {
                    let defaults = if role == "user" { serde_json::json!({"marker":{"text":">","mode":"first_line","style":"user_prefix"}}) } else { serde_json::json!({}) };
                    let options = merge_options(&merge_options(&defaults, &self.config["message"]), &self.config[role]);
                    if options["visible"] == false { return; }
                    let inner_width = panel_inner_width(&options, width);
                    let mut body = Vec::new();
                    match entry {
                        Entry::User { content } | Entry::Assistant { content, .. } => for part in content {
                            match part {
                                ContentPart::Text { text } if role == "assistant" => body.extend(crate::core::render::render_markdown_configured(text, inner_width, theme, &options["markdown"])),
                                ContentPart::Text { text } => body.extend(text.lines().map(|s| Line::raw(sanitize(s)))),
                                ContentPart::Thinking { text, .. } if self.config["thinking"]["visible"] != false => {
                                    let mut options = merge_options(&serde_json::json!({"label":{"text":"Thinking"},"style":"thinking"}), &self.config["thinking"]);
                                    let key = (assistant_index, thinking_index);
                                    thinking_index += 1;
                                    if self.focused_thinking == Some(key) {
                                        options["marker"] = serde_json::json!({"text":">","mode":"first_line","style":"title"});
                                    }
                                    let expanded = self.thinking_expanded.get(&key).copied().unwrap_or(options["display"] == "expanded");
                                    let limit = if expanded { usize::MAX } else if options["display"] == "collapsed" { 0 } else { options["preview_lines"].as_u64().unwrap_or(3) as usize };
                                    let thinking_width = panel_inner_width(&options, inner_width);
                                    let rows = text.lines().flat_map(|s| visual_rows_limited(Line::raw(sanitize(s)), usize::from(thinking_width), true, limit)).take(limit).collect();
                                    body.extend(panel(rows, &options, inner_width, theme, background));
                                },
                                ContentPart::Image { attachment } => body.push(Line::raw(format!("[Image · {} × {} · {} bytes]", attachment.width, attachment.height, attachment.bytes))),
                                ContentPart::ToolUse { .. } => {},
                                _ => {},
                            }
                        },
                        Entry::Notice(text) => body.extend(text.lines().map(|s| Line::raw(sanitize(s)))),
                        _ => {},
                    }
                    let inherited = background.patch(match role { "user" => theme.user_message, "error" => theme.error, "notice" => theme.dim, _ => theme.assistant_text });
                    if !body.is_empty() {
                        lines.extend(panel(message_rows(body, &options, width), &options, width, theme, inherited));
                        for _ in 0..self.config["spacing"].as_u64().unwrap_or(1).min(64) { lines.push(Line::styled(" ".repeat(width.into()), background)); }
                    }
                    return;
                }
            }
            match entry {
                Entry::User { content } => {
                    lines.push(Line::default());
                    for part in content {
                        if let ContentPart::Image { attachment } = part {
                            lines.push(Line::raw(format!("[Image · {} × {} · {} bytes]", attachment.width, attachment.height, attachment.bytes)));
                        }
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
                            ContentPart::Image { attachment } => {
                                lines.push(Line::raw(format!("[Image · {} × {} · {} bytes]", attachment.width, attachment.height, attachment.bytes)));
                            }
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
                    if configured {
                        self.card_rows.insert(call.clone(), durable_rows);
                        let options = merge_options(&self.config["tool"], &self.config["tools"][name]);
                        let mut options = merge_options(&options, &options["states"][if ctx.model.cancelled_tools.contains(call) { "cancelled" } else if *is_error { "error" } else { "success" }]);
                        if options["visible"] == false { return; }
                        if let Some(expanded) = self.expanded.get(call) {
                            options["display"] = serde_json::json!(if *expanded { "expanded" } else { "collapsed" });
                        }
                        if self.focused_call.as_ref() == Some(call) {
                            options["marker"] = serde_json::json!({"text":">","mode":"first_line","style":"title"});
                        }
                        let inherited = background.patch(if *is_error { theme.error } else { theme.tool_output });
                        let header_rows = if let Some(card) = self.cards.get(call) {
                            usize::from(card.first().is_some_and(|row| row.is_header || !row.structured))
                        } else { usize::from(options["header"]["visible"] != false) };
                        let limit = match options["display"].as_str().unwrap_or("preview") {
                            "collapsed" => header_rows,
                            "expanded" => usize::MAX,
                            _ => options["preview_lines"].as_u64().unwrap_or(6).min(10000) as usize + header_rows,
                        };
                        let mut body = if let Some(card) = self.cards.get(call) {
                            let inner = panel_inner_width(&options, width);
                            let row_limit = if options["display"] == "collapsed" { 1 } else { usize::MAX };
                            let mut rows = Vec::new();
                            for row in card.iter().take(row_limit) {
                                let rendered = card_rows(row.clone(), inner, theme);
                                if row.is_header {
                                    rows.extend(rendered.into_iter().map(|line| visual_rows(line, usize::from(inner), false).remove(0)));
                                } else { rows.extend(rendered); }
                                if rows.len() > limit { break; }
                            }
                            rows
                        } else {
                            let mut rows = Vec::new();
                            if options["header"]["visible"] != false {
                                let mut title = if options["header"]["show_name"] == false { String::new() } else { name.clone() };
                                if options["header"]["show_status"] == true { title.push_str(if ctx.model.cancelled_tools.contains(call) { " · cancelled" } else if *is_error { " · error" } else { " · done" }); }
                                if options["header"]["show_duration"] == true {
                                    if let Some(ms) = ctx.model.tool_durations.get(call) { title.push_str(&format!(" · {ms} ms")); }
                                }
                                let style = theme.resolve_style(&options["header"]["style"], theme.tool_name).unwrap_or(theme.tool_name);
                                rows.push(Line::styled(title, style));
                            }
                            if options["arguments"]["visible"] == true {
                                if let Some(args) = tool_args.get(call) {
                                    let style = theme.resolve_style(&options["arguments"]["style"], theme.dim).unwrap_or(theme.dim);
                                    rows.extend(visual_rows(Line::styled(sanitize(&args.to_string()), style), usize::from(panel_inner_width(&options, width)), options["arguments"]["wrap"] != false));
                                }
                            }
                            if options["output"]["visible"] != false {
                                let style = theme.resolve_style(&options["output"]["style"], inherited).unwrap_or(inherited);
                                rows.extend(output.lines().take(limit.saturating_add(1)).map(|s| Line::styled(sanitize(s), style)));
                            }
                            rows
                        };
                        let inner = usize::from(panel_inner_width(&options, width));
                        let mut laid_out = Vec::new();
                        for (index, row) in body.into_iter().enumerate() {
                            let remaining = limit.saturating_add(1).saturating_sub(laid_out.len());
                            if remaining == 0 { break; }
                            laid_out.extend(visual_rows_limited(row, inner, index < header_rows || options["output"]["wrap"] != false, remaining));
                        }
                        body = laid_out;
                        if body.len() > limit {
                            body.truncate(limit);
                            if limit > 1 { body.push(Line::styled("… more lines", theme.dim)); }
                        }
                        lines.extend(panel(body, &options, width, theme, inherited));
                        for _ in 0..options["spacing"].as_u64().unwrap_or(0).min(64) {
                            lines.push(Line::styled(" ".repeat(width.into()), background));
                        }
                        return;
                    }
                    // Lua card (host-published) shadows the built-in.
                    // The card owns its lines verbatim — no forced
                    // indent, no header; plugins format everything.
                    if let Some(card) = self.cards.get(call) {
                        for row in card.iter().cloned() {
                            lines.extend(card_rows(row, width, theme));
                        }
                        return;
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
                    for line in text.lines() {
                        for wrapped in textwrap::wrap(&sanitize(line), usize::from(width.max(1))) {
                            lines.push(Line::styled(wrapped.into_owned(), theme.dim));
                        }
                    }
                }
            }
            })();
            let cached = (fingerprint, state, std::mem::take(&mut lines));
            if entry_index < self.entry_cache.len() { self.entry_cache[entry_index] = cached; }
            else { self.entry_cache.push(cached); }
            }
            let bytes = row_bytes(&self.entry_cache[entry_index].2);
            self.retained_bytes -= self.resident_bytes.remove(&entry_index).unwrap_or(0);
            if bytes != 0 { self.resident_bytes.insert(entry_index, bytes); }
            self.retained_bytes += bytes;
            durable_rows += self.entry_cache[entry_index].2.len();
            if partial {
                let delta = self.entry_cache[entry_index].2.len() as isize - old_height as isize;
                if delta != 0 {
                    for end in &mut self.row_ends[entry_index..] { *end = end.saturating_add_signed(delta); }
                    for (call, index) in &self.cached_calls {
                        if *index > entry_index {
                            if let Some(row) = self.card_rows.get_mut(call) { *row = row.saturating_add_signed(delta); }
                        }
                    }
                }
            } else { self.row_ends.push(durable_rows); }
        }
        durable_rows = self.row_ends.last().copied().unwrap_or(0);
        self.cache_card_revision = card_revision;
        self.cache_history_revision = ctx.model.history_revision;
        self.cache_interaction = interaction;
        self.cache_focused_call = self.focused_call.clone();
        self.cache_expanded = self.expanded.clone();
        self.cache_focused_thinking = self.focused_thinking;
        self.cache_thinking_expanded = self.thinking_expanded.clone();
        self.cache_epoch = ctx.model.history_epoch;
        self.cache_view = view;
        self.durable_stamp = Some(stamp);
        }
        lines.clear();

        // Live stream below the durable entries.
        if let Some(live) = &ctx.model.live {
            lines.push(Line::default());
            if !live.thinking.is_empty() && self.config["thinking"]["visible"] != false {
                if configured {
                    let mut options = merge_options(&serde_json::json!({"label":{"text":"Thinking"},"style":"thinking"}), &self.config["thinking"]);
                    let key = (ctx.model.live_assistant_id.unwrap_or(ctx.model.next_assistant_id.max(assistant_count)), 0);
                    if self.focused_thinking == Some(key) { options["marker"] = serde_json::json!({"text":">","mode":"first_line","style":"title"}); }
                    let expanded = self.thinking_expanded.get(&key).copied().unwrap_or(options["display"] == "expanded");
                    let limit = if expanded { usize::MAX } else if options["display"] == "collapsed" { 0 } else { options["preview_lines"].as_u64().unwrap_or(3) as usize };
                    let inner = panel_inner_width(&options, width);
                    let rows = live.thinking.lines().flat_map(|s| visual_rows_limited(Line::raw(sanitize(s)), usize::from(inner), true, limit)).take(limit).collect();
                    lines.extend(panel(rows, &options, width, theme, background));
                } else if let Some(l) = live.thinking.lines().last() {
                    lines.push(Line::styled(format!("· {l}"), theme.thinking));
                }
            }
            if !live.text.is_empty() {
                let options = merge_options(&self.config["message"], &self.config["assistant"]);
                let body = crate::core::render::render_markdown_configured(&live.text, if configured { panel_inner_width(&options, width) } else { width }, theme, &options["markdown"]);
                if configured {
                    if options["visible"] != false {
                        lines.extend(panel(message_rows(body, &options, width), &options, width, theme, background.patch(theme.assistant_text)));
                    }
                } else { lines.extend(body); }
            }
            for (call, name) in &live.running_tools {
                self.card_rows.insert(call.clone(), durable_rows + lines.len());
                if configured {
                    let options = merge_options(&self.config["tool"], &self.config["tools"][name]);
                    let mut options = merge_options(&options, &options["states"]["running"]);
                    if options["visible"] == false { continue; }
                    if let Some(expanded) = self.expanded.get(call) {
                        options["display"] = serde_json::json!(if *expanded { "expanded" } else { "collapsed" });
                    }
                    if let Some(card) = self.cards.get(call) {
                        let inner = panel_inner_width(&options, width);
                        let limit = match options["display"].as_str() {
                            Some("expanded") => usize::MAX,
                            Some("collapsed") => 1,
                            _ => options["preview_lines"].as_u64().unwrap_or(8) as usize + 1,
                        };
                        let body = card.iter().take(limit).flat_map(|row| card_rows(row.clone(), inner, theme)).collect();
                        lines.extend(panel(body, &options, width, theme, background));
                        continue;
                    }
                    if options["header"]["visible"] != false {
                        let mut title = if options["header"]["show_name"] == false { String::new() } else { name.clone() };
                        if options["header"]["show_status"] == true { title.push_str(" · running"); }
                        let style = theme.resolve_style(&options["header"]["style"], theme.tool_name).unwrap_or(theme.tool_name);
                        lines.extend(panel(vec![Line::styled(title, style)], &options, width, theme, background));
                    }
                } else {
                    lines.push(Line::styled(format!("{name}…"), theme.tool_name));
                }
            }
        }

        self.total_rows = durable_rows + lines.len();
        self.viewport_height = usize::from(area.height);
        let height = self.viewport_height;
        let offset = usize::from(ctx.model.scroll_from_bottom).min(self.total_rows.saturating_sub(height));
        let mut end = self.total_rows - offset;
        let mut start = end.saturating_sub(height);
        if ctx.model.scroll_from_bottom != 0 && self.last_scroll != 0 {
            if let Some((id, row)) = &self.scroll_anchor {
                if let Some(&index) = self.cached_positions.get(id) {
                    let base = if index == 0 { 0 } else { self.row_ends[index - 1] };
                    let anchored = base + row.min(&(self.row_ends[index] - base).saturating_sub(1));
                    let delta = self.last_scroll as isize - ctx.model.scroll_from_bottom as isize;
                    start = anchored.saturating_add_signed(delta).min(self.total_rows.saturating_sub(height));
                    end = (start + height).min(self.total_rows);
                }
            }
            if let Some((identity, row)) = self.live_scroll_anchor {
                let base = if ctx.model.live.is_some() && identity == ctx.model.live_assistant_id {
                    Some(durable_rows)
                } else {
                    identity.and_then(|id| ctx.model.assistant_ids.iter()
                        .find_map(|(index, value)| (*value == id).then_some(*index)))
                        .map(|index| if index == 0 { 0 } else { self.row_ends[index - 1] })
                };
                if let Some(base) = base {
                    let delta = self.last_scroll as isize - ctx.model.scroll_from_bottom as isize;
                    start = (base + row).saturating_add_signed(delta).min(self.total_rows.saturating_sub(height));
                    end = (start + height).min(self.total_rows);
                }
            }
        }
        self.live_scroll_anchor = if ctx.model.scroll_from_bottom != 0 && start >= durable_rows && ctx.model.live.is_some() {
            Some((ctx.model.live_assistant_id, start - durable_rows))
        } else { None };
        self.last_scroll = ctx.model.scroll_from_bottom;
        let mut visible = Vec::with_capacity(height);
        let first = self.row_ends.partition_point(|end| *end <= start);
        self.scroll_anchor = if ctx.model.scroll_from_bottom != 0 {
            self.cached_ids.get(first).map(|id| (id.clone(), start.saturating_sub(if first == 0 { 0 } else { self.row_ends[first - 1] })))
        } else { None };
        let last = self.row_ends.partition_point(|row| *row < end).saturating_add(1).min(self.entry_cache.len());
        self.refill = (first..last).filter(|i| self.evicted.contains(i)).collect();
        if !self.refill.is_empty() {
            self.render(ctx, self.last_area, buf);
            return;
        }
        for index in first..self.entry_cache.len() {
            let base = if index == 0 { 0 } else { self.row_ends[index - 1] };
            if base >= end { break; }
            let rows = &self.entry_cache[index].2;
            visible.extend(rows[start.saturating_sub(base).min(rows.len())..end.saturating_sub(base).min(rows.len())].iter().cloned());
        }
        if end > durable_rows {
            visible.extend(lines[start.saturating_sub(durable_rows)..end - durable_rows].iter().cloned());
        }
        render_bottom_anchored(&visible, 0, area, buf);
        let budget = self.config["cache_bytes"].as_u64().unwrap_or(8 * 1024 * 1024) as usize;
        if self.retained_bytes > budget {
            let mut candidates: Vec<_> = self.resident_bytes.keys().copied().collect();
            candidates.sort_unstable_by_key(|i| std::cmp::Reverse(if *i < first { first - *i } else { i.saturating_sub(last.saturating_sub(1)) }));
            for index in candidates {
                if self.retained_bytes <= budget { break; }
                self.retained_bytes -= self.resident_bytes.remove(&index).unwrap_or(0);
                self.entry_cache[index].2 = Vec::new();
                self.evicted.insert(index);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn bounded_rows_match_unbounded_after_eviction_scroll_and_resize() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("bounded".into(), "fake".into());
        model.history_epoch = 1;
        model.history_revision = 1;
        for i in 0..150 {
            model.entries.push(Entry::Notice(format!("entry {i}: {}", "é 漢字 ".repeat(if i == 70 { 3000 } else { 8 }))));
            model.entry_ids.push(format!("id-{i}"));
        }
        let theme = Theme::default();
        for budget in [0, 1024, 8192] {
            let mut bounded = Chat {config:serde_json::json!({"cache_bytes":budget}), ..Default::default()};
            let mut reference = Chat {config:serde_json::json!({"cache_bytes":1_000_000_000}), ..Default::default()};
            for width in [40, 60, 20] {
                for offset in [0, 40, 1000, 50, 0, 400] {
                    model.scroll_from_bottom = offset;
                    let area = Rect::new(0,0,width,12);
                    let mut actual = Buffer::empty(area);
                    let mut expected = Buffer::empty(area);
                    bounded.render(&Ctx {model:&model,theme:&theme},area,&mut actual);
                    reference.render(&Ctx {model:&model,theme:&theme},area,&mut expected);
                    assert_eq!(actual, expected, "budget={budget}, width={width}, offset={offset}");
                    assert!(bounded.entry_cache.iter().map(|entry| row_bytes(&entry.2)).sum::<usize>() <= budget);
                    assert_eq!(bounded.row_ends, reference.row_ends);
                }
            }
        }
    }

    #[test]
    fn configured_terminal_states_visibility_and_modes_match_fresh_render() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("matrix".into(), "fake".into());
        model.history_revision = 1;
        model.history_epoch = 1;
        model.entries.push(Entry::ToolResult {call:"call".into(), name:"Read".into(), output:"first\nsecond\nthird".into(), is_error:false});
        model.entry_ids.push("event".into());
        let theme = Theme::default();
        let area = Rect::new(0,0,40,10);
        for state in ["success", "error", "cancelled"] {
            if let Entry::ToolResult {is_error, ..} = &mut model.entries[0] { *is_error = state != "success"; }
            model.cancelled_tools.clear();
            if state == "cancelled" { model.cancelled_tools.insert("call".into()); }
            for mode in ["collapsed", "preview", "expanded"] {
                for visible in [false, true] {
                    let config = serde_json::json!({"tool":{"display":mode,"preview_lines":1,"header":{"show_status":true},"states":{state:{"visible":visible,"style":{"bg":"#123456"}}}}});
                    let mut chat = Chat {config, ..Default::default()};
                    let mut buf = Buffer::empty(area);
                    chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
                    let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
                    assert_eq!(text.contains("Read"), visible, "{state}/{mode}");
                    assert_eq!(text.contains("first"), visible && mode != "collapsed", "{state}/{mode}");
                    assert_eq!(text.contains("third"), visible && mode == "expanded", "{state}/{mode}");
                    if visible { assert!(buf.content.iter().any(|cell| cell.bg == ratatui::style::Color::Rgb(0x12,0x34,0x56))); }
                }
            }
        }
    }

    #[test]
    fn card_publication_visits_only_changed_entry() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("cards".into(), "fake".into());
        model.history_epoch = 1;
        model.history_revision = 1;
        for i in 0..1000 {
            model.entries.push(Entry::ToolResult { call: format!("call-{i}"), name: "Read".into(), output: "old".into(), is_error: false });
            model.entry_ids.push(format!("event-{i}"));
        }
        let mut chat = Chat::default();
        let theme = Theme::default();
        let area = Rect::new(0,0,40,10);
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        for text in ["new\nextra\nrows", "short"] {
            let before = chat.entry_visits;
            chat.cards.insert("call-2".into(), vec![crate::modules::tool_cards::CardLine {text:text.into(), ..Default::default()}]);
            buf.reset();
            chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
            assert_eq!(chat.entry_visits, before + 1);
            let mut fresh = Chat {cards:chat.cards.clone(), ..Default::default()};
            let mut expected = Buffer::empty(area);
            fresh.render(&Ctx {model:&model,theme:&theme},area,&mut expected);
            assert_eq!(buf, expected);
            assert_eq!(chat.row_ends, fresh.row_ends);
            assert_eq!(chat.card_rows, fresh.card_rows);
        }
        let before = chat.entry_visits;
        chat.focused_call = Some("call-2".into());
        chat.expanded.insert("call-2".into(), true);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.entry_visits, before + 1);
        chat.cards.invalidate();
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        let mut fresh = Chat::default();
        fresh.render(&Ctx {model:&model,theme:&theme},area,&mut Buffer::empty(area));
        assert_eq!(chat.row_ends, fresh.row_ends);
    }

    #[test]
    fn appended_history_skips_cached_prefix() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("append".into(), "fake".into());
        model.history_epoch = 1;
        model.history_revision = 1;
        for i in 0..1000 {
            model.entries.push(Entry::Notice(format!("entry {i}")));
            model.entry_ids.push(format!("id-{i}"));
        }
        let theme = Theme::default();
        let area = Rect::new(0,0,40,10);
        let mut chat = Chat::default();
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.entry_visits, 1000);
        model.entries.push(Entry::Notice("new".into()));
        model.entry_ids.push("new-id".into());
        model.history_revision += 1;
        buf.reset();
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.entry_visits, 1001);
        let mut fresh = Chat::default();
        let mut expected = Buffer::empty(area);
        fresh.render(&Ctx {model:&model,theme:&theme},area,&mut expected);
        assert_eq!(buf, expected);
        model.entries.remove(0);
        model.entry_ids.remove(0);
        model.history_epoch += 1;
        model.history_revision += 1;
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.entry_visits, 2001);
    }

    #[test]
    fn live_scroll_stays_at_same_row_when_stream_grows() {
        use super::*;
        use crate::{app::{Model, LiveStep}, theme::Theme};
        let mut model = Model::new("live".into(), "fake".into());
        model.live_assistant_id = Some(1);
        model.live = Some(LiveStep { text: (0..100).map(|i| format!("row {i}\n\n")).collect(), ..Default::default() });
        model.scroll_from_bottom = 20;
        let theme = Theme::default();
        let area = Rect::new(0,0,40,10);
        let mut chat = Chat::default();
        let mut before = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut before);
        model.live.as_mut().unwrap().text.push_str(&"more text\n\n".repeat(100));
        let mut after = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut after);
        assert_eq!(before, after);
        model.scroll_from_bottom = 0;
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut after);
        assert_ne!(before, after);
    }

    #[test]
    fn unchanged_revision_skips_durable_rebuild_during_streaming() {
        use super::*;
        use crate::{app::{Model, LiveStep}, theme::Theme};
        let mut model = Model::new("fast".into(), "fake".into());
        model.history_revision = 1;
        model.entries.push(Entry::Notice("durable".into()));
        model.entry_ids.push("event".into());
        let mut chat = Chat::default();
        let theme = Theme::default();
        let area = Rect::new(0,0,40,10);
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        let ptr = chat.entry_cache[0].2.as_ptr();
        model.live = Some(LiveStep::default());
        for _ in 0..100 {
            model.live.as_mut().unwrap().text.push_str("word ");
            chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
            assert_eq!(ptr, chat.entry_cache[0].2.as_ptr());
        }
        model.entries[0] = Entry::Notice("updated".into());
        model.history_revision += 1;
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.entry_cache[0].0, entry_fingerprint(&model.entries[0]));
    }

    #[test]
    fn scrolled_view_retains_anchor_when_history_grows() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("anchor".into(), "fake".into());
        for i in 0..20 {
            model.entries.push(Entry::Notice(format!("entry {i}")));
            model.entry_ids.push(format!("event-{i}"));
        }
        model.scroll_from_bottom = 10;
        let theme = Theme::default();
        let area = Rect::new(0,0,40,5);
        let mut chat = Chat::default();
        let mut before = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut before);
        model.entries.push(Entry::Notice("new entry".into()));
        model.entry_ids.push("new-event".into());
        let mut after = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut after);
        assert_eq!(before, after);
        model.scroll_from_bottom = 11;
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut after);
        assert_eq!(chat.scroll_anchor, Some(("event-4".into(), 0)));
        model.scroll_from_bottom = 10;
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut after);
        assert_eq!(before, after);
        model.scroll_from_bottom = 0;
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut after);
        let row = (0..40).map(|x| after[(x,4)].symbol()).collect::<String>();
        assert!(row.contains("new entry"));
    }

    #[test]
    fn durable_cache_survives_removed_preceding_entry() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("stable".into(), "fake".into());
        model.entries = vec![Entry::Notice("first".into()), Entry::Notice("retained".into())];
        model.entry_ids = vec!["event-a".into(), "event-b".into()];
        let theme = Theme::default();
        let area = Rect::new(0,0,40,10);
        let mut chat = Chat::default();
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        let retained = chat.entry_cache[1].2.as_ptr();
        model.entries.remove(0);
        model.entry_ids.remove(0);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.entry_cache[0].2.as_ptr(), retained);
        assert_eq!(chat.cached_ids, vec!["event-b"]);
    }

    #[test]
    fn virtual_viewport_reuses_entries_and_matches_flat_render() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("virtual".into(), "fake".into());
        for i in 0..1000 { model.entries.push(Entry::Notice(format!("entry {i}"))); }
        let theme = Theme::default();
        let area = Rect::new(0,0,40,10);
        let mut chat = Chat::default();
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        let pointer = chat.entry_cache[0].2.as_ptr();
        for offset in [0, 1, 500, 990, 1000] {
            model.scroll_from_bottom = offset;
            let mut actual = Buffer::empty(area);
            chat.render(&Ctx {model:&model,theme:&theme},area,&mut actual);
            assert_eq!(pointer, chat.entry_cache[0].2.as_ptr());
            let flat:Vec<_> = chat.entry_cache.iter().flat_map(|entry| entry.2.iter().cloned()).collect();
            let mut expected = Buffer::empty(area);
            render_bottom_anchored(&flat, offset, area, &mut expected);
            assert_eq!(actual, expected);
        }
        model.entries[999] = Entry::Notice("changed".into());
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(pointer, chat.entry_cache[0].2.as_ptr());
        assert_eq!(chat.entry_cache[999].0, entry_fingerprint(&model.entries[999]));
        model.entries.truncate(3);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.entry_cache.len(),3);
        assert_eq!(chat.total_rows,3);
    }

    #[test]
    fn code_cards_preserve_multiline_syntax_state_and_allow_disabling_it() {
        use super::*;
        let source = "/* comment\ncontinued\n*/\nfn main() {}";
        let theme = crate::theme::Theme::default();
        let row = rness_kernel::presentation::StyledLine { block:Some(serde_json::json!({"kind":"code","text":source,"language":"rust","line_numbers":false})), ..Default::default() };
        let rendered = card_rows(row.clone(), 60, &theme);
        let expected = crate::core::highlight::highlight_code(source, "rust").unwrap();
        assert_eq!(rendered[1].spans[1].style.fg, expected[1].spans[0].style.fg);
        let mut plain = row;
        plain.block.as_mut().unwrap()["syntax_highlight"] = serde_json::json!(false);
        let plain = card_rows(plain, 60, &theme);
        assert_eq!(plain[1].spans[1].content, "continued");
        assert_eq!(plain[1].spans[1].style.fg, theme.code_block.fg);
    }

    #[test]
    fn error_visibility_is_independent_of_notice_visibility() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("qa".into(), "fake".into());
        model.entries.push(Entry::Notice("failure".into()));
        model.entries.push(Entry::Notice("ordinary notice".into()));
        model.error_entries.insert(0);
        let mut chat = Chat { config:serde_json::json!({"spacing":0,"error":{"visible":false}}), ..Default::default() };
        let area = Rect::new(0,0,40,5);
        let theme = Theme::default();
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.total_rows, 1);
        let text = (0..40).map(|x| buf[(x,4)].symbol()).collect::<String>();
        assert!(text.contains("ordinary notice"));
        assert!(!text.contains("failure"));
    }

    #[test]
    fn message_display_limits_visual_rows_without_mutating_content() {
        use super::*;
        let original = vec![Line::raw("abcdefghij")];
        let options = serde_json::json!({"display":"preview","preview_lines":2,"padding":{"left":1,"right":1}});
        let rows = message_rows(original.clone(), &options, 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].spans.iter().map(|s| s.content.as_ref()).collect::<String>(), "abc");
        assert_eq!(rows[1].spans.iter().map(|s| s.content.as_ref()).collect::<String>(), "def");
        assert!(message_rows(original.clone(), &serde_json::json!({"display":"collapsed"}), 5).is_empty());
        assert_eq!(message_rows(original.clone(), &serde_json::json!({"display":"expanded"}), 5), original);
    }

    #[test]
    fn bounded_layout_stops_before_consuming_large_spans() {
        use super::*;
        let rows = visual_rows_limited(Line::raw("x".repeat(1_000_000)), 80, true, 3);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.width() == 80));
        assert!(visual_rows_limited(Line::raw("hidden"), 80, true, 0).is_empty());
    }

    #[test]
    fn oversized_right_header_stays_inside_card() {
        use super::*;
        let row = rness_kernel::presentation::StyledLine {
            text:"left".into(),
            right:vec![rness_kernel::presentation::StyledSpan {text:"long status".into(),style:serde_json::Value::Null}],
            ..Default::default()
        };
        let rendered = card_line(row, 4, &crate::theme::Theme::default());
        assert_eq!(rendered.width(), 4);
        assert_eq!(rendered.spans.iter().map(|span| span.content.as_ref()).collect::<String>(), "long");
    }

    #[test]
    fn builtin_card_displays_requested_arguments_and_duration() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("qa".into(), "fake".into());
        model.entries.push(Entry::Assistant { model:"fake".into(), content:vec![ContentPart::ToolUse {
            call:"c".into(), name:"Read".into(), args:serde_json::json!({"path":"a.rs"}),
        }] });
        model.entries.push(Entry::ToolResult {call:"c".into(), name:"Read".into(), output:"content".into(), is_error:false});
        model.tool_durations.insert("c".into(), 42);
        let mut chat = Chat { config:serde_json::json!({"tool":{"display":"expanded","header":{"show_duration":true},"arguments":{"visible":true}}}), ..Default::default() };
        let theme = Theme::default();
        let area = Rect::new(0,0,60,10);
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        let text = (0..10).flat_map(|y| (0..60).map(move |x| (x,y))).map(|p| buf[p].symbol()).collect::<String>();
        assert!(text.contains("Read · 42 ms"));
        assert!(text.contains("a.rs"));
    }

    #[test]
    fn terminal_sanitizer_handles_osc_and_nonletter_csi() {
        assert_eq!(sanitize("\u{1b}]8;;https://example.com\u{1b}\\link\u{1b}]8;;\u{1b}\\"), "link");
        assert_eq!(sanitize("a\u{1b}[1~b"), "ab");
        assert_eq!(sanitize("界\tx"), "界      x");
    }

    #[test]
    fn terminal_tool_visibility_and_cancelled_state_are_applied() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("qa".into(), "fake".into());
        model.entries.push(Entry::ToolResult {call:"c".into(),name:"Read".into(),output:"output".into(),is_error:true});
        model.cancelled_tools.insert("c".into());
        let theme = Theme::default();
        let area = Rect::new(0,0,40,10);
        let mut buf = Buffer::empty(area);
        let mut chat = Chat {config:serde_json::json!({"tool":{"header":{"show_status":true},"states":{"cancelled":{"display":"collapsed"}}}}),..Default::default()};
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.total_rows,1);
        assert!(chat.entry_cache[0].2[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>().contains("cancelled"));
        chat.config["tool"]["visible"] = serde_json::json!(false);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.total_rows,0);
    }

    #[test]
    fn hidden_headers_do_not_consume_visual_preview_budget() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        let mut model = Model::new("qa".into(), "fake".into());
        model.entries.push(Entry::ToolResult { call:"c".into(), name:"Read".into(), output:"abcdefghijklmnop".into(), is_error:false });
        let theme = Theme::default();
        let mut chat = Chat { config:serde_json::json!({"tool":{"header":{"visible":false},"preview_lines":1}}), ..Default::default() };
        let area = Rect::new(0, 0, 4, 10);
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model, theme:&theme}, area, &mut buf);
        assert_eq!(chat.total_rows, 1);
        assert_eq!((0..4).map(|x| buf[(x,9)].symbol()).collect::<String>(), "abcd");
        chat.config["tool"]["display"] = serde_json::json!("collapsed");
        chat.render(&Ctx {model:&model, theme:&theme}, area, &mut buf);
        assert_eq!(chat.total_rows, 0);
    }

    #[test]
    fn visual_rows_preserve_graphemes_and_support_clipping() {
        use super::*;
        let text = "a\u{301}bc";
        let rows = visual_rows(Line::raw(text), 1, true);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].spans[0].content, "a\u{301}");
        let clipped = visual_rows(Line::raw("abcdef"), 3, false);
        assert_eq!(clipped.len(), 1);
        assert_eq!(clipped[0].spans.iter().map(|s| s.content.as_ref()).collect::<String>(), "abc");
    }

    #[test]
    fn provider_notice_wraps_without_losing_cause() {
        use super::*;
        use crate::{app::Model, theme::Theme};

        let mut model = Model::new("qa".into(), "fake".into());
        let text = "Provider error (fake): anthropic: stream ended before message_stop (connection closed or incomplete response)";
        model.entries.push(Entry::Notice(text.into()));
        let theme = Theme::default();
        for width in [40, 80, 120] {
            let area = Rect::new(0, 0, width, 12);
            let mut buf = Buffer::empty(area);
            Chat { cards: CardCache::default(), config: serde_json::Value::Null, focused_call: None, expanded: Default::default(), session: None, thinking_expanded: Default::default(), card_rows: Default::default(), total_rows: 0, viewport_height: 0, focused_thinking: None, last_area: Rect::default(), ..Default::default() }.render(&Ctx { model: &model, theme: &theme }, area, &mut buf);
            let rendered = (0..area.height).map(|y| {
                (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>()
            }).collect::<Vec<_>>().join(" ");
            assert_eq!(rendered.split_whitespace().collect::<Vec<_>>(), text.split_whitespace().collect::<Vec<_>>());
        }
    }

    #[test]
    fn message_background_fills_padding_and_wrapped_rows() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        use ratatui::style::Color;
        let mut model = Model::new("qa".into(), "fake".into());
        model.entries.push(Entry::User { content: vec![ContentPart::Text { text: "abcdefghijk".into() }] });
        let theme = Theme::default();
        let mut chat = Chat { cards: CardCache::default(), config: serde_json::json!({
            "spacing":0,"user":{"marker":false,"padding":{"left":1,"right":1,"top":1},"style":{"bg":"#3c3836"}}
        }) , focused_call: None, expanded: Default::default(), session: None, thinking_expanded: Default::default(), card_rows: Default::default(), total_rows: 0, viewport_height: 0, focused_thinking: None, last_area: Rect::default(), ..Default::default() };
        let area = Rect::new(0, 0, 8, 3);
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model, theme:&theme}, area, &mut buf);
        for y in 0..3 { for x in 0..8 { assert_eq!(buf[(x,y)].bg, Color::Rgb(60,56,54)); } }
        assert_eq!((0..8).map(|x| buf[(x,1)].symbol()).collect::<String>(), " abcdef ");
        assert_eq!((0..8).map(|x| buf[(x,2)].symbol()).collect::<String>(), " ghijk  ");
    }

    #[test]
    fn running_subagent_cards_expand_and_update_without_switching_sessions() {
        use super::*;
        use crate::{app::{Model, LiveStep}, theme::Theme};
        let mut model = Model::new("parent".into(), "fake".into());
        model.live = Some(LiveStep { running_tools: vec![("a".into(), "subagent".into()), ("b".into(), "subagent".into())], ..Default::default() });
        let theme = Theme::default();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut chat = Chat { config: serde_json::json!({"tool":{"display":"collapsed"}}), session: Some("parent".into()), ..Default::default() };
        for call in ["a", "b"] {
            chat.cards.insert(call.into(), vec![
                crate::modules::tool_cards::CardLine {text: format!("{call} running"), is_header:true, ..Default::default()},
                crate::modules::tool_cards::CardLine {text: "Bash cargo test".into(), ..Default::default()},
            ]);
        }
        let area = Rect::new(0, 0, 50, 10);
        let mut buf = Buffer::empty(area);
        chat.render(&ctx, area, &mut buf);
        assert!(chat.on_binding(&ctx, "toggle_tool").handled);
        chat.render(&ctx, area, &mut buf);
        assert_eq!(chat.expanded.get("b"), Some(&true));
        assert_eq!(chat.expanded.get("a"), None);
        assert!(buf.content.iter().map(|cell| cell.symbol()).collect::<String>().contains("Bash cargo test"));
        chat.cards.insert("b".into(), vec![crate::modules::tool_cards::CardLine { text: "tests passed".into(), ..Default::default() }]);
        chat.render(&ctx, area, &mut buf);
        assert!(buf.content.iter().map(|cell| cell.symbol()).collect::<String>().contains("tests passed"));
    }

    #[test]
    fn tool_focus_and_expansion_use_call_identity_and_respect_disabled_keys() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut model = Model::new("qa".into(), "fake".into());
        for call in ["a", "b"] { model.entries.push(Entry::ToolResult {
            call:call.into(), name:"Bash".into(), output:"one\ntwo\nthree".into(), is_error:false,
        }); }
        let theme = Theme::default();
        let ctx = Ctx {model:&model, theme:&theme};
        let mut chat = Chat { cards:CardCache::default(), config:serde_json::json!({"tool":{"display":"collapsed"}}), focused_call:None, expanded:Default::default(), session:Some("qa".into()), thinking_expanded:Default::default(), card_rows:Default::default(), total_rows:0, viewport_height:0, focused_thinking:None, last_area:Rect::default(), ..Default::default() };
        let toggle = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(chat.on_key(&ctx, toggle).handled);
        assert_eq!(chat.expanded.get("b"), Some(&true));
        chat.on_key(&ctx, KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        chat.on_key(&ctx, toggle);
        assert_eq!(chat.expanded.get("a"), Some(&true));
        assert_eq!(chat.expanded.get("b"), Some(&true));
        chat.config["keys"] = serde_json::json!({"toggle_tool":false});
        assert!(!chat.on_key(&ctx, toggle).handled);
        assert!(!chat.binding_help().iter().any(|line| line.contains("toggle_tool")));
        chat.config["keys"] = serde_json::json!({"toggle_tool":"f8"});
        assert!(chat.binding_help().iter().any(|line| line.contains("F(8)") && line.contains("toggle_tool")));
        assert!(!chat.on_key(&ctx, toggle).handled);
        assert!(chat.on_key(&ctx, KeyEvent::from(KeyCode::F(8))).handled);
        chat.config["keys"] = serde_json::json!({"previous_tool":"f8", "toggle_tool":"f8"});
        chat.focused_call = Some("b".into());
        let before = chat.expanded.get("b").copied().unwrap_or(false);
        assert!(chat.on_binding(&ctx, "toggle_tool").handled);
        assert_eq!(chat.focused_call.as_deref(), Some("b"));
        assert_eq!(chat.expanded.get("b"), Some(&!before));
    }

    #[test]
    fn navigation_returns_scroll_action_and_thinking_toggle_is_reversible() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut model = Model::new("qa".into(), "fake".into());
        model.entries.push(Entry::ToolResult {call:"a".into(),name:"Read".into(),output:"text".into(),is_error:false});
        let theme = Theme::default();
        let mut chat = Chat { cards:CardCache::default(),config:serde_json::json!({"tool":{}}),focused_call:None,expanded:Default::default(),session:Some("qa".into()),thinking_expanded:Default::default(),card_rows:std::collections::HashMap::from([("a".into(), 10)]),total_rows:100,viewport_height:20,focused_thinking:None, last_area:Rect::default(), ..Default::default() };
        let outcome = chat.on_key(&Ctx {model:&model,theme:&theme}, KeyEvent::new(KeyCode::Up,KeyModifiers::ALT));
        assert!(matches!(outcome.actions.as_slice(), [crate::app::Action::ScrollUp(70)]));
        let toggle = KeyEvent::new(KeyCode::Char('t'),KeyModifiers::ALT);
        assert!(!chat.on_key(&Ctx {model:&model,theme:&theme}, toggle).handled);
    }

    #[test]
    fn diff_blocks_keep_offsets_and_full_width_backgrounds() {
        use super::*;
        let row = crate::modules::tool_cards::CardLine { block:Some(serde_json::json!({
            "kind":"diff","before":"old\n","after":"new\n","old_start":42,"new_start":42,
            "styles":{"removed":{"bg":"#542525"},"added":{"bg":"#294b2d"}}
        })), ..Default::default() };
        let lines = card_rows(row, 30, &crate::theme::Theme::default());
        assert_eq!(lines.len(), 2);
        let text = |line: &Line| line.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text(&lines[0]).starts_with("  42 - old"));
        assert!(text(&lines[1]).starts_with("  42 + new"));
        assert_eq!(lines[1].width(), 30);
        assert_eq!(lines[1].spans.last().unwrap().style.bg, Some(ratatui::style::Color::Rgb(41,75,45)));
    }

    #[test]
    fn diff_context_omits_distant_lines_and_reports_counts() {
        use super::*;
        let before = (0..30).map(|n| format!("line {n}\n")).collect::<String>();
        let after = before.replace("line 15\n", "changed\n");
        let row = crate::modules::tool_cards::CardLine { block:Some(serde_json::json!({
            "kind":"diff","before":before,"after":after,"context_lines":1,"summary":true
        })), ..Default::default() };
        let lines = card_rows(row, 50, &crate::theme::Theme::default());
        let text = lines.iter().flat_map(|l| &l.spans).map(|s| s.content.as_ref()).collect::<String>();
        assert!(text.contains("Added 1 lines, removed 1 lines"));
        assert!(text.contains("line 14"));
        assert!(text.contains("line 16"));
        assert!(!text.contains("line 0"));
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn expansion_keeps_focused_header_visible_at_actual_width() {
        use super::*;
        use crate::{app::{Model, Action}, theme::Theme};
        use crossterm::event::{KeyCode,KeyEvent,KeyModifiers};
        for width in [24, 60, 100] {
            let mut model = Model::new("qa".into(),"fake".into());
            for id in ["first", "last"] { model.entries.push(Entry::ToolResult {call:id.into(),name:id.into(),output:(0..40).map(|i| format!("output line {i}\n")).collect(),is_error:false}); }
            let theme = Theme::default();
            let mut chat = Chat {config:serde_json::json!({"tool":{"display":"collapsed"}}), ..Default::default()};
            let area = Rect::new(0,0,width,10);
            chat.render(&Ctx {model:&model,theme:&theme},area,&mut Buffer::empty(area));
            for action in chat.on_key(&Ctx {model:&model,theme:&theme},KeyEvent::new(KeyCode::Char('o'),KeyModifiers::CONTROL)).actions {
                match action { Action::ScrollUp(n) => model.scroll_from_bottom += n, Action::ScrollDown(n) => model.scroll_from_bottom = model.scroll_from_bottom.saturating_sub(n), _=>{} }
            }
            let mut buf = Buffer::empty(area);
            chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
            let top = (0..width).map(|x|buf[(x,0)].symbol()).collect::<String>();
            assert!(top.contains("last"), "width={width}: {top}");
        }
    }

    #[test]
    fn live_thinking_expansion_survives_notices_and_multiple_blocks() {
        use super::*;
        use crate::{app::{Model,LiveStep},theme::Theme};
        use crossterm::event::{KeyCode,KeyEvent,KeyModifiers};
        let mut model = Model::new("qa".into(),"fake".into());
        model.live = Some(LiveStep {thinking:"one\ntwo\nthree".into(),..Default::default()});
        let theme = Theme::default();
        let mut chat = Chat {config:serde_json::json!({"thinking":{"display":"collapsed"}}),..Default::default()};
        let toggle = KeyEvent::new(KeyCode::Char('t'),KeyModifiers::ALT);
        assert!(chat.on_key(&Ctx {model:&model,theme:&theme},toggle).handled);
        model.entries.push(Entry::Notice("intervening event".into()));
        let area = Rect::new(0,0,40,12);
        let mut buf = Buffer::empty(area);
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        assert_eq!(chat.focused_thinking, Some((0,0)));
        model.live = None;
        model.entries.push(Entry::Assistant {model:"fake".into(),content:vec![
            ContentPart::Text {text:"before thinking".into()},
            ContentPart::Thinking {text:"one\ntwo\nthree".into(),signature:None},
            ContentPart::Text {text:"between blocks".into()},
            ContentPart::Thinking {text:"separate block".into(),signature:None},
        ]});
        chat.render(&Ctx {model:&model,theme:&theme},area,&mut buf);
        let text=(0..12).flat_map(|y|(0..40).map(move |x|(x,y))).map(|p|buf[p].symbol()).collect::<String>();
        assert!(text.contains("three"));
        assert_eq!(chat.thinking_expanded.get(&(0,0)),Some(&true));
        assert!(!text.contains("separate block"));
        let next = KeyEvent::new(KeyCode::Char('n'),KeyModifiers::ALT);
        assert!(chat.on_key(&Ctx {model:&model,theme:&theme},next).handled);
        assert_eq!(chat.focused_thinking, Some((0,1)));
        assert!(chat.on_key(&Ctx {model:&model,theme:&theme},toggle).handled);
        assert_eq!(chat.thinking_expanded.get(&(0,1)),Some(&true));
        assert_eq!(chat.thinking_expanded.get(&(0,0)),Some(&true));
    }

    #[test]
    fn thinking_bindings_are_overridable_and_disableable() {
        use super::*;
        use crate::{app::Model, theme::Theme};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut model = Model::new("qa".into(), "fake".into());
        model.entries.push(Entry::Assistant {model:"fake".into(), content: vec![
            ContentPart::Thinking {text:"one".into(),signature:None},
            ContentPart::Thinking {text:"two".into(),signature:None},
        ]});
        let theme = Theme::default();
        let ctx = Ctx {model:&model,theme:&theme};
        let mut chat = Chat {config:serde_json::json!({"keys":{
            "previous_thinking":"f6","next_thinking":false,"toggle_thinking":"f7"
        }}),..Default::default()};
        assert!(!chat.on_key(&ctx, KeyEvent::new(KeyCode::Char('p'),KeyModifiers::ALT)).handled);
        assert!(!chat.on_key(&ctx, KeyEvent::new(KeyCode::Char('n'),KeyModifiers::ALT)).handled);
        assert!(!chat.on_key(&ctx, KeyEvent::new(KeyCode::Char('t'),KeyModifiers::ALT)).handled);
        assert!(chat.on_key(&ctx, KeyEvent::new(KeyCode::F(7),KeyModifiers::NONE)).handled);
        assert!(chat.on_key(&ctx, KeyEvent::new(KeyCode::F(6),KeyModifiers::NONE)).handled);
        assert_eq!(chat.focused_thinking, Some((0,0)));
    }

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
