//! The run's event loop: Lua coroutines driven by a Rust scheduler on the
//! run's own blocking thread.
//!
//! Every hook (`agent`, `parallel`, `pipeline`, `phase`, `log`) is a thin
//! Lua function that `coroutine.yield`s a request to this loop through a
//! private upvalue — scripts get no `coroutine` library, so they cannot
//! yield to (or hide work from) the scheduler. The loop owns all run state;
//! Rust callbacks inside the VM are stateless (`json`, `compact`, `check`).
//!
//! Fatal errors never travel through Lua: the loop validates a hook's
//! request and ends the run on a violation. Budget/cancellation, raised by
//! the instruction hook, are made uncatchable by a sticky flag that the
//! sandbox's `pcall`/`xpcall` wrappers re-raise.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::{
    Function, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Thread, ThreadStatus,
    Value as LuaValue, VmState,
};
use rness_protocol::events::SessionId;
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    convert, AgentOutcome, ChildOutcome, ChildRequest, ChildRunner, Observer, WorkflowEvent,
    WorkflowLimits, WorkflowMeta, WorkflowResult, WorkflowStop,
};

const HOOK_EVERY: u32 = 1000;
const ROOT: usize = 0;
const SUPPORTED: &str = "label, phase, schema, role, provider";

/// Runs with the sandbox's `coroutine`, `pcall`, `xpcall` still intact and
/// receives `check` as its vararg. Returns the per-item pipeline driver and
/// the task guard every coroutine body runs under.
const PRELUDE: &str = r#"
local yield, raw_pcall, raw_xpcall, check, pack = coroutine.yield, pcall, xpcall, ..., table.pack
local raw_setmetatable, raw_rawget, unpack = setmetatable, rawget, table.unpack
-- Captured: scripts can rebind the globals, which would disable the guards.
local type, tostring, error = type, tostring, error
coroutine = nil
-- The string metatable is shared with the host: a script-set `__tostring`
-- there would run when mlua formats an error on the main state, where no
-- budget hook fires. `__metatable` hides it from getmetatable.
getmetatable("").__metatable = false
function agent(prompt, opts) return (yield("agent", prompt, opts)) end
function parallel(thunks) return (yield("parallel", thunks)) end
function pipeline(items, ...) return (yield("pipeline", items, pack(...))) end
function phase(title) yield("phase", title) end
function log(message) yield("log", message) end
function pcall(...) return check(raw_pcall(...)) end
-- The handler sees the error first: `check` marks a memory error sticky
-- (raising in a handler just fails the xpcall; the outer check re-raises).
function xpcall(f, handler, ...)
  return check(raw_xpcall(f, function(e) check(false, e) return handler(e) end, ...))
end
-- Finalizers run with hooks disabled (no budget, no cancellation), so no
-- script table may be marked for finalization. Lua 5.4 marks an object only
-- if `__gc` is present when the metatable is set, so this check is enough.
function setmetatable(t, mt)
  if type(mt) == "table" and raw_rawget(mt, "__gc") ~= nil then
    error("setmetatable: __gc metamethods are not allowed in workflow scripts", 2)
  end
  return raw_setmetatable(t, mt)
end
return function(stages, item, index)
  local value = item
  for i = 1, stages.n do
    value = stages[i](value, item, index)
    if value == nil then return nil end
  end
  return value
end, function(f, ...)
  -- Every task body runs under this guard: a non-string error object is
  -- stringified HERE, inside the hooked coroutine, so a looping
  -- `__tostring` stays under the budget (mlua would otherwise format it
  -- on the main state, where no hook fires). `check` first keeps memory
  -- and budget/cancel errors fatal.
  local r = pack(raw_pcall(f, ...))
  if r[1] then return unpack(r, 2, r.n) end
  local e = r[2]
  check(false, e)
  if type(e) ~= "string" then
    -- `__tostring` may itself raise a table: never let one escape.
    local ok, s = raw_pcall(tostring, e)
    check(ok, s)
    e = (ok and type(s) == "string") and s or "(error object is not a string)"
  end
  error(e, 0)
end
"#;

fn sandbox(limits: &WorkflowLimits) -> mlua::Result<Lua> {
    let libs = StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE;
    let lua = Lua::new_with(libs, LuaOptions::default())?;
    lua.set_memory_limit(limits.memory_bytes)?;
    for name in ["dofile", "loadfile", "load", "collectgarbage", "print"] {
        lua.globals().set(name, LuaValue::Nil)?;
    }
    Ok(lua)
}

fn too_large(script: &str, limits: &WorkflowLimits) -> Option<String> {
    (script.len() > limits.max_script_bytes).then(|| {
        format!(
            "workflow script is {} bytes, over the {}-byte limit",
            script.len(),
            limits.max_script_bytes
        )
    })
}

fn compile(lua: &Lua, name: &str, script: &str) -> Result<Function, String> {
    lua.load(script)
        .set_name(format!("=workflow:{name}"))
        .set_mode(mlua::ChunkMode::Text)
        .into_function()
        .map_err(|e| format!("workflow script does not parse: {}", render(&e)))
}

pub(super) fn compile_check(
    name: &str,
    script: &str,
    limits: &WorkflowLimits,
) -> Result<(), String> {
    if let Some(error) = too_large(script, limits) {
        return Err(error);
    }
    let lua = Lua::new_with(StdLib::NONE, LuaOptions::default()).map_err(|e| e.to_string())?;
    compile(&lua, name, script).map(|_| ())
}

/// One-line-ish message without mlua's stack traceback.
fn render(error: &mlua::Error) -> String {
    let text = error.to_string();
    let text = text.split("\nstack traceback:").next().unwrap_or(&text);
    let text = text.trim();
    // Errors re-raised by the task guard carry mlua's prefix twice.
    let mut text = text;
    while let Some(rest) = text.strip_prefix("runtime error: ") {
        text = rest;
    }
    text.to_owned()
}

fn is_memory(error: &mlua::Error) -> bool {
    match error {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::CallbackError { cause, .. } => is_memory(cause),
        mlua::Error::WithContext { cause, .. } => is_memory(cause),
        _ => false,
    }
}

fn memory_error(mib: usize) -> String {
    format!(
        "workflow exceeded its memory limit ({mib} MiB); child results are held in memory — return less, or summarize inside the children"
    )
}

fn default_label(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("");
    if line.chars().count() <= 48 {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(47).collect::<String>())
    }
}

pub(super) struct Run {
    pub meta: WorkflowMeta,
    pub script: String,
    pub args: Value,
    pub limits: WorkflowLimits,
    pub runner: Arc<dyn ChildRunner>,
    pub observer: Observer,
    pub cancel: CancellationToken,
    pub handle: tokio::runtime::Handle,
    /// Set while a Lua slice runs: the instant its budget ends. The async
    /// side detaches a worker stuck past it inside a C call (hooks can't fire).
    pub in_step: Arc<Mutex<Option<Instant>>>,
    /// Mirrors the started-agent count for a detached run's result.
    pub agents_started: Arc<std::sync::atomic::AtomicUsize>,
}

enum Msg {
    Started(usize, SessionId),
    Done(usize, ChildOutcome),
    Cancel,
}

/// Settles the child's slot even if the runner panics or the task is
/// dropped, so the loop can never wait on a child that will not report.
struct DoneGuard {
    tx: mpsc::UnboundedSender<Msg>,
    seq: usize,
    sent: bool,
}

impl DoneGuard {
    fn send(mut self, outcome: ChildOutcome) {
        self.sent = true;
        let _ = self.tx.send(Msg::Done(self.seq, outcome));
    }
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.tx.send(Msg::Done(
                self.seq,
                ChildOutcome::Fatal("child run ended without settling".into()),
            ));
        }
    }
}

enum End {
    Value(Value),
    Error(String),
    Cancelled,
}

struct Task {
    thread: Thread,
    /// `(join, index)` when this task is one item of a `parallel`/`pipeline`.
    join: Option<(usize, usize)>,
}

struct Join {
    waiter: usize,
    remaining: usize,
    results: Vec<LuaValue>,
}

struct Options {
    label: Option<String>,
    phase: Option<String>,
    role: Option<String>,
    provider: String,
    schema: Option<Value>,
}

impl Run {
    pub(super) fn execute(self) -> WorkflowResult {
        // The worker is a plain OS thread: enter the runtime so timers and
        // spawns made from it (dispose's timeout) find a reactor.
        let _runtime = self.handle.enter();
        let (tx, rx) = mpsc::unbounded_channel();
        let watcher = {
            let (tx, token) = (tx.clone(), self.cancel.clone());
            self.handle.spawn(async move {
                token.cancelled().await;
                let _ = tx.send(Msg::Cancel);
            })
        };
        let children = self.cancel.child_token();
        let (end, started) = match Sched::new(&self, tx, rx, children.clone()) {
            Ok(mut sched) => {
                let end = sched.drive();
                sched.dispose();
                (end, sched.started)
            }
            Err(error) => (End::Error(error), 0),
        };
        children.cancel();
        watcher.abort();
        let failed = |stop, error| WorkflowResult {
            stop,
            value: None,
            error: Some(error),
            agents_started: started,
        };
        match end {
            End::Value(value) => WorkflowResult {
                stop: WorkflowStop::Completed,
                value: Some(value),
                error: None,
                agents_started: started,
            },
            End::Error(error) => failed(WorkflowStop::Error, error),
            End::Cancelled => failed(WorkflowStop::Cancelled, "workflow run cancelled".into()),
        }
    }
}

struct Sched<'a> {
    run: &'a Run,
    lua: Lua,
    run_item: Function,
    /// Wraps every task body (see PRELUDE): first resume is `guard(f, ...)`.
    guard: Function,
    tasks: HashMap<usize, Task>,
    next_task: usize,
    joins: HashMap<usize, Join>,
    next_join: usize,
    ready: VecDeque<(usize, MultiValue)>,
    /// seq → (waiting task, schema requested)
    waiting: HashMap<usize, (usize, bool)>,
    queue: VecDeque<ChildRequest>,
    in_flight: usize,
    started: usize,
    phase: Option<String>,
    used: Duration,
    sticky: Arc<Mutex<Option<String>>>,
    tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    children: CancellationToken,
}

impl<'a> Sched<'a> {
    fn new(
        run: &'a Run,
        tx: mpsc::UnboundedSender<Msg>,
        rx: mpsc::UnboundedReceiver<Msg>,
        children: CancellationToken,
    ) -> Result<Self, String> {
        if let Some(error) = too_large(&run.script, &run.limits) {
            return Err(error);
        }
        let setup = |e: mlua::Error| format!("workflow sandbox setup failed: {e}");
        let lua = sandbox(&run.limits).map_err(setup)?;
        convert::install(&lua).map_err(setup)?;
        let sticky: Arc<Mutex<Option<String>>> = Arc::default();
        let globals = lua.globals();

        let flag = sticky.clone();
        let memory_mib = run.limits.memory_bytes / (1024 * 1024);
        let check = lua
            .create_function(move |_, values: MultiValue| {
                // The memory cap is fatal like the CPU budget: a pcall that
                // caught an allocation failure re-raises it, stickily.
                let caught_memory = matches!(values.front(), Some(LuaValue::Boolean(false)))
                    && match values.get(1) {
                        Some(LuaValue::Error(error)) => is_memory(error),
                        Some(LuaValue::String(s)) => s.as_bytes().as_ref() == b"not enough memory",
                        _ => false,
                    };
                let mut flag = flag.lock().unwrap();
                if caught_memory && flag.is_none() {
                    *flag = Some(memory_error(memory_mib));
                }
                match flag.clone() {
                    Some(error) => Err(mlua::Error::runtime(error)),
                    None => Ok(values),
                }
            })
            .map_err(setup)?;
        let (run_item, guard): (Function, Function) = lua
            .load(PRELUDE)
            .set_name("=workflow-prelude")
            .call(check)
            .map_err(setup)?;

        let json = lua.create_table().map_err(setup)?;
        let max_bytes = run.limits.memory_bytes;
        json.set(
            "encode",
            lua.create_function(move |lua, value: LuaValue| {
                let value = convert::from_lua(lua, &value, "value", max_bytes)
                    .map_err(mlua::Error::runtime)?;
                serde_json::to_string(&value).map_err(mlua::Error::external)
            })
            .map_err(setup)?,
        )
        .map_err(setup)?;
        json.set(
            "decode",
            lua.create_function(|lua, text: mlua::String| {
                let value: Value = serde_json::from_slice(&text.as_bytes())
                    .map_err(|e| mlua::Error::runtime(format!("json.decode: {e}")))?;
                convert::to_lua(lua, &value)
            })
            .map_err(setup)?,
        )
        .map_err(setup)?;
        globals.set("json", json).map_err(setup)?;
        globals
            .set(
                "compact",
                lua.create_function(|lua, list: Table| {
                    let len = convert::seq_len(lua, &list)?;
                    let mut kept = Vec::new();
                    for i in 1..=len {
                        let value: LuaValue = list.raw_get(i)?;
                        if !value.is_nil() {
                            kept.push(value);
                        }
                    }
                    convert::array(lua, kept)
                })
                .map_err(setup)?,
            )
            .map_err(setup)?;
        let args = match &run.args {
            Value::Null => Value::Object(Map::new()),
            other => other.clone(),
        };
        globals
            .set("args", convert::to_lua(&lua, &args).map_err(setup)?)
            .map_err(setup)?;

        let body = compile(&lua, &run.meta.name, &run.script)?;
        let root = lua.create_thread(guard.clone()).map_err(setup)?;
        let mut tasks = HashMap::new();
        tasks.insert(
            ROOT,
            Task {
                thread: root,
                join: None,
            },
        );
        let mut ready = VecDeque::new();
        ready.push_back((ROOT, MultiValue::from_vec(vec![LuaValue::Function(body)])));
        Ok(Self {
            run,
            lua,
            run_item,
            guard,
            tasks,
            next_task: ROOT + 1,
            joins: HashMap::new(),
            next_join: 0,
            ready,
            waiting: HashMap::new(),
            queue: VecDeque::new(),
            in_flight: 0,
            started: 0,
            phase: None,
            used: Duration::ZERO,
            sticky,
            tx,
            rx,
            children,
        })
    }

    fn emit(&self, event: WorkflowEvent) {
        (self.run.observer)(event);
    }

    fn drive(&mut self) -> End {
        loop {
            if self.run.cancel.is_cancelled() {
                return End::Cancelled;
            }
            if let Some((id, args)) = self.ready.pop_front() {
                if let Some(end) = self.step(id, args) {
                    return end;
                }
                continue;
            }
            self.admit();
            if self.in_flight == 0 {
                return End::Error(
                    "workflow stalled: no script task is runnable and no agent is running".into(),
                );
            }
            match self.run.handle.block_on(self.rx.recv()) {
                None => return End::Error("workflow child channel closed".into()),
                Some(Msg::Cancel) => return End::Cancelled,
                Some(Msg::Started(seq, child)) => {
                    self.emit(WorkflowEvent::AgentStarted { seq, child })
                }
                Some(Msg::Done(seq, outcome)) => {
                    if let Some(end) = self.settle(seq, outcome) {
                        return end;
                    }
                }
            }
        }
    }

    /// Resume one task for one slice under the CPU budget.
    fn step(&mut self, id: usize, args: MultiValue) -> Option<End> {
        let thread = self.tasks[&id].thread.clone();
        let budget = self.run.limits.script_budget;
        let begin = Instant::now();
        let deadline = begin + budget.saturating_sub(self.used);
        let (sticky, cancel) = (self.sticky.clone(), self.run.cancel.clone());
        thread.set_hook(HookTriggers::new().every_nth_instruction(HOOK_EVERY), move |_, _| {
            let reason = if cancel.is_cancelled() {
                "workflow run cancelled".to_owned()
            } else if Instant::now() >= deadline {
                format!(
                    "workflow script exceeded its CPU budget ({} ms of Lua execution; time waiting on agents is not counted)",
                    budget.as_millis()
                )
            } else {
                return Ok(VmState::Continue);
            };
            let mut flag = sticky.lock().unwrap();
            let error = flag.get_or_insert(reason).clone();
            Err(mlua::Error::runtime(error))
        });
        let result = {
            *self.run.in_step.lock().unwrap() = Some(deadline);
            let result = thread.resume::<MultiValue>(args);
            *self.run.in_step.lock().unwrap() = None;
            result
        };
        self.used += begin.elapsed();
        if let Some(error) = self.sticky.lock().unwrap().clone() {
            return Some(if self.run.cancel.is_cancelled() {
                End::Cancelled
            } else {
                End::Error(error)
            });
        }
        match result {
            Err(error) if is_memory(&error) => Some(End::Error(memory_error(
                self.run.limits.memory_bytes / (1024 * 1024),
            ))),
            Err(error) => self.finish(id, Err(error)),
            Ok(values) if thread.status() == ThreadStatus::Resumable => self.dispatch(id, values),
            Ok(values) => self.finish(id, Ok(values.into_iter().next().unwrap_or(LuaValue::Nil))),
        }
    }

    fn finish(&mut self, id: usize, result: mlua::Result<LuaValue>) -> Option<End> {
        let task = self.tasks.remove(&id)?;
        if id == ROOT {
            return Some(match result {
                Ok(value) => match convert::from_lua(
                    &self.lua,
                    &value,
                    "value",
                    self.run.limits.memory_bytes,
                ) {
                    Ok(value) => End::Value(value),
                    Err(error) => End::Error(format!(
                        "the workflow's return value is not plain JSON data — {error}. Return only tables, strings, numbers, booleans and nil"
                    )),
                },
                Err(error) => End::Error(format!("workflow script failed: {}", render(&error))),
            });
        }
        // An ordinary error inside an item resolves that item to nil.
        let value = result.unwrap_or(LuaValue::Nil);
        let (join, index) = task.join?;
        let entry = self.joins.get_mut(&join)?;
        entry.results[index] = value;
        entry.remaining -= 1;
        if entry.remaining > 0 {
            return None;
        }
        let Join {
            waiter, results, ..
        } = self.joins.remove(&join)?;
        self.resume_with_array(waiter, results)
    }

    fn resume_with_array(&mut self, waiter: usize, results: Vec<LuaValue>) -> Option<End> {
        match convert::array(&self.lua, results) {
            Ok(table) => {
                self.ready
                    .push_back((waiter, MultiValue::from_vec(vec![LuaValue::Table(table)])));
                None
            }
            Err(error) => Some(End::Error(format!(
                "workflow failed to collect results: {}",
                render(&error)
            ))),
        }
    }

    fn dispatch(&mut self, id: usize, values: MultiValue) -> Option<End> {
        let mut values = values.into_iter();
        let op = match values.next() {
            Some(LuaValue::String(op)) => op.to_string_lossy(),
            _ => {
                return Some(End::Error(
                    "workflow internal error: unexpected yield".into(),
                ))
            }
        };
        let mut arg = || values.next().unwrap_or(LuaValue::Nil);
        let outcome = match op.as_str() {
            "agent" => {
                let (prompt, opts) = (arg(), arg());
                self.agent(id, prompt, opts)
            }
            "parallel" => {
                let thunks = arg();
                self.parallel(id, thunks)
            }
            "pipeline" => {
                let (items, stages) = (arg(), arg());
                self.pipeline(id, items, stages)
            }
            "phase" => match arg() {
                LuaValue::String(title) if !title.as_bytes().is_empty() => {
                    let title = title.to_string_lossy();
                    self.phase = Some(title.clone());
                    self.emit(WorkflowEvent::Phase(title));
                    self.ready.push_front((id, MultiValue::new()));
                    Ok(())
                }
                _ => Err("phase() requires a non-empty title string".into()),
            },
            "log" => match arg() {
                LuaValue::String(message) => {
                    self.emit(WorkflowEvent::Log(message.to_string_lossy()));
                    self.ready.push_front((id, MultiValue::new()));
                    Ok(())
                }
                _ => Err("log() requires a message string".into()),
            },
            other => Err(format!("workflow internal error: unknown request {other}")),
        };
        outcome.err().map(End::Error)
    }

    fn agent(&mut self, id: usize, prompt: LuaValue, opts: LuaValue) -> Result<(), String> {
        let prompt = match &prompt {
            LuaValue::String(s) => s.to_string_lossy(),
            _ => String::new(),
        };
        if prompt.is_empty() {
            return Err("agent() requires a non-empty prompt string".into());
        }
        let opts = self.options(&opts)?;
        let max = self.run.limits.max_total_agents;
        if self.started >= max {
            return Err(format!(
                "this run reached its total agent cap ({max}) — a runaway-loop backstop; raise rness.workflow.max_total_agents if the scale is intentional"
            ));
        }
        self.started += 1;
        self.run
            .agents_started
            .store(self.started, std::sync::atomic::Ordering::Relaxed);
        let seq = self.started;
        let label = opts.label.unwrap_or_else(|| default_label(&prompt));
        let phase = opts.phase.or_else(|| self.phase.clone());
        self.emit(WorkflowEvent::AgentQueued {
            seq,
            label: label.clone(),
            phase: phase.clone(),
        });
        self.waiting.insert(seq, (id, opts.schema.is_some()));
        self.queue.push_back(ChildRequest {
            seq,
            prompt,
            label,
            phase,
            role: opts.role,
            provider: opts.provider,
            schema: opts.schema,
        });
        Ok(())
    }

    fn options(&self, raw: &LuaValue) -> Result<Options, String> {
        let record = match raw {
            LuaValue::Nil => Map::new(),
            // Options/schemas are small; 1 MiB bounds a hostile shared-subtree
            // expansion here (this runs outside the Lua step budget).
            LuaValue::Table(_) => match convert::from_lua(&self.lua, raw, "options", 1 << 20) {
                Ok(Value::Object(record)) => record,
                Ok(Value::Array(items)) if items.is_empty() => Map::new(),
                Ok(_) => return Err("agent() options must be a table of named fields".into()),
                Err(error) => {
                    return Err(format!("agent() options must be plain JSON data — {error}"))
                }
            },
            _ => return Err("agent() options must be a table".into()),
        };
        for key in record.keys() {
            match key.as_str() {
                "label" | "phase" | "schema" | "role" | "provider" => {}
                "effort" | "isolation" | "agentType" | "model" => {
                    return Err(format!(
                        "agent() option \"{key}\" is deferred and not supported yet (supported: {SUPPORTED})"
                    ))
                }
                _ => {
                    return Err(format!(
                        "agent() option \"{key}\" is not recognized (supported: {SUPPORTED})"
                    ))
                }
            }
        }
        let text = |key: &str| -> Result<Option<String>, String> {
            match record.get(key) {
                None => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(format!("agent() option \"{key}\" must be a string")),
            }
        };
        let (label, phase, role) = (text("label")?, text("phase")?, text("role")?);
        let provider = text("provider")?.unwrap_or_else(|| "spawn".into());
        if !matches!(provider.as_str(), "spawn" | "fork") {
            return Err(format!(
                "agent() option \"provider\" must be \"spawn\" or \"fork\" (got \"{provider}\")"
            ));
        }
        self.run
            .runner
            .check_role(role.as_deref())
            .map_err(|error| format!("agent() cannot start this child: {error}"))?;
        let schema = match record.get("schema") {
            None => None,
            Some(schema) => {
                let mut schema = schema.clone();
                convert::normalize_schema(&mut schema);
                crate::structured::check_object_schema(&schema).map_err(|violations| {
                    format!(
                        "agent() schema is outside the supported subset — {}",
                        violations.join("; ")
                    )
                })?;
                Some(schema)
            }
        };
        Ok(Options {
            label,
            phase,
            role,
            provider,
            schema,
        })
    }

    fn items(&self, table: &Table, hook: &str) -> Result<Vec<LuaValue>, String> {
        let len = convert::seq_len(&self.lua, table).map_err(|e| render(&e))?;
        let cap = self.run.limits.max_items_per_call;
        if len > cap {
            return Err(format!(
                "{hook} received {len} items — over the per-call cap ({cap}); split the work or raise rness.workflow.max_items_per_call"
            ));
        }
        (1..=len)
            .map(|i| table.raw_get(i).map_err(|e| render(&e)))
            .collect()
    }

    fn parallel(&mut self, id: usize, thunks: LuaValue) -> Result<(), String> {
        let LuaValue::Table(thunks) = thunks else {
            return Err("parallel() requires an array of functions".into());
        };
        let jobs = self
            .items(&thunks, "parallel()")?
            .into_iter()
            .enumerate()
            .map(|(i, thunk)| match thunk {
                LuaValue::Function(f) => Ok((f, MultiValue::new())),
                _ => Err(format!("parallel() item {} is not a function", i + 1)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.spawn(id, jobs)
    }

    fn pipeline(&mut self, id: usize, items: LuaValue, stages: LuaValue) -> Result<(), String> {
        let LuaValue::Table(items) = items else {
            return Err("pipeline() requires an items array".into());
        };
        let items = self.items(&items, "pipeline()")?;
        let LuaValue::Table(stages) = stages else {
            return Err("workflow internal error: pipeline stages".into());
        };
        let count: usize = stages.raw_get("n").map_err(|e| render(&e))?;
        if count == 0 {
            return Err("pipeline() requires at least one stage function".into());
        }
        for i in 1..=count {
            if !matches!(stages.raw_get(i), Ok(LuaValue::Function(_))) {
                return Err(format!("pipeline() stage {i} is not a function"));
            }
        }
        let jobs = items
            .into_iter()
            .enumerate()
            .map(|(i, item)| {
                let args = vec![
                    LuaValue::Table(stages.clone()),
                    item,
                    LuaValue::Integer(i as i64 + 1),
                ];
                (self.run_item.clone(), MultiValue::from_vec(args))
            })
            .collect();
        self.spawn(id, jobs)
    }

    /// Start one task per job; `waiter` resumes with the ordered results.
    fn spawn(&mut self, waiter: usize, jobs: Vec<(Function, MultiValue)>) -> Result<(), String> {
        if jobs.is_empty() {
            return match self.resume_with_array(waiter, Vec::new()) {
                Some(End::Error(error)) => Err(error),
                _ => Ok(()),
            };
        }
        let join = self.next_join;
        self.next_join += 1;
        self.joins.insert(
            join,
            Join {
                waiter,
                remaining: jobs.len(),
                results: vec![LuaValue::Nil; jobs.len()],
            },
        );
        for (index, (function, args)) in jobs.into_iter().enumerate() {
            let thread = self
                .lua
                .create_thread(self.guard.clone())
                .map_err(|e| format!("workflow could not start a task: {}", render(&e)))?;
            let mut args = args.into_vec();
            args.insert(0, LuaValue::Function(function));
            let args = MultiValue::from_vec(args);
            let id = self.next_task;
            self.next_task += 1;
            self.tasks.insert(
                id,
                Task {
                    thread,
                    join: Some((join, index)),
                },
            );
            self.ready.push_back((id, args));
        }
        Ok(())
    }

    /// Start queued children up to the concurrency cap (FIFO).
    fn admit(&mut self) {
        while self.in_flight < self.run.limits.max_concurrent_agents {
            let Some(request) = self.queue.pop_front() else {
                break;
            };
            self.in_flight += 1;
            let guard = DoneGuard {
                tx: self.tx.clone(),
                seq: request.seq,
                sent: false,
            };
            let (runner, tx, token) = (
                self.run.runner.clone(),
                self.tx.clone(),
                self.children.child_token(),
            );
            let seq = request.seq;
            self.run.handle.spawn(async move {
                let started = Box::new(move |child: SessionId| {
                    let _ = tx.send(Msg::Started(seq, child));
                });
                let outcome = runner.run(request, token, started).await;
                guard.send(outcome);
            });
        }
    }

    fn settle(&mut self, seq: usize, outcome: ChildOutcome) -> Option<End> {
        self.in_flight -= 1;
        let (waiter, schema) = self.waiting.remove(&seq)?;
        let (ended, value) = match outcome {
            ChildOutcome::Completed { structured, .. } if schema => match structured {
                Some(value) => match convert::to_lua(&self.lua, &value) {
                    Ok(value) => (AgentOutcome::Completed, value),
                    Err(error) => {
                        return Some(End::Error(format!(
                            "workflow could not load a child result: {}",
                            render(&error)
                        )))
                    }
                },
                None => (AgentOutcome::Failed, LuaValue::Nil),
            },
            ChildOutcome::Completed { text, .. } => match self.lua.create_string(&text) {
                Ok(text) => (AgentOutcome::Completed, LuaValue::String(text)),
                Err(error) => {
                    return Some(End::Error(format!(
                        "workflow could not load a child result: {}",
                        render(&error)
                    )))
                }
            },
            ChildOutcome::Failed(reason) => {
                // A member that could not even start is worth surfacing
                // (ordinary child failures live in the child's own log).
                if reason.starts_with("agent() could not start") {
                    self.emit(WorkflowEvent::Log(reason));
                }
                (AgentOutcome::Failed, LuaValue::Nil)
            }
            ChildOutcome::Cancelled => (AgentOutcome::Cancelled, LuaValue::Nil),
            ChildOutcome::Fatal(error) => {
                self.emit(WorkflowEvent::AgentEnded {
                    seq,
                    outcome: AgentOutcome::Failed,
                });
                return Some(End::Error(error));
            }
        };
        self.emit(WorkflowEvent::AgentEnded {
            seq,
            outcome: ended,
        });
        self.ready
            .push_back((waiter, MultiValue::from_vec(vec![value])));
        None
    }

    /// Wind down: cancel queued and running children, then give running
    /// ones a bounded grace period to settle (they settle on their own
    /// after that; the run does not wait).
    fn dispose(&mut self) {
        self.children.cancel();
        for request in std::mem::take(&mut self.queue) {
            self.waiting.remove(&request.seq);
            self.emit(WorkflowEvent::AgentEnded {
                seq: request.seq,
                outcome: AgentOutcome::Cancelled,
            });
        }
        let deadline = Instant::now() + self.run.limits.dispose_grace;
        while self.in_flight > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let next = self
                .run
                .handle
                .block_on(tokio::time::timeout(remaining, self.rx.recv()));
            match next {
                Ok(Some(Msg::Started(seq, child))) => {
                    self.emit(WorkflowEvent::AgentStarted { seq, child })
                }
                Ok(Some(Msg::Done(seq, outcome))) => {
                    self.in_flight -= 1;
                    if self.waiting.remove(&seq).is_some() {
                        let outcome = match outcome {
                            ChildOutcome::Completed { .. } => AgentOutcome::Completed,
                            ChildOutcome::Failed(_) | ChildOutcome::Fatal(_) => {
                                AgentOutcome::Failed
                            }
                            ChildOutcome::Cancelled => AgentOutcome::Cancelled,
                        };
                        self.emit(WorkflowEvent::AgentEnded { seq, outcome });
                    }
                }
                Ok(Some(Msg::Cancel)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
}
