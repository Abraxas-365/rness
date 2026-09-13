//! Default flavor job UI is user-driven and never consumes tool output.
use rness_kernel::presentation::TextProvider;
use rness_lua::plugin_host::LuaHost;
use serde_json::json;

#[tokio::test]
async fn default_manifest_resolves_statusline_and_jobs_plugins() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../flavors/default");
    let config = rness_lua::api::config::load(&root.join("init.lua")).unwrap();
    let specs = rness_lua::loader::discover_specs(&root, &config.plugin_specs).unwrap();
    assert!(specs.iter().any(|spec| spec.name == "statusline"));
    assert!(specs.iter().any(|spec| spec.name == "jobs"));
    assert!(!specs.iter().any(|spec| spec.name == "spinner"));
    let host = LuaHost::spawn().unwrap();
    for name in ["statusline", "jobs"] {
        let source = std::fs::read_to_string(root.join(format!("plugins/{name}.lua"))).unwrap();
        host.load(name, &source).await.unwrap();
    }
}

#[tokio::test]
async fn statusline_counts_current_session_jobs_while_idle_and_compacting() {
    let host = LuaHost::spawn().unwrap();
    host.load("stub", r#"
        rness.session = {usage=function() return {input=0} end, config=function() return {} end}
        counts = {one=2, two=1}
        agents = {
          one={{session='a1',running=true},{session='a2',running=true},{session='a3',running=false}},
          two={{session='a4',running=true}},
        }
        rness.jobs = {count=function(session) return counts[session] or 0 end}
        rness.subagents = {list=function(session) return agents[session] or {} end}
    "#).await.unwrap();
    host.load("statusline", include_str!("../../../flavors/default/plugins/statusline.lua")).await.unwrap();
    let one = json!({"session":"one","model":"test"});
    let text = host.status(one.clone()).await.unwrap().to_string();
    assert!(text.contains("idle") && text.contains("2 bg jobs") && text.contains("2 bg agents"), "{text}");
    let text = host.status(json!({"session":"two"})).await.unwrap().to_string();
    assert!(text.contains("1 bg job") && text.contains("1 bg agent") && !text.contains("2 bg jobs"), "{text}");
    host.fire_hook("frame", json!({"type":"compaction_started","session":"one"}));
    let text = host.status(one.clone()).await.unwrap().to_string();
    assert!(text.contains("compacting context") && text.contains("2 bg jobs") && text.contains("2 bg agents"), "{text}");
    host.load("settle", "counts.one = 0; agents.one = {}").await.unwrap();
    let settled = host.status(one.clone()).await.unwrap().to_string();
    assert!(!settled.contains("bg job") && !settled.contains("bg agent"), "{settled}");
    host.load("unavailable", "rness.jobs = nil; rness.subagents = nil").await.unwrap();
    assert!(host.status(one).await.is_some());
}

#[tokio::test]
async fn statusline_shows_background_agents_without_total_running_agent_count() {
    let host = LuaHost::spawn().unwrap();
    host.load("stub", r#"
        rness.session = {usage=function() return {input=0} end}
        rness.subagents = {list=function()
          return {{session='child',running=true}}
        end}
    "#).await.unwrap();
    host.load("statusline", include_str!("../../../flavors/default/plugins/statusline.lua")).await.unwrap();
    host.fire_hook("turn_start", json!({"session":"parent"}));
    host.fire_hook("turn_start", json!({"session":"child"}));
    let text = host.status(json!({"session":"parent","model":"test"}))
        .await.unwrap().to_string();
    assert!(text.contains("working"), "{text}");
    assert!(text.contains("1 bg agent"), "{text}");
    assert!(!text.contains("2 agents"), "{text}");
}

#[tokio::test]
async fn jobs_command_lists_inspects_and_stops_only_explicit_ids() {
    let host = LuaHost::spawn().unwrap();
    host.load("stub", r#"
        local job = {job_id='j1',kind='bash',label='cargo test',status='running',running=true,
                     cancellation_requested=false,output='test progress'}
        inspected, stopped = 0, 0
        rness.jobs = {
          list=function(session) assert(session=='one'); return {job} end,
          inspect=function(session,id) assert(session=='one' and id=='j1'); inspected=inspected+1; return job end,
          stop=function(session,id) assert(session=='one' and id=='j1'); stopped=stopped+1; return true end,
        }
        rness.commands.register = function(command) jobs_command = command end
    "#).await.unwrap();
    host.load("jobs", include_str!("../../../flavors/default/plugins/jobs.lua")).await.unwrap();
    host.load("assertions", r#"
        assert(jobs_command.name=='jobs' and jobs_command.allow_busy)
        local function run(input) return jobs_command.run({session='one',raw_input=input}) end
        assert(run('').message:find('cargo test',1,true))
        assert(run('list').message:find('j1',1,true))
        assert(inspected==0 and stopped==0)
        assert(run('j1').message:find('test progress',1,true))
        assert(inspected==1 and stopped==0)
        for _, input in ipairs({'stop','stop j1 extra','j1 extra'}) do
          assert(not pcall(run,input))
        end
        assert(stopped==0)
        assert(run('stop j1').message:find('Cancellation requested',1,true))
        assert(stopped==1)
        assert(not pcall(run,'foreign'))
        local choices=jobs_command.complete({session='one'})
        assert(table.concat(choices,' '):find('stop j1',1,true))
        rness.jobs.inspect=function() return {job_id='j1',kind='bash',label='cargo test',
          status='exited',exit_code=1,running=false,output='failed'} end
        assert(run('j1').message:find('exited (code 1)',1,true))
        rness.jobs.list=function() return {} end
        assert(run('').message:find('No background jobs',1,true))
    "#).await.unwrap();
}
