//! Syntax highlighting for fenced code blocks (syntect → ratatui spans).
//!
//! Loaded lazily once per process; unknown languages fall back to the
//! theme's plain code-block style so rendering never fails.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static syntect::highlighting::Theme {
    static THEME: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults();
        themes.themes.remove("base16-eighties.dark").expect("bundled syntect theme")
    })
}

/// Highlight `code` as `lang`. Returns one styled Line per source line,
/// or None when the language isn't recognized (caller falls back).
pub fn highlight_code(code: &str, lang: &str) -> Option<Vec<Line<'static>>> {
    if lang.is_empty() {
        return None;
    }
    let set = syntaxes();
    let syntax = set
        .find_syntax_by_token(lang)
        .or_else(|| set.find_syntax_by_extension(lang))?;

    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for source_line in code.lines() {
        // syntect state machines expect the trailing newline.
        let with_nl = format!("{source_line}\n");
        let regions = highlighter.highlight_line(&with_nl, set).ok()?;
        let spans: Vec<Span<'static>> = regions
            .into_iter()
            .map(|(style, text)| {
                Span::styled(text.trim_end_matches('\n').to_string(), convert(style))
            })
            .filter(|s| !s.content.is_empty())
            .collect();
        out.push(Line::from(spans));
    }
    Some(out)
}

/// syntect style → ratatui style. Foreground only: terminal background
/// stays whatever the theme/terminal uses.
fn convert(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
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
    fn rust_code_gets_colored_spans() {
        let lines = highlight_code("fn main() {}\nlet x = 1;", "rust").expect("rust known");
        assert_eq!(plain(&lines), vec!["fn main() {}", "let x = 1;"]);
        // At least one span deviates from default (i.e. actually colored).
        assert!(lines.iter().flat_map(|l| &l.spans).any(|s| s.style.fg.is_some()));
    }

    #[test]
    fn file_extensions_select_syntax() {
        for (extension, language, source) in [
            ("rs", "rust", "fn main() {}"),
            ("lua", "lua", "local x = 42"),
        ] {
            let lines = highlight_code(source, extension).expect("known file extension");
            assert_eq!(lines, highlight_code(source, language).unwrap());
            assert!(lines.iter().flat_map(|l| &l.spans).any(|s| s.style.fg.is_some()));
        }
        assert!(highlight_code("plain text", "unknown_extension").is_none());
    }

    #[test]
    fn unknown_language_falls_back() {
        assert!(highlight_code("whatever", "notalanguage").is_none());
        assert!(highlight_code("whatever", "").is_none());
    }

    #[test]
    fn text_is_preserved_exactly() {
        let src = "def f(x):\n    return x * 2";
        let lines = highlight_code(src, "python").expect("python known");
        assert_eq!(plain(&lines).join("\n"), src);
    }
}
