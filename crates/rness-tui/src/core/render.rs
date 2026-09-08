//! Markdown → styled lines. Non-swappable core widget.
//!
//! M3 scope: headings, bold/italic/inline code, fenced code blocks with
//! syntect highlighting, lists, paragraphs — wrapped to width. Tables
//! come later; the seam is `render_markdown`.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::core::highlight::highlight_code;
use crate::theme::Theme;

/// Render markdown into wrapped, styled lines.
pub fn render_markdown(source: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(10) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();

    // Current inline accumulation (spans of the paragraph being built).
    let mut inline: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = vec![Style::default()];
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut list_depth: usize = 0;

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

    let parser = Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH);
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
                    style_stack.push(theme.heading);
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
                    if code_lang.is_empty() {
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
                TagEnd::Paragraph | TagEnd::Item => flush_inline(&mut inline, &mut out),
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    match highlight_code(&code_buf, &code_lang) {
                        Some(lines) => out.extend(lines),
                        None => {
                            for l in code_buf.lines() {
                                out.push(Line::styled(l.to_string(), theme.code_block));
                            }
                        }
                    }
                    code_buf.clear();
                    out.push(Line::styled("```", theme.dim));
                }
                TagEnd::List(_) => list_depth = list_depth.saturating_sub(1),
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
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
            Event::Code(code) => inline.push(Span::styled(code.into_string(), theme.code)),
            Event::SoftBreak => inline.push(Span::raw(" ")),
            Event::HardBreak => flush_inline(&mut inline, &mut out),
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

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
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
        let lines =
            render_markdown("```rust\nfn main() {}\n```", 80, &Theme::default());
        let text = plain(&lines);
        assert_eq!(text, vec!["```rust", "fn main() {}", "```"]);
    }

    #[test]
    fn inline_code_and_bold_styled() {
        let theme = Theme::default();
        let lines = render_markdown("a `code` **bold**", 80, &theme);
        let spans = &lines[0].spans;
        assert!(spans.iter().any(|s| s.content == "code" && s.style == theme.code));
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
            code_line.spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            "fn main() {}"
        );
        // Highlighted: multiple spans with RGB foregrounds, not one flat style.
        assert!(code_line.spans.len() > 1, "expected colored spans: {code_line:?}");
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
            vec![
                "Uno.", "", "Dos.", "", "- a", "- b", "", "```", "x", "```", "", "Tres.",
            ]
        );
    }

    #[test]
    fn no_leading_blank_line() {
        let lines = render_markdown("hola", 80, &Theme::default());
        assert_eq!(plain(&lines), vec!["hola"]);
    }
}
