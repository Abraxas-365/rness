//! rness.session — the engine's SessionService surfaced to Lua.
//!
//! Injected AFTER the kernel mounts (the VM boots first), and
//! re-injected into every fresh VM after a hot reload — see
//! [`crate::plugin_host`]. Calls run on the VM actor thread; `send`
//! spawns the turn burst, so the tokio runtime handle captured at
//! install time is entered around it.
//!
//!   rness.session.list()            -> { id, ... }
//!   rness.session.phase(id)         -> "idle" | "running"
//!   rness.session.send(id, text)    -> "started"|"queued"|"logged"
//!   rness.session.fork(id)          -> child id
//!   rness.session.transcript(id)    -> { {role=, text=}, ... }
//!   rness.session.usage(id)         -> { input=, output= } (context tokens)
//!   rness.session.config(id[, cfg]) -> { reasoning=? } (get; with cfg: set —
//!     durable request/config event, resume remembers it)
//!   rness.session.compact(id, keep) -> { shadowed=, summary= } (blocks)
//!   rness.session.prune(id, opts)   -> pruned count (deterministic, instant)
//!     opts = { threshold=, head=, tail=, keep_turns= } (all required — no
//!     hidden defaults; dsh reference: 8192/4096/1024/2)

use mlua::LuaSerdeExt;
use std::sync::Arc;

use mlua::{Lua, Table};
use rness_engine::inbox::{Disposition, Phase};
use rness_engine::service::SessionService;
use rness_protocol::events::{ContentPart, UserIntent};

/// Private, single-use handoff; no Lua values cross onto the binding runtime.
pub(crate) type CompactionFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send>>;
pub(crate) struct CompactionRequest(pub(crate) CompactionFuture);
impl mlua::UserData for CompactionRequest {}

pub(crate) struct SearchRequest(pub(crate) std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>>);
impl mlua::UserData for SearchRequest {}

/// Yield from Lua rather than through a non-yieldable Rust callback frame.
pub(crate) fn compaction_wrapper(lua: &Lua, prepare: mlua::Function) -> mlua::Result<mlua::Function> {
    lua.load(r#"
        local prepare = ...
        local yield, raise = coroutine.yield, error
        return function(...)
            local ok, result = yield(prepare(...))
            if not ok then raise(result, 0) end
            return result
        end
    "#).call(prepare)
}

fn err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

pub fn install(
    lua: &Lua,
    rness: &Table,
    sessions: Arc<SessionService>,
    rt: tokio::runtime::Handle,
) -> Result<(), mlua::Error> {
    let images = lua.create_table()?;
    let image_service = sessions.clone();
    images.set("configure", lua.create_function(move |lua, options: Table| {
        let update: serde_json::Value = lua.from_value(mlua::Value::Table(options))?;
        let policy = image_service.image_policy(Some(update)).map_err(err)?;
        lua.to_value(&policy)
    })?)?;
    let image_service = sessions.clone();
    images.set("policy", lua.create_function(move |lua, ()| {
        lua.to_value(&image_service.image_policy(None).map_err(err)?)
    })?)?;
    let image_service = sessions.clone();
    images.set("processor", lua.create_function(move |_, (version, source): (String, Option<String>)| {
        let Some(source) = source else { return image_service.set_image_processor(None).map_err(err); };
        // A separate bounded VM avoids re-entering the retained plugin VM from
        // provider workers. Source evaluates to function(metadata, encoded_bytes).
        let vm = Lua::new();
        vm.set_memory_limit(128 * 1024 * 1024)?;
        vm.set_hook(mlua::HookTriggers::new().every_nth_instruction(10000), {
            let budget = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            move |_, _| {
                if budget.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 1000 { return Err(err("image processor instruction limit exceeded")); }
                Ok(mlua::VmState::Continue)
            }
        });
        let _: mlua::Function = vm.load(&source).eval()?;
        image_service.set_image_processor(Some((version, Arc::new(move |reference, bytes| {
            let vm = Lua::new();
            vm.set_memory_limit(128 * 1024 * 1024).map_err(|e| e.to_string())?;
            let started = std::time::Instant::now();
            vm.set_hook(mlua::HookTriggers::new().every_nth_instruction(10000), move |_, _| {
                if started.elapsed() > std::time::Duration::from_secs(2) { return Err(err("image processor deadline exceeded")); }
                Ok(mlua::VmState::Continue)
            });
            let callback: mlua::Function = vm.load(&source).eval().map_err(|e| e.to_string())?;
            let metadata = vm.to_value(reference).map_err(|e| e.to_string())?;
            let data = vm.create_string(bytes).map_err(|e| e.to_string())?;
            let output: mlua::String = callback.call((metadata, data)).map_err(|e| e.to_string())?;
            Ok(output.as_bytes().to_vec())
        })))).map_err(err)
    })?)?;
    rness.set("images", images)?;
    let session = lua.create_table()?;

    // Trusted plugin API, not a model tool. Each plugin instance owns its
    // provider; dropping the returned function releases the SQLite connection.
    // Creating the function performs no filesystem operations.
    let search_service = Arc::clone(&sessions);
    let search_rt = rt.clone();
    session.set("sqlite_search_provider", lua.create_function(move |lua, ()| {
        let sessions = Arc::clone(&search_service);
        // Separate versioned filename: leave the earlier prototype index alone.
        let path = sessions.store().root().join("session-search-v2.sqlite3");
        let provider = Arc::new(std::sync::Mutex::new(
            rness_engine::session_search::SqliteSessionSearch::new(path),
        ));
        let runtime = search_rt.clone();
        let prepare = lua.create_function(move |lua, (caller, operation, args): (String, String, Table)| {
            let request: rness_engine::session_search::QueryRequest = lua.from_value(mlua::Value::Table(args))?;
            let sessions = sessions.clone();
            let provider = provider.clone();
            let runtime = runtime.clone();
            Ok(SearchRequest(Box::pin(async move {
                runtime.spawn_blocking(move || {
                    // Do not queue unbounded blocking workers behind one index.
                    let mut provider = provider.try_lock().map_err(|_| "session search busy; retry shortly".to_string())?;
                    provider.execute(sessions.store(), &caller, &operation, request).map_err(|e| e.to_string())
                }).await.map_err(|e| e.to_string())?
            })))
        })?;
        compaction_wrapper(lua, prepare)
    })?)?;

    // Owned registration with cleanup on unload or failed plugin load.
    session.set("register_search_provider", lua.create_function(|lua, provider: mlua::Function| {
        let globals = lua.globals();
        let owner: Table = globals.get("__rness_load_owner")?;
        if globals.get::<Option<mlua::Function>>("__rness_search_provider")?.is_some() {
            return Err(err("session search provider already registered"));
        }
        globals.set("__rness_search_provider", provider.clone())?;
        let cleanup = lua.create_function(move |lua, ()| {
            let current = lua.globals().get::<Option<mlua::Function>>("__rness_search_provider")?;
            if current.is_some_and(|current| current.to_pointer() == provider.to_pointer()) {
                lua.globals().set("__rness_search_provider", mlua::Value::Nil)?;
            }
            Ok(true)
        })?;
        owner.push(cleanup.clone())?;
        globals.get::<Table>("__rness_loading_hooks")?.push(cleanup)?;
        Ok(())
    })?)?;
    session.set("search_provider", lua.create_function(|lua, ()| {
        lua.globals().get::<Option<mlua::Function>>("__rness_search_provider")?
            .ok_or_else(|| err("enable a session search provider plugin first"))
    })?)?;

    let s = Arc::clone(&sessions);
    session.set("model_capabilities", lua.create_function(move |lua, selection: Table| {
        let selection = lua.from_value(mlua::Value::Table(selection))?;
        lua.to_value(&s.model_capabilities(&selection))
    })?)?;
    let s = Arc::clone(&sessions);
    session.set("model_names", lua.create_function(move |lua, ()| {
        lua.to_value(&s.model_names())
    })?)?;
    let s = Arc::clone(&sessions);
    session.set("profiles", lua.create_function(move |lua, ()| {
        lua.to_value(&s.profile_names())
    })?)?;
    let s = Arc::clone(&sessions);
    session.set("profile_config", lua.create_function(move |lua, (name, provider): (String, Option<String>)| {
        lua.to_value(&s.profile_config(&name, provider.as_deref()).map_err(err)?)
    })?)?;
    let s = Arc::clone(&sessions);
    session.set("plan", lua.create_function(move |lua, (id, active): (String, Option<bool>)| {
        if let Some(active) = active { s.select_plan(&id, active).map_err(err)?; }
        lua.to_value(&s.plan(&id).map_err(err)?)
    })?)?;
    let s = Arc::clone(&sessions);
    session.set("tasks", lua.create_function(move |lua, id: String| {
        lua.to_value(&s.tasks(&id).map_err(err)?)
    })?)?;
    let s = Arc::clone(&sessions);
    session.set("agent", lua.create_function(move |lua, (id, name): (String, Option<String>)| {
        if let Some(name) = name { s.select_agent(&id, &name).map_err(err)?; }
        lua.to_value(&s.config(&id).map_err(err)?.agent)
    })?)?;

    let s = Arc::clone(&sessions);
    session.set(
        "list",
        lua.create_function(move |_, ()| s.list().map_err(err))?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "phase",
        lua.create_function(move |_, id: String| {
            Ok(match s.phase(&id) {
                Phase::Idle => "idle",
                Phase::Running => "running",
            })
        })?,
    )?;

    let s = Arc::clone(&sessions);
    let send_rt = rt.clone();
    session.set(
        "send",
        lua.create_function(move |_, (id, text): (String, String)| {
            // send() spawns the turn burst; enter the runtime for it.
            let _guard = send_rt.enter();
            let disposition = s
                .send(&id, UserIntent::Followup, vec![ContentPart::Text { text }])
                .map_err(err)?;
            Ok(match disposition {
                Disposition::Command(_) => "command",
                Disposition::StartTurn => "started",
                Disposition::Queued => "queued",
                Disposition::LogOnly => "logged",
            })
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "fork",
        lua.create_function(move |_, id: String| s.fork(&id, None).map_err(err))?,
    )?;

    // Branch queries — the DAG view over the store.
    let s = Arc::clone(&sessions);
    session.set(
        "parent",
        lua.create_function(move |lua, id: String| {
            match s.store().parent(&id).map_err(err)? {
                None => Ok(mlua::Value::Nil),
                Some(fork) => {
                    let t = lua.create_table()?;
                    t.set("session", fork.session)?;
                    t.set("at", fork.at)?;
                    Ok(mlua::Value::Table(t))
                }
            }
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "children",
        lua.create_function(move |lua, id: String| {
            let kids = s.store().children(&id, None).map_err(err)?;
            let out = lua.create_table()?;
            for k in kids {
                let t = lua.create_table()?;
                t.set("session", k.session)?;
                t.set("at", k.at)?;
                out.push(t)?;
            }
            Ok(out)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "ancestry",
        lua.create_function(move |lua, id: String| {
            let hops = s.store().ancestry(&id).map_err(err)?;
            let out = lua.create_table()?;
            for h in hops {
                let t = lua.create_table()?;
                t.set("session", h.session)?;
                t.set("forked_at", h.forked_at)?;
                out.push(t)?;
            }
            Ok(out)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "transcript",
        lua.create_function(move |lua, id: String| {
            use rness_engine::session::projection::TranscriptItem;
            let transcript = s.transcript(&id).map_err(err)?;
            let items = lua.create_table()?;
            for item in &transcript.items {
                let (role, text) = match item {
                    TranscriptItem::User { content, .. } => ("user", text_of(content)),
                    TranscriptItem::Assistant { content, .. } => ("assistant", text_of(content)),
                    TranscriptItem::Tool { result, .. } => ("tool", result.output.clone()),
                    TranscriptItem::Compaction { summary, .. } => ("summary", summary.clone()),
                    TranscriptItem::Attempt { .. } => continue,
                };
                let entry = lua.create_table()?;
                entry.set("role", role)?;
                entry.set("text", text)?;
                items.push(entry)?;
            }
            Ok(items)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set(
        "usage",
        lua.create_function(move |lua, id: String| {
            let replayed = s.replay(&id).map_err(err)?;
            let t = lua.create_table()?;
            let latest = replayed.history.iter().rev().find_map(|event| match &event.event {
                rness_protocol::events::SessionEvent::AssistantMessage(message) => Some(message.usage),
                _ => None,
            }).unwrap_or_default();
            t.set("input", latest.input_tokens)?;
            t.set("output", latest.output_tokens)?;
            t.set("turns", replayed.history.iter().filter(|event| matches!(event.event,
                rness_protocol::events::SessionEvent::TurnStarted { .. })).count())?;
            Ok(t)
        })?,
    )?;

    // rness.session.config(id)          -> { reasoning=? } (effective)
    // rness.session.config(id, {...})   -> set (durable request/config
    //   event; nil field = clear, back to the provider's default)
    let s = Arc::clone(&sessions);
    session.set(
        "config",
        lua.create_function(move |lua, (id, new): (String, Option<Table>)| {
            if let Some(new) = new {
                let config = mlua::LuaSerdeExt::from_value::<rness_protocol::events::CallConfig>(
                    lua, mlua::Value::Table(new),
                )?;
                let permit = lua.app_data_ref::<rness_engine::service::CommandPermit>()
                    .map(|permit| permit.clone());
                s.set_config_with_permit(&id, config, permit).map_err(err)?;
            }
            let config = s.config(&id).map_err(err)?;
            mlua::LuaSerdeExt::to_value(lua, &config)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    let block_rt = rt.clone();
    session.set(
        "compact",
        lua.create_function(move |lua, (id, keep): (String, Option<usize>)| {
            // Blocks the VM thread through the summarizer request — meant
            // for explicit commands / idle hooks, same stance as
            // subagents.start.
            let report = block_rt
                .block_on(s.compact(&id, keep.unwrap_or(2)))
                .map_err(err)?;
            let t = lua.create_table()?;
            t.set("shadowed", report.shadowed)?;
            t.set("summary", report.summary)?;
            Ok(t)
        })?,
    )?;

    let s = Arc::clone(&sessions);
    session.set("compaction_view", lua.create_function(move |lua, id: String| {
        let view = s.replay(&id).map_err(err)?.context;
        lua.to_value(&serde_json::json!({"messages": view.turns, "sources": view.sources}))
    })?)?;
    let s = Arc::clone(&sessions);
    let block_rt = rt.clone();
    session.set("compact_region", lua.create_function(move |lua, (id, opts): (String, Table)| {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Options {
            start: usize, end: usize,
            sources: Vec<rness_protocol::events::EventId>,
            policy: rness_engine::turn::compaction::Policy,
        }
        let opts: Options = lua.from_value(mlua::Value::Table(opts))?;
        if opts.start == 0 || opts.end < opts.start {
            return Err(err("region uses one-based inclusive message indices"));
        }
        let permit = lua.app_data_ref::<rness_engine::service::CommandPermit>().map(|permit| permit.clone());
        let cancel = lua.app_data_ref::<tokio_util::sync::CancellationToken>().map(|cancel| cancel.clone()).unwrap_or_default();
        block_rt.block_on(s.compact_region_with_permit(&id, opts.start - 1, opts.end, opts.sources, opts.policy, permit, cancel)).map_err(err)
    })?)?;

    let s = Arc::clone(&sessions);
    let prepare = lua.create_function(move |lua, (id, opts): (String, Table)| {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Options {
            start: usize, end: usize,
            sources: Vec<rness_protocol::events::EventId>,
            policy: rness_engine::turn::compaction::Policy,
        }
        let opts: Options = lua.from_value(mlua::Value::Table(opts))?;
        if opts.start == 0 || opts.end < opts.start {
            return Err(err("region uses one-based inclusive message indices"));
        }
        opts.policy.validate().map_err(err)?;
        let permit = lua.app_data_ref::<rness_engine::service::CommandPermit>()
            .map(|permit| permit.clone()).ok_or_else(|| err("compact_region_async requires a command"))?;
        let cancel = lua.app_data_ref::<tokio_util::sync::CancellationToken>()
            .map(|cancel| cancel.clone()).ok_or_else(|| err("compact_region_async requires command cancellation"))?;
        if cancel.is_cancelled() { return Err(err("command cancelled")); }
        let s = s.clone();
        Ok(CompactionRequest(Box::pin(async move {
            s.compact_region_with_permit(&id, opts.start - 1, opts.end, opts.sources, opts.policy, Some(permit), cancel)
                .await.map_err(|e| e.to_string())
        })))
    })?;
    session.set("compact_region_async", compaction_wrapper(lua, prepare)?)?;

    let s = Arc::clone(&sessions);
    session.set(
        "prune",
        lua.create_function(move |_, (id, opts): (String, Table)| {
            let opts = rness_engine::service::PruneOptions {
                threshold_chars: opts.get("threshold")?,
                head_chars: opts.get("head")?,
                tail_chars: opts.get("tail")?,
                keep_turns: opts.get("keep_turns")?,
            };
            s.prune_tool_results(&id, opts).map_err(err)
        })?,
    )?;

    rness.set("session", session)?;
    Ok(())
}

/// Concatenated text parts of a message (thinking/tool_use skipped).
fn text_of(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
