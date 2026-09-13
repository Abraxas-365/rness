/// Terminal-safe cell text: expand tabs (8-col stops) and drop control
/// chars / ANSI escapes. Raw tool output otherwise desyncs ratatui's
/// cell accounting — overlapping glyphs and stale-frame ghosting.
pub fn sanitize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut col = 0usize;
    let mut escape = 0u8;
    for ch in line.chars() {
        match escape {
            1 => {
                escape = match ch {
                    '[' => 2,
                    ']' => 3,
                    _ => 0,
                };
                continue;
            }
            2 => {
                if ('\u{40}'..='\u{7e}').contains(&ch) {
                    escape = 0;
                }
                continue;
            }
            3 => {
                if ch == '\u{7}' {
                    escape = 0;
                } else if ch == '\u{1b}' {
                    escape = 4;
                }
                continue;
            }
            4 => {
                escape = if ch == '\\' { 0 } else { 3 };
                continue;
            }
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
