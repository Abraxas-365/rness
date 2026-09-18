//! External apps: full-screen/sidebar components driven by an outside
//! provider (Lua plugins via the host bridge — but the TUI knows nothing
//! about Lua, same purity as ext_statusline).
//!
//! Split of responsibilities:
//! - The HOST (cli) knows the app roster and owns the round-trips: it
//!   polls views, delivers keys, applies action outcomes.
//! - This module renders shared state and forwards focused keys/toggles
//!   to the host through a channel.
//!
//! Flow: toggle key (e.g. ctrl+e) → `AppsState::toggle` → `wants()`
//! selects the component in → keys forward to the host as
//! `AppEvent::Key` → host calls the VM → publishes fresh lines via
//! `AppsState::publish` → next frame renders them.

use std::sync::{Arc, RwLock};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::component::{Component, Ctx, KeyOutcome};
use crate::slots::{Slots, OVERLAY, SIDEBAR};

/// One registered external app (mirrors the provider's spec).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppInfo {
    pub name: String,
    /// "sidebar" or "overlay".
    pub slot: String,
    pub title: String,
    /// Toggle key, e.g. "ctrl+e" (parsed by [`matches_keymap`]).
    pub keymap: Option<String>,
    pub refresh_ms: Option<u64>,
    pub capture_escape: bool,
    pub config: serde_json::Value,
}

/// What the components send the host.
#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    Unload(String),
    /// App became visible; the host should push a first view.
    Shown(String),
    /// A key for the focused app: (app, key string like "j", "enter").
    Key(String, String, u64),
}

#[derive(Default)]
struct Inner {
    key_help: std::collections::HashMap<String, Vec<String>>,
    generation: u64,
    input_epoch: Option<Arc<std::sync::atomic::AtomicU64>>,
    apps: Vec<AppInfo>,
    /// app name → published lines.
    views: std::collections::HashMap<String, Vec<String>>,
    /// slot → content rows available at last render (viewport hint the
    /// host passes to apps as `ctx.rows` so they can window long lists).
    viewports: std::collections::HashMap<String, (u16, u16)>,
    /// The one visible app per slot kind (single-active keeps focus sane).
    active: Option<String>,
    session: Option<String>,
    runtime_generation: u64,
}

/// Shared cell: host writes roster + views, components read; toggle flips.
#[derive(Clone, Default)]
pub struct AppsState {
    inner: Arc<RwLock<Inner>>,
    events: Arc<RwLock<Option<tokio::sync::mpsc::UnboundedSender<AppEvent>>>>,
}

impl AppsState {
    pub fn session(&self) -> Option<String> {
        self.inner.read().expect("apps lock").session.clone()
    }

    pub fn set_session(&self, session: String) {
        let mut inner = self.inner.write().expect("apps lock");
        if inner.session.as_ref() == Some(&session) {
            return;
        }
        inner.session = Some(session);
        inner.generation = inner
            .generation
            .checked_add(1)
            .expect("app generation exhausted");
        if let Some(epoch) = &inner.input_epoch {
            epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        inner.views.clear();
        inner.active = None;
    }

    /// Snapshot context and activation under one lock, never dispatching an old
    /// key with a newly selected session's identity.
    pub fn context_if_current(&self, generation: u64, name: &str) -> Option<serde_json::Value> {
        let inner = self.inner.read().expect("apps lock");
        if inner.generation != generation || inner.active.as_deref() != Some(name) {
            return None;
        }
        let app = inner.apps.iter().find(|app| app.name == name)?;
        let mut ctx = serde_json::json!({"session":inner.session.as_ref()?});
        if let Some((rows, cols)) = inner.viewports.get(&app.slot) {
            ctx["rows"] = serde_json::json!(rows);
            ctx["cols"] = serde_json::json!(cols);
        }
        Some(ctx)
    }

    pub fn runtime_generation(&self) -> u64 {
        self.inner.read().expect("apps lock").runtime_generation
    }

    /// Reload may replace callbacks without changing any app metadata.
    pub fn set_runtime_generation(&self, generation: u64) {
        let mut inner = self.inner.write().expect("apps lock");
        if inner.runtime_generation == generation {
            return;
        }
        inner.runtime_generation = generation;
        inner.generation = inner
            .generation
            .checked_add(1)
            .expect("app generation exhausted");
        if let Some(epoch) = &inner.input_epoch {
            epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        inner.views.clear();
    }

    pub fn track_input_epoch(&self, epoch: Arc<std::sync::atomic::AtomicU64>) {
        self.inner.write().expect("apps lock").input_epoch = Some(epoch);
    }
    pub fn set_key_help(&self, help: std::collections::HashMap<String, Vec<String>>) {
        self.inner.write().expect("apps lock").key_help = help;
    }

    /// Host: set the roster (boot and after hot reload).
    pub fn set_apps(&self, apps: Vec<AppInfo>) {
        let mut inner = self.inner.write().expect("apps lock");
        if inner.apps != apps {
            inner.generation = inner
                .generation
                .checked_add(1)
                .expect("app generation exhausted");
            if let Some(epoch) = &inner.input_epoch {
                epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            inner.views.clear();
        }
        // Active app that vanished (reload) closes.
        if let Some(active) = &inner.active {
            if !apps.iter().any(|a| &a.name == active) {
                inner.active = None;
            }
        }
        inner
            .views
            .retain(|name, _| apps.iter().any(|app| &app.name == name));
        inner.apps = apps;
        let active = inner.active.clone();
        drop(inner);
        // Refresh visible content even when a reload keeps identical metadata.
        if let Some(active) = active {
            self.send(AppEvent::Shown(active));
        }
    }

    pub fn generation(&self) -> u64 {
        self.inner.read().expect("apps lock").generation
    }

    /// Invalidate even if a replacement keeps the same app metadata.
    pub fn invalidate(&self) {
        let mut inner = self.inner.write().expect("apps lock");
        inner.generation = inner
            .generation
            .checked_add(1)
            .expect("app generation exhausted");
        if let Some(epoch) = &inner.input_epoch {
            epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        inner.views.clear();
        inner.active = None;
    }

    pub fn publish_if_current(&self, generation: u64, name: &str, lines: Vec<String>) -> bool {
        let mut inner = self.inner.write().expect("apps lock");
        if inner.generation != generation || !inner.apps.iter().any(|app| app.name == name) {
            return false;
        }
        inner.views.insert(name.into(), lines);
        true
    }

    /// Host: publish a rendered view for an app.
    pub fn publish(&self, name: &str, lines: Vec<String>) {
        let mut inner = self.inner.write().expect("apps lock");
        if inner.apps.iter().any(|app| app.name == name) {
            inner.views.insert(name.into(), lines);
        }
    }

    /// Host: where app events should go.
    pub fn connect(&self, tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
        *self.events.write().expect("apps lock") = Some(tx);
    }

    pub fn active_overlay(&self) -> bool {
        self.active_in_slot(OVERLAY).is_some()
    }

    pub fn activation(&self) -> (Option<String>, u64) {
        let inner = self.inner.read().expect("apps lock");
        (inner.active.clone(), inner.generation)
    }

    pub fn close_activation(&self, name: &str, generation: u64) {
        let mut inner = self.inner.write().expect("apps lock");
        if inner.active.as_deref() == Some(name) && inner.generation == generation {
            inner.generation = inner
                .generation
                .checked_add(1)
                .expect("app generation exhausted");
            if let Some(epoch) = &inner.input_epoch {
                epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            inner.active = None;
        }
    }

    pub fn active(&self) -> Option<String> {
        self.inner.read().expect("apps lock").active.clone()
    }

    /// Content rows the app's slot had at last render (host passes this
    /// to Lua views as `ctx.rows` so long lists can window themselves).
    pub fn rows_for(&self, name: &str) -> Option<u16> {
        let inner = self.inner.read().expect("apps lock");
        let app = inner.apps.iter().find(|a| a.name == name)?;
        inner.viewports.get(&app.slot).map(|&(rows, _)| rows)
    }

    pub fn cols_for(&self, name: &str) -> Option<u16> {
        let inner = self.inner.read().expect("apps lock");
        let app = inner.apps.iter().find(|a| a.name == name)?;
        inner.viewports.get(&app.slot).map(|&(_, cols)| cols)
    }

    /// Hide the active app (host applies this after a "close" outcome).
    pub fn close(&self) {
        let mut inner = self.inner.write().expect("apps lock");
        inner.generation = inner
            .generation
            .checked_add(1)
            .expect("app generation exhausted");
        if let Some(epoch) = &inner.input_epoch {
            epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        inner.active = None;
    }

    /// Apply synchronous host effects only while this interaction still owns focus.
    /// The callback must not call back into AppsState.
    pub fn apply_key_if_current(
        &self,
        generation: u64,
        name: &str,
        effect: impl FnOnce() -> bool,
    ) -> bool {
        let mut inner = self.inner.write().expect("apps lock");
        if inner.generation != generation || inner.active.as_deref() != Some(name) {
            return false;
        }
        if effect() {
            inner.generation = inner
                .generation
                .checked_add(1)
                .expect("app generation exhausted");
            if let Some(epoch) = &inner.input_epoch {
                epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            inner.active = None;
        }
        true
    }

    /// Refresh only the activation whose action batch was just applied.
    pub fn refresh_if_current(&self, generation: u64, name: &str) {
        let inner = self.inner.read().expect("apps lock");
        if inner.generation == generation && inner.active.as_deref() == Some(name) {
            self.send(AppEvent::Shown(name.into()));
        }
    }

    pub fn request_unload(&self, name: String) {
        self.send(AppEvent::Unload(name));
    }

    fn send(&self, ev: AppEvent) {
        if let Some(tx) = self.events.read().expect("apps lock").as_ref() {
            let _ = tx.send(ev);
        }
    }

    /// Open only a registered app. Reopening starts a fresh activation, so stale
    /// views and key effects from a previous command cannot leak into it.
    pub fn open(&self, name: &str) -> bool {
        let mut inner = self.inner.write().expect("apps lock");
        if !inner.apps.iter().any(|app| app.name == name) {
            return false;
        }
        inner.generation = inner
            .generation
            .checked_add(1)
            .expect("app generation exhausted");
        if let Some(epoch) = &inner.input_epoch {
            epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        inner.active = Some(name.into());
        inner.views.remove(name);
        drop(inner);
        self.send(AppEvent::Shown(name.into()));
        true
    }

    pub fn refresh_ms(&self) -> Option<u64> {
        let inner = self.inner.read().expect("apps lock");
        let active = inner.active.as_ref()?;
        inner
            .apps
            .iter()
            .find(|app| &app.name == active)?
            .refresh_ms
            .map(|ms| ms.clamp(50, 60_000))
    }

    /// Toggle the app bound to this key, if any. Returns true if handled.
    fn toggle_by_key(&self, key: &KeyEvent) -> bool {
        let mut inner = self.inner.write().expect("apps lock");
        let Some(app) = inner
            .apps
            .iter()
            .find(|a| a.keymap.as_deref().is_some_and(|m| matches_keymap(m, key)))
            .map(|a| a.name.clone())
        else {
            return false;
        };
        inner.generation = inner
            .generation
            .checked_add(1)
            .expect("app generation exhausted");
        if let Some(epoch) = &inner.input_epoch {
            epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if inner.active.as_deref() == Some(&app) {
            inner.active = None;
        } else {
            inner.active = Some(app.clone());
            drop(inner);
            self.send(AppEvent::Shown(app));
        }
        true
    }

    fn active_in_slot(&self, slot: &str) -> Option<AppInfo> {
        let inner = self.inner.read().expect("apps lock");
        let active = inner.active.as_ref()?;
        inner
            .apps
            .iter()
            .find(|a| &a.name == active && a.slot == slot)
            .cloned()
    }

    fn view_of(&self, name: &str) -> Vec<String> {
        self.inner
            .read()
            .expect("apps lock")
            .views
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
}

/// "ctrl+e", "f2", "alt+b" → does this KeyEvent match? (shared key
/// language — see crate::keys).
fn matches_keymap(map: &str, key: &KeyEvent) -> bool {
    crate::keys::Chord::parse(map).is_some_and(|c| c.matches(key))
}

/// KeyEvent → the plain string handed to app `on_key` ("j", "enter"...).
fn key_string(key: &KeyEvent) -> Option<String> {
    let mut s = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl+");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        s.push_str("alt+");
    }
    match key.code {
        KeyCode::Char(c) => s.push(c),
        KeyCode::Enter => s.push_str("enter"),
        KeyCode::Esc => s.push_str("esc"),
        KeyCode::Up => s.push_str("up"),
        KeyCode::Down => s.push_str("down"),
        KeyCode::Left => s.push_str("left"),
        KeyCode::Right => s.push_str("right"),
        KeyCode::PageUp => s.push_str("pageup"),
        KeyCode::PageDown => s.push_str("pagedown"),
        KeyCode::Home => s.push_str("home"),
        KeyCode::End => s.push_str("end"),
        KeyCode::Tab => s.push_str("tab"),
        KeyCode::Backspace => s.push_str("backspace"),
        _ => return None,
    }
    Some(s)
}

/// Renders the active external app of one slot. Mounted once per slot
/// kind; `wants()` selects it in while an app of that slot is active.
struct ExtApp {
    state: AppsState,
    slot: &'static str,
}

impl Component for ExtApp {
    fn binding_help(&self) -> Vec<String> {
        let Some(app) = self.state.active_in_slot(self.slot) else {
            return Vec::new();
        };
        let mut lines = vec![format!(
            "App {}: Escape closes; other keys are forwarded to the plugin",
            app.name
        )];
        if let Some(help) = self
            .state
            .inner
            .read()
            .expect("apps lock")
            .key_help
            .get(&app.name)
        {
            lines.extend(help.iter().map(|line| format!("Plugin-declared: {line}")));
        }
        if let Some(key) = app.keymap {
            lines.push(format!("App {}: {key:?} toggles closed", app.name));
        }
        lines
    }

    fn name(&self) -> &str {
        "ext_apps"
    }

    fn wants(&self, _ctx: &Ctx<'_>) -> bool {
        self.state.active_in_slot(self.slot).is_some()
    }

    fn height(&self, _ctx: &Ctx<'_>, width: u16) -> Option<u16> {
        let app = self.state.active_in_slot(self.slot)?;
        match self.slot {
            // Reinterpreted as width by the sidebar slot.
            SIDEBAR => Some(
                app.config["width"]
                    .as_u64()
                    .unwrap_or(u64::from(34.min(width / 3)))
                    .min(u64::from(width)) as u16,
            ),
            _ => {
                let lines = self
                    .state
                    .view_of(&app.name)
                    .len()
                    .saturating_add(2)
                    .clamp(5, 30);
                Some(
                    app.config["height"]
                        .as_u64()
                        .unwrap_or(lines as u64)
                        .min(u64::from(u16::MAX)) as u16,
                )
            }
        }
    }

    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        let Some(app) = self.state.active_in_slot(self.slot) else {
            return;
        };
        use crate::core::terminal_text::sanitize;
        use ratatui::widgets::BorderType;
        let width = app.config["width"]
            .as_u64()
            .unwrap_or(u64::from(area.width))
            .min(u64::from(area.width)) as u16;
        let area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y,
            width,
            area.height,
        );
        let resolve = |value: &serde_json::Value, fallback| {
            ctx.theme.resolve_style(value, fallback).unwrap_or(fallback)
        };
        let style = resolve(&app.config["style"], ctx.theme.overlay);
        let border = &app.config["border"];
        let kind = border["kind"].as_str().unwrap_or("plain");
        let block = Block::default()
            .borders(if kind == "none" {
                Borders::NONE
            } else {
                Borders::ALL
            })
            .border_type(match kind {
                "rounded" => BorderType::Rounded,
                "double" => BorderType::Double,
                _ => BorderType::Plain,
            })
            .border_style(resolve(&border["style"], ctx.theme.overlay_border))
            .title(Line::styled(
                format!(" {} ", sanitize(&app.title)),
                resolve(&app.config["title_style"], style),
            ));
        let content = block.inner(area);
        let viewport = (content.height, content.width);
        let previous = self
            .state
            .inner
            .write()
            .expect("apps lock")
            .viewports
            .insert(self.slot.into(), viewport);
        if previous != Some(viewport) {
            self.state.send(AppEvent::Shown(app.name.clone()));
        }
        let lines: Vec<Line> = self
            .state
            .view_of(&app.name)
            .into_iter()
            .map(|line| Line::from(sanitize(&line)))
            .collect();
        // Clear symbols AND old style flags before painting the opaque panel.
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].reset();
                buf[(x, y)].set_style(style);
            }
        }
        Paragraph::new(lines)
            .style(style)
            .block(block)
            .render(area, buf);
    }

    fn on_key(&mut self, _ctx: &Ctx<'_>, key: KeyEvent) -> KeyOutcome {
        let Some(app) = self.state.active_in_slot(self.slot) else {
            return KeyOutcome::pass();
        };
        // Legacy apps close locally. Opted-in apps can use Esc for back navigation.
        if key.code == KeyCode::Esc && !app.capture_escape {
            self.state.close();
            return KeyOutcome::consumed();
        }
        // The toggle key closes too.
        if self.state.toggle_by_key(&key) {
            return KeyOutcome::consumed();
        }
        if let Some(k) = key_string(&key) {
            self.state
                .send(AppEvent::Key(app.name, k, self.state.generation()));
            return KeyOutcome::consumed();
        }
        KeyOutcome::consumed()
    }
}

/// Global keymap hook: shell calls this for keys nothing focused wanted.
/// Returns true if a toggle fired.
pub fn handle_global_key(state: &AppsState, key: &KeyEvent) -> bool {
    state.toggle_by_key(key)
}

/// Mount one ExtApp per slot kind. Returns the shared state handle.
pub fn install(slots: &mut Slots) -> AppsState {
    let state = AppsState::default();
    slots.mount(
        SIDEBAR,
        10,
        Box::new(ExtApp {
            state: state.clone(),
            slot: SIDEBAR,
        }),
    );
    slots.mount(
        OVERLAY,
        5,
        Box::new(ExtApp {
            state: state.clone(),
            slot: OVERLAY,
        }),
    );
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Model;
    use crate::theme::Theme;

    #[test]
    fn command_open_refresh_and_escape_are_opt_in_and_generation_safe() {
        let mut slots = Slots::default();
        let state = install(&mut slots);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.connect(tx);
        state.set_apps(vec![AppInfo {
            name: "jobs".into(),
            slot: OVERLAY.into(),
            refresh_ms: Some(250),
            capture_escape: true,
            ..Default::default()
        }]);
        assert_eq!(state.refresh_ms(), None);
        assert!(!state.open("missing"));
        assert!(state.open("jobs"));
        assert_eq!(state.refresh_ms(), Some(250));
        assert_eq!(rx.try_recv().unwrap(), AppEvent::Shown("jobs".into()));
        let old = state.generation();
        assert!(state.open("jobs"));
        assert!(!state.publish_if_current(old, "jobs", vec!["stale".into()]));
        assert_eq!(rx.try_recv().unwrap(), AppEvent::Shown("jobs".into()));
        let model = Model::new("s".into(), "m".into());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        slots
            .focused_mut(&ctx)
            .unwrap()
            .on_key(&ctx, key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            rx.try_recv().unwrap(),
            AppEvent::Key("jobs".into(), "esc".into(), state.generation())
        );
        assert_eq!(state.active().as_deref(), Some("jobs"));
        let generation = state.generation();
        state.set_session("s".into());
        assert_eq!(state.context_if_current(generation, "jobs"), None);
        state.open("jobs");
        let generation = state.generation();
        assert_eq!(
            state.context_if_current(generation, "jobs").unwrap()["session"],
            "s"
        );
        state.set_runtime_generation(7);
        assert_eq!(state.context_if_current(generation, "jobs"), None);
        assert_eq!(state.active().as_deref(), Some("jobs"));
        let generation = state.generation();
        state.set_session("other".into());
        assert_eq!(state.active(), None);
        assert_eq!(state.refresh_ms(), None);
        assert!(!state.publish_if_current(generation, "jobs", vec!["old session".into()]));
    }

    #[test]
    fn app_layout_styles_and_output_are_safe() {
        let state = AppsState::default();
        state.set_apps(vec![AppInfo {
            name: "jobs".into(),
            slot: OVERLAY.into(),
            title: "jobs\x1b[2J".into(),
            config: serde_json::json!({"width":20,"height":8,
                "style":{"fg":"red"}, "border":{"kind":"none"}}),
            ..Default::default()
        }]);
        state.open("jobs");
        state.publish("jobs", vec!["a\tb\x1b[2J\x07".into()]);
        let mut component = ExtApp {
            state: state.clone(),
            slot: OVERLAY,
        };
        let model = Model::new("s".into(), "m".into());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };
        assert_eq!(component.height(&ctx, 80), Some(8));
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        component.render(&ctx, area, &mut buf);
        assert_eq!(state.cols_for("jobs"), Some(20));
        assert!(buf
            .content
            .iter()
            .all(|cell| !cell.symbol().chars().any(char::is_control)));
        assert!(buf.content.iter().any(|cell| cell.symbol() == "b"));
        for width in 0..3 {
            let tiny = Rect::new(0, 0, width, 1);
            component.render(&ctx, tiny, &mut Buffer::empty(tiny));
        }
    }

    #[test]
    fn roster_refresh_requests_visible_content_even_with_same_metadata() {
        let state = AppsState::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.connect(tx);
        let app = AppInfo {
            name: "probe".into(),
            slot: "overlay".into(),
            title: "Before".into(),
            keymap: Some("ctrl+y".into()),
            ..Default::default()
        };
        state.set_apps(vec![app.clone()]);
        state.toggle_by_key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(rx.try_recv().unwrap(), AppEvent::Shown("probe".into()));
        state.set_apps(vec![app.clone()]);
        assert_eq!(rx.try_recv().unwrap(), AppEvent::Shown("probe".into()));
        state.set_apps(vec![AppInfo {
            title: "After".into(),
            ..app
        }]);
        assert_eq!(rx.try_recv().unwrap(), AppEvent::Shown("probe".into()));
        assert_eq!(state.active().as_deref(), Some("probe"));
    }

    #[test]
    fn generations_reject_views_after_same_name_remount() {
        let state = AppsState::default();
        state.set_apps(roster());
        let old = state.generation();
        state.set_apps(roster());
        assert_eq!(old, state.generation());
        state.set_apps(Vec::new());
        state.set_apps(roster());
        assert!(!state.publish_if_current(old, "tree", vec!["stale".into()]));
        let current = state.generation();
        assert!(state.publish_if_current(current, "tree", vec!["new".into()]));
        state.invalidate();
        assert!(state.view_of("tree").is_empty());
        assert!(!state.publish_if_current(current, "tree", vec!["late".into()]));
    }

    #[test]
    fn removed_apps_drop_views_focus_and_late_publications() {
        let state = AppsState::default();
        state.set_apps(roster());
        state.publish("tree", vec!["old".into()]);
        state.inner.write().unwrap().active = Some("tree".into());
        state.set_apps(Vec::new());
        assert!(state.active().is_none());
        assert!(state.view_of("tree").is_empty());
        state.publish("tree", vec!["late".into()]);
        state.set_apps(roster());
        assert!(state.view_of("tree").is_empty());
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn task_navigation_keys_reach_apps() {
        for (code, expected) in [
            (KeyCode::PageUp, "pageup"),
            (KeyCode::PageDown, "pagedown"),
            (KeyCode::Home, "home"),
            (KeyCode::End, "end"),
        ] {
            assert_eq!(
                key_string(&KeyEvent::new(code, KeyModifiers::NONE)).as_deref(),
                Some(expected)
            );
        }
    }

    fn roster() -> Vec<AppInfo> {
        vec![
            AppInfo {
                name: "tree".into(),
                slot: SIDEBAR.into(),
                title: "files".into(),
                keymap: Some("ctrl+e".into()),
                ..Default::default()
            },
            AppInfo {
                name: "sessions".into(),
                slot: OVERLAY.into(),
                title: "sessions".into(),
                keymap: Some("ctrl+s".into()),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn stale_key_effects_cannot_close_or_act_on_reopened_apps() {
        let state = AppsState::default();
        state.set_apps(roster());
        let toggle = key(KeyCode::Char('e'), KeyModifiers::CONTROL);
        handle_global_key(&state, &toggle);
        let old = state.generation();
        state.close();
        handle_global_key(&state, &toggle);
        assert!(!state.apply_key_if_current(old, "tree", || panic!("stale effect")));
        let current = state.generation();
        assert!(!state.apply_key_if_current(current, "sessions", || panic!("wrong app")));
        assert!(state.apply_key_if_current(current, "tree", || true));
        assert_eq!(state.active(), None);
        handle_global_key(&state, &toggle);
        let current = state.generation();
        state.invalidate();
        assert!(!state.apply_key_if_current(current, "tree", || panic!("invalidated effect")));
    }

    #[test]
    fn toggle_key_activates_and_deactivates() {
        let state = AppsState::default();
        state.set_apps(roster());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.connect(tx);

        let ctrl_e = key(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(handle_global_key(&state, &ctrl_e));
        assert_eq!(state.active(), Some("tree".into()));
        assert_eq!(rx.try_recv().unwrap(), AppEvent::Shown("tree".into()));

        assert!(handle_global_key(&state, &ctrl_e));
        assert_eq!(state.active(), None);

        // Unbound key: not handled.
        assert!(!handle_global_key(
            &state,
            &key(KeyCode::Char('x'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn active_app_receives_keys_and_esc_closes() {
        let mut slots = Slots::default();
        let state = install(&mut slots);
        state.set_apps(roster());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.connect(tx.clone());
        state.publish("sessions", vec!["a".into(), "b".into()]);

        handle_global_key(&state, &key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(rx.try_recv().unwrap(), AppEvent::Shown("sessions".into()));

        let model = Model::new("s".into(), "m".into());
        let theme = Theme::default();
        let ctx = Ctx {
            model: &model,
            theme: &theme,
        };

        // Overlay is focused now; a key forwards to the host.
        let focused = slots.focused_mut(&ctx).expect("overlay focused");
        assert_eq!(focused.name(), "ext_apps");
        let out = focused.on_key(&ctx, key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(out.handled);
        assert_eq!(
            rx.try_recv().unwrap(),
            AppEvent::Key("sessions".into(), "j".into(), state.generation())
        );

        // Esc closes locally.
        let out = focused.on_key(&ctx, key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(out.handled);
        assert_eq!(state.active(), None);
    }

    #[test]
    fn reload_that_drops_the_active_app_closes_it() {
        let state = AppsState::default();
        state.set_apps(roster());
        handle_global_key(&state, &key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(state.active(), Some("tree".into()));
        state.set_apps(vec![]);
        assert_eq!(state.active(), None);
    }

    #[test]
    fn keymap_parsing() {
        let ctrl_e = key(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(matches_keymap("ctrl+e", &ctrl_e));
        assert!(!matches_keymap("ctrl+x", &ctrl_e));
        assert!(!matches_keymap("e", &ctrl_e));
        assert!(matches_keymap(
            "f2",
            &key(KeyCode::F(2), KeyModifiers::NONE)
        ));
        assert!(matches_keymap(
            "alt+b",
            &key(KeyCode::Char('b'), KeyModifiers::ALT)
        ));
    }
}
