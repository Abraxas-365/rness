//! rness.timer — one-shot and repeating callbacks.
//!
//! ```lua
//! local id = rness.timer.after(5, function() ... end)   -- once, in 5s
//! local id = rness.timer.every(60, function() ... end)  -- every 60s
//! rness.timer.cancel(id)                                 -- idempotent
//! ```
//!
//! Deadlines live on a lazily spawned `lua-timer` thread; callbacks always
//! run on the VM thread (the host delivers `FireTimer` through its command
//! channel). Callbacks are synchronous: they cannot yield, so APIs such as
//! `rness.llm.complete` are unavailable there (use `rness.session.send`).
//! Timers created by a plugin are cancelled when it unloads or fails to
//! load. Timers are process-local and never persisted.

use mlua::{Function, Lua, Table, Value as LuaValue};
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Receives the id of each due timer; the host forwards it to the VM thread.
pub type TimerSink = Box<dyn Fn(u64) + Send>;

const TIMERS: &str = "__rness_timers";
const MARKS: &str = "__rness_timer_marks";
const MAX_SECONDS: f64 = 366.0 * 24.0 * 60.0 * 60.0;
const MIN_EVERY_SECONDS: f64 = 1.0;

enum Msg {
    Schedule {
        id: u64,
        at: Instant,
        every: Option<Duration>,
    },
    Cancel(u64),
}

struct TimerState {
    tx: Option<mpsc::Sender<Msg>>,
    sink: Arc<Mutex<Option<TimerSink>>>,
    /// Ids handed to the sink whose callback has not started yet. A busy VM
    /// thread thus sees at most one pending fire per timer (no burst).
    in_flight: Arc<Mutex<HashSet<u64>>>,
    next_id: u64,
}

pub fn install(lua: &Lua, rness: &Table) -> mlua::Result<()> {
    lua.set_app_data(TimerState {
        tx: None,
        sink: Arc::new(Mutex::new(None)),
        in_flight: Arc::new(Mutex::new(HashSet::new())),
        next_id: 1,
    });
    lua.globals().set(TIMERS, lua.create_table()?)?;
    let marks: Table = lua
        .load("return setmetatable({}, { __mode = 'k' })")
        .eval()?;
    lua.globals().set(MARKS, marks)?;

    let timer = lua.create_table()?;
    timer.set(
        "after",
        lua.create_function(|lua, (seconds, callback): (f64, Function)| {
            create(lua, seconds, callback, false)
        })?,
    )?;
    timer.set(
        "every",
        lua.create_function(|lua, (seconds, callback): (f64, Function)| {
            create(lua, seconds, callback, true)
        })?,
    )?;
    timer.set(
        "cancel",
        lua.create_function(|lua, id: u64| cancel(lua, id))?,
    )?;
    rness.set("timer", timer)?;
    Ok(())
}

/// Install the delivery sink. Without one, due timers are dropped.
pub fn set_sink(lua: &Lua, sink: TimerSink) {
    if let Some(state) = lua.app_data_ref::<TimerState>() {
        *state.sink.lock().unwrap() = Some(sink);
    }
}

/// Run a due timer's callback on the VM thread. Unknown/cancelled ids are ignored.
pub fn fire(lua: &Lua, id: u64) -> mlua::Result<()> {
    if let Some(state) = lua.app_data_ref::<TimerState>() {
        state.in_flight.lock().unwrap().remove(&id);
    }
    let timers: Table = lua.globals().get(TIMERS)?;
    let Some(entry) = timers.get::<Option<Table>>(id)? else {
        return Ok(());
    };
    if !entry.get::<bool>("every")? {
        timers.set(id, LuaValue::Nil)?;
    }
    entry.get::<Function>("fn")?.call::<()>(())
}

fn create(lua: &Lua, seconds: f64, callback: Function, every: bool) -> mlua::Result<u64> {
    let name = if every { "every" } else { "after" };
    if !seconds.is_finite() || seconds <= 0.0 || seconds > MAX_SECONDS {
        return Err(mlua::Error::runtime(format!(
            "rness.timer.{name}: seconds must be > 0 and <= {MAX_SECONDS}"
        )));
    }
    if every && seconds < MIN_EVERY_SECONDS {
        return Err(mlua::Error::runtime(format!(
            "rness.timer.every: seconds must be >= {MIN_EVERY_SECONDS}"
        )));
    }
    let callback = crate::runtime::owned_callback(lua, callback)?;
    let owner = crate::runtime::subscription_owner(lua)?;

    let (id, tx) = {
        let mut state = lua
            .app_data_mut::<TimerState>()
            .ok_or_else(|| mlua::Error::runtime("rness.timer unavailable"))?;
        let id = state.next_id;
        state.next_id += 1;
        if state.tx.is_none() {
            let (tx, rx) = mpsc::channel();
            let sink = state.sink.clone();
            let in_flight = state.in_flight.clone();
            std::thread::Builder::new()
                .name("lua-timer".into())
                .spawn(move || drive(rx, sink, in_flight))
                .map_err(mlua::Error::external)?;
            state.tx = Some(tx);
        }
        (id, state.tx.clone().unwrap())
    };

    let entry = lua.create_table()?;
    entry.set("fn", callback)?;
    entry.set("every", every)?;
    if let Some(owner) = &owner {
        entry.set("owner", owner.clone())?;
    }
    lua.globals().get::<Table>(TIMERS)?.set(id, entry)?;

    let period = Duration::from_secs_f64(seconds);
    tx.send(Msg::Schedule {
        id,
        at: Instant::now() + period,
        every: every.then_some(period),
    })
    .map_err(|_| mlua::Error::runtime("rness.timer: driver stopped"))?;

    if let Some(owner) = owner {
        track_owner(lua, owner)?;
    }
    Ok(id)
}

fn cancel(lua: &Lua, id: u64) -> mlua::Result<bool> {
    let timers: Table = lua.globals().get(TIMERS)?;
    let existed = timers.get::<Option<Table>>(id)?.is_some();
    timers.set(id, LuaValue::Nil)?;
    if let Some(state) = lua.app_data_ref::<TimerState>() {
        if let Some(tx) = &state.tx {
            let _ = tx.send(Msg::Cancel(id));
        }
    }
    Ok(existed)
}

/// Register one cancel-all closure per owner table (and per loading-cleanup
/// table) so unload / failed load cancels the plugin's timers.
fn track_owner(lua: &Lua, owner: Table) -> mlua::Result<()> {
    let marks: Table = lua.globals().get(MARKS)?;
    let mut targets = vec![owner.clone()];
    if let Some(cleanup) = lua
        .globals()
        .get::<Option<Table>>("__rness_loading_hooks")?
    {
        if cleanup != owner {
            targets.push(cleanup);
        }
    }
    for target in targets {
        if marks.get::<Option<bool>>(target.clone())?.unwrap_or(false) {
            continue;
        }
        marks.set(target.clone(), true)?;
        let owner = owner.clone();
        let cancel_all = lua.create_function(move |lua, ()| {
            let timers: Table = lua.globals().get(TIMERS)?;
            let mut owned = Vec::new();
            for pair in timers.pairs::<u64, Table>() {
                let (id, entry) = pair?;
                if entry.get::<Option<Table>>("owner")?.as_ref() == Some(&owner) {
                    owned.push(id);
                }
            }
            for id in owned {
                cancel(lua, id)?;
            }
            Ok(true)
        })?;
        target.push(cancel_all)?;
    }
    Ok(())
}

fn drive(
    rx: mpsc::Receiver<Msg>,
    sink: Arc<Mutex<Option<TimerSink>>>,
    in_flight: Arc<Mutex<HashSet<u64>>>,
) {
    let mut timers: HashMap<u64, (Instant, Option<Duration>)> = HashMap::new();
    loop {
        let next = timers.values().map(|(at, _)| *at).min();
        let msg = match next {
            None => match rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => return,
            },
            Some(at) => {
                let now = Instant::now();
                if at <= now {
                    None
                } else {
                    match rx.recv_timeout(at - now) {
                        Ok(msg) => Some(msg),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        };
        match msg {
            Some(Msg::Schedule { id, at, every }) => {
                timers.insert(id, (at, every));
            }
            Some(Msg::Cancel(id)) => {
                timers.remove(&id);
            }
            None => {
                let now = Instant::now();
                let mut due: Vec<(Instant, u64)> = timers
                    .iter()
                    .filter(|(_, (at, _))| *at <= now)
                    .map(|(id, (at, _))| (*at, *id))
                    .collect();
                due.sort();
                for (at, id) in due {
                    match timers.get(&id).and_then(|(_, every)| *every) {
                        // Skip missed slots rather than bursting to catch up.
                        Some(period) => {
                            let mut next = at + period;
                            if next <= now {
                                next = now + period;
                            }
                            timers.insert(id, (next, Some(period)));
                        }
                        None => {
                            timers.remove(&id);
                        }
                    }
                    if let Some(sink) = sink.lock().unwrap().as_ref() {
                        // Previous fire not yet run by the VM: skip this slot.
                        if in_flight.lock().unwrap().insert(id) {
                            sink(id);
                        }
                    }
                }
            }
        }
    }
}
