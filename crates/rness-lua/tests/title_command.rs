//! The default flavor's `/title` command and `session-title` policy plugin
//! over a stubbed session API.
use rness_lua::runtime::LuaRuntime;

/// Run a `return function(opts) … end` plugin file with `opts` (Lua source).
fn setup(file: &str, opts: &str) -> String {
    format!("local setup = (function() {file} end)()\nsetup({opts})")
}

#[test]
fn title_command_shows_pins_unpins_and_regenerates() {
    let mut host = LuaRuntime::new().unwrap();
    host.load(
        "fixture",
        r#"
        state = { title = nil, source = nil }
        rness.commands.register = function(command) title_cmd = command end
        rness.session = {
          title_info = function(id)
            assert(id == 's1')
            if not state.title then return nil end
            return { title = state.title, source = state.source }
          end,
          title = function(id, text, source)
            if text then state.title, state.source = text, source or 'user' end
            return state.title
          end,
          generate_title = function(id, request)
            if fail then error('provider down') end
            assert(request.profile == 'fast')
            return 'Generated'
          end,
        }
    "#,
    )
    .unwrap();
    host.load(
        "title",
        &setup(include_str!("../../../flavors/default/plugins/title.lua"), "{ profile = 'fast' }"),
    )
    .unwrap();
    host.load(
        "assertions",
        r#"
        local function run(input) return title_cmd.run({ session = 's1', raw_input = input }).message end
        assert(run('') == '(no title)')
        assert(run('  My session  ') == 'Title set: My session')
        assert(state.source == 'user')
        assert(run('') == 'My session (pinned)')
        assert(run('unpin') == 'Title unpinned')
        assert(state.title == 'My session' and state.source == 'model')
        assert(run('') == 'My session')
        assert(run('unpin') == 'Title is not pinned')
        assert(run('auto') == 'Title: Generated')
        assert(state.source == 'user', '/title auto pins')
        fail = true
        local message = run('auto')
        assert(message:find('Title generation failed: ', 1, true) and message:find('provider down', 1, true), message)
    "#,
    )
    .unwrap();
}

/// Stub session + task APIs: tasks run when `drain()` is called.
const SESSION_TITLE_FIXTURE: &str = r#"
    offers, requests, queue, cancelled = {}, {}, {}, {}
    pinned = {}
    rness.session = {
      title_info = function(id)
        if pinned[id] then return { title = 'Mine', source = 'user' } end
        return nil
      end,
      title_fallback = function(text, words, max) return 'fb:' .. text end,
      offer_title = function(id, title, source, max)
        table.insert(offers, id .. '|' .. source .. '|' .. title .. '|' .. max)
        return true
      end,
      generate_title = function(id, request)
        table.insert(requests, id .. '|' .. request.prompts)
        if fail then error('down') end
        return 'Model ' .. id
      end,
    }
    local next_id = 0
    rness.task = {
      spawn = function(fn)
        next_id = next_id + 1
        queue[next_id] = fn
        return next_id
      end,
      cancel = function(id) queue[id] = nil; cancelled[#cancelled + 1] = id end,
    }
    function drain()
      for id = 1, next_id do
        local fn = queue[id]
        if fn then queue[id] = nil; fn() end
      end
    end
    hooks = {}
    rness.hook.on = function(event, fn) hooks[event] = fn end
    function prompt(session, index, text, parent)
      hooks.prompt({ session = session, index = index, text = text or 'hello there', parent = parent })
    end
"#;

fn session_title_host(opts: &str) -> LuaRuntime {
    let mut host = LuaRuntime::new().unwrap();
    host.load("fixture", SESSION_TITLE_FIXTURE).unwrap();
    host.load(
        "session-title",
        &setup(include_str!("../../../flavors/default/plugins/session-title.lua"), opts),
    )
    .unwrap();
    host
}

#[test]
fn session_title_first_prompt_mode() {
    let mut host = session_title_host("{ max_bytes = 60 }");
    host.load(
        "assertions",
        r#"
        prompt('s1', 1, 'fix the parser')
        assert(offers[1] == 's1|fallback|fb:fix the parser|60', offers[1])
        drain()
        assert(requests[1] == 's1|first', requests[1])
        assert(offers[2] == 's1|model|Model s1|60', offers[2])
        -- Later prompts never retitle in first mode.
        prompt('s1', 2)
        drain()
        assert(#requests == 1 and #offers == 2)
        -- Delegated children are skipped entirely.
        prompt('child', 1, 'x', 's1')
        drain()
        assert(#requests == 1 and #offers == 2)
        -- A pinned session keeps its fallback-free user title.
        pinned.s2 = true
        prompt('s2', 1)
        drain()
        assert(#requests == 1)
        -- Failure is logged, nothing offered.
        fail = true
        prompt('s3', 1)
        drain()
        assert(#requests == 2 and offers[#offers] == 's3|fallback|fb:hello there|60')
    "#,
    )
    .unwrap();
}

#[test]
fn session_title_all_prompts_mode_supersedes_inflight() {
    let mut host = session_title_host("{ mode = 'all', fallback = false }");
    host.load(
        "assertions",
        r#"
        prompt('s1', 1)
        prompt('s1', 2)            -- supersedes the first request
        assert(cancelled[1] == 1, 'older request cancelled')
        drain()
        assert(#requests == 1 and requests[1] == 's1|all')
        assert(#offers == 1 and offers[1] == 's1|model|Model s1|80')
        prompt('s1', 3)
        drain()
        assert(#requests == 2)
    "#,
    )
    .unwrap();
}

#[test]
fn session_title_rejects_bad_mode_and_off_mode_only_falls_back() {
    let mut host = LuaRuntime::new().unwrap();
    host.load("fixture", SESSION_TITLE_FIXTURE).unwrap();
    let bad = host.load(
        "bad",
        &setup(include_str!("../../../flavors/default/plugins/session-title.lua"), "{ mode = 'sometimes' }"),
    );
    assert!(bad.is_err());
    let mut host = session_title_host("{ mode = 'off' }");
    host.load(
        "assertions",
        r#"
        prompt('s1', 1)
        drain()
        assert(#requests == 0 and #offers == 1 and offers[1]:find('|fallback|', 1, true))
    "#,
    )
    .unwrap();
}
