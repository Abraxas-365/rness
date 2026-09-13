//! Both shipped plan commands describe queued changes without reporting the opposite mode.
#[test]
fn plan_command_messages_describe_the_next_ai_step() {
    for source in [
        include_str!("../../../flavors/default/plugins/plan.lua"),
        include_str!("../../../examples/plugins/plan.lua"),
    ] {
        let lua = mlua::Lua::new();
        lua.load(r#"
            state = { active = false }
            rness = {
                plan = { enable = function() end },
                commands = { register = function(command) plan_command = command end },
                session = { plan = function(session, active)
                    assert(session == "test")
                    if active ~= nil then state.pending = active end
                    return state
                end },
            }
        "#).exec().unwrap();
        lua.load(source).exec().unwrap();
        lua.load(r#"
            local function check(input, message)
                local result = plan_command.run { session = "test", raw_input = input }
                assert(result.message == message, result.message)
                assert(result.data == state)
            end
            check("status", "Plan mode inactive")
            for _, input in ipairs({ "on", "on", "status", "", " on " }) do
                check(input, "Plan mode will activate on the next AI step")
                assert(state.pending == true)
            end
            state = { active = true }
            check("status", "Plan mode active")
            check("off", "Plan mode will deactivate on the next AI step")
            assert(state.pending == false)
            check("status", "Plan mode will deactivate on the next AI step")
            check("on", "Plan mode will activate on the next AI step")
            assert(state.pending == true)
        "#).exec().unwrap();
    }
}
