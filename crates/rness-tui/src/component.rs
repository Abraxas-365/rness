//! The component seam: everything visible mounts as a [`Component`] in a
//! named slot. Vendor modules and (later) Lua components use the SAME
//! trait — equal power, hot-mountable, disposable.

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::{Action, Model};
use crate::theme::Theme;

/// Render context handed to every component each frame.
pub struct Ctx<'a> {
    pub model: &'a Model,
    pub theme: &'a Theme,
}

/// One mounted UI piece. Rendering is immediate-mode against the shared
/// [`Model`]; input goes to the focused component first (bail), then to
/// global keymaps.
pub trait Component: Send {
    /// Stable identity (for unmounting / debugging).
    fn name(&self) -> &str;

    fn priority(&self) -> Option<i32> { None }

    /// Chain select (dsh pattern): whether this component wants to render
    /// given the current model. The highest-priority willing component in
    /// a slot wins. Default: always. Modal features return true only
    /// while their state is pending — appearing/disappearing is driven by
    /// model state, never by shell-side mount/unmount.
    fn wants(&self, _ctx: &Ctx<'_>) -> bool {
        true
    }

    /// How many rows this component wants given `width` (slots stack
    /// vertically). `None` = fill remaining space.
    fn height(&self, ctx: &Ctx<'_>, width: u16) -> Option<u16>;

    /// Draw into `area` of `buf`.
    fn render(&mut self, ctx: &Ctx<'_>, area: Rect, buf: &mut Buffer);

    /// Handle a key when focused. Return actions to apply; `handled`
    /// stops propagation.
    fn on_key(&mut self, _ctx: &Ctx<'_>, _key: KeyEvent) -> KeyOutcome {
        KeyOutcome::pass()
    }

    fn on_paste(&mut self, _ctx: &Ctx<'_>, _text: &str) -> KeyOutcome {
        KeyOutcome::consumed()
    }

    /// Receive a broadcast [`Action::Custom`]. Components ignore
    /// everything they don't recognize.
    fn on_action(&mut self, _ctx: &Ctx<'_>, _name: &str, _payload: &serde_json::Value) {}
}

pub struct KeyOutcome {
    pub handled: bool,
    pub actions: Vec<Action>,
}

impl KeyOutcome {
    pub fn pass() -> Self {
        Self { handled: false, actions: vec![] }
    }
    pub fn consumed() -> Self {
        Self { handled: true, actions: vec![] }
    }
    pub fn act(actions: Vec<Action>) -> Self {
        Self { handled: true, actions }
    }
}
