//! Session title mechanism: terminal-safe normalization and human-prompt
//! extraction. Everything else — when to title, the fallback, the model
//! call and its prompt — is Lua policy (`flavors/default/plugins/title.lua`).

use rness_protocol::events::{ContentPart, MessageSource, SessionEvent, TitleSource, UserIntent};

/// Default and hard cap on a stored title, in bytes (dsh `maxTitleBytes`).
pub const DEFAULT_MAX_BYTES: usize = 80;
pub const MAX_BYTES_LIMIT: usize = 200;

pub fn validate_max_bytes(max_bytes: usize) -> Result<(), String> {
    if max_bytes == 0 || max_bytes > MAX_BYTES_LIMIT {
        return Err(format!("title max_bytes must be 1..={MAX_BYTES_LIMIT}"));
    }
    Ok(())
}

pub(crate) fn is_control(c: char) -> bool {
    // C0/C1 controls (whitespace is collapsed separately) and invisible /
    // directional marks that can make a displayed title deceptive.
    (c.is_control() && !c.is_whitespace())
        || matches!(c,
            '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}')
}

/// Remove terminal escape sequences and controls; one whitespace-collapsed line.
fn clean(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // OSC: ESC ] … (BEL | ESC \ | end)
            '\u{1b}' if chars.peek() == Some(&']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                        if c == '\u{1b}' {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            '\u{9d}' => {
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
            // CSI: ESC [ params intermediates final
            '\u{1b}' if chars.peek() == Some(&'[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            '\u{9b}' => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // Other two-byte ESC sequences.
            '\u{1b}' => {
                if chars.peek().is_some_and(|c| ('@'..='_').contains(c)) {
                    chars.next();
                }
            }
            c if c.is_whitespace() => out.push(' '),
            c if is_control(c) => {}
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, max_bytes: usize) -> &str {
    &text[..text.floor_char_boundary(max_bytes.min(text.len()))]
}

/// A terminal-safe one-line title within `max_bytes` (may be empty).
pub fn normalize(input: &str, max_bytes: usize) -> String {
    truncate(&clean(input), max_bytes).trim_end().to_owned()
}

/// Text of a human-typed prompt (not injected context, job notices, …).
pub fn human_text(event: &SessionEvent) -> Option<String> {
    let SessionEvent::UserMessage(m) = event else {
        return None;
    };
    if m.intent == UserIntent::Inject
        || !matches!(m.source, None | Some(MessageSource::ExternalPrompt { .. }))
    {
        return None;
    }
    let text = m
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!clean(&text).is_empty()).then_some(text)
}

/// Latest title in a session's own events: `(text, source)`.
pub fn current<'a>(
    events: impl DoubleEndedIterator<Item = &'a SessionEvent>,
) -> Option<(String, TitleSource)> {
    events.rev().find_map(|e| match e {
        SessionEvent::Title(t) => Some((t.title.clone(), t.source)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_strips_escapes_and_controls() {
        let dirty =
            "  Fix\u{1b}[31m the\u{1b}]0;evil\u{7} \u{202E}bug\u{0}\n\tnow \u{1b}]2;x\u{1b}\\!";
        assert_eq!(normalize(dirty, 80), "Fix the bug now !");
        assert_eq!(normalize("日本語のタイトル", 7), "日本"); // never splits a code point
        assert_eq!(normalize("\u{1b}[1m\u{1b}[0m", 80), "");
    }

    #[test]
    fn max_bytes_validation() {
        assert!(validate_max_bytes(80).is_ok());
        assert!(validate_max_bytes(0).is_err());
        assert!(validate_max_bytes(201).is_err());
    }
}
