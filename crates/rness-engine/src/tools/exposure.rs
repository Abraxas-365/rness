use super::{ToolCall, ToolRegistry, ToolSpec};
use mlua::{Lua, LuaSerdeExt};
use rness_protocol::events::{SessionEvent, ToolResult, ToolResultContentPart};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Native,
    Ptc,
    Both,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Exposure {
    pub mode: Mode,
    pub deferred: Vec<String>,
}

impl Exposure {
    pub fn specs(&self, tools: &ToolRegistry, activated: &BTreeSet<String>) -> Vec<ToolSpec> {
        let mut specs = if self.mode == Mode::Ptc {
            Vec::new()
        } else {
            tools
                .specs()
                .into_iter()
                .filter(|spec| {
                    (!tools.is_deferred(&spec.name) && !self.deferred.contains(&spec.name))
                        || activated.contains(&spec.name)
                })
                .collect()
        };
        if !self.deferred.is_empty() || tools.has_deferred() || self.mode != Mode::Native {
            specs.push(ToolSpec { name: "ToolSearch".into(), description: "Discover permitted tools and their full JSON schemas. query accepts keywords (ranked partial matches, top 20) or select:Name,Other (case-insensitive full names, no result cap). Returns tools, total_matches, truncated, missing names, and guidance. Discovered tools become available next step; in ptc mode call them through run_code.".into(), input_schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}) });
        }
        if self.mode != Mode::Native {
            specs.push(ToolSpec { name:"run_code".into(), description:"Execute isolated Lua to orchestrate tools. Use tools.call(name, args) to get {output,is_error,content}; return a JSON-serializable value. Discover schemas with ToolSearch first. No filesystem, process, network, imports or config access except through permitted tools. Use tools.parallel({{name=...,args=...},...}) for up to four concurrent calls with results in input order. Shared max 32 calls, 60s execution budget, 16 MiB VM memory. Tools still require normal approvals. Recursive run_code, ToolSearch and workflow are forbidden inside programs.".into(), input_schema:json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false}) });
        }
        specs
    }

    pub fn activated(history: &[rness_protocol::events::Envelope]) -> BTreeSet<String> {
        history
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::ToolsActivated { names } => Some(names),
                _ => None,
            })
            .flatten()
            .cloned()
            .collect()
    }

    pub fn search(
        &self,
        tools: &ToolRegistry,
        args: &Value,
    ) -> Result<(String, Vec<String>), String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("query must be a nonempty string")?;
        let query = query.trim();
        let exact = query.strip_prefix("select:");
        let requested: Vec<_> = exact
            .map(|names| {
                names
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if exact.is_some() && requested.is_empty() {
            return Err("select: requires at least one tool name".into());
        }
        let words: BTreeSet<_> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut matches: Vec<_> = tools
            .specs()
            .into_iter()
            // ptc omits the workflow tool (dsh): programs cannot host it.
            .filter(|spec| self.mode != Mode::Ptc || spec.name != crate::workflow::TOOL)
            .filter_map(|spec| {
                let name = spec.name.to_lowercase();
                let description = spec.description.to_lowercase();
                let score = if exact.is_some() {
                    usize::from(
                        requested
                            .iter()
                            .any(|requested| requested.eq_ignore_ascii_case(&spec.name)),
                    )
                } else {
                    words
                        .iter()
                        .map(|word| {
                            if name == *word {
                                8
                            } else if name.contains(word.as_str()) {
                                4
                            } else if description.contains(word.as_str()) {
                                1
                            } else {
                                0
                            }
                        })
                        .sum()
                };
                (score > 0).then_some((score, spec))
            })
            .collect();
        matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        let total_matches = matches.len();
        let missing: Vec<_> = requested
            .iter()
            .filter(|name| {
                !matches
                    .iter()
                    .any(|(_, spec)| name.eq_ignore_ascii_case(&spec.name))
            })
            .copied()
            .collect();
        if exact.is_none() {
            matches.truncate(20);
        }
        let names: Vec<_> = matches.iter().map(|(_, spec)| spec.name.clone()).collect();
        let guidance = if matches.is_empty() {
            "No permitted registered tools matched. Try a shorter keyword or select: with a known tool name. An empty result does not establish whether an MCP server is connected."
        } else if total_matches > matches.len() {
            "Results are truncated to the top 20 matches. Narrow the query or use select:Name,Other for specific tools."
        } else if !missing.is_empty() {
            "Some requested names were not found among permitted registered tools. Check their spelling and tool registration."
        } else {
            "Returned tools are available next step; in ptc mode use run_code."
        };
        let output = serde_json::to_string(&json!({
            "tools": matches.iter().map(|(_, s)| json!({"name":s.name,"description":s.description,"input_schema":s.input_schema})).collect::<Vec<_>>(),
            "total_matches": total_matches,
            "truncated": total_matches > matches.len(),
            "missing": missing,
            "guidance": guidance,
        })).map_err(|e| e.to_string())?;
        Ok((output, names))
    }
}

pub fn result(call: &ToolCall, output: Result<String, String>) -> ToolResult {
    let is_error = output.is_err();
    let output = output.unwrap_or_else(|e| e);
    ToolResult {
        call: call.call.clone(),
        name: call.name.clone(),
        content: vec![ToolResultContentPart::Text {
            text: output.clone(),
        }],
        output,
        is_error,
        duration_ms: 0,
        tasks: None,
        plan_review: None,
        presentation: None,
    }
}

pub async fn program(
    tools: Arc<ToolRegistry>,
    session: String,
    call: ToolCall,
    cancel: CancellationToken,
    audit: Option<
        tokio::sync::mpsc::Sender<(
            ToolCall,
            Option<ToolResult>,
            tokio::sync::oneshot::Sender<bool>,
        )>,
    >,
) -> (ToolResult, Vec<(ToolCall, ToolResult)>) {
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
        for parallel in [false, true] {
        let count = count.clone(); let cancel = cancel.clone(); let tools = tools.clone();
        let session = session.clone(); let audit = audit.clone(); let saved = saved.clone();
        let handle = handle.clone(); let parent = call.call.clone();
        api.set(if parallel { "parallel" } else { "call" }, lua.create_function(move |lua, values: mlua::MultiValue| {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Request { name: String, args: Value }
            let requests: Vec<Request> = if parallel {
                if values.len() != 1 { return Err(mlua::Error::runtime("tools.parallel expects an array of {name,args}")); }
                lua.from_value(values.front().cloned().unwrap())?
            } else {
                let (name,args): (String,mlua::Value) = mlua::FromLuaMulti::from_lua_multi(values,lua)?;
                vec![Request { name, args:lua.from_value(args)? }]
            };
            if requests.len() > 32 || requests.iter().any(|r| matches!(r.name.as_str(), "run_code" | "ToolSearch" | crate::workflow::TOOL)) {
                return Err(mlua::Error::runtime("batch exceeds call budget or contains recursive control-tool calls (run_code, ToolSearch and workflow are not callable from programs)"));
            }
            let index = count.fetch_add(requests.len(), std::sync::atomic::Ordering::Relaxed);
            if index.saturating_add(requests.len()) > 32 || cancel.is_cancelled() || Instant::now() >= deadline { return Err(mlua::Error::runtime("program call budget exceeded or cancelled")); }
            let nested: Vec<_> = requests.into_iter().enumerate().map(|(offset,r)| ToolCall {call:format!("{parent}/{}",index+offset),name:r.name,args:r.args}).collect();
            let token = cancel.child_token();
            let _guard = token.clone().drop_guard();
            let outputs = handle.block_on(async {
                // Persist every intent before dispatch. Channel acknowledgments serialize log writes.
                for call in &nested {
                    if let Some(audit) = &audit {
                        let (tx,rx) = tokio::sync::oneshot::channel();
                        tokio::select! {
                            _ = token.cancelled() => return Err("program cancelled".to_string()),
                            result = async { audit.send((call.clone(),None,tx)).await.map_err(|_| "audit channel closed")?; if !rx.await.unwrap_or(false) { return Err("cannot persist program call"); } Ok(()) } => result.map_err(str::to_owned)?,
                        }
                    }
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let timer_token = token.clone();
                let timer = tokio::spawn(async move { tokio::time::sleep(remaining).await; timer_token.cancel(); });
                let outputs = tools.dispatch(&session,&nested,if parallel { 4 } else { 1 },&token).await;
                timer.abort();
                for (call,output) in nested.iter().zip(&outputs) {
                    if let Some(audit) = &audit {
                        let (tx,rx) = tokio::sync::oneshot::channel();
                        audit.send((call.clone(),Some(output.clone()),tx)).await.map_err(|_| "audit channel closed".to_string())?;
                        if !rx.await.unwrap_or(false) { return Err("cannot persist program result".into()); }
                    }
                    saved.lock().unwrap().push((call.clone(),output.clone()));
                }
                Ok::<_,String>(outputs)
            }).map_err(mlua::Error::runtime)?;
            let outputs: Vec<_> = outputs.into_iter().map(|output| json!({"output":output.output,"is_error":output.is_error,"content":output.content})).collect();
            if parallel { lua.to_value(&outputs) } else { lua.to_value(&outputs[0]) }
        }).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        }
        lua.globals().set("tools", api).map_err(|e| e.to_string())?;
        let value = lua.load(code).set_mode(mlua::ChunkMode::Text).eval::<mlua::Value>().map_err(|e| e.to_string())?;
        let value: Value = lua.from_value(value).map_err(|e| e.to_string())?;
        serde_json::to_string(&value).map_err(|e| e.to_string())
    }).await.unwrap_or_else(|e| Err(format!("program worker failed: {e}")));
    let nested = std::mem::take(&mut *records.lock().unwrap());
    let mut outer = result(&original, executed);
    // Nested audit events are not model transcript. Carry admitted image blocks
    // on the outer result too; JSON metadata alone does not deliver image input.
    let mut seen = std::collections::HashSet::new();
    for (_, output) in &nested {
        for part in &output.content {
            if let ToolResultContentPart::Image { attachment } = part {
                if seen.insert(attachment.id.clone()) {
                    outer.content.push(part.clone());
                }
            }
        }
    }
    (outer, nested)
}
