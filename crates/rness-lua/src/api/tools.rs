//! Engine-facing bridge: a Lua-registered tool exposed through the same
//! `Tool` trait every Rust tool implements — Lua plugins are peers.

use rness_engine::tools::Tool;

use crate::plugin_host::LuaHost;
use crate::runtime::LuaToolSpec;

/// Adapter: one Lua tool as an engine tool. Registered into the
/// composition root's ToolRegistry like any built-in.
pub struct LuaTool {
    context: serde_json::Value,
    spec: LuaToolSpec,
    host: LuaHost,
}

impl LuaTool {
    pub fn new(spec: LuaToolSpec, host: LuaHost) -> Self {
        Self { spec, host, context: serde_json::json!({}) }
    }
}

#[async_trait::async_trait]
impl Tool for LuaTool {
    fn for_workspace(&self, session: &String, workspace: &std::path::Path) -> Option<std::sync::Arc<dyn Tool>> {
        Some(std::sync::Arc::new(Self { spec: self.spec.clone(), host: self.host.clone(), context: serde_json::json!({"session": session, "workspace":workspace}) }))
    }
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.spec.input_schema.clone()
    }

    fn plan_config(&self) -> Option<rness_engine::plan::PlanConfig> {
        self.spec.plan.as_ref().filter(|plan| !plan.alive.is_cancelled()).map(|plan| plan.config.clone())
    }
    async fn review_plan(&self, session: &str, call: &str, args: serde_json::Value, cancel: &tokio_util::sync::CancellationToken) -> Result<(String, rness_protocol::events::PlanReview), String> {
        self.spec.plan.as_ref().ok_or("not a plan tool")?.review(session, call, args, cancel).await
    }
    fn sensitive(&self) -> bool {
        self.spec.sensitive
    }

    async fn execute_with_tasks(&self, session: &String, call: &str, args: serde_json::Value, cancel: &tokio_util::sync::CancellationToken) -> Result<(String, Option<rness_protocol::events::TaskSnapshot>), String> {
        if let Some(config) = &self.spec.tasks {
            rness_engine::tasks::TaskWrite(config.clone()).execute_with_tasks(session, call, args, cancel).await
        } else {
            self.execute_call(session, call, args, cancel).await.map(|output| (output, None))
        }
    }

    async fn execute_presented(&self, session: &String, call: &String, args: serde_json::Value, cancel: &tokio_util::sync::CancellationToken) -> Result<(Vec<rness_protocol::events::ToolResultContentPart>, Option<rness_protocol::events::TaskSnapshot>, bool, Option<serde_json::Value>), String> {
        if self.spec.tasks.is_some() {
            return self.execute_rich(session, call, args, cancel).await.map(|(content, tasks, error)| (content, tasks, error, None));
        }
        let mut context = self.context.clone();
        context["session"] = serde_json::json!(session);
        context["call"] = serde_json::json!(call);
        let (output, presentation) = self.host.call_tool_presented(&self.spec.name, args, context).await?;
        Ok((vec![rness_protocol::events::ToolResultContentPart::Text { text: output }], None, false, presentation))
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, String> {
        self.host.call_tool_context(&self.spec.name, args, self.context.clone()).await
    }
}

/// Register every Lua tool currently in the VM into the registry.
pub async fn register_lua_tools(
    registry: &rness_engine::tools::ToolRegistry,
    host: &LuaHost,
) -> usize {
    let specs = host.tool_specs().await;
    let mut n = 0;
    for spec in specs {
        match registry.try_register(std::sync::Arc::new(LuaTool::new(spec, host.clone()))) {
            Ok(()) => n += 1,
            Err(error) => tracing::warn!("{error}"),
        }
    }
    n
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use std::sync::Arc;

    struct Native;
    #[async_trait::async_trait]
    impl Tool for Native {
        fn name(&self) -> &str { "owned" }
        async fn execute(&self, _: serde_json::Value) -> Result<String, String> { Ok("native".into()) }
    }

    #[tokio::test]
    async fn plugin_second_return_is_presentation_not_model_output() {
        let host = LuaHost::spawn().unwrap();
        host.load("presented", r#"
            rness.tool.register {name='snapshot', run=function(args, ctx)
                return 'model output', {version=1, before='private snapshot', call=ctx.call}
            end}
        "#).await.unwrap();
        let registry = rness_engine::tools::ToolRegistry::default();
        register_lua_tools(&registry, &host).await;
        let results = registry.dispatch(&"s".into(), &[rness_engine::tools::ToolCall {
            call:"c".into(), name:"snapshot".into(), args:serde_json::json!({}),
        }], 1, &tokio_util::sync::CancellationToken::new()).await;
        assert_eq!(results[0].output, "model output");
        assert_eq!(results[0].presentation.as_ref().unwrap()["before"], "private snapshot");
        assert_eq!(results[0].presentation.as_ref().unwrap()["call"], "c");
        assert_eq!(host.call_tool("snapshot", serde_json::json!({})).await.unwrap(), "model output");
    }

    #[tokio::test]
    async fn native_tasks_lifecycle_and_validation() {
        let host = LuaHost::spawn().unwrap();
        let registry = rness_engine::tools::ToolRegistry::default();
        assert!(host.load("broken", "rness.tasks.enable(); error('broken')").await.is_err());
        assert!(host.tool_specs().await.is_empty());
        host.load("tasks", "rness.tasks.enable { allow_parallel_in_progress=false }").await.unwrap();
        let installed = sync_lua_tools(&registry, &host, &[]).await;
        let args = serde_json::json!({"tasks":[{"id":"1","content":"test","status":"in_progress"}]});
        let call = rness_engine::tools::ToolCall { call: "c".into(), name: "TaskWrite".into(), args: args.clone() };
        let result = registry.dispatch(&"s".into(), &[call.clone()], 1, &tokio_util::sync::CancellationToken::new()).await;
        assert!(result[0].tasks.is_some());
        let cancel = tokio_util::sync::CancellationToken::new(); cancel.cancel();
        let result = registry.dispatch(&"s".into(), &[call], 1, &cancel).await;
        assert!(result[0].is_error);
        assert!(result[0].tasks.is_none());
        host.unload("tasks").await.unwrap();
        assert!(sync_lua_tools(&registry, &host, &installed).await.is_empty());
        assert!(registry.get("TaskWrite").is_none());
        host.load("again", "rness.tasks.enable()").await.unwrap();
        let installed = sync_lua_tools(&registry, &host, &[]).await;
        host.load("off", "rness.tasks.disable()").await.unwrap();
        assert!(sync_lua_tools(&registry, &host, &installed).await.is_empty());
    }

    #[tokio::test]
    async fn stale_owner_cannot_replace_or_remove_a_new_registration() {
        let host = LuaHost::spawn().unwrap();
        host.load("owner", "rness.tool.register { name='owned', run=function() return 'lua' end }").await.unwrap();
        let registry = rness_engine::tools::ToolRegistry::default();
        let installed = sync_lua_tools(&registry, &host, &[]).await;
        assert_eq!(installed.len(), 1);
        let current = sync_lua_tools(&registry, &host, &installed).await;
        assert_eq!(current.len(), 1);
        assert!(!registry.unregister_if_current(&installed[0]));
        let native: Arc<dyn Tool> = Arc::new(Native);
        registry.replace(native.clone()).unwrap();
        assert!(sync_lua_tools(&registry, &host, &current).await.is_empty());
        assert!(Arc::ptr_eq(&registry.get("owned").unwrap(), &native));
        let empty = LuaHost::spawn().unwrap();
        assert!(sync_lua_tools(&registry, &empty, &current).await.is_empty());
        assert!(Arc::ptr_eq(&registry.get("owned").unwrap(), &native));
        assert_eq!(installed[0].execute(serde_json::json!({})).await.unwrap(), "lua");
    }
}

/// Handles of implementations actually installed by this host.
pub type InstalledTools = Vec<std::sync::Arc<dyn Tool>>;

/// Reconcile registrations without mutating implementations owned by others.
pub async fn sync_lua_tools(
    registry: &rness_engine::tools::ToolRegistry,
    host: &LuaHost,
    previous: &[std::sync::Arc<dyn Tool>],
) -> InstalledTools {
    sync_lua_tool_specs(registry, host, previous, host.tool_specs().await)
}

pub(crate) fn sync_lua_tool_specs(
    registry: &rness_engine::tools::ToolRegistry,
    host: &LuaHost,
    previous: &[std::sync::Arc<dyn Tool>],
    specs: Vec<LuaToolSpec>,
) -> InstalledTools {
    let current: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
    for tool in previous {
        if !current.iter().any(|name| name == tool.name()) {
            registry.unregister_if_current(tool);
        }
    }
    let mut installed = Vec::new();
    for spec in specs {
        let name = spec.name.clone();
        let tool: std::sync::Arc<dyn Tool> = std::sync::Arc::new(LuaTool::new(spec, host.clone()));
        let result = if let Some(expected) = previous.iter().find(|tool| tool.name() == name) {
            registry.replace_if_current(expected, tool.clone())
        } else {
            registry.try_register(tool.clone())
        };
        match result {
            Ok(()) => installed.push(tool),
            Err(error) => tracing::warn!("{error}"),
        }
    }
    installed
}
