//! Agent-control grammar and the existing CommandResult data action contract.
use rness_lua::runtime::LuaRuntime;
use serde_json::json;

fn host() -> LuaRuntime {
    let mut host = LuaRuntime::new().unwrap();
    host.load(
        "fixture",
        r#"
        rness.subagents = {
          list = function(root)
            assert(root == 'main')
            return {
              {alias='a1', session='child-full', parent='main', depth=1, running=false},
              {alias='a2', session='grandchild-full', parent='child-full', depth=2, running=true},
            }
          end,
          steer_user = function(caller, target, text)
            assert(caller == 'main' and target == 'grandchild-full')
            assert(text == 'keep  spaces\nand lines')
            steered = true
          end,
          interrupt = function(caller, target)
            assert(caller == 'main' and target == 'grandchild-full')
            stopped = (stopped or 0) + 1
          end,
        }
    "#,
    )
    .unwrap();
    host.load(
        "controls",
        include_str!("../../../flavors/default/plugins/agent-controls.lua"),
    )
    .unwrap();
    host
}

#[test]
fn monitor_returns_allowlisted_data_action_and_full_ids() {
    let host = host();
    for (input, child) in [
        ("", None),
        (" a1 ", Some("child-full")),
        ("child-full", Some("child-full")),
        ("a2", Some("grandchild-full")),
    ] {
        let result = host
            .call_command("agents", json!({"session":"main", "raw_input":input}))
            .unwrap();
        let mut expected = json!({"action":"agents:open", "session":"main"});
        if let Some(child) = child {
            expected["agent"] = json!(child);
        }
        assert_eq!(result.data, expected);
        assert!(result.message.is_empty());
    }
    for input in [
        "child",
        "a0",
        "a01",
        "a3",
        "main",
        "other",
        "a1 unknown",
        "a1 stop extra",
    ] {
        assert!(
            host.call_command("agents", json!({"session":"main", "raw_input":input}))
                .is_err(),
            "{input}"
        );
    }
}

#[test]
fn steering_requires_text_and_stop_retains_compatibility() {
    let mut host = host();
    for input in ["a2 steer", "a2 steer \n  "] {
        let error = host
            .call_command("agents", json!({"session":"main", "raw_input":input}))
            .unwrap_err();
        assert!(error.contains("Usage: /agents <id> steer <message>"));
    }
    host.call_command(
        "agents",
        json!({"session":"main", "raw_input":"a2 steer keep  spaces\nand lines"}),
    )
    .unwrap();
    for input in ["a2 stop", "stop grandchild-full", "stop a2", "stop"] {
        let result = host
            .call_command("agents", json!({"session":"main", "raw_input":input}))
            .unwrap();
        assert!(
            result
                .message
                .contains("Confirm: /agents grandchild-full stop confirm")
        );
        assert!(
            result
                .message
                .contains("Queued messages are not cleared; descendants keep running.")
        );
    }
    host.load("before-confirm", "assert(steered); assert(stopped == nil)")
        .unwrap();
    host.call_command(
        "agents",
        json!({"session":"main", "raw_input":"grandchild-full stop confirm"}),
    )
    .unwrap();
    host.load("check", "assert(stopped == 1)").unwrap();
    let choices = host
        .complete_command("agents", json!({"session":"main"}))
        .unwrap();
    assert!(choices.contains(&"a1".into()));
    assert!(choices.contains(&"a2 steer".into()));
    assert!(choices.contains(&"stop a2".into()));
    assert!(!choices.contains(&"stop a1".into()));
}

#[test]
fn aliases_use_runtime_identity_not_list_position() {
    let mut host = host();
    host.load(
        "removed",
        r#"
      rness.subagents.list = function()
        return {{alias='a2', session='grandchild-full', parent='main', depth=1, running=false}}
      end
    "#,
    )
    .unwrap();
    assert!(
        host.call_command("agents", json!({"session":"main", "raw_input":"a1"}))
            .is_err()
    );
    let result = host
        .call_command("agents", json!({"session":"main", "raw_input":"a2"}))
        .unwrap();
    assert_eq!(result.data["agent"], "grandchild-full");
}

#[test]
fn inspect_completion_precedes_stop_and_steer_even_after_trailing_spaces() {
    let host = host();
    for raw in [
        "",
        " ",
        "  ",
        " a1",
        " a1 ",
        " a1  ",
        " child-full",
        " child-full ",
    ] {
        let choices = host
            .complete_command("agents", json!({"session":"main", "raw_input":raw}))
            .unwrap();
        assert!(choices.contains(&"stop".into()));
        assert!(choices.contains(&"a1 steer".into()));
        // Model the host's command prefix and input's prefix filtering. No
        // mutation verb may become the first match for an exact inspect input.
        let typed = format!("agents{raw}");
        let first = choices
            .iter()
            .map(|value| format!("agents {value}"))
            .find(|candidate| candidate.starts_with(&typed))
            .unwrap();
        assert_eq!(first.trim(), typed.trim(), "raw={raw:?}");
        let result = host
            .call_command(
                "agents",
                json!({"session":"main", "raw_input":first.strip_prefix("agents").unwrap()}),
            )
            .unwrap();
        assert_eq!(result.data["action"], "agents:open");
    }
}
