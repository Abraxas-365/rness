//! String-keyed slots with priority shadowing (the dsh ui-slots pattern,
//! sized for a TUI).
//!
//! Open registry: slots are string keys, not a closed enum — a future Lua
//! component registers into `"statusline"` exactly like a vendor module.
//! Per slot, many components may mount; the RENDERED one (the winner) is
//! the highest-priority component whose [`Component::wants`] returns true
//! — that is dsh's chain `select`, so modal features (approval, pickers)
//! appear by publishing state, never by shell-driven mount/unmount.
//!
//! Layout contract (top to bottom): `message_body` fills, then
//! `input_footer`, then `statusline`. `overlay` draws centered OVER
//! everything when its winner wants to show.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, Ctx};

/// Well-known slot keys. Constants, not an enum: the set is open.
pub const MESSAGE_BODY: &str = "message_body";
pub const INPUT_FOOTER: &str = "input_footer";
pub const STATUSLINE: &str = "statusline";
pub const OVERLAY: &str = "overlay";
/// Left column beside the message body (file tree etc.). Its winner's
/// `height(ctx, width)` is reinterpreted as WIDTH (columns).
pub const SIDEBAR: &str = "sidebar";

struct Mounted {
    priority: i32,
    component: Box<dyn Component>,
}

/// Mounted components per slot key, priority-ordered (highest first;
/// insertion order breaks ties — first mounted wins).
#[derive(Default)]
pub struct Slots {
    slots: std::collections::HashMap<String, Vec<Mounted>>,
}

impl Slots {
    /// Mount a component into a named slot with a priority.
    pub fn mount(&mut self, slot: &str, priority: i32, component: Box<dyn Component>) {
        let list = self.slots.entry(slot.to_string()).or_default();
        // Stable position: after existing entries of >= priority.
        let at = list.partition_point(|m| m.priority >= priority);
        list.insert(at, Mounted { priority, component });
    }

    /// Remove a component by name; returns whether anything was removed.
    pub fn unmount(&mut self, slot: &str, name: &str) -> bool {
        let Some(list) = self.slots.get_mut(slot) else { return false };
        let before = list.len();
        list.retain(|m| m.component.name() != name);
        list.len() != before
    }

    /// The winner: highest-priority mounted component that wants to show.
    fn winner_mut(&mut self, slot: &str, ctx: &Ctx<'_>) -> Option<&mut Box<dyn Component>> {
        self.slots
            .get_mut(slot)?
            .iter_mut()
            .filter(|m| m.component.wants(ctx))
            .enumerate()
            .max_by_key(|(index, m)| (m.component.priority().unwrap_or(m.priority), std::cmp::Reverse(*index)))
            .map(|(_, m)| m)
            .map(|m| &mut m.component)
    }

    fn winner(&self, slot: &str, ctx: &Ctx<'_>) -> Option<&dyn Component> {
        self.slots
            .get(slot)?
            .iter()
            .filter(|m| m.component.wants(ctx))
            .enumerate()
            .max_by_key(|(index, m)| (m.component.priority().unwrap_or(m.priority), std::cmp::Reverse(*index)))
            .map(|(_, m)| m)
            .map(|m| m.component.as_ref())
    }

    /// Whether a modal overlay currently wants the screen.
    pub fn overlay_active(&self, ctx: &Ctx<'_>) -> bool {
        self.winner(OVERLAY, ctx).is_some()
    }

    pub fn focused_binding_help(&self, ctx: &Ctx<'_>) -> Vec<String> {
        for slot in [OVERLAY, SIDEBAR, INPUT_FOOTER] {
            if let Some(component) = self.winner(slot, ctx) {
                let mut lines = vec![format!("Focus: {} ({slot})", component.name())];
                let help = component.binding_help();
                if help.is_empty() { lines.push("This component does not publish binding descriptions.".into()); }
                lines.extend(help);
                return lines;
            }
        }
        vec!["No focused component".into()]
    }

    pub fn resolved_binding_help(&self, ctx: &Ctx<'_>, map: &crate::keymaps::ScopedKeymap) -> Vec<String> {
        use crate::keymaps::{Scope, BindingLayer};
        let mut lines = Vec::new();
        for (slot, scope) in [(INPUT_FOOTER, Scope::Promptbox), (MESSAGE_BODY, Scope::Messagebox)] {
            let Some(component) = self.winner(slot, ctx) else { continue; };
            lines.push(format!("{} controls (when focused/applicable):", component.name()));
            let bindings = component.bindings();
            for line in component.binding_help() {
                let shadowed = bindings.iter().find_map(|binding| {
                    if !line.contains(&format!("{:?} → {} (when applicable)", binding.chord, binding.action)) { return None; }
                    let key = crossterm::event::KeyEvent::new(binding.chord.code, binding.chord.mods);
                    if map.layer(&scope, &key) != Some(BindingLayer::User) { return None; }
                    map.lookup_exact(&scope, &key).filter(|action| {
                        !component.captures_input() || action.strip_prefix("core.promptbox.").is_some_and(|action| {
                            action == "noop" || ((action.starts_with("completion_") || action.starts_with("preview_") || action == "close_preview")
                                && bindings.iter().any(|binding| binding.action == action))
                        })
                    }).map(|action| format!("{:?} → {action} (replaces {})", binding.chord, binding.action))
                });
                lines.push(shadowed.unwrap_or(line));
            }
        }
        if self.modal_active(ctx) { lines.extend(self.focused_binding_help(ctx)); }
        lines
    }

    pub fn binding_help(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for components in self.slots.values() {
            for mounted in components {
                lines.extend(mounted.component.binding_help());
            }
        }
        lines.sort();
        lines.dedup();
        lines
    }

    pub fn modal_active(&self, ctx: &Ctx<'_>) -> bool {
        self.winner(OVERLAY, ctx).is_some() || self.winner(SIDEBAR, ctx).is_some()
    }

    pub fn prompt_focused(&self, ctx: &Ctx<'_>) -> bool {
        self.winner(OVERLAY, ctx).is_none() && self.winner(SIDEBAR, ctx).is_none()
            && self.winner(INPUT_FOOTER, ctx).is_some()
    }

    /// The focus target: the active overlay, else an active sidebar,
    /// else the input footer.
    pub fn focused_mut(&mut self, ctx: &Ctx<'_>) -> Option<&mut Box<dyn Component>> {
        if self.winner(OVERLAY, ctx).is_some() {
            return self.winner_mut(OVERLAY, ctx);
        }
        if self.winner(SIDEBAR, ctx).is_some() {
            return self.winner_mut(SIDEBAR, ctx);
        }
        self.winner_mut(INPUT_FOOTER, ctx)
    }

    pub fn message_binding(&mut self, ctx: &Ctx<'_>, action: &str) -> crate::component::KeyOutcome {
        if self.modal_active(ctx) { return crate::component::KeyOutcome::pass(); }
        self.winner_mut(MESSAGE_BODY, ctx).map(|c| c.on_binding(ctx, action)).unwrap_or_else(crate::component::KeyOutcome::pass)
    }

    pub fn message_key(&mut self, ctx: &Ctx<'_>, key: crossterm::event::KeyEvent) -> crate::component::KeyOutcome {
        if self.overlay_active(ctx) || self.winner(SIDEBAR, ctx).is_some() {
            return crate::component::KeyOutcome::pass();
        }
        self.winner_mut(MESSAGE_BODY, ctx).map(|c| c.on_key(ctx, key)).unwrap_or_else(crate::component::KeyOutcome::pass)
    }

    /// Deliver a custom action to every mounted component.
    pub fn broadcast(&mut self, ctx: &Ctx<'_>, name: &str, payload: &serde_json::Value) {
        for list in self.slots.values_mut() {
            for m in list.iter_mut() {
                m.component.on_action(ctx, name, payload);
            }
        }
    }

    /// Render the full stack into `area`.
    pub fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        // Fixed-height slots claim from the bottom; message body fills.
        let mut bottom = area.bottom();
        for slot in [STATUSLINE, INPUT_FOOTER] {
            if let Some(c) = self.winner_mut(slot, ctx) {
                let want = c.height(ctx, area.width).unwrap_or(1);
                let h = want.min(bottom - area.y);
                if h == 0 {
                    continue;
                }
                let rect = Rect::new(area.x, bottom - h, area.width, h);
                c.render(ctx, rect, buf);
                bottom -= h;
            }
        }

        let mut body = Rect::new(area.x, area.y, area.width, bottom - area.y);
        // Sidebar: carve a left column off the body. `height` doubles as
        // width here (Component has one size hint; the slot decides axis).
        if let Some(c) = self.winner_mut(SIDEBAR, ctx) {
            let w = c.height(ctx, area.width).unwrap_or(30).min(body.width / 2);
            if w > 0 {
                let rect = Rect::new(body.x, body.y, w, body.height);
                c.render(ctx, rect, buf);
                body = Rect::new(body.x + w, body.y, body.width - w, body.height);
            }
        }
        if let Some(c) = self.winner_mut(MESSAGE_BODY, ctx) {
            c.render(ctx, body, buf);
        }

        // Overlay: centered box over everything.
        if let Some(c) = self.winner_mut(OVERLAY, ctx) {
            let h = c.height(ctx, area.width).unwrap_or(area.height / 2).min(area.height);
            let w = if area.width < 60 { area.width } else { area.width.saturating_sub(8) };
            let rect = Rect::new(
                area.x + (area.width - w) / 2,
                area.y + (area.height.saturating_sub(h)) / 2,
                w,
                h,
            );
            c.render(ctx, rect, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Model;
    use crate::theme::Theme;

    struct Probe {
        name: &'static str,
        wants: bool,
    }

    impl Component for Probe {
        fn name(&self) -> &str {
            self.name
        }
        fn height(&self, _ctx: &Ctx<'_>, _width: u16) -> Option<u16> {
            Some(1)
        }
        fn render(&mut self, _ctx: &Ctx<'_>, _area: Rect, _buf: &mut Buffer) {}
        fn wants(&self, _ctx: &Ctx<'_>) -> bool {
            self.wants
        }
    }

    fn ctx_fixture() -> (Model, Theme) {
        (Model::new("s".into(), "m".into()), Theme::default())
    }

    #[test]
    fn dynamic_priority_changes_render_and_focus_winner() {
        use std::sync::{Arc, atomic::{AtomicI32, Ordering}};
        struct Dynamic(Arc<AtomicI32>);
        impl Component for Dynamic {
            fn name(&self) -> &str { "dynamic" }
            fn priority(&self) -> Option<i32> { Some(self.0.load(Ordering::SeqCst)) }
            fn height(&self, _: &Ctx<'_>, _: u16) -> Option<u16> { Some(1) }
            fn render(&mut self, _: &Ctx<'_>, _: Rect, _: &mut Buffer) {}
        }
        let (model, theme) = ctx_fixture();
        let ctx = Ctx { model: &model, theme: &theme };
        let priority = Arc::new(AtomicI32::new(20));
        let mut slots = Slots::default();
        slots.mount(OVERLAY, 10, Box::new(Probe { name: "base", wants: true }));
        slots.mount(OVERLAY, 0, Box::new(Dynamic(priority.clone())));
        assert_eq!(slots.winner(OVERLAY, &ctx).unwrap().name(), "dynamic");
        assert_eq!(slots.focused_mut(&ctx).unwrap().name(), "dynamic");
        priority.store(5, Ordering::SeqCst);
        assert_eq!(slots.winner(OVERLAY, &ctx).unwrap().name(), "base");
        assert_eq!(slots.focused_mut(&ctx).unwrap().name(), "base");
    }

    #[test]
    fn higher_priority_shadows_lower() {
        let (model, theme) = ctx_fixture();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut slots = Slots::default();
        slots.mount(STATUSLINE, 0, Box::new(Probe { name: "base", wants: true }));
        slots.mount(STATUSLINE, 10, Box::new(Probe { name: "fancy", wants: true }));
        assert_eq!(slots.winner_mut(STATUSLINE, &ctx).unwrap().name(), "fancy");
    }

    #[test]
    fn wants_false_falls_through_to_next() {
        let (model, theme) = ctx_fixture();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut slots = Slots::default();
        slots.mount(OVERLAY, 0, Box::new(Probe { name: "base", wants: true }));
        slots.mount(OVERLAY, 10, Box::new(Probe { name: "modal", wants: false }));
        assert_eq!(slots.winner_mut(OVERLAY, &ctx).unwrap().name(), "base");
    }

    #[test]
    fn no_willing_component_means_inactive() {
        let (model, theme) = ctx_fixture();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut slots = Slots::default();
        slots.mount(OVERLAY, 0, Box::new(Probe { name: "modal", wants: false }));
        assert!(!slots.overlay_active(&ctx));
        assert!(slots.winner_mut(OVERLAY, &ctx).is_none());
    }

    #[test]
    fn unmount_removes_by_name() {
        let (model, theme) = ctx_fixture();
        let ctx = Ctx { model: &model, theme: &theme };
        let mut slots = Slots::default();
        slots.mount(STATUSLINE, 5, Box::new(Probe { name: "a", wants: true }));
        assert!(slots.unmount(STATUSLINE, "a"));
        assert!(!slots.unmount(STATUSLINE, "a"));
        assert!(slots.winner_mut(STATUSLINE, &ctx).is_none());
    }
}
