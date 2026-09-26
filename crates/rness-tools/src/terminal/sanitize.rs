//! Turning raw PTY bytes into text a model can read.
//!
//! Two stages:
//! - [`Sanitizer`] runs on the reader thread. It strips escape sequences
//!   (CSI, OSC, charset selection) from the stream, carrying sequences that
//!   are split across reads, and reports the events rness cares about: the
//!   controlled shell's prompt marker (`OSC 133;D;<exit>`) and a switch to
//!   the alternate screen (a full-screen program such as vim or top).
//! - [`render`] runs when a result is built. It applies what carriage
//!   return, backspace and other control characters mean on a terminal, so
//!   progress bars collapse to their final state.
//!
//! This is deliberately not a terminal emulator: line-oriented output is
//! the supported contract.

/// Longest CSI sequence kept while waiting for its final byte.
const MAX_CSI: usize = 64;
/// Longest OSC payload kept while waiting for its terminator.
const MAX_OSC: usize = 4096;

/// Something the stream said besides text. `at` counts text bytes emitted
/// by the same [`Sanitizer::push`] call before the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The controlled shell is about to print its prompt; the previous
    /// command exited with this code.
    Prompt { at: usize, exit: Option<i32> },
    /// A full-screen program switched to the alternate screen.
    AltScreen { at: usize },
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi(Vec<u8>),
    Osc(Vec<u8>),
    /// Inside OSC, just saw ESC (possible `ESC \` terminator).
    OscEsc(Vec<u8>),
    /// `ESC (` and friends take one more byte.
    Charset,
}

/// Streaming escape-sequence stripper.
#[derive(Debug, Default)]
pub struct Sanitizer {
    state: State,
}

impl Sanitizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one chunk; return its text bytes and events.
    pub fn push(&mut self, chunk: &[u8]) -> (Vec<u8>, Vec<Event>) {
        let mut text = Vec::with_capacity(chunk.len());
        let mut events = Vec::new();
        for &byte in chunk {
            self.state = match std::mem::take(&mut self.state) {
                State::Ground if byte == 0x1b => State::Esc,
                State::Ground => {
                    text.push(byte);
                    State::Ground
                }
                State::Esc => match byte {
                    b'[' => State::Csi(Vec::new()),
                    b']' => State::Osc(Vec::new()),
                    b'(' | b')' | b'*' | b'+' => State::Charset,
                    0x1b => State::Esc,
                    // Two-byte sequences (save cursor, keypad mode, ...).
                    _ => State::Ground,
                },
                State::Charset => State::Ground,
                State::Csi(mut params) => {
                    if (0x40..=0x7e).contains(&byte) {
                        if byte == b'h' && is_alt_screen(&params) {
                            events.push(Event::AltScreen { at: text.len() });
                        }
                        State::Ground
                    } else if params.len() >= MAX_CSI {
                        // Not a sequence we understand; drop it.
                        State::Ground
                    } else {
                        params.push(byte);
                        State::Csi(params)
                    }
                }
                State::Osc(mut payload) => match byte {
                    0x07 => {
                        osc_event(&payload, text.len(), &mut events);
                        State::Ground
                    }
                    0x1b => State::OscEsc(payload),
                    _ if payload.len() >= MAX_OSC => State::Ground,
                    _ => {
                        payload.push(byte);
                        State::Osc(payload)
                    }
                },
                State::OscEsc(mut payload) => {
                    if byte == b'\\' {
                        osc_event(&payload, text.len(), &mut events);
                        State::Ground
                    } else {
                        // A stray ESC inside the payload; keep scanning.
                        payload.push(0x1b);
                        payload.push(byte);
                        State::Osc(payload)
                    }
                }
            };
        }
        (text, events)
    }
}

fn is_alt_screen(params: &[u8]) -> bool {
    matches!(params, b"?1049" | b"?1047" | b"?47")
}

fn osc_event(payload: &[u8], at: usize, events: &mut Vec<Event>) {
    let Some(rest) = payload.strip_prefix(b"133;D") else {
        return;
    };
    let exit = rest
        .strip_prefix(b";")
        .and_then(|code| std::str::from_utf8(code).ok())
        .and_then(|code| code.trim().parse().ok());
    events.push(Event::Prompt { at, exit });
}

/// Apply terminal control characters to sanitized bytes:
/// - `\r\n` is a newline; a lone `\r` starts the line over (progress bars
///   keep their final state);
/// - backspace deletes the previous character;
/// - tabs and newlines stay; other control characters are dropped.
pub fn render(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    // Byte index in `out` where the current line starts.
    let mut line_start = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                out.push('\n');
                line_start = out.len();
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.truncate(line_start),
            '\u{8}' => {
                if out.len() > line_start {
                    out.pop();
                }
            }
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(chunks: &[&[u8]]) -> (String, Vec<Event>) {
        let mut sanitizer = Sanitizer::new();
        let mut text = Vec::new();
        let mut events = Vec::new();
        for chunk in chunks {
            let (bytes, evs) = sanitizer.push(chunk);
            text.extend(bytes);
            events.extend(evs);
        }
        (String::from_utf8(text).unwrap(), events)
    }

    #[test]
    fn strips_colors_cursor_moves_and_titles() {
        let (text, events) =
            clean(&[b"\x1b[1;31mred\x1b[0m \x1b[2K\x1b[?2004hok\x1b]0;title\x07\x1b(Bdone\x1b="]);
        assert_eq!(text, "red okdone");
        assert!(events.is_empty());
    }

    #[test]
    fn prompt_marker_reports_exit_code_and_position() {
        let (text, events) = clean(&[b"out\n\x1b]133;D;130\x07rness$ "]);
        assert_eq!(text, "out\nrness$ ");
        assert_eq!(
            events,
            vec![Event::Prompt {
                at: 4,
                exit: Some(130)
            }]
        );
        // ST terminator and a missing code.
        let (_, events) = clean(&[b"\x1b]133;D\x1b\\"]);
        assert_eq!(events, vec![Event::Prompt { at: 0, exit: None }]);
    }

    #[test]
    fn sequences_split_across_reads_are_carried() {
        let (text, events) = clean(&[b"a\x1b", b"[3", b"1mb\x1b]13", b"3;D;", b"0\x07c"]);
        assert_eq!(text, "abc");
        assert_eq!(
            events,
            vec![Event::Prompt {
                at: 0,
                exit: Some(0)
            }]
        );
    }

    #[test]
    fn alternate_screen_is_reported() {
        let (text, events) = clean(&[b"x\x1b[?1049hy"]);
        assert_eq!(text, "xy");
        assert_eq!(events, vec![Event::AltScreen { at: 1 }]);
    }

    #[test]
    fn runaway_sequences_do_not_grow_without_bound() {
        let mut long = b"\x1b[".to_vec();
        long.extend(std::iter::repeat_n(b'1', MAX_CSI + 10));
        long.extend(b"m-after");
        let (text, _) = clean(&[&long]);
        assert!(text.ends_with("m-after"), "{text}");
        assert!(text.len() < 20, "{text}");
    }

    #[test]
    fn render_applies_carriage_return_backspace_and_controls() {
        assert_eq!(render(b"a\r\nb\r\n"), "a\nb\n");
        assert_eq!(render(b"10%\r50%\r100%\ndone"), "100%\ndone");
        assert_eq!(render(b"cc\x08d\x07\tx"), "cd\tx");
        assert_eq!(render(b"line\n\x08x"), "line\nx");
        assert_eq!(render("héllo ✓\n".as_bytes()), "héllo ✓\n");
    }
}
