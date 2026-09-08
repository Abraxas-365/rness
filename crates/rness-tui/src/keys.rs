//! One key language for the whole TUI. `"ctrl+e"`, `"shift+up"`,
//! `"pageup"`, `"f2"` — the SAME strings work for app toggle keymaps
//! (`rness.ui.app{keymap=}`) and host action bindings
//! (`rness.keymaps.set`). Parsed here, once.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A parsed chord: modifiers + normalized code. Hashable so the host
/// keymap can be a plain table lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub mods: KeyModifiers,
    pub code: KeyCode,
}

impl Chord {
    /// Parse `"ctrl+k"`, `"shift+up"`, `"pagedown"`, `"f5"`… `None` on
    /// anything unrecognized (callers decide whether that's an error).
    pub fn parse(s: &str) -> Option<Chord> {
        let mut mods = KeyModifiers::NONE;
        let mut code = None;
        for part in s.split('+') {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" => mods |= KeyModifiers::CONTROL,
                "alt" => mods |= KeyModifiers::ALT,
                "shift" => mods |= KeyModifiers::SHIFT,
                p => {
                    code = Some(match p {
                        "enter" => KeyCode::Enter,
                        "esc" => KeyCode::Esc,
                        "tab" => KeyCode::Tab,
                        "space" => KeyCode::Char(' '),
                        "up" => KeyCode::Up,
                        "down" => KeyCode::Down,
                        "left" => KeyCode::Left,
                        "right" => KeyCode::Right,
                        "pageup" => KeyCode::PageUp,
                        "pagedown" => KeyCode::PageDown,
                        "home" => KeyCode::Home,
                        "end" => KeyCode::End,
                        "backspace" => KeyCode::Backspace,
                        "delete" => KeyCode::Delete,
                        _ if p.len() == 1 => KeyCode::Char(p.chars().next().unwrap()),
                        _ if p.starts_with('f') => match p[1..].parse::<u8>() {
                            Ok(n) => KeyCode::F(n),
                            Err(_) => return None,
                        },
                        _ => return None,
                    })
                }
            }
        }
        Some(Chord { mods, code: normalize(code?) })
    }

    pub fn of(key: &KeyEvent) -> Chord {
        Chord { mods: key.modifiers, code: normalize(key.code) }
    }

    pub fn matches(&self, key: &KeyEvent) -> bool {
        *self == Chord::of(key)
    }
}

fn normalize(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shared_language() {
        let c = Chord::parse("ctrl+e").unwrap();
        assert!(c.matches(&KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)));
        assert!(!c.matches(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)));

        let c = Chord::parse("shift+up").unwrap();
        assert!(c.matches(&KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));

        let c = Chord::parse("pagedown").unwrap();
        assert!(c.matches(&KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)));

        let c = Chord::parse("f2").unwrap();
        assert!(c.matches(&KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)));

        assert!(Chord::parse("hyper+q").is_none());
        assert!(Chord::parse("fzz").is_none());
    }

    #[test]
    fn char_case_is_normalized() {
        let c = Chord::parse("ctrl+E").unwrap();
        assert!(c.matches(&KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)));
    }
}
