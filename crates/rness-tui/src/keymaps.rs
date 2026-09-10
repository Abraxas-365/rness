//! The host's rebindable keymap: chord → named action.
//!
//! Mechanism/policy split, applied to keys: the TUI executes ACTIONS
//! (scroll, cancel, quit); WHICH chord fires which action is a table the
//! composition root seeds (visibly, in one place) and Lua may rewrite at
//! runtime (`rness.keymaps.set`) — the Neovim model. No hidden defaults:
//! the seed below IS the stock keymap, greppable, and `list()` shows the
//! live table.
//!
//! Scope: keys the host handles AFTER focused components pass. The
//! editor's own emacs-style bindings and app toggle keys are separate
//! seams (input.rs, ext_apps.rs).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crossterm::event::KeyEvent;

use crate::keys::Chord;

/// Every rebindable host action, by wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAction {
    ScrollUpLine,
    ScrollDownLine,
    ScrollUpPage,
    ScrollDownPage,
    CancelOrQuit,
    Quit,
}

impl HostAction {
    /// The names Lua uses. Fail-loud on unknowns at the API edge.
    pub fn from_name(name: &str) -> Option<HostAction> {
        Some(match name {
            "scroll_up" => HostAction::ScrollUpLine,
            "scroll_down" => HostAction::ScrollDownLine,
            "scroll_up_page" => HostAction::ScrollUpPage,
            "scroll_down_page" => HostAction::ScrollDownPage,
            "cancel_or_quit" => HostAction::CancelOrQuit,
            "quit" => HostAction::Quit,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            HostAction::ScrollUpLine => "scroll_up",
            HostAction::ScrollDownLine => "scroll_down",
            HostAction::ScrollUpPage => "scroll_up_page",
            HostAction::ScrollDownPage => "scroll_down_page",
            HostAction::CancelOrQuit => "cancel_or_quit",
            HostAction::Quit => "quit",
        }
    }
}

/// Shared chord→action table. The shell reads it per keypress; the host
/// (Lua bridge) rewrites it on plugin load / hot reload.
#[derive(Clone)]
pub struct KeymapState {
    inner: Arc<RwLock<HashMap<Chord, HostAction>>>,
}

impl KeymapState {
    /// The stock keymap — the ONE place the shipped bindings live.
    pub fn stock() -> Self {
        let mut m = HashMap::new();
        let mut bind = |s: &str, a: HostAction| {
            m.insert(Chord::parse(s).expect("stock chord parses"), a);
        };
        bind("ctrl+c", HostAction::CancelOrQuit);
        bind("ctrl+d", HostAction::Quit);
        bind("pageup", HostAction::ScrollUpPage);
        bind("pagedown", HostAction::ScrollDownPage);
        bind("shift+up", HostAction::ScrollUpLine);
        bind("shift+down", HostAction::ScrollDownLine);
        Self { inner: Arc::new(RwLock::new(m)) }
    }

    pub fn lookup(&self, key: &KeyEvent) -> Option<HostAction> {
        self.inner.read().expect("keymap lock").get(&Chord::of(key)).copied()
    }

    /// Bind a chord to an action. Replaces whatever held that chord.
    pub fn set(&self, chord: Chord, action: HostAction) {
        self.inner.write().expect("keymap lock").insert(chord, action);
    }

    /// Remove a binding entirely.
    pub fn unset(&self, chord: Chord) {
        self.inner.write().expect("keymap lock").remove(&chord);
    }

    /// Snapshot for introspection (`rness.keymaps.list`, --dump-config).
    pub fn entries(&self) -> Vec<(Chord, HostAction)> {
        let mut v: Vec<_> =
            self.inner.read().expect("keymap lock").iter().map(|(c, a)| (*c, *a)).collect();
        v.sort_by_key(|(_, a)| a.name());
        v
    }

    /// Rebuild as stock + `binds` applied in order (`None` action =
    /// unbind). This is the host↔Lua sync point: plugins declare binds,
    /// a reload recomputes from stock so removed plugins revert.
    /// Returns a message per rejected bind — the host logs them loudly.
    pub fn rebuild(&self, binds: &[(String, Option<String>)]) -> Vec<String> {
        let mut errors = Vec::new();
        let fresh = KeymapState::stock();
        for (chord_s, action_s) in binds {
            let Some(chord) = Chord::parse(chord_s) else {
                errors.push(format!("keymaps: unparseable chord '{chord_s}'"));
                continue;
            };
            match action_s {
                None => fresh.unset(chord),
                Some(name) => match HostAction::from_name(name) {
                    Some(action) => fresh.set(chord, action),
                    None => errors.push(format!("keymaps: unknown action '{name}'")),
                },
            }
        }
        let computed = std::mem::take(&mut *fresh.inner.write().expect("keymap lock"));
        *self.inner.write().expect("keymap lock") = computed;
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn stock_bindings_resolve() {
        let km = KeymapState::stock();
        assert_eq!(km.lookup(&KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)), None);
        assert_eq!(km.lookup(&KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT)), None);
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(km.lookup(&pgup), Some(HostAction::ScrollUpPage));
        let none = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        assert_eq!(km.lookup(&none), None);
    }

    #[test]
    fn rebind_and_unbind() {
        let km = KeymapState::stock();
        km.set(Chord::parse("ctrl+k").unwrap(), HostAction::ScrollUpLine);
        let ctrl_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(km.lookup(&ctrl_k), Some(HostAction::ScrollUpLine));

        km.unset(Chord::parse("ctrl+d").unwrap());
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(km.lookup(&ctrl_d), None);
    }

    #[test]
    fn action_names_round_trip() {
        for a in [
            HostAction::ScrollUpLine,
            HostAction::ScrollDownLine,
            HostAction::ScrollUpPage,
            HostAction::ScrollDownPage,
            HostAction::CancelOrQuit,
            HostAction::Quit,
        ] {
            assert_eq!(HostAction::from_name(a.name()), Some(a));
        }
        assert_eq!(HostAction::from_name("teleport"), None);
    }
}
