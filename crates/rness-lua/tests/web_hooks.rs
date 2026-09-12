use rness_engine::tools::{ToolCall, ToolRegistry};
use rness_lua::plugin_host::LuaHost;
use rness_tools::web::{register_with_hooks, Config, WebHooks};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor="multi_thread")]
async fn owned_hooks_reload_unload_and_failed_reload() {
    let host = LuaHost::spawn().unwrap();
    let source = "rness.web_hooks.register('fetch',{after=function(result) result.content='first'; return result end})";
    host.load("web",source).await.unwrap();
    let input = json!({"content":"original"});
    let cancel = CancellationToken::new();
    assert_eq!(host.transform("fetch","after",input.clone(),json!({}),&cancel).await.unwrap()["content"],"first");
    assert!(host.load("duplicate",source).await.is_err());
    assert!(host.reload(vec![rness_lua::loader::PluginSource {name:"web".into(),source:format!("{source}; error('bad reload')")}]).await.is_err());
    assert_eq!(host.transform("fetch","after",input.clone(),json!({}),&cancel).await.unwrap()["content"],"first");
    host.reload(vec![rness_lua::loader::PluginSource {name:"web".into(),source:source.replace("first","second")}]).await.unwrap();
    assert_eq!(host.transform("fetch","after",input.clone(),json!({}),&cancel).await.unwrap()["content"],"second");
    host.reload(vec![]).await.unwrap();
    assert_eq!(host.transform("fetch","after",input.clone(),json!({}),&cancel).await.unwrap(),input);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_callbacks_transform_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("init.lua");
    std::fs::write(
        &path,
        r#"
        rness.web = {
            search = {provider='duckduckgo', before=function(request,ctx)
                assert(ctx.session == 'session'); request.query = 'rewritten'; return request
            end, after=function(result,ctx)
                result.sources[2] = nil; result.content = 'summary'; return result
            end},
            fetch = {timeout_ms=500, before=function(request,ctx)
                request.url = 'http://127.0.0.1:1/private'; return request
            end, after=function(result,ctx) error('must not run') end}
        }
    "#,
    )
    .unwrap();
    let (host, startup) = LuaHost::spawn_from_init(path).unwrap();
    let config: Config = serde_json::from_value(startup.web.unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let context = json!({"session":"session","call":"call"});
    let request = host
        .transform(
            "search",
            "before",
            json!({"query":"original"}),
            context.clone(),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(request["query"], "rewritten");
    let result = host
        .transform(
            "search",
            "after",
            json!({"sources":[{"url":"https://a"},{"url":"https://b"}],"truncated":false}),
            context,
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(result["sources"].as_array().unwrap().len(), 1);
    assert_eq!(result["content"], "summary");
    let registry = ToolRegistry::default();
    register_with_hooks(&registry, config, Arc::new(host)).unwrap();
    let results = registry
        .dispatch(
            &"session".into(),
            &[ToolCall {
                call: "call".into(),
                name: "web_fetch".into(),
                args: json!({"url":"https://example.com"}),
            }],
            1,
            &cancel,
        )
        .await;
    assert!(results[0].is_error);
    assert!(
        results[0].output.contains("non-public"),
        "{}",
        results[0].output
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_errors_cancellation_and_actor_recovery() {
    let host = LuaHost::spawn().unwrap();
    host.load("hooks", r#"rness.web = {fetch={
        before=function(request) if request.loop then while true do end end; error('rejected by policy') end,
        after=function(result) return 'invalid' end
    }}"#).await.unwrap();
    let cancel = CancellationToken::new();
    assert!(host
        .transform("fetch", "before", json!({}), json!({}), &cancel)
        .await
        .unwrap_err()
        .contains("rejected by policy"));
    assert!(host
        .transform("fetch", "after", json!({}), json!({}), &cancel)
        .await
        .unwrap_err()
        .contains("must return a table"));
    let token = cancel.clone();
    let other = host.clone();
    let running = tokio::spawn(async move {
        other
            .transform("fetch", "before", json!({"loop":true}), json!({}), &token)
            .await
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    assert!(running.await.unwrap().is_err());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        host.transform(
            "search",
            "before",
            json!({"query":"ok"}),
            json!({}),
            &CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["query"], "ok");
}
