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
fn completion_hints_show_role_and_bounded_task_without_changing_values() {
    let mut host = LuaRuntime::new().unwrap();
    host.load("hint-fixture", r#"
        rness.commands.register = function(command) controls = command end
        rness.commands.completion_descriptions = true
        rness.subagents = {list = function(root, details)
          assert(root == 'main' and details == true)
          return {
            {alias='a1', session='child-full', name='scout', task='  Trace\n  compaction settings  ', running=true},
            {alias='a2', session='other-full', task=string.rep('界', 125), running=false},
          }
        end}
    "#).unwrap();
    host.load(
        "controls",
        include_str!("../../../flavors/default/plugins/agent-controls.lua"),
    )
    .unwrap();
    host.load(
        "hint-assertions",
        r#"
        local choices = controls.complete({session='main'})
        local found = {}
        for _, item in ipairs(choices) do
          assert(type(item) == 'table')
          assert(not item.value:find('full', 1, true))
          found[item.value] = item.description
        end
        assert(found.a1 == 'scout · Trace compaction settings')
        assert(found['steer a1'] == found.a1 and found['stop a1'] == found.a1)
        assert(found['a1 steer'] == nil and found['a1 stop'] == nil)
        assert(found['steer'] == 'Choose an agent to steer')
        assert(found.a2 == 'subagent · ' .. string.rep('界', 120) .. '…')
        local exact = controls.complete({session='main', raw_input=' a1 '})[1]
        assert(exact.value == 'a1 ' and exact.description == found.a1)
        -- Older binaries still receive string completions, never hint text.
        rness.commands.completion_descriptions = nil
        for _, item in ipairs(controls.complete({session='main'})) do
          assert(type(item) == 'string' and not item:find('scout', 1, true))
        end
    "#,
    )
    .unwrap();
}

#[test]
fn runtime_parses_mixed_completion_items_and_preserves_legacy_values() {
    let mut host = host();
    let items = host.complete_command_items("agents", json!({"session":"main"})).unwrap();
    assert!(items.contains(&("a1".into(), "subagent".into())));
    host.load("mixed", r#"
        assert(rness.commands.completion_descriptions)
        rness.commands.register {
          name='mixed', run=function() end,
          complete=function() return {'plain', {value='rich', description='hint'}, {value='bare'}} end,
        }
    "#).unwrap();
    assert_eq!(host.complete_command_items("mixed", json!({})).unwrap(), vec![
        ("plain".into(), "".into()), ("rich".into(), "hint".into()), ("bare".into(), "".into()),
    ]);
    assert_eq!(host.complete_command("mixed", json!({})).unwrap(), ["plain", "rich", "bare"]);
    host.load("invalid", r#"
        rness.commands.register {name='invalid', run=function() end,
          complete=function() return {{description='missing value'}} end}
    "#).unwrap();
    assert!(host.complete_command_items("invalid", json!({})).is_err());
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
        "steer",
        "steer unknown message",
        "stop a2 extra",
        "stop a2 confirm extra",
        "stop confirm",
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
    for input in ["a2 steer", "a2 steer \n  ", "steer a2", "steer a2 \n  "] {
        let error = host
            .call_command("agents", json!({"session":"main", "raw_input":input}))
            .unwrap_err();
        assert!(error.contains("Usage: /agents steer <id> <message>"));
    }
    for input in [
        "a2 steer keep  spaces\nand lines",
        "steer a2 keep  spaces\nand lines",
        "steer grandchild-full keep  spaces\nand lines",
    ] {
        let result = host
            .call_command("agents", json!({"session":"main", "raw_input":input}))
            .unwrap();
        assert_eq!(result.message, "User steering sent to subagent a2");
    }
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
    for (index, input) in ["grandchild-full stop confirm", "stop grandchild-full confirm", "stop a2 confirm"].iter().enumerate() {
        host.call_command("agents", json!({"session":"main", "raw_input":input})).unwrap();
        host.load(&format!("check-{index}"), &format!("assert(stopped == {})", index + 1)).unwrap();
    }
    let choices = host
        .complete_command("agents", json!({"session":"main"}))
        .unwrap();
    assert!(choices.contains(&"a1".into()));
    assert!(choices.contains(&"steer a2".into()));
    assert!(choices.contains(&"stop a2".into()));
    assert!(!choices.contains(&"stop a1".into()));
    assert!(choices.iter().all(|choice| !choice.contains("-full")));
}

#[test]
fn stop_listing_uses_aliases_and_completion_falls_back_without_aliases() {
    let mut host = host();
    host.load(
        "active",
        r#"
        rness.subagents.list = function()
          return {
            {alias='a1', session='child-full', parent='main', depth=1, running=true},
            {alias='a2', session='grandchild-full', parent='child-full', depth=2, running=true},
          }
        end
    "#,
    )
    .unwrap();
    let result = host
        .call_command("agents", json!({"session":"main", "raw_input":"stop"}))
        .unwrap();
    assert!(result.message.contains("a1  depth=1"));
    assert!(result.message.contains("a2  depth=2"));
    assert!(!result.message.contains("-full"));

    host.load(
        "legacy",
        r#"
        rness.subagents.list = function()
          return {{session='child-full', parent='main', depth=1, running=true}}
        end
    "#,
    )
    .unwrap();
    let choices = host
        .complete_command("agents", json!({"session":"main"}))
        .unwrap();
    assert!(choices.contains(&"child-full".into()));
    assert!(choices.contains(&"steer child-full".into()));
    assert!(choices.contains(&"stop child-full".into()));
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
        assert!(choices.contains(&"steer a1".into()));
        if raw.contains("a1") {
            assert!(choices.contains(&"a1 steer".into()));
            assert!(choices.contains(&"a1 stop".into()));
        }
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
