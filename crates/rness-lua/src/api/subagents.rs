//! rness.subagents — delegation from Lua (peer of the model's tool).
//!
//! Blocking by design: `start` parks the VM actor thread on the child's
//! completion via the tokio handle — Lua stalls, the engine doesn't
//! (same stance as rness.http). Plugins wanting concurrency start
//! several children from several key presses, or use the session API.
//!
//!   rness.subagents.providers()          -> { "fork", "spawn" }
//!   rness.subagents.start(provider, {parent=, prompt=})
//!       -> { session=, stop="completed"|"aborted"|"error", output= }
//!   rness.subagents.delegation(id)       -> {parent=, depth=, mode=} | nil
//!   rness.subagents.start_continuable(provider, {parent=, prompt=}) -> child id
//!   rness.subagents.send_message(sender, target, text)
//!   rness.subagents.steer_user(caller, target, text) -- trusted user controls
//!   rness.subagents.list(root, details?) -> all descendants, including one-shot
//!       details=true adds name (role or "subagent") and task (bounded prompt hint)
//!   rness.subagents.interrupt(caller, target)
//!   rness.subagents.children(root, scope?) -> { {session=, parent=, depth=, running=} }

use std::sync::Arc;

use mlua::{Lua, LuaSerdeExt, Table};
use rness_engine::subagent::{StopReason, SubagentRequest, SubagentRuntime};

fn err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

pub fn install(
    lua: &Lua,
    rness: &Table,
    runtime: Arc<SubagentRuntime>,
    sessions: Arc<rness_engine::service::SessionService>,
    rt: tokio::runtime::Handle,
) -> Result<(), mlua::Error> {
    let subagents = lua.create_table()?;
    let r = Arc::clone(&runtime);
    subagents.set("roster", lua.create_function(move |lua, ()| lua.to_value(&r.roster()))?)?;

    let r = Arc::clone(&runtime);
    subagents.set(
        "providers",
        lua.create_function(move |_, ()| Ok(r.provider_names()))?,
    )?;

    let r = Arc::clone(&runtime);
    let block_rt = rt.clone();
    subagents.set(
        "start",
        lua.create_function(move |lua, (provider, spec): (String, Table)| {
            let request = SubagentRequest {
                agent: spec.get("agent")?,
                parent: spec.get("parent")?,
                prompt: spec.get("prompt")?,
            };
            let run = block_rt
                .block_on(r.start(&provider, request))
                .map_err(err)?;
            let t = lua.create_table()?;
            t.set("session", run.session)?;
            t.set(
                "stop",
                match run.stop {
                    StopReason::Completed => "completed",
                    StopReason::Aborted => "aborted",
                    StopReason::Error => "error",
                },
            )?;
            t.set("output", run.output)?;
            Ok(t)
        })?,
    )?;

    let hint_sessions = Arc::clone(&sessions);
    subagents.set(
        "delegation",
        lua.create_function(move |lua, id: String| {
            match sessions.store().delegation(&id).map_err(err)? {
                None => Ok(mlua::Value::Nil),
                Some(d) => {
                    let t = lua.create_table()?;
                    t.set("parent", d.parent)?;
                    t.set("depth", d.depth)?;
                    t.set(
                        "mode",
                        match d.mode {
                            rness_protocol::branch::DelegationMode::OneShot => "one_shot",
                            rness_protocol::branch::DelegationMode::Continuable => "continuable",
                        },
                    )?;
                    Ok(mlua::Value::Table(t))
                }
            }
        })?,
    )?;

    // start_continuable / send_message may start a turn task
    // (tokio::spawn) — enter the runtime so the VM actor thread has a
    // reactor context.
    let r = Arc::clone(&runtime);
    let handle = rt.clone();
    subagents.set(
        "start_continuable",
        lua.create_function(move |_, (provider, spec): (String, Table)| {
            let request = SubagentRequest {
                agent: spec.get("agent")?,
                parent: spec.get("parent")?,
                prompt: spec.get("prompt")?,
            };
            let _g = handle.enter();
            r.start_continuable(&provider, request).map_err(err)
        })?,
    )?;

    let r = Arc::clone(&runtime);
    let handle = rt.clone();
    subagents.set(
        "send_message",
        lua.create_function(move |_, (sender, target, text): (String, String, String)| {
            let _g = handle.enter();
            r.send_message(&sender, &target, text).map_err(err)?;
            Ok(())
        })?,
    )?;

    let r = Arc::clone(&runtime);
    let handle = rt.clone();
    subagents.set(
        "steer_user",
        lua.create_function(move |_, (caller, target, text): (String, String, String)| {
            let _g = handle.enter();
            r.steer_user(&caller, &target, text).map_err(err)?;
            Ok(())
        })?,
    )?;

    let r = Arc::clone(&runtime);
    subagents.set(
        "interrupt",
        lua.create_function(move |_, (caller, target): (String, String)| {
            r.interrupt(&caller, &target).map_err(err)
        })?,
    )?;

    let r = Arc::clone(&runtime);
    subagents.set(
        "children",
        lua.create_function(move |lua, (root, scope): (String, Option<String>)| {
            let descendants = scope.as_deref() == Some("descendants");
            let list = r.list_children(&root, descendants).map_err(err)?;
            let out = lua.create_table()?;
            for (i, c) in list.into_iter().enumerate() {
                let t = lua.create_table()?;
                t.set("session", c.session)?;
                t.set("parent", c.parent)?;
                t.set("depth", c.depth)?;
                t.set("running", c.running)?;
                out.set(i + 1, t)?;
            }
            Ok(out)
        })?,
    )?;

    let r = Arc::clone(&runtime);
    // Retain bounded hints, not histories, for the currently detailed tree.
    // Keeping every member avoids cache thrashing on large descendant lists.
    let hints = std::sync::Mutex::new(std::collections::HashMap::<
        String, rness_engine::session::hints::AgentHintReader,
    >::new());
    subagents.set("list", lua.create_function(move |lua, (root, details): (String, Option<bool>)| {
        let children = r.list_agents(&root).map_err(err)?;
        let mut cache = if details.unwrap_or(false) {
            let mut cache = hints.lock().expect("agent hints lock");
            let ids: std::collections::HashSet<_> = children.iter().map(|child| &child.session).collect();
            cache.retain(|id, _| ids.contains(id));
            Some(cache)
        } else {
            None
        };
        let out = lua.create_table()?;
        for (i, child) in children.into_iter().enumerate() {
            let t = lua.create_table()?;
            t.set("alias", child.alias)?;
            t.set("session", child.session.clone())?;
            t.set("parent", child.parent)?;
            t.set("depth", child.depth)?;
            t.set("running", child.running)?;
            if let Some(cache) = cache.as_mut() {
                let (name, task) = cache.entry(child.session.clone()).or_default()
                    .read(hint_sessions.store(), &child.session).map_err(err)?;
                t.set("name", name)?;
                t.set("task", task)?;
            }
            out.set(i + 1, t)?;
        }
        Ok(out)
    })?)?;

    rness.set("subagents", subagents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rness_engine::session::branch::SessionStore;
    use rness_engine::tools::ToolRegistry;
    use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
    use rness_protocol::branch::{Delegation, DelegationMode};
    use rness_protocol::events::{ContentPart, SessionEvent, UserIntent, UserMessage};

    struct UnusedProvider;
    #[async_trait::async_trait]
    impl Provider for UnusedProvider {
        fn model(&self) -> &str { "unused" }
        async fn step(&self, _: StepRequest<'_>, _: &tokio_util::sync::CancellationToken) -> StepOutcome {
            panic!("listing must not run a turn")
        }
    }

    #[test]
    fn list_details_are_optional_and_default_does_not_read_history() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let root = store.create(None).unwrap();
        let mut child = store.create_delegated(None, Delegation {
            parent: root.session().clone(), call: None, depth: 1, mode: DelegationMode::OneShot,
        }).unwrap();
        child.append(&SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text: "original task".into() }], source: None,
        })).unwrap();
        let sessions = Arc::new(rness_engine::service::SessionService::new(
            store, Arc::new(UnusedProvider), Arc::new(ToolRegistry::default()),
            Default::default(), Arc::new(rness_kernel::EventBus::default()),
        ));
        let runtime = Arc::new(SubagentRuntime::new(sessions.clone(), 3));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let lua = Lua::new();
        let rness = lua.create_table().unwrap();
        install(&lua, &rness, runtime, sessions, rt.handle().clone()).unwrap();
        lua.globals().set("rness", rness).unwrap();
        lua.globals().set("root", root.session().clone()).unwrap();
        lua.globals().set("child", child.session().clone()).unwrap();
        lua.load(r#"
            local plain = rness.subagents.list(root)
            local explicit = rness.subagents.list(root, false)
            local detailed = rness.subagents.list(root, true)
            assert(#plain == 1 and #detailed == 1)
            for _, key in ipairs({'alias', 'session', 'parent', 'depth', 'running'}) do
                assert(plain[1][key] == detailed[1][key])
                assert(plain[1][key] == explicit[1][key])
            end
            assert(plain[1].session == child and plain[1].parent == root)
            assert(plain[1].name == nil and plain[1].task == nil)
            assert(explicit[1].name == nil and explicit[1].task == nil)
            assert(detailed[1].name == 'subagent' and detailed[1].task == 'original task')
        "#).exec().unwrap();
        // A corrupt non-header event must not affect the cheap statusline path.
        let mut writer = std::fs::OpenOptions::new().append(true).open(child.path()).unwrap();
        writer.write_all(b"not json\n").unwrap();
        lua.load("assert(#rness.subagents.list(root) == 1); assert(#rness.subagents.list(root, false) == 1)").exec().unwrap();
        assert!(lua.load("rness.subagents.list(root, true)").exec().is_err());
    }
}
