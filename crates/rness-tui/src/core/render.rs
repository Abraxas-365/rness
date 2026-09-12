//! Markdown → styled lines. Non-swappable core widget.
//!
//! M3 scope: headings, bold/italic/inline code, fenced code blocks with
//! syntect highlighting, lists, tables, paragraphs — wrapped to width.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::core::highlight::highlight_code;
use crate::theme::Theme;

/// Render markdown into wrapped, styled lines.
pub fn render_markdown(source: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    render_markdown_configured(source, width, theme, &serde_json::Value::Null)
}

struct CachedMarkdown {
    source: String,
    width: u16,
    theme: Theme,
    config: serde_json::Value,
    lines: Vec<Line<'static>>,
    bytes: usize,
}

thread_local! {
    static MARKDOWN_CACHE: std::cell::RefCell<std::collections::VecDeque<CachedMarkdown>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

pub fn render_markdown_configured(
    source: &str,
    width: u16,
    theme: &Theme,
    config: &serde_json::Value,
) -> Vec<Line<'static>> {
    if let Some(lines) = MARKDOWN_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let index = cache.iter().position(|entry| {
            entry.width == width
                && entry.source == source
                && entry.theme == *theme
                && entry.config == *config
        })?;
        let entry = cache.remove(index)?;
        let lines = entry.lines.clone();
        cache.push_back(entry);
        Some(lines)
    }) {
        return lines;
    }
    let lines = render_markdown_uncached(source, width, theme, config);
    let bytes = source
        .len()
        .saturating_add(config.to_string().len())
        .saturating_add(
            lines
                .iter()
                .map(|line| {
                    std::mem::size_of::<Line<'static>>()
                        + line
                            .spans
                            .iter()
                            .map(|span| std::mem::size_of::<Span<'static>>() + span.content.len())
                            .sum::<usize>()
                })
                .sum::<usize>(),
        );
    const BUDGET: usize = 4 * 1024 * 1024;
    if bytes <= BUDGET / 4 {
        MARKDOWN_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let mut retained = cache.iter().map(|entry| entry.bytes).sum::<usize>();
            while cache.len() >= 128 || retained.saturating_add(bytes) > BUDGET {
                if let Some(entry) = cache.pop_front() {
                    retained -= entry.bytes;
                } else {
                    break;
                }
            }
            cache.push_back(CachedMarkdown {
                source: source.to_owned(),
                width,
                theme: theme.clone(),
                config: config.clone(),
                lines: lines.clone(),
                bytes,
            });
        });
    }
    lines
}

fn render_markdown_uncached(
    source: &str,
    width: u16,
    theme: &Theme,
    config: &serde_json::Value,
) -> Vec<Line<'static>> {
    let resolve = |value: &serde_json::Value, fallback| {
        theme.resolve_style(value, fallback).unwrap_or(fallback)
    };
    let heading = resolve(&config["heading"], theme.heading);
    let inline_code = resolve(&config["inline_code"], theme.code);
    let code_style = resolve(&config["code_block"]["style"], theme.code_block);
    let width = width.max(1) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();

    // Current inline accumulation (spans of the paragraph being built).
    let mut inline: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut list_depth: usize = 0;
    let mut table: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    let mut table_row: Vec<Vec<Span<'static>>> = Vec::new();
    let mut table_cell: Vec<Span<'static>> = Vec::new();
    let mut in_table = false;

    let flush_inline = |inline: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
        if inline.is_empty() {
            return;
        }
        out.extend(wrap_spans(std::mem::take(inline), width));
    };

    // Blank line between block-level elements (paragraph, heading, code
    // block, list) — markdown's visual rhythm.
    let separate = |out: &mut Vec<Line<'static>>| {
        if out.last().is_some_and(|l| !l.spans.is_empty()) {
            out.push(Line::default());
        }
    };

    let parser = Parser::new_ext(
        source,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
    );
    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    flush_inline(&mut inline, &mut out);
                    separate(&mut out);
                }
                Tag::Heading { .. } => {
                    flush_inline(&mut inline, &mut out);
                    separate(&mut out);
                    style_stack.push(heading);
                }
                Tag::Link { .. } => {
                    style_stack.push(resolve(&config["link"], *style_stack.last().unwrap()))
                }
                Tag::BlockQuote(_) => {
                    style_stack.push(resolve(&config["quote"], *style_stack.last().unwrap()))
                }
                Tag::Emphasis => {
                    let top = *style_stack.last().expect("style stack");
                    style_stack.push(top.add_modifier(Modifier::ITALIC));
                }
                Tag::Strong => {
                    let top = *style_stack.last().expect("style stack");
                    style_stack.push(top.add_modifier(Modifier::BOLD));
                }
                Tag::CodeBlock(kind) => {
                    flush_inline(&mut inline, &mut out);
                    separate(&mut out);
                    in_code_block = true;
                    code_lang.clear();
                    code_buf.clear();
                    if let CodeBlockKind::Fenced(lang) = kind {
                        code_lang = lang.to_string();
                    }
                    if code_lang.is_empty() || config["code_block"]["show_language"] == false {
                        out.push(Line::styled("```", theme.dim));
                    } else {
                        out.push(Line::styled(format!("```{code_lang}"), theme.dim));
                    }
                }
                Tag::List(_) => {
                    if list_depth == 0 {
                        flush_inline(&mut inline, &mut out);
                        separate(&mut out);
                    }
                    list_depth += 1;
                }
                Tag::Item => {
                    flush_inline(&mut inline, &mut out);
                    inline.push(Span::raw(format!("{}- ", "  ".repeat(list_depth - 1))));
                }
                Tag::Table(_) => {
                    flush_inline(&mut inline, &mut out);
                    separate(&mut out);
                    table.clear();
                    in_table = true;
                }
                Tag::TableHead | Tag::TableRow => table_row.clear(),
                Tag::TableCell => table_cell.clear(),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) | TagEnd::Emphasis | TagEnd::Strong => {
                    if tag == TagEnd::Heading(pulldown_cmark::HeadingLevel::H1)
                        || matches!(tag, TagEnd::Heading(_))
                    {
                        flush_inline(&mut inline, &mut out);
                    }
                    style_stack.pop();
                }
                TagEnd::Link | TagEnd::BlockQuote(_) => {
                    style_stack.pop();
                }
                TagEnd::Paragraph | TagEnd::Item => flush_inline(&mut inline, &mut out),
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    let highlighted = if config["code_block"]["syntax_highlight"] == false {
                        None
                    } else {
                        highlight_code(&code_buf, &code_lang)
                    };
                    let code_lines = highlighted.unwrap_or_else(|| {
                        code_buf.lines().map(|l| Line::raw(l.to_owned())).collect()
                    });
                    let left = config["code_block"]["padding"]["left"]
                        .as_u64()
                        .unwrap_or(0)
                        .min(64) as usize;
                    let right = config["code_block"]["padding"]["right"]
                        .as_u64()
                        .unwrap_or(0)
                        .min(64) as usize;
                    for _ in 0..config["code_block"]["padding"]["top"]
                        .as_u64()
                        .unwrap_or(0)
                        .min(64)
                    {
                        out.push(Line::styled(" ".repeat(width), code_style));
                    }
                    for line in code_lines {
                        let mut spans = vec![Span::styled(" ".repeat(left), code_style)];
                        spans.extend(line.spans.into_iter().map(|span| {
                            let mut style = code_style.patch(span.style);
                            if config["code_block"]["style"].get("bg").is_some() {
                                style.bg = code_style.bg;
                            }
                            span.style(style)
                        }));
                        spans.push(Span::styled(" ".repeat(right), code_style));
                        out.push(Line::from(spans).style(code_style));
                    }
                    for _ in 0..config["code_block"]["padding"]["bottom"]
                        .as_u64()
                        .unwrap_or(0)
                        .min(64)
                    {
                        out.push(Line::styled(" ".repeat(width), code_style));
                    }
                    code_buf.clear();
                    out.push(Line::styled("```", theme.dim));
                }
                TagEnd::List(_) => list_depth = list_depth.saturating_sub(1),
                TagEnd::TableCell => table_row.push(std::mem::take(&mut table_cell)),
                TagEnd::TableHead | TagEnd::TableRow => table.push(std::mem::take(&mut table_row)),
                TagEnd::Table => {
                    in_table = false;
                    out.extend(render_table(&table, width, theme));
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_table {
                    table_cell.push(Span::styled(
                        text.into_string(),
                        *style_stack.last().expect("style stack"),
                    ));
                } else if in_code_block {
                    // Tabs desync ratatui's cell accounting — expand here
                    // (8-col stops) so highlight/plain paths are both safe.
                    if text.contains('\t') {
                        for (i, l) in text.split('\n').enumerate() {
                            if i > 0 {
                                code_buf.push('\n');
                            }
                            let mut col = 0usize;
                            for ch in l.chars() {
                                if ch == '\t' {
                                    let next = (col / 8 + 1) * 8;
                                    for _ in col..next {
                                        code_buf.push(' ');
                                    }
                                    col = next;
                                } else {
                                    code_buf.push(ch);
                                    col += 1;
                                }
                            }
                        }
                    } else {
                        code_buf.push_str(&text);
                    }
                } else {
                    inline.push(Span::styled(
                        text.into_string(),
                        *style_stack.last().expect("style stack"),
                    ));
                }
            }
            Event::Code(code) => {
                let span = Span::styled(code.into_string(), inline_code);
                if in_table {
                    table_cell.push(span);
                } else {
                    inline.push(span);
                }
            }
            Event::SoftBreak => {
                if in_table {
                    table_cell.push(Span::raw(" "));
                } else {
                    inline.push(Span::raw(" "));
                }
            }
            Event::HardBreak => {
                if in_table {
                    table_cell.push(Span::raw(" "));
                } else {
                    flush_inline(&mut inline, &mut out);
                }
            }
            Event::Rule => {
                flush_inline(&mut inline, &mut out);
                out.push(Line::styled("─".repeat(width), theme.dim));
            }
            _ => {}
        }
    }
    flush_inline(&mut inline, &mut out);
    out
}

fn render_table(
    table: &[Vec<Vec<Span<'static>>>],
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    let columns = table.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }

    let mut column_widths = (0..columns)
        .map(|column| {
            table
                .iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.iter().map(|span| span.content.width()).sum::<usize>())
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect::<Vec<_>>();
    let available = width.saturating_sub(columns * 3 + 1).max(columns);
    while column_widths.iter().sum::<usize>() > available {
        let Some((column, _)) = column_widths
            .iter()
            .enumerate()
            .filter(|(_, &size)| size > 1)
            .max_by_key(|(_, &size)| size)
        else {
            break;
        };
        column_widths[column] -= 1;
    }

    let border = |left, join, right| {
        let mut spans = vec![Span::styled(left, theme.dim)];
        for (column, cell_width) in column_widths.iter().enumerate() {
            spans.push(Span::styled("─".repeat(cell_width + 2), theme.dim));
            spans.push(Span::styled(
                if column + 1 == columns { right } else { join },
                theme.dim,
            ));
        }
        Line::from(spans)
    };

    let mut out = vec![border("┌", "┬", "┐")];
    for (row_index, row) in table.iter().enumerate() {
        let cells = (0..columns)
            .map(|column| {
                let lines = row
                    .get(column)
                    .map(|cell| wrap_spans(cell.clone(), column_widths[column]))
                    .unwrap_or_default();
                if lines.is_empty() {
                    vec![Line::default()]
                } else {
                    lines
                }
            })
            .collect::<Vec<_>>();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            let mut spans = vec![Span::styled("│ ", theme.dim)];
            for (column, cell) in cells.iter().enumerate() {
                if let Some(line) = cell.get(line_index) {
                    spans.extend(line.spans.iter().cloned().map(|span| {
                        if row_index == 0 {
                            let style = span.style;
                            span.style(style.add_modifier(Modifier::BOLD))
                        } else {
                            span
                        }
                    }));
                    let used = line
                        .spans
                        .iter()
                        .map(|span| span.content.width())
                        .sum::<usize>();
                    spans.push(Span::raw(
                        " ".repeat(column_widths[column].saturating_sub(used)),
                    ));
                } else {
                    spans.push(Span::raw(" ".repeat(column_widths[column])));
                }
                spans.push(Span::styled(
                    if column + 1 == columns {
                        " │"
                    } else {
                        " │ "
                    },
                    theme.dim,
                ));
            }
            out.push(Line::from(spans));
        }
        if row_index == 0 && table.len() > 1 {
            out.push(border("├", "┼", "┤"));
        }
    }
    out.push(border("└", "┴", "┘"));
    out
}

/// Greedy word-wrap over styled spans, preserving styles across breaks.
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for span in spans {
        let style = span.style;
        for word in split_keep_spaces(&span.content) {
            let w = word.width();
            if used + w > width && used > 0 {
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
                if word.trim().is_empty() {
                    continue; // don't start a line with the breaking space
                }
            }
            current.push(Span::styled(word.to_string(), style));
            used += w;
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// Split into words and the spaces between them, preserved.
fn split_keep_spaces(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let split = rest
            .char_indices()
            .find(|(_, c)| (c.is_whitespace()) != (rest.starts_with(char::is_whitespace)))
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (part, tail) = rest.split_at(split);
        parts.push(part);
        rest = tail;
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_cache_tracks_width_theme_config_and_evicts() {
        MARKDOWN_CACHE.with(|cache| cache.borrow_mut().clear());
        let mut theme = Theme::default();
        let config = serde_json::Value::Null;
        let first = render_markdown_configured("hello world", 80, &theme, &config);
        assert_eq!(
            first,
            render_markdown_configured("hello world", 80, &theme, &config)
        );
        MARKDOWN_CACHE.with(|cache| assert_eq!(cache.borrow().len(), 1));
        render_markdown_configured("hello world", 5, &theme, &config);
        theme.heading = Style::default().fg(ratatui::style::Color::Red);
        render_markdown_configured("hello world", 5, &theme, &config);
        render_markdown_configured(
            "hello world",
            5,
            &theme,
            &serde_json::json!({"code_block":{"syntax_highlight":false}}),
        );
        MARKDOWN_CACHE.with(|cache| assert_eq!(cache.borrow().len(), 4));
        for i in 0..140 {
            render_markdown_configured(&format!("entry {i}"), 80, &theme, &config);
        }
        MARKDOWN_CACHE.with(|cache| {
            let cache = cache.borrow();
            assert_eq!(cache.len(), 128);
            assert!(cache.iter().map(|entry| entry.bytes).sum::<usize>() <= 4 * 1024 * 1024);
        });
    }

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn paragraphs_wrap_to_width() {
        let lines = render_markdown("one two three four five", 10, &Theme::default());
        let text = plain(&lines);
        assert!(text.len() > 1, "{text:?}");
        assert!(text.iter().all(|l| l.len() <= 10), "{text:?}");
    }

    #[test]
    fn code_blocks_pass_through_unwrapped() {
        let lines = render_markdown("```rust\nfn main() {}\n```", 80, &Theme::default());
        let text = plain(&lines);
        assert_eq!(text, vec!["```rust", "fn main() {}", "```"]);
    }

    #[test]
    fn inline_code_and_bold_styled() {
        let theme = Theme::default();
        let lines = render_markdown("a `code` **bold**", 80, &theme);
        let spans = &lines[0].spans;
        assert!(spans
            .iter()
            .any(|s| s.content == "code" && s.style == theme.code));
        assert!(spans
            .iter()
            .any(|s| s.content == "bold" && s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn lists_get_bullets() {
        let lines = render_markdown("- uno\n- dos", 80, &Theme::default());
        let text = plain(&lines);
        assert_eq!(text, vec!["- uno", "- dos"]);
    }

    #[test]
    fn fenced_rust_is_syntax_highlighted() {
        let lines = render_markdown("```rust\nfn main() {}\n```", 80, &Theme::default());
        let code_line = &lines[1];
        assert_eq!(
            code_line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "fn main() {}"
        );
        // Highlighted: multiple spans with RGB foregrounds, not one flat style.
        assert!(
            code_line.spans.len() > 1,
            "expected colored spans: {code_line:?}"
        );
        assert!(code_line
            .spans
            .iter()
            .any(|s| matches!(s.style.fg, Some(ratatui::style::Color::Rgb(..)))));
    }

    #[test]
    fn unfenced_or_unknown_lang_uses_plain_code_style() {
        let theme = Theme::default();
        let lines = render_markdown("```notalanguage\nhola mundo\n```", 80, &theme);
        let text = plain(&lines);
        assert_eq!(text, vec!["```notalanguage", "hola mundo", "```"]);
        assert_eq!(lines[1].style, theme.code_block);
    }

    #[test]
    fn code_block_tabs_are_expanded() {
        // Tabs reach ratatui cells verbatim otherwise and desync layout.
        let lines = render_markdown("```go\nfunc x() {\n\treturn\n}\n```", 80, &Theme::default());
        let text = plain(&lines);
        assert_eq!(text[2], "        return");
    }

    #[test]
    fn block_elements_are_separated_by_blank_lines() {
        let src = "Uno.\n\nDos.\n\n- a\n- b\n\n```\nx\n```\n\nTres.";
        let lines = render_markdown(src, 80, &Theme::default());
        assert_eq!(
            plain(&lines),
            vec!["Uno.", "", "Dos.", "", "- a", "- b", "", "```", "x", "```", "", "Tres.",]
        );
    }

    #[test]
    fn configured_markdown_styles_and_plain_code_preserve_text() {
        let theme = Theme::default();
        let config = serde_json::json!({
            "heading":{"fg":"#ff0000"}, "link":{"underline":true},
            "code_block":{"syntax_highlight":false,"show_language":false,"style":{"bg":"#1d2021"}}
        });
        let lines = render_markdown_configured(
            "# Title\n\n[link](https://example.com)\n\n```rust\nlet x = 1;\n```",
            40,
            &theme,
            &config,
        );
        assert!(lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("Title")
                && s.style.fg == Some(ratatui::style::Color::Rgb(255, 0, 0))));
        assert!(lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("link")
                && s.style.add_modifier.contains(Modifier::UNDERLINED)));
        let text = lines
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("let x = 1;"));
        assert!(!text.contains("```rust"));
    }

    #[test]
    fn tables_are_rendered_in_columns() {
        let source = "| Parámetro | Ejemplo |\n| --- | --- |\n| APPLICATION | cargo-api |\n| VERSION | 1.0.3 |";
        let lines = render_markdown(source, 80, &Theme::default());
        let text = plain(&lines);
        assert_eq!(
            text,
            vec![
                "┌─────────────┬───────────┐",
                "│ Parámetro   │ Ejemplo   │",
                "├─────────────┼───────────┤",
                "│ APPLICATION │ cargo-api │",
                "│ VERSION     │ 1.0.3     │",
                "└─────────────┴───────────┘",
            ]
        );
        assert!(lines[1]
            .spans
            .iter()
            .any(|span| span.content.contains("Parámetro")
                && span.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn tables_wrap_cells_to_available_width() {
        use unicode_width::UnicodeWidthStr;

        let source = "| Name | Description |\n| --- | --- |\n| cargo | deployment artifact requiring validation |";
        let lines = render_markdown(source, 24, &Theme::default());
        let text = plain(&lines);
        assert!(text.iter().all(|line| line.width() <= 24), "{text:?}");
        assert!(text.iter().any(|line| line.contains("deployment")));
        assert!(text.iter().any(|line| line.contains("validation")));
    }

    #[test]
    fn no_leading_blank_line() {
        let lines = render_markdown("hola", 80, &Theme::default());
        assert_eq!(plain(&lines), vec!["hola"]);
    }
}
