use rness_lua::runtime::LuaRuntime;
use serde_json::json;

#[test]
fn session_cards_render_success_error_running_and_malformed_results() {
    let mut runtime = LuaRuntime::new().unwrap();
    runtime.load("provider-stub", "rness.session = { search_provider = function() return function() end end }").unwrap();
    runtime.load("session-search", include_str!("../../../examples/plugins/session-search.lua")).unwrap();
    let specs = runtime.tool_specs();
    assert_eq!(specs.len(), 5);
    for spec in &specs {
        if let Some(required) = spec.input_schema.get("required") {
            assert!(required.is_array(), "{}: required must be an array", spec.name);
        }
        if spec.name == "session_trace" {
            assert!(spec.input_schema.get("required").is_none());
        } else {
            assert!(spec.input_schema["required"].as_array().is_some_and(|values| !values.is_empty()));
        }
    }
    for name in ["session_search", "session_event_search", "session_event_read", "session_event_trace", "session_trace"] {
        for output in [
            json!({"items":[{"session_id":"s","event_ref":"e","event_type":"user/message","surface":"current","snippet":"🦀 saved text"}],"next_cursor":"more"}).to_string(),
            json!({"items":[],"next_cursor":null}).to_string(),
            json!({"chunk":"🦀".repeat(10000),"next_cursor":"more"}).to_string(),
            json!({"items":[{"from":"a","relationship":"replaces","to":"b"}],"snapshot_truncated":true}).to_string(),
            "invalid JSON".into(),
        ] {
            let lines = runtime.tool_card(name, &json!({"query":"🦀 needle"}), &output, false).expect("card renders");
            assert!(!lines.is_empty());
            assert!(lines.len() < 24);
        }
        assert!(runtime.tool_card(name, &json!({}), "denied", true).is_some());
        assert!(runtime.tool_card_presented(name, &json!({}), "", false,
            Some(&json!({"status":"running","duration_ms":12}))).is_some());
    }
}
