//! Terminal-safe text. Ratatui writes a cell's symbol verbatim, so any
//! control character (ESC/OSC/CSI, C1, CR, BS, BEL, NUL, TAB) that reaches
//! a buffer cell is executed by the user's terminal: title hijack, leaving
//! the alternate screen, clearing it, OSC 52 clipboard writes, cursor
//! desync. Two layers keep it out:
//! - [`sanitize`] / [`sanitize_text`] at the text sources (precise: drops
//!   whole escape sequences, expands tabs to the right columns);
//!   [`sanitize_markdown`] keeps tabs for the markdown parser (they are
//!   CommonMark syntax) and the renderer expands them per text event
//!   ([`expand_tabs`]);
//! - [`scrub_buffer`] at the final buffer (guard for unlisted paths).

use std::borrow::Cow;

use ratatui::buffer::Buffer;

/// Bidirectional override/embedding/isolate controls. They are not
/// `is_control()` (category Cf) but reorder the visible text (spoofing),
/// and terminals disagree on how to render them.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// A character that must never reach a terminal cell: controls (C0, DEL,
/// C1) and bidi overrides.
pub fn is_terminal_unsafe(c: char) -> bool {
    c.is_control() || is_bidi_control(c)
}

use is_terminal_unsafe as is_unsafe;

/// The escape-stripping state machine shared by the single-line and the
/// multi-line sanitizer.
#[derive(Default)]
struct Sanitizer {
    col: usize,
    /// 0 none, 1 after ESC, 2 CSI, 3 OSC/string, 4 ESC inside OSC,
    /// 5 ESC intermediates (`ESC ( B`, `ESC # 8`, `ESC SP F`…).
    escape: u8,
    /// Pass `\t` through instead of expanding it (markdown source).
    keep_tabs: bool,
}

impl Sanitizer {
    /// Ends the current line: an unterminated escape stops here so it
    /// cannot swallow the rest of a message, and tab columns restart.
    fn newline(&mut self) {
        self.escape = 0;
        self.col = 0;
    }

    fn push(&mut self, ch: char, out: &mut String) {
        match self.escape {
            1 => {
                self.escape = match ch {
                    '[' => 2,
                    // OSC, DCS, SOS, PM, APC: a string until BEL or ST.
                    ']' | 'P' | 'X' | '^' | '_' => 3,
                    // nF: intermediates, then one final byte (charset
                    // designation `ESC ( B`, DEC tests `ESC # 8`…).
                    '\u{20}'..='\u{2f}' => 5,
                    _ => 0,
                };
                return;
            }
            5 => {
                match ch {
                    '\u{20}'..='\u{2f}' => {}
                    '\u{30}'..='\u{7e}' => self.escape = 0,
                    // Not a final byte: the sequence is malformed; end it
                    // and treat the character as text.
                    _ => {
                        self.escape = 0;
                        self.push(ch, out);
                    }
                }
                return;
            }
            2 => {
                match ch {
                    // Parameters and intermediates.
                    '\u{20}'..='\u{3f}' => {}
                    '\u{40}'..='\u{7e}' => self.escape = 0,
                    // Not a CSI byte: the sequence is malformed (often a
                    // stray C1 from mis-decoded text); end it and treat the
                    // character as text (an ESC starts a new sequence).
                    _ => {
                        self.escape = 0;
                        self.push(ch, out);
                    }
                }
                return;
            }
            3 => {
                if ch == '\u{7}' || ch == '\u{9c}' {
                    self.escape = 0;
                } else if ch == '\u{1b}' {
                    self.escape = 4;
                }
                return;
            }
            4 => {
                self.escape = if ch == '\\' { 0 } else { 3 };
                return;
            }
            _ => {}
        }
        match ch {
            '\u{1b}' => self.escape = 1,
            // C1 CSI: 8-bit `ESC [`; its parameters and final byte go too.
            '\u{9b}' => self.escape = 2,
            '\t' if self.keep_tabs => out.push('\t'),
            '\t' => {
                let next = (self.col / 8 + 1) * 8;
                for _ in self.col..next {
                    out.push(' ');
                }
                self.col = next;
            }
            c if is_unsafe(c) => {}
            c => {
                out.push(c);
                self.col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
    }
}

/// Terminal-safe cell text for one line: expand tabs (8-col stops) and
/// drop control chars / ANSI escapes / bidi overrides. Raw tool output
/// otherwise desyncs ratatui's cell accounting — overlapping glyphs and
/// stale-frame ghosting.
pub fn sanitize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut state = Sanitizer::default();
    for ch in line.chars() {
        state.push(ch, &mut out);
    }
    out
}

/// Byte-level fast check: does `text` contain any character that
/// [`sanitize_text`] would change? Controls other than `\n` (C0, DEL), C1
/// (UTF-8 `C2 80..9F`) and bidi controls (`E2 80 AA..AE`, `E2 81 A6..A9`).
/// No char decoding, so clean text costs one pass over the bytes.
fn needs_sanitizing(text: &str) -> bool {
    needs_sanitizing_except(text, b'\n')
}

/// [`needs_sanitizing`] that also accepts `\t` ([`sanitize_markdown`]).
fn needs_sanitizing_markdown(text: &str) -> bool {
    needs_sanitizing_except(text, b'\t')
}

fn needs_sanitizing_except(text: &str, allowed: u8) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| match b {
        b'\n' => false,
        b if b == allowed => false,
        0x00..=0x1f | 0x7f => true,
        0xc2 => matches!(bytes.get(i + 1), Some(0x80..=0x9f)),
        0xe2 => matches!(
            (bytes.get(i + 1), bytes.get(i + 2)),
            (Some(0x80), Some(0xaa..=0xae)) | (Some(0x81), Some(0xa6..=0xa9))
        ),
        _ => false,
    })
}

/// Multi-line [`sanitize`]: keeps `\n`, turns `\r\n` and a lone `\r` into
/// `\n` (CommonMark line endings), restarts tab columns per line and ends
/// any unterminated escape at the line break. Borrows when the text is
/// already clean (the common case; no allocation).
pub fn sanitize_text(text: &str) -> Cow<'_, str> {
    if !needs_sanitizing(text) {
        return Cow::Borrowed(text);
    }
    sanitize_lines(text, Sanitizer::default())
}

/// [`sanitize_text`] for markdown source: drops escapes and controls but
/// keeps `\t`, which is CommonMark syntax (list nesting, indented code,
/// marker spacing). Expanding tabs to 8-column stops before parsing would
/// change the structure (CommonMark uses 4-column tab stops), so the
/// renderer expands them in the parsed text instead ([`expand_tabs`]).
/// Borrows when the text is clean apart from tabs.
pub fn sanitize_markdown(text: &str) -> Cow<'_, str> {
    if !needs_sanitizing_markdown(text) {
        return Cow::Borrowed(text);
    }
    sanitize_lines(
        text,
        Sanitizer {
            keep_tabs: true,
            ..Default::default()
        },
    )
}

/// Expand tabs to 8-column stops, restarting columns at each `\n`
/// (columns count display width). Borrows when there is no tab. The
/// input must already be sanitized.
pub fn expand_tabs(text: &str) -> Cow<'_, str> {
    if !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    sanitize_lines(text, Sanitizer::default())
}

fn sanitize_lines(text: &str, mut state: Sanitizer) -> Cow<'_, str> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                state.newline();
                out.push('\n');
            }
            '\n' => {
                state.newline();
                out.push('\n');
            }
            ch => state.push(ch, &mut out),
        }
    }
    Cow::Owned(out)
}

/// Last line of defence: no control character ever leaves the process in
/// a cell symbol. Unsafe characters are removed from each cell (a cell
/// left empty becomes a space). Cannot repair the column drift a tab or
/// CR would have caused — the source layer does that — but it removes the
/// terminal-takeover class entirely: an ESC byte never reaches the
/// terminal, at most its printable payload does.
pub fn scrub_buffer(buf: &mut Buffer) {
    for cell in buf.content.iter_mut() {
        if cell.symbol().chars().any(is_unsafe) {
            let clean: String = cell.symbol().chars().filter(|c| !is_unsafe(*c)).collect();
            cell.set_symbol(if clean.is_empty() { " " } else { &clean });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(text: &str) -> String {
        sanitize_text(text).into_owned()
    }

    #[test]
    fn strips_title_alt_screen_and_clear_sequences() {
        assert_eq!(clean("a\x1b]0;EVILTITLE\x07b"), "ab");
        assert_eq!(clean("a\x1b[?1049lb"), "ab");
        assert_eq!(clean("a\x1b[2J\x1b[Hb"), "ab");
        assert_eq!(clean("a\x1bcb"), "ab");
        assert_eq!(sanitize("a\x1b]0;EVILTITLE\x07b"), "ab");
    }

    #[test]
    fn osc_terminators_and_unterminated_strings() {
        // ST (ESC \) and the C1 ST terminate.
        assert_eq!(
            clean("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
        assert_eq!(clean("a\x1b]0;t\u{9c}b"), "ab");
        // Unterminated OSC 52 consumes only to the end of its line.
        assert_eq!(clean("a\x1b]52;c;ZXZpbA==\nnext line"), "a\nnext line");
        assert_eq!(sanitize("a\x1b]52;c;ZXZpbA=="), "a");
        // An escape split by a newline ends at the newline.
        assert_eq!(clean("x\x1b[\n31mred"), "x\n31mred");
        // DCS / APC strings are dropped whole.
        assert_eq!(clean("a\x1bP1$r\x1b\\b\x1b_apc\x07c"), "abc");
    }

    #[test]
    fn c1_controls_drop_only_the_control() {
        assert_eq!(clean("a\u{85}b\u{90}c\u{9c}d"), "abcd");
    }

    #[test]
    fn c1_csi_is_dropped_like_esc_bracket() {
        assert_eq!(clean("a\u{9b}31mb"), "ab");
        assert_eq!(clean("a\u{9b}?1049lb\u{9b}2Jc"), "abc");
        assert_eq!(sanitize("\u{9b}1;31mred\u{9b}0m"), "red");
        assert_eq!(sanitize_markdown("x\u{9b}31m\ty"), "x\ty");
        // A stray C1 CSI before non-CSI text does not swallow the text.
        assert_eq!(clean("a\u{9b}漢字 b"), "a漢字 b");
        assert_eq!(clean("a\x1b[1;漢b"), "a漢b");
        assert_eq!(clean("a\u{9b}1\x1b[31mb"), "ab");
    }

    #[test]
    fn esc_intermediate_sequences_are_dropped_whole() {
        // Charset designation (G0..G3), DEC line attributes/tests, S7C1T.
        assert_eq!(clean("a\x1b(Bb\x1b)0c\x1b*Ad\x1b+<e"), "abcde");
        assert_eq!(clean("a\x1b#8b\x1b#6c\x1b F\x1b%Gd"), "abcd");
        // Several intermediates before the final byte.
        assert_eq!(clean("a\x1b(!@b"), "ab");
        assert_eq!(sanitize("\x1b(0lqk\x1b(B"), "lqk");
        // A malformed nF sequence ends at a non-final byte, which stays.
        assert_eq!(clean("a\x1b(漢b"), "a漢b");
        assert_eq!(clean("a\x1b(\nb"), "a\nb");
    }

    #[test]
    fn carriage_returns_backspace_bell_nul() {
        assert_eq!(clean("one\r\ntwo"), "one\ntwo");
        assert_eq!(clean("10%\r50%\rdone"), "10%\n50%\ndone");
        assert_eq!(clean("ab\x08\x08XY\x07z\x00!"), "abXYz!");
        assert_eq!(sanitize("bell\u{7}cr\r"), "bellcr");
    }

    #[test]
    fn tabs_expand_per_line_with_wide_chars() {
        assert_eq!(clean("\tx"), "        x");
        assert_eq!(clean("1234567\tx"), "1234567 x");
        assert_eq!(clean("12345678\tx"), "12345678        x");
        assert_eq!(clean("界\tx"), "界      x");
        assert_eq!(clean("1234\n\tx"), "1234\n        x");
    }

    #[test]
    fn bidi_overrides_are_removed() {
        assert_eq!(clean("a\u{202e}desrever\u{202c}b"), "adesreverb");
        assert_eq!(clean("\u{2066}iso\u{2069}"), "iso");
        // Joiners and combining marks are legitimate text.
        assert_eq!(clean("👨\u{200d}👩 e\u{301}"), "👨\u{200d}👩 e\u{301}");
    }

    #[test]
    fn clean_text_is_borrowed() {
        for text in ["", "plain ascii\nlines", "漢字 👍🏽\n e\u{301}"] {
            assert!(matches!(sanitize_text(text), Cow::Borrowed(_)), "{text:?}");
        }
        assert!(matches!(sanitize_text("a\tb"), Cow::Owned(_)));
    }

    #[test]
    fn byte_fast_path_matches_char_predicate() {
        for c in (0u32..0x3000).filter_map(char::from_u32) {
            let text = format!("x{c}y");
            assert_eq!(
                needs_sanitizing(&text),
                c != '\n' && is_unsafe(c),
                "U+{:04X}",
                c as u32
            );
            assert_eq!(
                needs_sanitizing_markdown(&text),
                c != '\n' && c != '\t' && is_unsafe(c),
                "U+{:04X}",
                c as u32
            );
        }
    }

    #[test]
    fn markdown_variant_keeps_tabs_and_strips_the_rest() {
        // Tabs are markdown syntax: kept, and the text still borrows.
        for text in ["-\titem", "- a\n\t- b", "\tcode\n"] {
            assert!(
                matches!(sanitize_markdown(text), Cow::Borrowed(t) if t == text),
                "{text:?}"
            );
        }
        assert_eq!(
            sanitize_markdown("-\ta\x1b[31m\tb\r\n\x1b]0;t\x07\tc\u{202e}"),
            "-\ta\tb\n\tc"
        );
        // Expansion after parsing matches the plain sanitizer.
        for text in ["\tx", "12345678\tx", "界\tx", "1234\n\tx", "a\x1b[1m\tb"] {
            assert_eq!(
                expand_tabs(&sanitize_markdown(text)),
                clean(text),
                "{text:?}"
            );
        }
        assert!(matches!(expand_tabs("no tabs"), Cow::Borrowed(_)));
    }

    #[test]
    fn idempotent_and_never_emits_controls() {
        // Deterministic pseudo-random strings (xorshift) over a hostile alphabet.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let alphabet: Vec<char> =
            "\x1b[]0;?\x07\\\r\n\t\x08\x00ab漢 \u{9b}\u{9c}\u{85}\u{202e}mPX(#B"
                .chars()
                .collect();
        for _ in 0..2000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 64) as usize;
            let mut text = String::new();
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                text.push(alphabet[(seed % alphabet.len() as u64) as usize]);
            }
            let once = clean(&text);
            assert!(
                once.chars().all(|c| c == '\n' || !is_unsafe(c)),
                "{text:?} -> {once:?}"
            );
            assert_eq!(clean(&once), once, "{text:?}");
            let line = sanitize(&text);
            assert!(line.chars().all(|c| !is_unsafe(c)), "{text:?} -> {line:?}");
            let markdown = sanitize_markdown(&text);
            assert!(
                markdown
                    .chars()
                    .all(|c| c == '\n' || c == '\t' || !is_unsafe(c)),
                "{text:?} -> {markdown:?}"
            );
            assert_eq!(sanitize_markdown(&markdown), markdown, "{text:?}");
            assert_eq!(expand_tabs(&markdown), once, "{text:?}");
        }
    }

    #[test]
    fn scrub_buffer_replaces_forged_cells() {
        use ratatui::layout::Rect;
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        buf[(0, 0)].set_symbol("\x1b");
        buf[(1, 0)].set_symbol("a\u{202e}");
        buf[(2, 0)].set_symbol("\t");
        buf[(3, 0)].set_symbol("b");
        scrub_buffer(&mut buf);
        let symbols: Vec<&str> = buf.content.iter().map(|c| c.symbol()).collect();
        assert_eq!(symbols, vec![" ", "a", " ", "b"]);
    }
}
