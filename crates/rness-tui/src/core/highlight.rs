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
        themes
            .themes
            .remove("base16-eighties.dark")
            .expect("bundled syntect theme")
    })
}

/// Highlight `code` as `lang`. Returns one styled Line per source line,
/// or None when the language isn't recognized (caller falls back).
pub fn highlight_code(code: &str, lang: &str) -> Option<Vec<Line<'static>>> {
    highlight_code_limited(code, lang, usize::MAX)
}

/// Highlight at most `max_lines` physical source lines, preserving syntax state
/// from the beginning of the source without processing the remaining lines.
///
/// Results are memoized: highlighting is width-independent, but the chat
/// re-renders every card on each resize, and syntect dominated that rebuild
/// (seconds on long sessions).
pub fn highlight_code_limited(
    code: &str,
    lang: &str,
    max_lines: usize,
) -> Option<Vec<Line<'static>>> {
    if lang.is_empty() {
        return None;
    }
    let key = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (code, lang, max_lines).hash(&mut hasher);
        hasher.finish()
    };
    if let Some(hit) = HIGHLIGHTS.with(|cache| cache.borrow().get(key)) {
        return hit;
    }
    let result = highlight_uncached(code, lang, max_lines);
    HIGHLIGHTS.with(|cache| cache.borrow_mut().insert(key, result.clone()));
    result
}

/// Bounded by total source bytes so huge outputs cannot pin memory.
const HIGHLIGHT_CACHE_BYTES: usize = 16 * 1024 * 1024;

thread_local! {
    static HIGHLIGHTS: std::cell::RefCell<HighlightCache> = Default::default();
}

#[derive(Default)]
struct HighlightCache {
    entries: std::collections::HashMap<u64, (Option<Vec<Line<'static>>>, usize)>,
    order: std::collections::VecDeque<u64>,
    bytes: usize,
}

impl HighlightCache {
    fn get(&self, key: u64) -> Option<Option<Vec<Line<'static>>>> {
        self.entries.get(&key).map(|(lines, _)| lines.clone())
    }

    fn insert(&mut self, key: u64, lines: Option<Vec<Line<'static>>>) {
        let size = lines.as_ref().map_or(0, |lines| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .map(|span| span.content.len() + std::mem::size_of::<Span<'static>>())
                .sum::<usize>()
        }) + 64;
        if size > HIGHLIGHT_CACHE_BYTES / 4 {
            return;
        }
        if let Some((_, old)) = self.entries.insert(key, (lines, size)) {
            self.bytes -= old;
        } else {
            self.order.push_back(key);
        }
        self.bytes += size;
        while self.bytes > HIGHLIGHT_CACHE_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((_, old)) = self.entries.remove(&oldest) {
                self.bytes -= old;
            }
        }
    }
}

fn highlight_uncached(code: &str, lang: &str, max_lines: usize) -> Option<Vec<Line<'static>>> {
    let set = syntaxes();
    let syntax = set
        .find_syntax_by_token(lang)
        .or_else(|| set.find_syntax_by_extension(lang))?;

    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut out = Vec::new();
    for source_line in code.lines().take(max_lines) {
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
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn rust_code_gets_colored_spans() {
        let lines = highlight_code("fn main() {}\nlet x = 1;", "rust").expect("rust known");
        assert_eq!(plain(&lines), vec!["fn main() {}", "let x = 1;"]);
        // At least one span deviates from default (i.e. actually colored).
        assert!(lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.style.fg.is_some()));
    }

    #[test]
    fn file_extensions_select_syntax() {
        for (extension, language, source) in [
            ("rs", "rust", "fn main() {}"),
            ("lua", "lua", "local x = 42"),
        ] {
            let lines = highlight_code(source, extension).expect("known file extension");
            assert_eq!(lines, highlight_code(source, language).unwrap());
            assert!(lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.fg.is_some()));
        }
        assert!(highlight_code("plain text", "unknown_extension").is_none());
    }

    #[test]
    fn unknown_language_falls_back() {
        assert!(highlight_code("whatever", "notalanguage").is_none());
        assert!(highlight_code("whatever", "").is_none());
    }

    #[test]
    fn limited_highlighting_matches_full_prefix() {
        for source in [
            "",
            "/* Unicode 界🙂\ncontinued comment\n*/\nlet café = \"é\";\n",
            "\n\r\nlet x = 1;\r\n",
        ] {
            let full = highlight_code(source, "rust").unwrap();
            for limit in [0, 1, 2, 3, 4, usize::MAX] {
                let bounded = highlight_code_limited(source, "rust", limit).unwrap();
                assert_eq!(bounded, full[..full.len().min(limit)]);
            }
        }
        for language in ["", "notalanguage"] {
            for limit in [0, 1, usize::MAX] {
                assert!(highlight_code_limited("界\ntext", language, limit).is_none());
            }
        }
    }

    #[test]
    fn cached_results_match_uncached_and_stay_bounded() {
        let source = "fn main() {}\nlet x = 1;\n";
        for limit in [1, usize::MAX] {
            let first = highlight_code_limited(source, "rust", limit);
            assert_eq!(first, highlight_uncached(source, "rust", limit));
            assert_eq!(highlight_code_limited(source, "rust", limit), first);
        }
        assert!(highlight_code_limited("x", "notalanguage", 1).is_none());
        let mut cache = HighlightCache::default();
        let line = vec![Line::raw("x".repeat(1024))];
        for key in 0..40_000 {
            cache.insert(key, Some(line.clone()));
        }
        assert!(cache.bytes <= HIGHLIGHT_CACHE_BYTES);
        assert_eq!(cache.entries.len(), cache.order.len());
        assert!(cache.get(39_999).is_some() && cache.get(0).is_none());
    }

    #[test]
    fn text_is_preserved_exactly() {
        let src = "def f(x):\n    return x * 2";
        let lines = highlight_code(src, "python").expect("python known");
        assert_eq!(plain(&lines).join("\n"), src);
    }
}
