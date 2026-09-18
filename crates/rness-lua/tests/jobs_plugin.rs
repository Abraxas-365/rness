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
    host.load(
        "statusline",
        include_str!("../../../flavors/default/plugins/statusline.lua"),
    )
    .await
    .unwrap();
    let one = json!({"session":"one","model":"test"});
    let text = host.status(one.clone()).await.unwrap().to_string();
    assert!(
        text.contains("idle") && text.contains("2 bg jobs") && text.contains("2 bg agents"),
        "{text}"
    );
    let text = host
        .status(json!({"session":"two"}))
        .await
        .unwrap()
        .to_string();
    assert!(
        text.contains("1 bg job") && text.contains("1 bg agent") && !text.contains("2 bg jobs"),
        "{text}"
    );
    host.fire_hook(
        "frame",
        json!({"type":"compaction_started","session":"one"}),
    );
    let text = host.status(one.clone()).await.unwrap().to_string();
    assert!(
        text.contains("compacting context")
            && text.contains("2 bg jobs")
            && text.contains("2 bg agents"),
        "{text}"
    );
    host.load("settle", "counts.one = 0; agents.one = {}")
        .await
        .unwrap();
    let settled = host.status(one.clone()).await.unwrap().to_string();
    assert!(
        !settled.contains("bg job") && !settled.contains("bg agent"),
        "{settled}"
    );
    host.load("unavailable", "rness.jobs = nil; rness.subagents = nil")
        .await
        .unwrap();
    assert!(host.status(one).await.is_some());
}

#[tokio::test]
async fn statusline_shows_background_agents_without_total_running_agent_count() {
    let host = LuaHost::spawn().unwrap();
    host.load(
        "stub",
        r#"
        rness.session = {usage=function() return {input=0} end}
        rness.subagents = {list=function()
          return {{session='child',running=true}}
        end}
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
    host.fire_hook("turn_start", json!({"session":"parent"}));
    host.fire_hook("turn_start", json!({"session":"child"}));
    let text = host
        .status(json!({"session":"parent","model":"test"}))
        .await
        .unwrap()
        .to_string();
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
    host.load(
        "jobs",
        include_str!("../../../flavors/default/plugins/jobs.lua"),
    )
    .await
    .unwrap();
    host.load("assertions", r#"
        assert(jobs_command.name=='jobs' and jobs_command.allow_busy)
        local function run(input) return jobs_command.run({session='one',raw_input=input}) end
        assert(run('').data.action=='app:open' and run('').data.app=='jobs' and run('').data.session=='one')
        assert(run('list').message:find('j1',1,true))
        assert(inspected==0 and stopped==0)
        assert(run('j1').data.app=='jobs')
        assert(inspected==1 and stopped==0)
        for _, input in ipairs({'stop','stop j1 extra','j1 extra'}) do
          assert(not pcall(run,input))
        end
        assert(stopped==0)
        assert(run('stop j1').message:find('Cancellation requested',1,true))
        assert(stopped==1)
        assert(not pcall(run,'foreign'))
        local choices=jobs_command.complete({session='one'})
        local found={}
        for _, item in ipairs(choices) do found[item.value]=item.description end
        assert(found['stop j1']=='bash · running · cargo test')
        for _,raw in ipairs({'',' ',' j1',' j1 '}) do
          local first=jobs_command.complete({session='one',raw_input=raw})[1]
          assert(first.value==raw:gsub('^%s','',1))
        end
        rness.jobs.inspect=function() return {job_id='j1',kind='bash',label='cargo test',
          status='exited',exit_code=1,running=false,output='failed'} end
        assert(run('j1').data.app=='jobs')
        -- Active hints remain Unicode-bounded, including the exact boundary.
        rness.jobs.list=function() return {
          {job_id='long',kind='bash',label=string.rep('界',125),status='running',running=true},
          {job_id='exact',kind='bash',label=string.rep('界',120),status='running',running=true},
        } end
        found={}
        for _, item in ipairs(jobs_command.complete({session='one'})) do found[item.value]=item.description end
        assert(found.long=='bash · running · '..string.rep('界',120)..'…')
        assert(found.exact=='bash · running · '..string.rep('界',120))
        rness.jobs.list=function() return {} end
        assert(run('list').message:find('No active background jobs',1,true))
    "#).await.unwrap();
}

async fn monitor_host() -> LuaHost {
    monitor_host_config("{}").await
}

async fn monitor_host_config(config: &str) -> LuaHost {
    let host = LuaHost::spawn().unwrap();
    host.load("fixture", r#"
        jobs = {
          {job_id='j1',kind='bash',label='first',status='running',running=true,output='first output'},
          {job_id='j2',kind='subagent',label='second',status='cancelling',running=true,
            cancellation_requested=true,output='second output'},
          {job_id='done',kind='bash',label='finished',status='exited',running=false,exit_code=7,output='retained'},
        }
        rness.jobs.list=function(session) return session=='one' and jobs or {} end
        rness.jobs.inspect=function(session,id)
          assert(session=='one', 'foreign session')
          for _, job in ipairs(jobs) do if job.job_id==id then return job end end
          error('missing')
        end
        rness.jobs.stop=function() error('navigation must never stop jobs') end
        rness.commands.register=function(command) jobs_command=command end
        function open_jobs(input, session)
          return jobs_command.run({raw_input=input or '',session=session or 'one'})
        end
    "#).await.unwrap();
    host.load(
        "initial-options",
        &format!("rness.jobs.config = {config}; original_setup = rness.jobs.setup"),
    )
    .await
    .unwrap();
    host.load(
        "jobs",
        include_str!("../../../flavors/default/plugins/jobs.lua"),
    )
    .await
    .unwrap();
    host.load(
        "unchanged-api",
        "assert(rness.jobs.setup == original_setup)",
    )
    .await
    .unwrap();
    host
}

#[tokio::test]
async fn monitor_navigation_retains_finished_jobs_and_session_isolation() {
    use rness_lua::runtime::AppKeyOutcome::{Consumed, Pass};
    let host = monitor_host().await;
    let ctx = json!({"session":"one","rows":8,"cols":100});
    let lines = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert!(lines[0].contains("(3)"));
    assert!(lines[1].starts_with("> j1") && lines[2].contains("stopping"));
    assert!(lines[3].contains("finished") && lines[3].contains("exited (code 7)"));
    assert_eq!(host.app_key("jobs", "z", ctx.clone()).await.unwrap(), Pass);
    assert_eq!(
        host.app_key("jobs", "j", ctx.clone()).await.unwrap(),
        Consumed
    );
    assert!(host.app_view("jobs", ctx.clone()).await.unwrap()[2].starts_with("> j2"));
    host.app_key("jobs", "enter", ctx.clone()).await.unwrap();
    assert!(host
        .app_view("jobs", ctx.clone())
        .await
        .unwrap()
        .join("\n")
        .contains("second output"));
    host.load(
        "settle",
        "jobs[2].running=false; jobs[2].status='killed'; jobs[2].output='final output'",
    )
    .await
    .unwrap();
    let finished = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert!(finished[0].contains("killed") && finished.join("\n").contains("final output"));
    let other = host
        .app_view("jobs", json!({"session":"two","rows":8,"cols":100}))
        .await
        .unwrap();
    assert!(other[1].contains("No background jobs"), "{other:?}");
    assert_eq!(host.app_view("jobs", ctx.clone()).await.unwrap(), finished);
    assert_eq!(
        host.app_key("jobs", "esc", ctx.clone()).await.unwrap(),
        Consumed
    );
    let lines = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert!(lines[0].contains("(3)") && lines[2].starts_with("> j2"));
    assert!(lines[2].contains("killed"));
    host.app_key("jobs", "j", ctx.clone()).await.unwrap();
    host.app_key("jobs", "enter", ctx.clone()).await.unwrap();
    let retained = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert!(retained[0].contains("exited (code 7)") && retained.join("\n").contains("retained"));
    host.app_key("jobs", "esc", ctx.clone()).await.unwrap();
    assert_eq!(
        host.app_key("jobs", "esc", ctx.clone()).await.unwrap(),
        Pass
    );
    host.load("exact", r#"
        assert(open_jobs('done').data.app=='jobs')
        assert(not pcall(open_jobs,'j1','two'))
        for _,raw in ipairs({' done',' done '}) do
          local first=jobs_command.complete({session='one',raw_input=raw})[1]
          assert(first.value==raw:gsub('^%s','',1) and first.description:find('exited (code 7)',1,true))
        end
    "#).await.unwrap();
    assert!(host.app_view("jobs", ctx.clone()).await.unwrap()[0].contains("exited (code 7)"));
    host.load("reset", "open_jobs('')").await.unwrap();
    assert!(host.app_view("jobs", ctx).await.unwrap()[0].contains("Background jobs"));
}

#[tokio::test]
async fn monitor_live_tail_pause_scroll_end_wrap_and_small_viewports() {
    let host = monitor_host_config(
        r#"{ render=function(ctx,m,lines)
      if m.mode=='detail' then assert(#m.output<=8192 and utf8.len(m.output)) end
      return lines
    end }"#,
    )
    .await;
    host.load(
        "output",
        r#"
        jobs[1].output=''
        for i=1,30 do jobs[1].output=jobs[1].output..'line '..i..'\n' end
        jobs[1].output=jobs[1].output..'LAST'
        open_jobs('j1')
    "#,
    )
    .await
    .unwrap();
    let ctx = json!({"session":"one","rows":9,"cols":100});
    assert!(host
        .app_view("jobs", ctx.clone())
        .await
        .unwrap()
        .join("\n")
        .contains("LAST"));
    host.load(
        "between-frame-and-key",
        "jobs[1].output=jobs[1].output..' ARRIVED AFTER FRAME'",
    )
    .await
    .unwrap();
    host.app_key("jobs", " ", ctx.clone()).await.unwrap();
    let frozen = host.app_view("jobs", ctx.clone()).await.unwrap().join("\n");
    assert!(frozen.contains("LAST") && !frozen.contains("ARRIVED AFTER FRAME"));
    host.app_key("jobs", "home", ctx.clone()).await.unwrap();
    let paused = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert!(paused[1].contains("PAUSED") && paused[3] == "line 1");
    host.load("append", "jobs[1].output=jobs[1].output..' NEW OUTPUT'")
        .await
        .unwrap();
    assert_eq!(host.app_view("jobs", ctx.clone()).await.unwrap(), paused);
    host.app_key("jobs", "pagedown", ctx.clone()).await.unwrap();
    assert_eq!(
        host.app_view("jobs", ctx.clone()).await.unwrap()[3],
        "line 6"
    );
    host.app_key("jobs", "pageup", ctx.clone()).await.unwrap();
    assert_eq!(host.app_view("jobs", ctx.clone()).await.unwrap(), paused);
    host.app_key("jobs", "end", ctx.clone()).await.unwrap();
    let lines = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert!(lines[1].contains("FOLLOW") && lines.join("\n").contains("NEW OUTPUT"));
    host.app_key("jobs", " ", ctx.clone()).await.unwrap();
    assert!(host.app_view("jobs", ctx.clone()).await.unwrap()[1].contains("PAUSED"));
    host.app_key("jobs", " ", ctx.clone()).await.unwrap();
    host.load("ansi-unicode", r#"
        jobs[1].output=string.rep('DROP',3000)..'\27[31m'..string.rep('界',100)..' café END-MARKER\27[0m\27]0;BAD-TITLE\7'
    "#).await.unwrap();
    let narrow = json!({"session":"one","rows":20,"cols":18});
    let lines = host.app_view("jobs", narrow.clone()).await.unwrap();
    let output = lines.iter().skip(3).cloned().collect::<Vec<_>>().join("");
    assert!(
        output.contains("END-MARKER") && output.contains("café"),
        "{lines:?}"
    );
    assert!(
        !output.contains('\u{1b}') && !output.contains("[31m") && !output.contains("BAD-TITLE")
    );
    host.load("tiny-output", "jobs[1].output='VISIBLE'")
        .await
        .unwrap();
    for rows in 1..=4 {
        let lines = host
            .app_view("jobs", json!({"session":"one","rows":rows,"cols":80}))
            .await
            .unwrap();
        assert!(
            lines.iter().take(rows).any(|line| line == "VISIBLE"),
            "{rows}: {lines:?}"
        );
    }
    host.load("tiny-list", "open_jobs('')").await.unwrap();
    let lines = host
        .app_view("jobs", json!({"session":"one","rows":1,"cols":80}))
        .await
        .unwrap();
    assert!(lines[0].contains("j1"), "{lines:?}");
    for (rows, cols) in [(0, 0), (1, 1), (2, 2), (4, 3), (10000, 10000)] {
        let lines = host
            .app_view("jobs", json!({"session":"one","rows":rows,"cols":cols}))
            .await
            .unwrap();
        assert!(lines.len() <= 512 && lines.iter().map(String::len).sum::<usize>() <= 65536);
    }
}

#[tokio::test]
async fn monitor_custom_options_keys_render_override_and_validation() {
    use rness_lua::runtime::AppKeyOutcome::{Close, Consumed, Pass};
    let host = monitor_host_config(
        r#"{
          title='My processes', refresh_ms=500, layout={height=12,width=70,style='dim'},
          keys={down='n',up=false,back={'b','esc'}},
          text={empty='Nothing running',marker='* '},
          render=function(ctx,m,lines)
            assert(m.session==ctx.session and m.mode=='list')
            lines[1]='Custom '..#m.jobs
            return lines
          end,
        }
    "#,
    )
    .await;
    host.load("mutate-shared-options", "rness.jobs.config.title='Changed'; rness.jobs.config.keys.back[1]='x'; rness.jobs.config.layout.height=50").await.unwrap();
    assert_eq!(host.app_specs().await[0].title, "My processes");
    let ctx = json!({"session":"one","rows":8,"cols":100});
    assert_eq!(host.app_key("jobs", "j", ctx.clone()).await.unwrap(), Pass);
    assert_eq!(
        host.app_key("jobs", "n", ctx.clone()).await.unwrap(),
        Consumed
    );
    assert_eq!(host.app_key("jobs", "k", ctx.clone()).await.unwrap(), Pass);
    let lines = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert_eq!(lines[0], "Custom 3");
    assert!(lines[2].starts_with("* j2"));
    assert_eq!(lines.len(), 12);
    let help = lines.iter().find(|line| line.contains(": select")).unwrap();
    assert!(
        help.contains("n: select") && !help.contains("k") && !help.contains("j/"),
        "{help}"
    );
    assert_eq!(host.app_key("jobs", "b", ctx.clone()).await.unwrap(), Close);
    let host = monitor_host_config(
        r#"{
      view=function(ctx) return {'Full '..ctx.session, string.rep('界',30000)} end,
      on_key=function(key,ctx) if key=='x' then return 'close' end end,
    }"#,
    )
    .await;
    let lines = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert_eq!(lines[0], "Full one");
    assert!(lines[1].len() <= 8192 && lines[1].chars().all(|c| c == '界'));
    assert_eq!(host.app_key("jobs", "x", ctx.clone()).await.unwrap(), Close);
    for config in [
        "{refresh_ms=0}",
        "{refresh_ms=1.5}",
        "{layout={height=0}}",
        "{title=3}",
        "{view='bad'}",
        "{keys={down=7}}",
        "{keys={unknown='x'}}",
        "{text={empty=false}}",
    ] {
        let invalid = LuaHost::spawn().unwrap();
        invalid
            .load("options", &format!("rness.jobs.config={config}"))
            .await
            .unwrap();
        assert!(invalid
            .load(
                "jobs",
                include_str!("../../../flavors/default/plugins/jobs.lua")
            )
            .await
            .is_err());
    }
    host.unload("jobs").await.unwrap();
}

#[tokio::test]
async fn monitor_list_paging_keeps_selected_identity_as_jobs_settle() {
    let host = monitor_host().await;
    host.load(
        "many",
        r#"
        jobs={}
        for i=1,20 do jobs[i]={job_id='j'..i,kind='bash',label='task '..i,
          status='running',running=true,output='output '..i} end
    "#,
    )
    .await
    .unwrap();
    let ctx = json!({"session":"one","rows":5,"cols":80});
    host.app_key("jobs", "pagedown", ctx.clone()).await.unwrap();
    let lines = host.app_view("jobs", ctx.clone()).await.unwrap();
    assert!(lines[3].starts_with("> j4 "), "{lines:?}");
    host.load("remove-earlier", "jobs[1].running=false")
        .await
        .unwrap();
    assert!(host.app_view("jobs", ctx.clone()).await.unwrap()[3].starts_with("> j4 "));
    host.app_key("jobs", "end", ctx.clone()).await.unwrap();
    assert!(host.app_view("jobs", ctx.clone()).await.unwrap()[3].starts_with("> j20 "));
    host.app_key("jobs", "enter", ctx.clone()).await.unwrap();
    assert!(host.app_view("jobs", ctx.clone()).await.unwrap()[0].starts_with("j20 "));
    host.load("evict", "table.remove(jobs,20)").await.unwrap();
    assert_eq!(
        host.app_view("jobs", ctx.clone()).await.unwrap()[0],
        "Job no longer available"
    );
    host.app_key("jobs", "esc", ctx.clone()).await.unwrap();
    host.app_key("jobs", "home", ctx.clone()).await.unwrap();
    assert!(host.app_view("jobs", ctx).await.unwrap()[1].starts_with("> j1 "));
}
