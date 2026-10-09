//! Execution budgets for every Rust→Lua entry on the VM thread.
//!
//! One raw count hook is installed on the main Lua thread when the VM is
//! built. Lua copies a thread's hook into every coroutine it creates, so all
//! plugin code — callbacks, coroutines, tasks, commands — runs under it. The
//! hook checks a stack of active [`Scope`]s (deadline, cancel token,
//! instruction allowance). When one expires it raises an error and becomes
//! sticky: the hook then fires on every instruction and raises again until
//! the scope ends, so `pcall`/`xpcall`/`coroutine.wrap` cannot swallow it.
//! The standard `debug` library is not loaded, so plugins cannot remove it.
//!
//! Limits: hooks do not fire inside one long native call (a C function such
//! as a pathological `string.find`, or a slow Rust API). The budget is
//! enforced on the next Lua instruction after the call returns.
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::{ffi, Lua};
use tokio_util::sync::CancellationToken;

/// Instructions between budget checks while no scope has expired.
const COUNT: i32 = 1000;

/// Which entry point a scope bounds. Wall-clock budgets are deliberately
/// generous: they exist to free a wedged VM, not to police speed. They
/// also count time spent inside native APIs (e.g. a first
/// `rness.session.usage` over a long log), which is why the host disables a
/// handler only after several consecutive expiries (see `Strikes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Budget {
    /// One notification handler (`rness.hook.on` for turn_start, frame, …).
    Hook,
    /// The statusline provider (function or table form).
    Statusline,
    /// An app `view` or `on_key` callback.
    App,
    /// A plugin action (key binding).
    Action,
    /// One `rness.timer` callback.
    Timer,
    /// A plugin chunk's top-level code (load or hot reload).
    Load,
    /// init.lua at startup, including the plugins it loads.
    Startup,
    /// A tool-card renderer.
    Card,
    /// An interception chain (pre_step, pre_tool, …): cancel + 30 s.
    Intercept,
    /// A `tool_execute` chain resume: cancel + caller deadline.
    Execute,
    /// A web transform hook: cancel + 30 s.
    Web,
    /// A slash command resume: cancel only (user-driven; Ctrl-C stops it).
    Command,
    /// A command completion: cancel only.
    Complete,
    /// A Lua tool resume: cancel only (the turn's token).
    Tool,
    /// A background task resume: cancel + instruction allowance.
    Task,
    /// Closing an abandoned command coroutine's to-be-closed variables.
    Cleanup,
}

impl Budget {
    pub(crate) fn time(self) -> Option<Duration> {
        let ms = match self {
            Budget::Hook => 500,
            Budget::Statusline => 250,
            Budget::App => 1_000,
            Budget::Action => 1_000,
            Budget::Timer => 1_000,
            Budget::Load => 5_000,
            Budget::Startup => 30_000,
            Budget::Card => 100,
            Budget::Intercept | Budget::Web => 30_000,
            Budget::Execute
            | Budget::Command
            | Budget::Complete
            | Budget::Tool
            | Budget::Task
            | Budget::Cleanup => return None,
        };
        Some(Duration::from_millis(ms))
    }

    fn instructions(self) -> Option<u64> {
        match self {
            // ~30 M instructions between two awaits of one task.
            Budget::Task => Some(30_000_000),
            Budget::Cleanup => Some(10_000),
            _ => None,
        }
    }

    fn cancelled(self) -> &'static str {
        match self {
            Budget::Command => "command cancelled",
            Budget::Complete => "completion cancelled",
            Budget::Tool => "tool call cancelled",
            Budget::Task => "task cancelled",
            Budget::Intercept => "hook cancelled or timed out",
            Budget::Execute => "tool_execute hook cancelled or timed out",
            Budget::Web => "web hook cancelled or timed out",
            _ => "cancelled",
        }
    }

    fn exceeded(self) -> String {
        match self {
            Budget::Intercept => "hook cancelled or timed out".into(),
            Budget::Execute => "tool_execute hook cancelled or timed out".into(),
            Budget::Web => "web hook cancelled or timed out".into(),
            Budget::Task => "task exceeded its instruction budget between awaits".into(),
            Budget::Cleanup => "command cleanup instruction limit exceeded".into(),
            other => format!(
                "{} exceeded its {} ms time budget",
                other.label(),
                other.time().unwrap_or_default().as_millis()
            ),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Budget::Hook => "hook handler",
            Budget::Statusline => "statusline",
            Budget::App => "app callback",
            Budget::Action => "plugin action",
            Budget::Timer => "timer callback",
            Budget::Load => "plugin load",
            Budget::Startup => "init.lua",
            Budget::Card => "tool card",
            Budget::Intercept => "interception hook",
            Budget::Execute => "tool_execute hook",
            Budget::Web => "web hook",
            Budget::Command => "command",
            Budget::Complete => "completion",
            Budget::Tool => "tool",
            Budget::Task => "task",
            Budget::Cleanup => "command cleanup",
        }
    }
}

/// Why a scope stopped its code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// The caller cancelled (Ctrl-C, turn cancel, dropped request).
    Cancelled,
    /// The scope ran out of time or instructions: a strike.
    Exceeded,
}

struct Frame {
    id: u64,
    deadline: Option<Instant>,
    cancel: Option<CancellationToken>,
    instructions: Option<u64>,
    budget: Budget,
    expired: Option<(Expiry, String)>,
}

impl Frame {
    fn check(&mut self, step: u64, now: &mut Option<Instant>) -> Option<(Expiry, String)> {
        if self.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Some((Expiry::Cancelled, self.budget.cancelled().into()));
        }
        if let Some(left) = &mut self.instructions {
            *left = left.saturating_sub(step);
            if *left == 0 {
                return Some((Expiry::Exceeded, self.budget.exceeded()));
            }
        }
        if let Some(deadline) = self.deadline {
            if *now.get_or_insert_with(Instant::now) >= deadline {
                return Some((Expiry::Exceeded, self.budget.exceeded()));
            }
        }
        None
    }
}

#[derive(Default)]
struct State {
    frames: Vec<Frame>,
    next_id: u64,
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<State>>);

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

static KEY: u8 = 0;

fn key() -> *const c_void {
    &KEY as *const u8 as *const c_void
}

/// Install the budget hook on a fresh VM, before any plugin code runs.
pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    let shared = Shared::default();
    let state = Arc::as_ptr(&shared.0) as *mut c_void;
    // app_data keeps the state alive exactly as long as the VM.
    lua.set_app_data(shared);
    lua.set_app_data(Strikes::default());
    // SAFETY: plain C API calls on the main thread with stack space checked
    // by exec_raw; the pointer stays valid while the app_data is held.
    unsafe {
        lua.exec_raw::<()>((), |l| {
            ffi::lua_pushlightuserdata(l, state);
            ffi::lua_rawsetp(l, ffi::LUA_REGISTRYINDEX, key());
            ffi::lua_sethook(l, Some(hook), ffi::LUA_MASKCOUNT, COUNT);
        })
    }
}

unsafe extern "C-unwind" fn hook(l: *mut ffi::lua_State, _ar: *mut ffi::lua_Debug) {
    if let Some(message) = check(l) {
        ffi::lua_pushlstring(l, message.as_ptr() as *const _, message.len());
        // No Rust value with a destructor may be live across the longjmp.
        drop(message);
        ffi::lua_error(l);
    }
}

unsafe fn check(l: *mut ffi::lua_State) -> Option<String> {
    let kind = ffi::lua_rawgetp(l, ffi::LUA_REGISTRYINDEX, key());
    let state = if kind == ffi::LUA_TLIGHTUSERDATA {
        ffi::lua_touserdata(l, -1) as *const Mutex<State>
    } else {
        std::ptr::null()
    };
    ffi::lua_pop(l, 1);
    if state.is_null() {
        return None;
    }
    let count = ffi::lua_gethookcount(l);
    let message = {
        let mut state = (*state).lock().unwrap_or_else(|e| e.into_inner());
        let step = count.max(1) as u64;
        let mut now = None;
        let mut message = None;
        for frame in state.frames.iter_mut() {
            if frame.expired.is_none() {
                frame.expired = frame.check(step, &mut now);
            }
            if message.is_none() {
                message = frame.expired.as_ref().map(|(_, m)| m.clone());
            }
        }
        message
    };
    // Sticky: once expired, check every instruction until the scope ends.
    // Threads left at 1 by an earlier expiry return to the cheap rate here.
    let want = if message.is_some() { 1 } else { COUNT };
    if count != want {
        ffi::lua_sethook(l, Some(hook), ffi::LUA_MASKCOUNT, want);
    }
    message
}

/// One bounded entry into Lua. Build, then [`Scope::run`].
pub(crate) struct Scope {
    budget: Budget,
    deadline: Option<Instant>,
    cancel: Option<CancellationToken>,
}

impl Scope {
    pub(crate) fn new(budget: Budget) -> Self {
        Self {
            budget,
            deadline: budget.time().map(|t| Instant::now() + t),
            cancel: None,
        }
    }

    pub(crate) fn cancel(mut self, token: &CancellationToken) -> Self {
        self.cancel = Some(token.clone());
        self
    }

    /// Replace the wall-clock deadline (caller-supplied, e.g. tool_execute).
    pub(crate) fn deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Run `f` under this scope. Returns its value and, if the scope expired
    /// while `f` ran, why. The callback-owner globals are restored on expiry
    /// (Lua wrappers that would restore them are cut short).
    pub(crate) fn run<R>(self, lua: &Lua, f: impl FnOnce() -> R) -> (R, Option<Expiry>) {
        let Some(shared) = lua.app_data_ref::<Shared>().map(|s| s.clone()) else {
            return (f(), None);
        };
        let globals = lua.globals();
        let owner: mlua::Value = globals
            .raw_get("__rness_callback_owner")
            .unwrap_or(mlua::Value::Nil);
        let depth: mlua::Value = globals
            .raw_get("__rness_callback_depth")
            .unwrap_or(mlua::Value::Nil);
        let id = {
            let mut state = shared.lock();
            let id = state.next_id;
            state.next_id += 1;
            state.frames.push(Frame {
                id,
                deadline: self.deadline,
                cancel: self.cancel,
                instructions: self.budget.instructions(),
                budget: self.budget,
                expired: None,
            });
            id
        };
        /// Pops the frame (and any inner frame a panic left behind).
        struct Pop<'a>(&'a Shared, u64);
        impl Pop<'_> {
            fn take(&self) -> Option<Expiry> {
                let mut state = self.0.lock();
                let index = state.frames.iter().position(|f| f.id == self.1)?;
                let frame = &mut state.frames[index];
                // A final check catches overruns the hook could not see
                // (a long native call followed by a few instructions).
                let expired = frame
                    .expired
                    .take()
                    .or_else(|| frame.check(0, &mut None))
                    .map(|(e, _)| e);
                state.frames.truncate(index);
                expired
            }
        }
        impl Drop for Pop<'_> {
            fn drop(&mut self) {
                self.take();
            }
        }
        let pop = Pop(&shared, id);
        let value = f();
        let expired = pop.take();
        if expired.is_some() {
            let _ = globals.raw_set("__rness_callback_owner", owner);
            let _ = globals.raw_set("__rness_callback_depth", depth);
        }
        (value, expired)
    }
}

/// The tightest deadline and every cancel token of the active scopes, for
/// blocking APIs (`rness.http`, `rness.process`) that would otherwise hold
/// the VM past its budget inside one native call.
pub(crate) fn limits(lua: &Lua) -> (Option<Instant>, Vec<CancellationToken>) {
    let Some(shared) = lua.app_data_ref::<Shared>().map(|s| s.clone()) else {
        return (None, Vec::new());
    };
    let state = shared.lock();
    let deadline = state.frames.iter().filter_map(|f| f.deadline).min();
    let tokens = state
        .frames
        .iter()
        .filter_map(|f| f.cancel.clone())
        .collect();
    (deadline, tokens)
}

/// Consecutive budget overruns after which a recurring handler (hook,
/// statusline, app view, timer) is disabled until the plugin reloads.
pub(crate) const STRIKES: u32 = 3;

/// Per-handler overrun counts, disabled handlers, and pending notices for
/// the user. Lives in app_data; borrowed only outside Lua calls.
#[derive(Default)]
struct Strikes {
    counts: std::collections::HashMap<String, u32>,
    disabled: std::collections::HashSet<String>,
    notices: Vec<(Option<String>, String)>,
}

/// Record one bounded call of handler `key`. A clean finish resets its
/// count; a cancel leaves it; an overrun adds a strike. Returns true when
/// this call reached [`STRIKES`] and the handler is now disabled. `key` is
/// built only when needed: the common clean call is nearly free.
pub(crate) fn strike(lua: &Lua, key: impl FnOnce() -> String, expiry: Option<&Expiry>) -> bool {
    let Some(mut strikes) = lua.app_data_mut::<Strikes>() else {
        return false;
    };
    match expiry {
        None => {
            if !strikes.counts.is_empty() {
                strikes.counts.remove(&key());
            }
            false
        }
        Some(Expiry::Cancelled) => false,
        Some(Expiry::Exceeded) => {
            let key = key();
            let count = strikes.counts.entry(key.clone()).or_default();
            *count += 1;
            if *count < STRIKES {
                return false;
            }
            strikes.counts.remove(&key);
            strikes.disabled.insert(key);
            true
        }
    }
}

pub(crate) fn is_disabled(lua: &Lua, key: &str) -> bool {
    lua.app_data_ref::<Strikes>()
        .is_some_and(|s| s.disabled.contains(key))
}

/// Queue a user-facing notice (shown in `session` when known; always logged).
pub(crate) fn notice(lua: &Lua, session: Option<String>, text: String) {
    tracing::warn!(target: "lua", "{text}");
    if let Some(mut strikes) = lua.app_data_mut::<Strikes>() {
        if strikes.notices.len() < 64 {
            strikes.notices.push((session, text));
        }
    }
}

pub(crate) fn take_notices(lua: &Lua) -> Vec<(Option<String>, String)> {
    lua.app_data_mut::<Strikes>()
        .map(|mut s| std::mem::take(&mut s.notices))
        .unwrap_or_default()
}

/// The standard "disabled" notice text.
pub(crate) fn disabled_text(what: &str, budget: Budget) -> String {
    format!(
        "Lua {what} exceeded its {} ms budget {STRIKES} times in a row and was disabled until the plugin is reloaded",
        budget.time().unwrap_or_default().as_millis()
    )
}

/// Resolves when any token is cancelled or `deadline` passes; the value
/// says which (true = deadline).
pub(crate) async fn expired(deadline: Option<Instant>, tokens: Vec<CancellationToken>) -> bool {
    let wait_deadline = async {
        match deadline {
            Some(d) => tokio::time::sleep_until(d.into()).await,
            None => std::future::pending().await,
        }
    };
    let wait_cancel = async {
        if tokens.is_empty() {
            return std::future::pending().await;
        }
        loop {
            if tokens.iter().any(|t| t.is_cancelled()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        biased;
        _ = wait_cancel => false,
        _ = wait_deadline => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm() -> Lua {
        let lua = Lua::new();
        install(&lua).unwrap();
        lua
    }

    fn spin(lua: &Lua, budget: Budget, code: &str) -> (mlua::Result<()>, Option<Expiry>, Duration) {
        let t = Instant::now();
        let f = lua.load(code).into_function().unwrap();
        let (r, e) = Scope::new(budget).run(lua, || f.call::<()>(()));
        (r, e, t.elapsed())
    }

    #[test]
    fn runaway_loop_is_stopped() {
        let lua = vm();
        let (r, e, took) = spin(&lua, Budget::Card, "while true do end");
        assert!(r.is_err());
        assert_eq!(e, Some(Expiry::Exceeded));
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn pcall_cannot_swallow_expiry() {
        let lua = vm();
        for code in [
            "while true do pcall(function() while true do end end) end",
            "while true do xpcall(function() while true do end end, function(e) return e end) end",
            "while true do local co = coroutine.wrap(function() while true do end end); pcall(co) end",
            "while true do pcall(coroutine.resume, coroutine.create(function() while true do end end)) end",
            "while true do pcall(error, 'x') end",
        ] {
            let (r, e, took) = spin(&lua, Budget::Card, code);
            assert!(r.is_err(), "{code}");
            assert_eq!(e, Some(Expiry::Exceeded), "{code}");
            assert!(took < Duration::from_secs(2), "{code}: {took:?}");
        }
    }

    #[test]
    fn coroutine_created_before_scope_is_bounded() {
        let lua = vm();
        lua.load("co = coroutine.create(function() while true do end end)")
            .exec()
            .unwrap();
        let (r, e, took) = spin(&lua, Budget::Card, "coroutine.resume(co) while true do end");
        assert!(r.is_err());
        assert_eq!(e, Some(Expiry::Exceeded));
        assert!(took < Duration::from_secs(2));
    }

    #[test]
    fn code_after_scope_runs_normally_and_cheaply() {
        let lua = vm();
        let _ = spin(
            &lua,
            Budget::Card,
            "while true do pcall(function() end) end",
        );
        // No scope: unbounded, and the sticky per-instruction rate is gone.
        let n: i64 = lua
            .load("local n = 0 for i = 1, 3000000 do n = n + 1 end return n")
            .eval()
            .unwrap();
        assert_eq!(n, 3_000_000);
        let (r, e, _) = spin(
            &lua,
            Budget::Hook,
            "local x = 0 for i = 1, 1000 do x = x + i end",
        );
        assert!(r.is_ok());
        assert_eq!(e, None);
    }

    #[test]
    fn cancel_stops_and_is_reported() {
        let lua = vm();
        let token = CancellationToken::new();
        let t2 = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            t2.cancel();
        });
        let f = lua
            .load("while true do pcall(function() while true do end end) end")
            .into_function()
            .unwrap();
        let (r, e) = Scope::new(Budget::Command)
            .cancel(&token)
            .run(&lua, || f.call::<()>(()));
        assert!(r.unwrap_err().to_string().contains("command cancelled"));
        assert_eq!(e, Some(Expiry::Cancelled));
    }

    #[test]
    fn instruction_allowance_applies_to_tasks() {
        let lua = vm();
        let (r, e, _) = spin(&lua, Budget::Task, "while true do end");
        assert!(r.unwrap_err().to_string().contains("instruction budget"));
        assert_eq!(e, Some(Expiry::Exceeded));
    }

    #[test]
    fn nested_scopes_use_the_tightest() {
        let lua = vm();
        let f = lua.load("while true do end").into_function().unwrap();
        let t = Instant::now();
        let ((r, inner), outer) = Scope::new(Budget::Startup).run(&lua, || {
            Scope::new(Budget::Card).run(&lua, || f.call::<()>(()))
        });
        assert!(r.is_err());
        assert_eq!(inner, Some(Expiry::Exceeded));
        assert_eq!(outer, None);
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn owner_globals_restored_on_expiry() {
        let lua = vm();
        lua.globals()
            .set("__rness_callback_owner", "outer")
            .unwrap();
        let _ = spin(
            &lua,
            Budget::Card,
            "__rness_callback_owner = 'inner' while true do end",
        );
        let owner: String = lua.globals().get("__rness_callback_owner").unwrap();
        assert_eq!(owner, "outer");
    }

    #[test]
    fn limits_report_tightest_deadline() {
        let lua = vm();
        assert!(limits(&lua).0.is_none());
        let ((deadline, _), _) = Scope::new(Budget::Hook).run(&lua, || limits(&lua));
        let left = deadline.unwrap().saturating_duration_since(Instant::now());
        assert!(left <= Duration::from_millis(500));
    }
}
