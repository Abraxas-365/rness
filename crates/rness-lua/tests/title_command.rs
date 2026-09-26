//! The default flavor's pure-Lua `title` plugin (`/title` + automatic
//! titles) over stubbed session, llm and task APIs.
use rness_lua::runtime::LuaRuntime;

/// Run a `return function(opts) … end` plugin file with `opts` (Lua source).
fn setup(file: &str, opts: &str) -> String {
    format!("local setup = (function() {file} end)()\nsetup({opts})")
}

const PLUGIN: &str = include_str!("../../../flavors/default/plugins/title.lua");

/// Stubs: session state, recorded llm calls, and tasks that run on `drain()`.
const FIXTURE: &str = r#"
    state, offers, calls, queue, cancelled, pinned, notices = {}, {}, {}, {}, {}, {}, {}
    prompts = { s1 = { 'fix the parser' } }
    rness.commands.register = function(command) title_cmd = command end
    rness.session = {
      notify = function(id, text) table.insert(notices, id .. '|' .. text) end,
      prompts = function(id) return prompts[id] or { 'hello there' } end,
      title_info = function(id)
        if pinned[id] then return { title = 'Mine', source = 'user' } end
        if not state[id] then return nil end
        return { title = state[id].title, source = state[id].source }
      end,
      title = function(id, text, source)
        if text then state[id] = { title = text, source = source or 'user' } end
        return state[id] and state[id].title
      end,
      offer_title = function(id, title, source, max)
        table.insert(offers, id .. '|' .. source .. '|' .. title .. '|' .. max)
        return true
      end,
    }
    rness.llm = {
      complete = function(req)
        table.insert(calls, req)
        if fail then error('provider down') end
        return 'Model ' .. req.session
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

fn host(opts: &str) -> LuaRuntime {
    let mut host = LuaRuntime::new().unwrap();
    host.load("fixture", FIXTURE).unwrap();
    host.load("title", &setup(PLUGIN, opts)).unwrap();
    host
}

#[test]
fn title_command_shows_pins_unpins_and_regenerates() {
    let mut host = host("{ auto = 'off', fallback = false, profile = 'fast', system = 'Custom prompt' }");
    host.load(
        "assertions",
        r#"
        assert(hooks.prompt == nil, 'auto off without fallback registers no hook')
        assert(title_cmd.allow_busy == true, '/title runs during a turn')
        local function run(input) return title_cmd.run({ session = 's1', raw_input = input }).message end
        assert(run('') == '(no title)')
        assert(run('  My session  ') == 'Title set: My session')
        assert(state.s1.source == 'user')
        assert(run('') == 'My session (pinned)')
        assert(run('unpin') == 'Title unpinned')
        assert(state.s1.title == 'My session' and state.s1.source == 'model')
        assert(run('') == 'My session')
        assert(run('unpin') == 'Title is not pinned')
        -- /title auto returns at once; the model call runs as a task.
        assert(run('auto') == 'Generating title…')
        assert(#calls == 0 and state.s1.title == 'My session')
        drain()
        assert(state.s1.title == 'Model s1' and state.s1.source == 'user', '/title auto pins')
        assert(notices[1] == 's1|Title: Model s1', notices[1])
        local call = calls[1]
        assert(call.profile == 'fast' and call.system == 'Custom prompt' and call.max_output_tokens == 64)
        assert(call.prompt:find('"text":"fix the parser"', 1, true), call.prompt)
        -- A rename supersedes an in-flight /title auto.
        run('auto')
        assert(run('Mine now') == 'Title set: Mine now')
        drain()
        assert(#calls == 1 and state.s1.title == 'Mine now' and #notices == 1)
        -- Failures are reported by notice.
        fail = true
        run('auto')
        drain()
        assert(notices[2]:find('s1|Title generation failed: ', 1, true) and notices[2]:find('provider down', 1, true), notices[2])
        -- No prompts: refused at once, no task.
        prompts.empty = {}
        local message = title_cmd.run({ session = 'empty', raw_input = 'auto' }).message
        assert(message == 'Title generation failed: no prompt to title yet', message)
    "#,
    )
    .unwrap();
}

#[test]
fn auto_first_prompt_mode() {
    let mut host = host("{ max_bytes = 60 }");
    host.load(
        "assertions",
        r#"
        prompt('s1', 1, '  fix   the parser in lexer.rs now please ')
        assert(offers[1] == 's1|fallback|fix the parser in lexer.rs|60', offers[1])
        drain()
        assert(#calls == 1 and calls[1].session == 's1' and calls[1].timeout == 60)
        assert(calls[1].system:find('Create a concise title', 1, true))
        assert(offers[2] == 's1|model|Model s1|60', offers[2])
        -- Later prompts never retitle in first mode.
        prompt('s1', 2)
        drain()
        assert(#calls == 1 and #offers == 2)
        -- Delegated children are skipped entirely.
        prompt('child', 1, 'x', 's1')
        drain()
        assert(#calls == 1 and #offers == 2)
        -- A pinned session gets no model request.
        pinned.s2 = true
        prompt('s2', 1)
        drain()
        assert(#calls == 1)
        -- Failure is logged, nothing offered.
        fail = true
        prompt('s3', 1)
        drain()
        assert(#calls == 2 and offers[#offers] == 's3|fallback|hello there|60')
        fail = false
        -- An explicit /title auto in flight is not superseded by auto titling.
        title_cmd.run({ session = 's4', raw_input = 'auto' })
        prompt('s4', 1)
        assert(#cancelled == 0, 'explicit request kept')
        drain()
        assert(#calls == 3 and state.s4.title == 'Model s4' and state.s4.source == 'user')
    "#,
    )
    .unwrap();
}

#[test]
fn auto_all_prompts_mode_frames_every_prompt_and_supersedes() {
    let mut host = host("{ auto = 'all', fallback = false, max_input_bytes = 120 }");
    host.load(
        "assertions",
        r#"
        prompts.s1 = { 'alpha one', 'beta two', 'gamma three' }
        prompt('s1', 1)
        prompt('s1', 2)            -- supersedes the first request
        assert(cancelled[1] == 1, 'older request cancelled')
        drain()
        assert(#calls == 1 and #offers == 1 and offers[1] == 's1|model|Model s1|80')
        local framed = calls[1].prompt
        assert(framed:find('gamma three', 1, true) and #framed <= 120, framed)
        assert(not framed:find('alpha one', 1, true), 'oldest prompt dropped to fit')
        -- A single oversized prompt is trimmed on a UTF-8 boundary.
        prompts.s1 = { string.rep('é', 200) }
        prompt('s1', 3)
        drain()
        framed = calls[2].prompt
        assert(#framed <= 120 and utf8.len(framed), framed)
    "#,
    )
    .unwrap();
}

#[test]
fn rejects_bad_auto_and_off_mode_only_falls_back() {
    let mut bad = LuaRuntime::new().unwrap();
    bad.load("fixture", FIXTURE).unwrap();
    assert!(bad.load("bad", &setup(PLUGIN, "{ auto = 'sometimes' }")).is_err());
    let mut host = host("{ auto = 'off' }");
    host.load(
        "assertions",
        r#"
        prompt('s1', 1)
        drain()
        assert(#calls == 0 and #offers == 1 and offers[1]:find('|fallback|', 1, true))
    "#,
    )
    .unwrap();
}
