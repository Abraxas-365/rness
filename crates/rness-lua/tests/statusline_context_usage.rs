//! Default statusline keeps engine estimates separate from provider billing usage.
use rness_kernel::presentation::TextProvider;
use rness_lua::plugin_host::LuaHost;
use serde_json::json;

async fn host() -> LuaHost {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "usage-stub",
        r#"
        calls = {}
        inputs = {one = 120000, two = 3000}
        rness.session = {
          usage = function(session)
            calls[session] = (calls[session] or 0) + 1
            return {input = inputs[session] or 0}
          end,
          config = function() error('UI must not resolve thresholds') end,
        }
        rness.compaction = {default = {threshold_tokens = 165000}}
    "#,
    )
    .await
    .unwrap();
    host.load(
        "statusline",
        include_str!("../../../flavors/default/plugins/statusline.lua"),
    )
    .await
    .unwrap();
    host
}

async fn status(host: &LuaHost, session: &str) -> String {
    host.status(json!({"session": session, "model": "test"}))
        .await
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn old_binary_and_resume_show_unknown_estimate_without_provider_ratio() {
    let host = host().await;
    for _ in 0..5 {
        let text = status(&host, "one").await;
        assert!(text.contains("? est tok"), "{text}");
        assert!(text.contains("120.0k last input"), "{text}");
        assert!(!text.contains('/'), "{text}");
    }
    host.load("cached", "assert(calls.one == 1)").await.unwrap();
    // A host without the optional usage service still renders the unknown estimate.
    host.load("old-service", "rness.session.usage = nil")
        .await
        .unwrap();
    let text = status(&host, "two").await;
    assert!(
        text.contains("? est tok") && text.contains("idle"),
        "{text}"
    );
    assert!(host.status(json!({})).await.is_some());
}

#[tokio::test]
async fn estimates_are_session_local_and_remain_visible_during_compaction() {
    let host = host().await;
    host.fire_hook(
        "frame",
        json!({"type":"context_usage", "session":"one",
        "estimated_tokens":46300, "threshold_tokens":150000}),
    );
    let text = status(&host, "one").await;
    assert!(text.contains("46.3k/150.0k est tok"), "{text}");
    assert!(text.contains("120.0k last input"), "{text}");
    assert!(!text.contains("120.0k/"), "{text}");
    let other = status(&host, "two").await;
    assert!(
        other.contains("? est tok") && other.contains("3.0k last input"),
        "{other}"
    );
    assert!(!other.contains("46.3k"), "{other}");

    host.fire_hook(
        "frame",
        json!({"type":"compaction_started", "session":"one"}),
    );
    let text = status(&host, "one").await;
    assert!(
        text.contains("compacting context") && text.contains("46.3k/150.0k est tok"),
        "{text}"
    );
    assert!(text.contains("120.0k last input"), "{text}");
    host.fire_hook(
        "frame",
        json!({"type":"context_usage", "session":"one",
        "estimated_tokens":12500, "threshold_tokens":150000}),
    );
    host.fire_hook(
        "frame",
        json!({"type":"compaction_finished", "session":"one", "changed":true}),
    );
    host.fire_hook("turn_end", json!({"session":"one"}));
    let text = status(&host, "one").await;
    assert!(
        text.contains("idle") && text.contains("12.5k/150.0k est tok"),
        "{text}"
    );
    assert!(!text.contains("46.3k"), "{text}");
}

#[tokio::test]
async fn no_policy_omits_denominator_and_history_change_invalidates_only_its_session() {
    let host = host().await;
    // Test both JSON null (the engine's Option::None) and an absent field.
    host.fire_hook(
        "frame",
        json!({"type":"context_usage", "session":"one",
        "estimated_tokens":9000, "threshold_tokens":null}),
    );
    host.fire_hook(
        "frame",
        json!({"type":"context_usage", "session":"two",
        "estimated_tokens":0}),
    );
    let text = status(&host, "one").await;
    assert!(
        text.contains("9.0k est tok") && !text.contains('/'),
        "{text}"
    );
    assert!(status(&host, "two").await.contains("0.0k est tok"));
    host.fire_hook("frame", json!({"type":"history_changed", "session":"one"}));
    let text = status(&host, "one").await;
    assert!(
        text.contains("? est tok") && !text.contains("9.0k est tok"),
        "{text}"
    );
    assert!(text.contains("120.0k last input"), "{text}");
    assert!(status(&host, "two").await.contains("0.0k est tok"));
    host.fire_hook(
        "frame",
        json!({"type":"context_usage", "session":"one",
        "estimated_tokens":2000, "threshold_tokens":10000}),
    );
    assert!(status(&host, "one").await.contains("2.0k/10.0k est tok"));
    // Removing a previously reported policy must also remove its denominator.
    host.fire_hook(
        "frame",
        json!({"type":"context_usage", "session":"one",
        "estimated_tokens":2500, "threshold_tokens":null}),
    );
    let text = status(&host, "one").await;
    assert!(
        text.contains("2.5k est tok") && !text.contains('/'),
        "{text}"
    );
}

#[tokio::test]
async fn provider_usage_refreshes_on_commit_and_turn_end_not_render_or_estimate_frames() {
    let host = host().await;
    status(&host, "one").await;
    host.load("advance-input", "inputs.one = 121000")
        .await
        .unwrap();
    host.fire_hook(
        "frame",
        json!({"type":"context_usage", "session":"one",
        "estimated_tokens":50000, "threshold_tokens":150000}),
    );
    for _ in 0..5 {
        assert!(status(&host, "one").await.contains("120.0k last input"));
    }
    host.load("cached", "assert(calls.one == 1)").await.unwrap();
    host.fire_hook(
        "frame",
        json!({"type":"step_committed", "session":"one", "step":1}),
    );
    assert!(status(&host, "one").await.contains("121.0k last input"));
    host.load(
        "advance-input-again",
        "assert(calls.one == 2); inputs.one = 122000",
    )
    .await
    .unwrap();
    host.fire_hook("turn_end", json!({"session":"one"}));
    for _ in 0..5 {
        let text = status(&host, "one").await;
        assert!(
            text.contains("122.0k last input") && text.contains("50.0k/150.0k est tok"),
            "{text}"
        );
    }
    host.load("still-cached", "assert(calls.one == 3)")
        .await
        .unwrap();
}
