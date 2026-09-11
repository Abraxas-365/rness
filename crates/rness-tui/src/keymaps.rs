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

/// A configured component shortcut, shared by dispatch and help.
#[derive(Clone, Copy)]
pub struct ComponentBinding {
    pub action: &'static str,
    pub chord: Chord,
}

impl ComponentBinding {
    pub fn matches(&self, action: &str, key: &KeyEvent) -> bool {
        self.action == action && self.matches_key(key)
    }

    pub fn matches_key(&self, key: &KeyEvent) -> bool {
        use crossterm::event::KeyModifiers;
        if self.chord.code != key.code { return false; }
        match self.action {
            "delete_previous" | "cursor_left" | "cursor_right" | "cursor_home" | "cursor_end" | "completion_dismiss" => true,
            "cursor_up" | "cursor_down" => !key.modifiers.contains(KeyModifiers::SHIFT),
            "newline" => key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT),
            "submit" => !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT),
            _ => self.chord.matches(key),
        }
    }

    pub fn help(&self, scope: &str) -> String {
        format!("{scope}: {:?} → {} (when applicable)", self.chord, self.action)
    }
}

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

/// Input ownership is resolved before binding priority.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    Global,
    Promptbox,
    Messagebox,
    App(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindingLayer {
    PluginDefault,
    CoreDefault,
    User,
}

#[derive(Debug, Clone)]
pub struct ScopedBinding {
    pub owner: String,
    pub scope: Scope,
    pub chord: Chord,
    pub action: String,
    pub layer: BindingLayer,
}

/// Rebuilt from declarations on publication; never mutates another owner's mapping.
#[derive(Debug, Default)]
pub struct ScopedKeymap {
    pub generation: u64,
    layers: HashMap<(Scope, Chord), BindingLayer>,
    bindings: HashMap<(Scope, Chord), String>,
    pub diagnostics: Vec<String>,
}

impl ScopedKeymap {
    pub fn resolve(declarations: &[ScopedBinding]) -> Result<Self, Vec<String>> {
        let mut groups: HashMap<(Scope, Chord), Vec<&ScopedBinding>> = HashMap::new();
        for binding in declarations {
            groups.entry((binding.scope.clone(), binding.chord)).or_default().push(binding);
        }
        let mut result = Self::default();
        let mut errors = Vec::new();
        for (key, candidates) in groups {
            let highest = candidates.iter().map(|b| b.layer).max().expect("nonempty group");
            let winners: Vec<_> = candidates.iter().filter(|b| b.layer == highest).collect();
            let first = winners[0];
            let conflict = winners.iter().any(|b| b.action != first.action);
            if conflict {
                let mut owners: Vec<_> = winners.iter().map(|b| b.owner.as_str()).collect();
                owners.sort_unstable();
                owners.dedup();
                let message = format!("binding conflict in {:?} for {:?}: {}", key.0, key.1, owners.join(", "));
                if highest == BindingLayer::User { errors.push(message); }
                else { result.diagnostics.push(message); }
                continue;
            }
            for candidate in candidates.iter().filter(|b| b.layer < highest) {
                result.diagnostics.push(format!("binding for {} in {:?} is shadowed by {}", candidate.owner, key.0, first.owner));
            }
            result.layers.insert(key.clone(), highest);
            result.bindings.insert(key, first.action.clone());
        }
        result.diagnostics.sort();
        errors.sort();
        if errors.is_empty() { Ok(result) } else { Err(errors) }
    }

    pub fn layer(&self, scope: &Scope, key: &KeyEvent) -> Option<BindingLayer> {
        let chord = Chord::of(key);
        self.layers.get(&(scope.clone(), chord)).or_else(|| {
            if matches!(scope, Scope::App(_) | Scope::Global) { None } else { self.layers.get(&(Scope::Global, chord)) }
        }).copied()
    }

    pub fn host_help(&self, host: &KeymapState) -> Vec<String> {
        let mut lines: Vec<_> = host.entries().into_iter().map(|(chord, action)| {
            let replacement = self.bindings.get(&(Scope::Global, chord))
                .filter(|_| self.layers.get(&(Scope::Global, chord)) == Some(&BindingLayer::User));
            match replacement {
                Some(name) => format!("{chord:?} → {name} (shadows core.{})", action.name()),
                None => format!("{chord:?} → core.{}", action.name()),
            }
        }).collect();
        lines.sort();
        lines
    }

    pub fn effective_help(&self) -> Vec<String> {
        let mut lines: Vec<_> = self.bindings.iter().map(|((scope, chord), action)| format!("{scope:?} {chord:?} → {action}")).collect();
        lines.sort();
        lines.extend(self.diagnostics.iter().cloned());
        lines
    }

    pub fn lookup_exact(&self, scope: &Scope, key: &KeyEvent) -> Option<&str> {
        self.bindings.get(&(scope.clone(), Chord::of(key))).map(String::as_str)
    }

    pub fn lookup(&self, scope: &Scope, key: &KeyEvent) -> Option<&str> {
        let chord = Chord::of(key);
        self.bindings.get(&(scope.clone(), chord)).or_else(|| {
            if matches!(scope, Scope::App(_) | Scope::Global) { None }
            else { self.bindings.get(&(Scope::Global, chord)) }
        }).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn scoped_bindings_respect_focus_priority_and_modal_capture() {
        let binding = |scope, layer, action: &str| ScopedBinding {
            owner: action.into(), scope, layer, action: action.into(), chord: Chord::parse("<F6>").unwrap(),
        };
        let declarations = vec![
            binding(Scope::Global, BindingLayer::User, "global"),
            binding(Scope::Promptbox, BindingLayer::PluginDefault, "insert"),
            binding(Scope::Messagebox, BindingLayer::CoreDefault, "core"),
            binding(Scope::Messagebox, BindingLayer::PluginDefault, "plugin"),
        ];
        let map = ScopedKeymap::resolve(&declarations).unwrap();
        let key = KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE);
        assert_eq!(map.lookup(&Scope::Promptbox, &key), Some("insert"));
        assert_eq!(map.lookup(&Scope::Messagebox, &key), Some("core"));
        assert_eq!(map.lookup(&Scope::App("panel".into()), &key), None);
        assert_eq!(map.diagnostics.len(), 1);
        let rebuilt = ScopedKeymap::resolve(&declarations[..1]).unwrap();
        assert_eq!(rebuilt.lookup(&Scope::Promptbox, &key), Some("global"));
    }

    #[test]
    fn scoped_conflicts_are_order_independent_and_user_conflicts_fail() {
        let mut bindings: Vec<_> = ["a", "b"].into_iter().map(|owner| ScopedBinding {
            owner: owner.into(), action: format!("{owner}.open"), scope: Scope::Promptbox,
            chord: Chord::parse("f6").unwrap(), layer: BindingLayer::PluginDefault,
        }).collect();
        let key = KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE);
        let map = ScopedKeymap::resolve(&bindings).unwrap();
        assert_eq!(map.lookup(&Scope::Promptbox, &key), None);
        bindings.reverse();
        assert_eq!(map.diagnostics, ScopedKeymap::resolve(&bindings).unwrap().diagnostics);
        bindings.iter_mut().for_each(|b| b.layer = BindingLayer::User);
        assert!(ScopedKeymap::resolve(&bindings).is_err());
        bindings[1].action = bindings[0].action.clone();
        assert!(ScopedKeymap::resolve(&bindings).is_ok());
    }

    #[test]
    fn host_help_reports_user_shadowing_and_disabled_keys() {
        let host = KeymapState::stock();
        host.unset(Chord::parse("ctrl+d").unwrap());
        let map = ScopedKeymap::resolve(&[ScopedBinding {
            owner: "user".into(), scope: Scope::Global, chord: Chord::parse("pageup").unwrap(),
            action: "review.inspect".into(), layer: BindingLayer::User,
        }]).unwrap();
        let help = map.host_help(&host).join("\n");
        assert!(help.contains("review.inspect (shadows core.scroll_up_page)"));
        assert!(!help.contains("→ core.quit"));
    }

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
