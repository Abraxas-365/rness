//! rness.task — background Lua work that can await async APIs.
//!
//! ```lua
//! local id = rness.task.spawn(function(session)
//!   local title = rness.llm.complete { session = session, prompt = "…" } -- yields; VM stays free
//!   rness.session.offer_title(session, title, "model")
//! end, ev.session)
//! rness.task.cancel(id)   -- idempotent
//! ```
//!
//! Hooks, timers and statusline renderers are synchronous: they cannot
//! yield. A task is a coroutine run on the VM thread whose yielding calls
//! (`rness.llm.complete`, session search, …) run on the
//! async runtime while other plugins keep the VM. Spawning only queues the
//! task, so it is safe from inside any callback. A task belongs to the
//! plugin that spawned it and is cancelled when that plugin unloads or
//! fails to load; `rness.task.cancel` also aborts an in-flight await.
//! Errors are logged, never propagated. Tasks are process-local.

use mlua::{Function, Lua, Table, Value as LuaValue};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Delivers task wake-ups to the VM thread (the host's command channel).
pub type TaskSink = Box<dyn Fn(TaskWake) + Send + Sync>;

/// A wake-up for the VM actor.
pub enum TaskWake {
    /// Run a task that was just spawned.
    Start(u64),
    /// An awaited future finished; resume the task with its result.
    Resume(u64, Result<serde_json::Value, String>),
}

const TASKS: &str = "__rness_tasks";
const MARKS: &str = "__rness_task_marks";
/// Cooperative per-resume budget (instruction hook every 1000 steps).
const RESUME_BUDGET: usize = 30_000;
/// Concurrent tasks per VM; guards against unbounded spawn loops.
const MAX_TASKS: usize = 256;

struct TaskState {
    sink: Arc<Mutex<Option<TaskSink>>>,
    runtime: Option<tokio::runtime::Handle>,
    next_id: u64,
    /// Cancellation per live task (read by yielding APIs while it runs).
    tokens: HashMap<u64, tokio_util::sync::CancellationToken>,
}

pub fn install(lua: &Lua, rness: &Table) -> mlua::Result<()> {
    lua.set_app_data(TaskState {
        sink: Arc::new(Mutex::new(None)),
        runtime: None,
        next_id: 1,
        tokens: HashMap::new(),
    });
    lua.globals().set(TASKS, lua.create_table()?)?;
    let marks: Table = lua
        .load("return setmetatable({}, { __mode = 'k' })")
        .eval()?;
    lua.globals().set(MARKS, marks)?;

    let task = lua.create_table()?;
    task.set(
        "spawn",
        lua.create_function(|lua, (callback, args): (Function, mlua::MultiValue)| {
            spawn(lua, callback, args)
        })?,
    )?;
    task.set("cancel", lua.create_function(|lua, id: u64| cancel(lua, id))?)?;
    task.set(
        "running",
        lua.create_function(|lua, ()| {
            Ok(lua
                .app_data_ref::<TaskState>()
                .map_or(0, |state| state.tokens.len()))
        })?,
    )?;
    rness.set("task", task)?;
    Ok(())
}

/// Install the wake-up sink. Without one, spawning fails.
pub fn set_sink(lua: &Lua, sink: TaskSink) {
    if let Some(state) = lua.app_data_ref::<TaskState>() {
        *state.sink.lock().unwrap() = Some(sink);
    }
}

/// The async runtime awaited futures run on (installed with the session).
pub fn set_runtime(lua: &Lua, runtime: tokio::runtime::Handle) {
    if let Some(mut state) = lua.app_data_mut::<TaskState>() {
        state.runtime = Some(runtime);
    }
}

fn spawn(lua: &Lua, callback: Function, args: mlua::MultiValue) -> mlua::Result<u64> {
    let owner = crate::runtime::subscription_owner(lua)?;
    let (id, sink) = {
        let mut state = lua
            .app_data_mut::<TaskState>()
            .ok_or_else(|| mlua::Error::runtime("rness.task unavailable"))?;
        if state.sink.lock().unwrap().is_none() {
            return Err(mlua::Error::runtime("rness.task: no task runner in this host"));
        }
        if state.tokens.len() >= MAX_TASKS {
            return Err(mlua::Error::runtime(format!(
                "rness.task: too many running tasks (max {MAX_TASKS})"
            )));
        }
        let id = state.next_id;
        state.next_id += 1;
        (id, state.sink.clone())
    };
    // Build and register everything fallible before the task becomes live.
    let entry = lua.create_table()?;
    entry.set("thread", lua.create_thread(callback)?)?;
    let packed = lua.create_table()?;
    let count = args.len();
    for (i, value) in args.into_iter().enumerate() {
        packed.raw_set(i + 1, value)?;
    }
    packed.set("n", count)?;
    entry.set("args", packed)?;
    if let Some(owner) = &owner {
        entry.set("owner", owner.clone())?;
        track_owner(lua, owner.clone())?;
    }
    lua.globals().get::<Table>(TASKS)?.set(id, entry)?;
    if let Some(mut state) = lua.app_data_mut::<TaskState>() {
        state
            .tokens
            .insert(id, tokio_util::sync::CancellationToken::new());
    }
    if let Some(sink) = sink.lock().unwrap().as_ref() {
        sink(TaskWake::Start(id));
    }
    Ok(id)
}

fn cancel(lua: &Lua, id: u64) -> mlua::Result<bool> {
    let tasks: Table = lua.globals().get(TASKS)?;
    let existed = tasks.get::<Option<Table>>(id)?.is_some();
    tasks.set(id, LuaValue::Nil)?;
    if let Some(mut state) = lua.app_data_mut::<TaskState>() {
        if let Some(token) = state.tokens.remove(&id) {
            token.cancel();
        }
    }
    Ok(existed)
}

/// One cancel-all closure per (cleanup target, owner) pair: the owner's own
/// table, plus the cleanup table of a plugin loading right now (so a failed
/// load cancels tasks spawned during it).
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
        let owners = match marks.get::<Option<Table>>(target.clone())? {
            Some(owners) => owners,
            None => {
                let owners: Table = lua
                    .load("return setmetatable({}, { __mode = 'k' })")
                    .eval()?;
                marks.set(target.clone(), owners.clone())?;
                owners
            }
        };
        if owners.get::<Option<bool>>(owner.clone())?.unwrap_or(false) {
            continue;
        }
        owners.set(owner.clone(), true)?;
        let owner = owner.clone();
        let cancel_all = lua.create_function(move |lua, ()| {
            let tasks: Table = lua.globals().get(TASKS)?;
            let mut owned = Vec::new();
            for pair in tasks.pairs::<u64, Table>() {
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

/// Run a task until it finishes or awaits. Unknown (cancelled) ids and
/// stale resumes are ignored. Errors are returned for logging.
pub fn wake(lua: &Lua, wake: TaskWake) -> Result<(), String> {
    let (id, args) = match wake {
        TaskWake::Start(id) => (id, None),
        TaskWake::Resume(id, result) => (id, Some(result)),
    };
    let run = move || -> mlua::Result<Option<crate::api::session::CommandFuture>> {
        let tasks: Table = lua.globals().get(TASKS)?;
        let Some(entry) = tasks.get::<Option<Table>>(id)? else {
            return Ok(None);
        };
        let thread: mlua::Thread = entry.get("thread")?;
        let token = lua
            .app_data_ref::<TaskState>()
            .and_then(|state| state.tokens.get(&id).cloned())
            .unwrap_or_default();
        let owner: LuaValue = entry.get("owner")?;
        let args: mlua::MultiValue = match args {
            None => {
                let packed: Table = entry.get("args")?;
                let n: usize = packed.get("n")?;
                (1..=n)
                    .map(|i| packed.raw_get::<LuaValue>(i))
                    .collect::<mlua::Result<_>>()?
            }
            Some(Ok(value)) => {
                use mlua::LuaSerdeExt;
                (true, lua.to_value(&value)?).into_lua_multi(lua)?
            }
            Some(Err(error)) => (false, error).into_lua_multi(lua)?,
        };
        // Task context: its owner (so registrations it makes are owned)
        // and a per-resume instruction budget; cancel also stops a loop.
        let globals = lua.globals();
        let prev_owner: LuaValue = globals.get("__rness_callback_owner")?;
        let prev_depth: LuaValue = globals.get("__rness_callback_depth")?;
        globals.set("__rness_callback_owner", owner)?;
        globals.set("__rness_callback_depth", 1)?;
        let budget = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_token = token.clone();
        thread.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(1000),
            move |_, _| {
                if hook_token.is_cancelled() {
                    Err(mlua::Error::runtime("task cancelled"))
                } else if budget.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= RESUME_BUDGET {
                    Err(mlua::Error::runtime("task exceeded its instruction budget between awaits"))
                } else {
                    Ok(mlua::VmState::Continue)
                }
            },
        );
        // Replaced on every resume; dropped with the finished thread.
        let result = thread.resume::<mlua::MultiValue>(args);
        globals.set("__rness_callback_owner", prev_owner)?;
        globals.set("__rness_callback_depth", prev_depth)?;
        let finished = |lua: &Lua| -> mlua::Result<()> {
            lua.globals().get::<Table>(TASKS)?.set(id, LuaValue::Nil)?;
            if let Some(mut state) = lua.app_data_mut::<TaskState>() {
                state.tokens.remove(&id);
            }
            Ok(())
        };
        let mut values = match result {
            Ok(values) => values,
            Err(error) => {
                finished(lua)?;
                return Err(error);
            }
        };
        if thread.status() != mlua::ThreadStatus::Resumable {
            finished(lua)?;
            return Ok(None);
        }
        // Cancelled during this resume (itself, or its plugin unloaded):
        // drop whatever it yielded without running it.
        if lua.globals().get::<Table>(TASKS)?.get::<Option<Table>>(id)?.is_none() {
            return Ok(None);
        }
        let request = match values.pop_front() {
            Some(LuaValue::UserData(request))
                if values.is_empty() && request.is::<crate::api::session::CommandYield>() =>
            {
                request.take::<crate::api::session::CommandYield>()?.0
            }
            Some(LuaValue::UserData(request))
                if values.is_empty() && request.is::<crate::api::session::SearchRequest>() =>
            {
                request.take::<crate::api::session::SearchRequest>()?.0
            }
            _ => {
                finished(lua)?;
                return Err(mlua::Error::runtime(
                    "tasks may only yield through async rness APIs",
                ));
            }
        };
        Ok(Some(request))
    };
    let future = match run() {
        Ok(Some(future)) => future,
        Ok(None) => return Ok(()),
        Err(error) => return Err(crate::runtime::user_message(&error)),
    };
    let (sink, runtime, token) = {
        let state = lua
            .app_data_ref::<TaskState>()
            .ok_or("rness.task unavailable")?;
        (
            state.sink.clone(),
            state.runtime.clone(),
            state.tokens.get(&id).cloned().unwrap_or_default(),
        )
    };
    let Some(runtime) = runtime else {
        let _ = cancel(lua, id);
        return Err("rness.task: no async runtime (host not mounted)".into());
    };
    // Resume even if the future is dropped or panics; a cancelled task is
    // unknown by then, so its late resume is ignored.
    struct Completion {
        sink: Arc<Mutex<Option<TaskSink>>>,
        id: u64,
        result: Option<Result<serde_json::Value, String>>,
    }
    impl Drop for Completion {
        fn drop(&mut self) {
            let result = self
                .result
                .take()
                .unwrap_or_else(|| Err("task await stopped".into()));
            if let Some(sink) = self.sink.lock().unwrap().as_ref() {
                sink(TaskWake::Resume(self.id, result));
            }
        }
    }
    let mut completion = Completion { sink, id, result: None };
    runtime.spawn(async move {
        completion.result = Some(tokio::select! {
            result = future => result,
            _ = token.cancelled() => Err("task cancelled".into()),
        });
        drop(completion);
    });
    Ok(())
}

use mlua::IntoLuaMulti;
