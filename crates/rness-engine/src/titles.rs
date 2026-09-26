//! Session title mechanism: terminal-safe normalization, human-prompt
//! extraction, and a single bounded model request (dsh `session-title` +
//! `session-title-llm`). *When* to title — fallback, first prompt, every
//! prompt — is plugin policy (`flavors/default/plugins/session-title.lua`).

use rness_protocol::events::{ContentPart, MessageSource, SessionEvent, TitleSource, UserIntent};
use serde::Deserialize;

/// Default and hard cap on a stored title, in bytes (dsh `maxTitleBytes`).
pub const DEFAULT_MAX_BYTES: usize = 80;
pub const MAX_BYTES_LIMIT: usize = 200;

/// Which human prompts a title request is generated from.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TitlePrompts {
    /// The session's first human prompt (dsh `first-prompt`).
    #[default]
    First,
    /// Every human prompt, oldest first (dsh `all-prompts`).
    All,
}

/// One title request — all fields optional (dsh base defaults).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TitleRequest {
    pub prompts: TitlePrompts,
    /// Model profile (default: the session's model).
    pub profile: Option<String>,
    /// Seconds before the request is abandoned.
    pub timeout: u64,
    pub target_words: u32,
    pub target_cjk_characters: u32,
    /// Framed-input cap; oldest prompts are dropped, then the rest trimmed
    /// to fit (dsh rejects oversized input instead).
    pub max_input_bytes: usize,
    pub max_output_tokens: u32,
    /// Cap on the returned title.
    pub max_bytes: usize,
}

impl Default for TitleRequest {
    fn default() -> Self {
        Self {
            prompts: TitlePrompts::First,
            profile: None,
            timeout: 60,
            target_words: 5,
            target_cjk_characters: 10,
            max_input_bytes: 4096,
            max_output_tokens: 64,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl TitleRequest {
    pub fn validate(&self) -> Result<(), String> {
        let positive = [
            ("timeout", self.timeout as usize),
            ("target_words", self.target_words as usize),
            ("target_cjk_characters", self.target_cjk_characters as usize),
            ("max_input_bytes", self.max_input_bytes),
            ("max_output_tokens", self.max_output_tokens as usize),
            ("max_bytes", self.max_bytes),
        ];
        for (name, value) in positive {
            if value == 0 {
                return Err(format!("title {name} must be positive"));
            }
        }
        validate_max_bytes(self.max_bytes)
    }

    pub fn system_prompt(&self) -> String {
        format!(
            "Create a concise title for an AI coding-assistant session from the supplied human messages.\n\
             Return only the title on one line, **in plain text of natural language**, with no quotes, prefix, explanation, Markdown, XML, or terminal control codes. No code is allowed.\n\
             Use the language of the messages.\n\
             Aim for about {} words in non-CJK languages or {} CJK characters.",
            self.target_words, self.target_cjk_characters
        )
    }

    /// The user message for the title request, within `max_input_bytes`:
    /// the oldest prompts are dropped first, then the oldest kept one is
    /// trimmed (JSON escaping can grow text, so shrink until it fits).
    pub fn frame(&self, prompts: &[String]) -> String {
        const HEAD: &str = "Generate the session title from this JSON array of human messages:\n";
        let render = |texts: &[&str]| {
            let messages: Vec<_> = texts.iter().map(|t| serde_json::json!({ "text": t })).collect();
            format!("{HEAD}{}", serde_json::Value::Array(messages))
        };
        let mut texts: Vec<&str> = prompts.iter().map(String::as_str).collect();
        while texts.len() > 1 && render(&texts).len() > self.max_input_bytes {
            texts.remove(0);
        }
        let full = render(&texts);
        if full.len() <= self.max_input_bytes || texts.is_empty() {
            return full;
        }
        let prompt = texts[0];
        let mut keep = self
            .max_input_bytes
            .saturating_sub(HEAD.len() + 16)
            .min(prompt.len());
        loop {
            let cut = &prompt[..prompt.floor_char_boundary(keep)];
            texts[0] = cut;
            let framed = render(&texts);
            if framed.len() <= self.max_input_bytes || cut.is_empty() {
                return framed;
            }
            keep = cut.len() * 3 / 4;
        }
    }
}

pub fn validate_max_bytes(max_bytes: usize) -> Result<(), String> {
    if max_bytes == 0 || max_bytes > MAX_BYTES_LIMIT {
        return Err(format!("title max_bytes must be 1..={MAX_BYTES_LIMIT}"));
    }
    Ok(())
}

fn is_control(c: char) -> bool {
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

/// The first `words` words of `prompt`, within `max_bytes`.
pub fn fallback(prompt: &str, words: usize, max_bytes: usize) -> String {
    let cleaned = clean(prompt);
    let head = cleaned.split(' ').take(words).collect::<Vec<_>>().join(" ");
    truncate(&head, max_bytes).trim_end().to_owned()
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
    fn fallback_takes_words_within_bytes() {
        assert_eq!(
            fallback("one two three four five six", 5, 40),
            "one two three four five"
        );
        assert_eq!(fallback("abcdefghij klmnop", 5, 12), "abcdefghij k");
        assert_eq!(fallback("a\n\nb", 5, 40), "a b");
    }

    #[test]
    fn frame_fits_the_input_budget() {
        let config = TitleRequest {
            max_input_bytes: 200,
            ..TitleRequest::default()
        };
        let long = "\"quoted\" ".repeat(100);
        let framed = config.frame(&[long]);
        assert!(framed.len() <= 200, "{}", framed.len());
        assert!(framed.contains("\\\"quoted\\\""));
        assert!(config.frame(&["short".into()]).ends_with(r#"[{"text":"short"}]"#));
        // All-prompts: the oldest prompts are dropped first.
        let many: Vec<String> = (0..20).map(|i| format!("prompt number {i}")).collect();
        let framed = config.frame(&many);
        assert!(framed.len() <= 200, "{}", framed.len());
        assert!(framed.contains("prompt number 19") && !framed.contains("prompt number 0\""));
        assert!(config.frame(&["a".into(), "b".into()]).ends_with(r#"[{"text":"a"},{"text":"b"}]"#));
    }

    #[test]
    fn request_validation() {
        assert!(TitleRequest::default().validate().is_ok());
        assert!(TitleRequest {
            max_bytes: 201,
            ..TitleRequest::default()
        }
        .validate()
        .is_err());
        assert!(TitleRequest {
            timeout: 0,
            ..TitleRequest::default()
        }
        .validate()
        .is_err());
    }
}
