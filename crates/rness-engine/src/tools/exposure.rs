use std::{collections::BTreeSet, sync::{Arc, Mutex}, time::{Duration, Instant}};
use mlua::{Lua, LuaSerdeExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use super::{ToolCall, ToolRegistry, ToolSpec};
use rness_protocol::events::{SessionEvent, ToolResult, ToolResultContentPart};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode { #[default] Native, Ptc, Both }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Exposure {
    pub mode: Mode,
    pub deferred: Vec<String>,
}

impl Exposure {
    pub fn specs(&self, tools: &ToolRegistry, activated: &BTreeSet<String>) -> Vec<ToolSpec> {
        let mut specs = if self.mode == Mode::Ptc { Vec::new() } else {
            tools.specs().into_iter().filter(|spec| !self.deferred.contains(&spec.name) || activated.contains(&spec.name)).collect()
        };
        if !self.deferred.is_empty() || self.mode != Mode::Native {
            specs.push(ToolSpec { name: "ToolSearch".into(), description: "Discover permitted tools and their full JSON schemas. query accepts keywords or select:Name,Other. Discovered tools become available next step; in ptc mode call them through run_code.".into(), input_schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}) });
        }
        if self.mode != Mode::Native {
            specs.push(ToolSpec { name:"run_code".into(), description:"Execute isolated Lua to orchestrate tools. Use tools.call(name, args) to get {output,is_error,content}; return a JSON-serializable value. Discover schemas with ToolSearch first. No filesystem, process, network, imports or config access except through permitted tools. Sequential calls, max 32 calls, 60s execution budget, 16 MiB VM memory. Tools still require normal approvals. Recursive run_code and ToolSearch are forbidden inside programs.".into(), input_schema:json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false}) });
        }
        specs
    }

    pub fn activated(history: &[rness_protocol::events::Envelope]) -> BTreeSet<String> {
        history.iter().filter_map(|event| match &event.event { SessionEvent::ToolsActivated { names } => Some(names), _ => None }).flatten().cloned().collect()
    }

    pub fn search(&self, tools: &ToolRegistry, args: &Value) -> Result<(String, Vec<String>), String> {
        let query = args.get("query").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).ok_or("query must be a nonempty string")?;
        let exact = query.strip_prefix("select:");
        let words: Vec<_> = query.to_lowercase().split_whitespace().map(str::to_owned).collect();
        let matches: Vec<_> = tools.specs().into_iter().filter(|spec| {
            if let Some(names) = exact { names.split(',').any(|name| name.trim() == spec.name) }
            else { let text = format!("{} {}", spec.name, spec.description).to_lowercase(); words.iter().all(|word| text.contains(word)) }
        }).take(20).collect();
        let names = matches.iter().map(|s| s.name.clone()).collect();
        let output = serde_json::to_string(&matches.iter().map(|s| json!({"name":s.name,"description":s.description,"input_schema":s.input_schema})).collect::<Vec<_>>()).map_err(|e| e.to_string())?;
        Ok((output, names))
    }
}

pub fn result(call: &ToolCall, output: Result<String, String>) -> ToolResult {
    let is_error = output.is_err();
    let output = output.unwrap_or_else(|e| e);
    ToolResult { call:call.call.clone(), name:call.name.clone(), content:vec![ToolResultContentPart::Text{text:output.clone()}], output, is_error, duration_ms:0, tasks:None, plan_review:None, presentation:None }
}

pub async fn program(tools: Arc<ToolRegistry>, session: String, call: ToolCall, cancel: CancellationToken, audit: Option<tokio::sync::mpsc::Sender<(ToolCall, Option<ToolResult>, tokio::sync::oneshot::Sender<bool>)>>) -> (ToolResult, Vec<(ToolCall, ToolResult)>) {
    let records = Arc::new(Mutex::new(Vec::new()));
    let saved = records.clone();
    let original = call.clone();
    let handle = tokio::runtime::Handle::current();
    let executed = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let code = call.args.get("code").and_then(Value::as_str).ok_or("code must be a string")?;
        if code.len() > 65536 { return Err("program exceeds 64 KiB".into()); }
        let lua = Lua::new_with(mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH, mlua::LuaOptions::default()).map_err(|e| e.to_string())?;
        lua.set_memory_limit(16 * 1024 * 1024).map_err(|e| e.to_string())?;
        for name in ["dofile", "loadfile", "load", "collectgarbage", "print"] { lua.globals().set(name, mlua::Value::Nil).map_err(|e| e.to_string())?; }
        let deadline = Instant::now() + Duration::from_secs(60);
        let hook_cancel = cancel.clone();
        lua.set_hook(mlua::HookTriggers::new().every_nth_instruction(1000), move |_, _| {
            if hook_cancel.is_cancelled() || Instant::now() >= deadline { return Err(mlua::Error::runtime("program cancelled or execution budget exceeded")); }
            Ok(mlua::VmState::Continue)
        });
        let api = lua.create_table().map_err(|e| e.to_string())?;
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        api.set("call", lua.create_function(move |lua, (name, args): (String, mlua::Value)| {
            let index = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if index >= 32 || cancel.is_cancelled() || Instant::now() >= deadline { return Err(mlua::Error::runtime("program call budget exceeded or cancelled")); }
            if name == "run_code" || name == "ToolSearch" { return Err(mlua::Error::runtime("recursive control-tool calls are forbidden")); }
            let nested = ToolCall { call:format!("{}/{}", call.call, index), name, args:lua.from_value(args)? };
            if let Some(audit) = &audit {
                let (tx, rx) = tokio::sync::oneshot::channel();
                handle.block_on(audit.send((nested.clone(), None, tx))).map_err(mlua::Error::external)?;
                if !handle.block_on(rx).unwrap_or(false) { return Err(mlua::Error::runtime("cannot persist program call")); }
            }
            let token = cancel.child_token();
            let timer_token = token.clone();
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timer = handle.spawn(async move { tokio::time::sleep(remaining).await; timer_token.cancel(); });
            let mut output = handle.block_on(tools.dispatch(&session, std::slice::from_ref(&nested), 1, &token));
            timer.abort();
            let output = output.remove(0);
            if let Some(audit) = &audit {
                let (tx, rx) = tokio::sync::oneshot::channel();
                handle.block_on(audit.send((nested.clone(), Some(output.clone()), tx))).map_err(mlua::Error::external)?;
                if !handle.block_on(rx).unwrap_or(false) { return Err(mlua::Error::runtime("cannot persist program result")); }
            }
            saved.lock().unwrap().push((nested, output.clone()));
            lua.to_value(&json!({"output":output.output,"is_error":output.is_error,"content":output.content}))
        }).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        lua.globals().set("tools", api).map_err(|e| e.to_string())?;
        let value = lua.load(code).set_mode(mlua::ChunkMode::Text).eval::<mlua::Value>().map_err(|e| e.to_string())?;
        let value: Value = lua.from_value(value).map_err(|e| e.to_string())?;
        serde_json::to_string(&value).map_err(|e| e.to_string())
    }).await.unwrap_or_else(|e| Err(format!("program worker failed: {e}")));
    let nested = std::mem::take(&mut *records.lock().unwrap());
    (result(&original, executed), nested)
}
