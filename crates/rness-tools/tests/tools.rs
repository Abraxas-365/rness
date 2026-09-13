//! Built-in tool tests: freshness invariant, unique-match edits,
//! windowed reads, ignore-aware glob/grep, bash execution + timeout.

use std::sync::Arc;

use rness_engine::tools::Tool;
use rness_tools::jobs::{JobKillTool, JobListTool, JobOutputTool, JobRegistry};
use rness_tools::{bash::BashTool, edit::EditTool, glob::GlobTool, grep::GrepTool, read::ReadTool, write::WriteTool, Workspace};
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn large_write_captures_changed_window_without_unchanged_file() {
    let dir = TempDir::new().unwrap();
    let workspace = ws(&dir);
    let prefix = "unchanged\n".repeat(10000);
    let before = format!("{prefix}old é\nlast\n");
    let after = format!("{prefix}new ê\nlast\n");
    std::fs::write(dir.path().join("large.txt"), before).unwrap();
    ReadTool::new(workspace.clone()).execute(json!({"path":"large.txt","limit":1})).await.unwrap();
    let (_, _, _, metadata) = WriteTool::new(workspace).execute_presented(
        &"s".into(), &"w".into(), json!({"path":"large.txt","content":after}),
        &tokio_util::sync::CancellationToken::new(),
    ).await.unwrap();
    let metadata = metadata.unwrap();
    assert_eq!(metadata["changes_complete"], true);
    assert_eq!(metadata["hunks"][0]["old_start"], 10001);
    assert_eq!(metadata["hunks"][0]["before"], "old é\n");
    assert_eq!(metadata["hunks"][0]["after"], "new ê\n");
    assert!(serde_json::to_vec(&metadata).unwrap().len() < 1024);
}

#[tokio::test]
async fn large_edit_merges_overlapping_line_windows() {
    let dir = TempDir::new().unwrap();
    let workspace = ws(&dir);
    std::fs::write(dir.path().join("large.txt"), format!("{}old old\n", "padding\n".repeat(10000))).unwrap();
    ReadTool::new(workspace.clone()).execute(json!({"path":"large.txt","limit":1})).await.unwrap();
    let (_, _, _, metadata) = EditTool::new(workspace).execute_presented(
        &"s".into(), &"e".into(), json!({"path":"large.txt","old_string":"old","new_string":"new","replace_all":true}),
        &tokio_util::sync::CancellationToken::new(),
    ).await.unwrap();
    let metadata = metadata.unwrap();
    assert_eq!(metadata["captured_replacements"], 2);
    assert_eq!(metadata["hunks"].as_array().unwrap().len(), 1);
    assert_eq!(metadata["hunks"][0]["before"], "old old\n");
    assert_eq!(metadata["hunks"][0]["after"], "new new\n");
}

#[tokio::test]
async fn large_write_captures_separated_changes() {
    let dir = TempDir::new().unwrap();
    let workspace = ws(&dir);
    let middle = "unchanged\n".repeat(10000);
    std::fs::write(dir.path().join("large.txt"), format!("old\n{middle}old\n")).unwrap();
    ReadTool::new(workspace.clone()).execute(json!({"path":"large.txt","limit":1})).await.unwrap();
    let (_, _, _, metadata) = WriteTool::new(workspace).execute_presented(
        &"s".into(), &"w".into(), json!({"path":"large.txt","content":format!("new\n{middle}new\n")}),
        &tokio_util::sync::CancellationToken::new(),
    ).await.unwrap();
    let metadata = metadata.unwrap();
    assert_eq!(metadata["changes_complete"], true);
    assert_eq!(metadata["hunks"].as_array().unwrap().len(), 2);
    assert!(serde_json::to_vec(&metadata).unwrap().len() < 2048);
}

#[tokio::test]
async fn large_write_reports_when_change_capture_is_incomplete() {
    let dir = TempDir::new().unwrap();
    let tool = WriteTool::new(ws(&dir));
    let content = "x".repeat(100_000);
    let (output, _, error, metadata) = tool.execute_presented(
        &"s".into(), &"w".into(), json!({"path":"new.txt","content":content}),
        &tokio_util::sync::CancellationToken::new(),
    ).await.unwrap();
    assert!(!error);
    assert!(!output.is_empty());
    let metadata = metadata.unwrap();
    assert_eq!(metadata["changes_complete"], false);
    assert_eq!(metadata["truncated"], true);
    assert!(metadata.get("hunks").is_none());
    assert_eq!(std::fs::read_to_string(dir.path().join("new.txt")).unwrap(), content);
}

fn ws(dir: &TempDir) -> Arc<Workspace> {
    Workspace::new(dir.path())
}

async fn exec(tool: &dyn Tool, args: serde_json::Value) -> Result<String, String> {
    tool.execute(args).await
}

#[tokio::test]
async fn job_metadata_matches_consumed_window_and_is_immutable() {
    let jobs = JobRegistry::new();
    let (id, writer) = jobs.start("test", "snapshot".into());
    writer.append(b"abc");
    let output = JobOutputTool::new(jobs.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
    let (_, _, _, first) = output.execute_presented(&"s".into(), &"c".into(), json!({"job_id":id}), &cancel).await.unwrap();
    let first = first.unwrap();
    writer.append(b"de");
    let (_, _, _, second) = output.execute_presented(&"s".into(), &"d".into(), json!({"job_id":id}), &cancel).await.unwrap();
    assert_eq!(first["start_byte"], 0);
    assert_eq!(first["end_byte"], 3);
    assert_eq!(second.unwrap()["start_byte"], 3);
    let list = JobListTool::new(jobs);
    let (_, _, _, metadata) = list.execute_presented(&"s".into(), &"l".into(), json!({}), &cancel).await.unwrap();
    writer.settle(rness_tools::jobs::JobStatus::Exited(Some(0)));
    assert_eq!(metadata.unwrap()["jobs"][0]["status"], "[status: running]");
}

#[tokio::test]
async fn read_and_bash_capture_execution_facts_without_changing_output() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\n").unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let read = ReadTool::new(ws(&dir));
    let (_, _, _, metadata) = read.execute_presented(&"s".into(), &"r".into(), json!({"path":"a.rs","offset":2,"limit":1}), &cancel).await.unwrap();
    let metadata = metadata.unwrap();
    assert_eq!(metadata["text"], "two\n");
    assert_eq!(metadata["start_line"], 2);
    assert_eq!(metadata["total_lines"], 3);
    assert_eq!(metadata["truncated"], true);
    let bash = BashTool::new(ws(&dir), JobRegistry::default());
    let (content, _, is_error, metadata) = bash.execute_presented(&"s".into(), &"b".into(), json!({"command":"printf out; printf err >&2; exit 7", "description":"Capture streams"}), &cancel).await.unwrap();
    assert!(!is_error);
    let metadata = metadata.unwrap();
    assert_eq!(metadata["exit_code"], 7);
    assert_eq!(metadata["stdout_bytes"], 3);
    assert_eq!(metadata["stderr_bytes"], 3);
    assert_eq!(content, vec![rness_protocol::events::ToolResultContentPart::Text { text:"out\nerr\n[exit code: 7]".into() }]);
}

#[tokio::test]
async fn large_edit_captures_bounded_fragments_with_shifted_offsets() {
    let dir = TempDir::new().unwrap();
    let workspace = ws(&dir);
    let text = format!("{}target\nbetween\ntarget\n", "padding\n".repeat(10000));
    std::fs::write(dir.path().join("large.txt"), text).unwrap();
    ReadTool::new(workspace.clone()).execute(json!({"path":"large.txt","limit":1})).await.unwrap();
    let (_, _, _, metadata) = EditTool::new(workspace).execute_presented(
        &"s".into(), &"e".into(),
        json!({"path":"large.txt","old_string":"target","new_string":"first\nsecond","replace_all":true}),
        &tokio_util::sync::CancellationToken::new(),
    ).await.unwrap();
    let metadata = metadata.unwrap();
    assert_eq!(metadata["captured_replacements"], 2);
    assert_eq!(metadata["hunks"][0]["old_start"], 10001);
    assert_eq!(metadata["hunks"][1]["old_start"], 10003);
    assert_eq!(metadata["hunks"][1]["new_start"], 10004);
    assert_eq!(metadata["hunks"][0]["fragment"], false);
    assert_eq!(metadata["hunks"][0]["before"], "target\n");
    assert_eq!(metadata["hunks"][0]["after"], "first\nsecond\n");
    assert!(serde_json::to_vec(&metadata).unwrap().len() < 50 * 1024);
}

#[tokio::test]
async fn large_edit_preserves_surrounding_text_and_eof_in_line_windows() {
    let dir = TempDir::new().unwrap();
    let workspace = ws(&dir);
    std::fs::write(dir.path().join("large.txt"), format!("{}prefix target suffix", "padding\n".repeat(10000))).unwrap();
    ReadTool::new(workspace.clone()).execute(json!({"path":"large.txt","limit":1})).await.unwrap();
    let (_, _, _, metadata) = EditTool::new(workspace).execute_presented(
        &"s".into(), &"e".into(), json!({"path":"large.txt","old_string":"target","new_string":"replacement"}),
        &tokio_util::sync::CancellationToken::new(),
    ).await.unwrap();
    let metadata = metadata.unwrap();
    assert_eq!(metadata["hunks"][0]["before"], "prefix target suffix");
    assert_eq!(metadata["hunks"][0]["after"], "prefix replacement suffix");
    assert_eq!(metadata["hunks"][0]["fragment"], false);
    assert_eq!(metadata["hunks"][0]["new_start"], 10001);
}

#[tokio::test]
async fn write_and_edit_capture_immutable_execution_snapshots() {
    let dir = TempDir::new().unwrap();
    let workspace = ws(&dir);
    let write = WriteTool::new(workspace.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
    let (_, _, _, metadata) = write.execute_presented(&"s".into(), &"w".into(), json!({"path":"a.rs","content":"old\n"}), &cancel).await.unwrap();
    let metadata = metadata.unwrap();
    assert_eq!(metadata["created"], true);
    assert_eq!(metadata["before"], "");
    assert_eq!(metadata["after"], "old\n");
    let edit = EditTool::new(workspace);
    let (_, _, _, metadata) = edit.execute_presented(&"s".into(), &"e".into(), json!({"path":"a.rs","old_string":"old","new_string":"new"}), &cancel).await.unwrap();
    let metadata = metadata.unwrap();
    std::fs::write(dir.path().join("a.rs"), "later").unwrap();
    assert_eq!(metadata["before"], "old\n");
    assert_eq!(metadata["after"], "new\n");
    assert_eq!(metadata["old_start"], 1);
}

#[tokio::test]
async fn session_workspaces_isolate_files_shell_skills_and_freshness() {
    let launch = TempDir::new().unwrap();
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    for (dir, marker) in [(&a, "alpha"), (&b, "beta")] {
        std::fs::write(dir.path().join("file.txt"), marker).unwrap();
        std::fs::create_dir_all(dir.path().join(".rness/skills")).unwrap();
        std::fs::write(dir.path().join(".rness/skills/review.md"), format!("---\nname: review\ndescription: {marker}\n---\n{marker}")).unwrap();
    }
    let registry = rness_engine::tools::ToolRegistry::default();
    rness_tools::register_all(&registry, ws(&launch));
    rness_tools::skills::register_skills(&registry, rness_tools::skills::default_roots(launch.path()));
    let ra = registry.for_workspace(&"a".into(), a.path());
    let rb = registry.for_workspace(&"b".into(), b.path());
    let (oa, ob) = tokio::join!(
        async { ra.get("Read").unwrap().execute(json!({"path":"file.txt"})).await.unwrap() },
        async { rb.get("Read").unwrap().execute(json!({"path":"file.txt"})).await.unwrap() }
    );
    assert!(oa.contains("alpha"));
    assert!(ob.contains("beta"));
    for (registry, marker) in [(&ra, "alpha"), (&rb, "beta")] {
        let shell = registry.get("Bash").unwrap().execute(json!({"command":"cat file.txt", "description":"Read marker"})).await.unwrap();
        assert!(shell.contains(marker));
        let skill = registry.get("skill").unwrap().execute(json!({"name":"review"})).await.unwrap();
        assert!(skill.contains(marker));
        assert!(registry.specs().iter().find(|s| s.name == "skill").unwrap().description.contains(marker));
    }
    let other = registry.for_workspace(&"other".into(), a.path());
    assert!(other.get("Write").unwrap().execute(json!({"path":"file.txt", "content":"blocked"})).await.is_err());
    let next_turn = registry.for_workspace(&"a".into(), a.path());
    next_turn.get("Write").unwrap().execute(json!({"path":"file.txt", "content":"updated"})).await.unwrap();
    assert_eq!(std::fs::read_to_string(b.path().join("file.txt")).unwrap(), "beta");
    assert!(!launch.path().join("file.txt").exists());
}

// -- Read ------------------------------------------------------------------

#[tokio::test]
async fn read_returns_numbered_lines_and_windows() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let read = ReadTool::new(ws(&dir));

    let out = exec(&read, json!({"path": "f.txt"})).await.unwrap();
    assert!(out.contains("1\talpha"));
    assert!(out.contains("3\tgamma"));

    let out = exec(&read, json!({"path": "f.txt", "offset": 2, "limit": 1})).await.unwrap();
    assert!(out.contains("2\tbeta"));
    assert!(!out.contains("alpha"));
    assert!(out.contains("1 more lines"));
}

#[tokio::test]
async fn read_missing_file_is_an_error() {
    let dir = TempDir::new().unwrap();
    let read = ReadTool::new(ws(&dir));
    assert!(exec(&read, json!({"path": "nope.txt"})).await.is_err());
}

#[tokio::test]
async fn read_rejects_non_positive_window_args() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    let read = ReadTool::new(ws(&dir));
    let err = exec(&read, json!({"path": "f.txt", "offset": 0})).await.unwrap_err();
    assert!(err.contains("'offset' must be a positive integer"), "{err}");
    let err = exec(&read, json!({"path": "f.txt", "limit": -1})).await.unwrap_err();
    assert!(err.contains("'limit' must be a positive integer"), "{err}");
}

#[tokio::test]
async fn read_footer_names_continuation_offset() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\n").unwrap();
    let read = ReadTool::new(ws(&dir));
    let out = exec(&read, json!({"path": "f.txt", "limit": 2})).await.unwrap();
    assert!(out.contains("file has 4 lines"), "{out}");
    assert!(out.contains("continue with offset=3"), "{out}");
}

// -- Write + freshness -----------------------------------------------------

#[tokio::test]
async fn write_creates_file_and_parents() {
    let dir = TempDir::new().unwrap();
    let write = WriteTool::new(ws(&dir));
    exec(&write, json!({"path": "a/b/new.txt", "content": "hi"})).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("a/b/new.txt")).unwrap(), "hi");
}

#[tokio::test]
async fn overwriting_unread_existing_file_is_refused() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "original").unwrap();
    let write = WriteTool::new(ws(&dir));
    let err = exec(&write, json!({"path": "f.txt", "content": "clobber"})).await.unwrap_err();
    assert!(err.contains("has not been read"));
    // Content untouched.
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "original");
}

#[tokio::test]
async fn read_then_write_succeeds_and_own_write_stays_fresh() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "original").unwrap();
    let workspace = ws(&dir);
    let read = ReadTool::new(workspace.clone());
    let write = WriteTool::new(workspace);

    exec(&read, json!({"path": "f.txt"})).await.unwrap();
    exec(&write, json!({"path": "f.txt", "content": "v2"})).await.unwrap();
    // Our own write marked it seen — a second write is still allowed.
    exec(&write, json!({"path": "f.txt", "content": "v3"})).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "v3");
}

// -- Edit ------------------------------------------------------------------

#[tokio::test]
async fn edit_requires_read_first() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "hello world").unwrap();
    let edit = EditTool::new(ws(&dir));
    let err = exec(&edit, json!({"path": "f.txt", "old_string": "world", "new_string": "rness"}))
        .await
        .unwrap_err();
    assert!(err.contains("has not been read"));
}

#[tokio::test]
async fn edit_replaces_unique_match() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "hello world").unwrap();
    let workspace = ws(&dir);
    let read = ReadTool::new(workspace.clone());
    let edit = EditTool::new(workspace);
    exec(&read, json!({"path": "f.txt"})).await.unwrap();
    exec(&edit, json!({"path": "f.txt", "old_string": "world", "new_string": "rness"}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "hello rness");
}

#[tokio::test]
async fn ambiguous_edit_is_refused_unless_replace_all() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "aa aa aa").unwrap();
    let workspace = ws(&dir);
    let read = ReadTool::new(workspace.clone());
    let edit = EditTool::new(workspace);
    exec(&read, json!({"path": "f.txt"})).await.unwrap();

    let err = exec(&edit, json!({"path": "f.txt", "old_string": "aa", "new_string": "b"}))
        .await
        .unwrap_err();
    assert!(err.contains("3 times"));

    exec(
        &edit,
        json!({"path": "f.txt", "old_string": "aa", "new_string": "b", "replace_all": true}),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "b b b");
}

#[tokio::test]
async fn edit_after_external_modification_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "hello").unwrap();
    let workspace = ws(&dir);
    let read = ReadTool::new(workspace.clone());
    let edit = EditTool::new(workspace);
    exec(&read, json!({"path": "f.txt"})).await.unwrap();

    // External change with a strictly newer mtime.
    std::fs::write(&path, "changed externally").unwrap();
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    let f = std::fs::File::options().append(true).open(&path).unwrap();
    f.set_modified(future).unwrap();

    let err = exec(&edit, json!({"path": "f.txt", "old_string": "changed", "new_string": "x"}))
        .await
        .unwrap_err();
    assert!(err.contains("changed on disk"));
}

#[tokio::test]
async fn same_mtime_different_length_is_still_stale() {
    // Version = mtime + length: a rewrite that lands on the SAME mtime
    // (coarse clocks) is caught by the length change.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, "hello").unwrap();
    let workspace = ws(&dir);
    let read = ReadTool::new(workspace.clone());
    let edit = EditTool::new(workspace);
    exec(&read, json!({"path": "f.txt"})).await.unwrap();

    let seen_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, "hello, but longer now").unwrap();
    let f = std::fs::File::options().append(true).open(&path).unwrap();
    f.set_modified(seen_mtime).unwrap(); // force identical mtime

    let err = exec(&edit, json!({"path": "f.txt", "old_string": "hello", "new_string": "x"}))
        .await
        .unwrap_err();
    assert!(err.contains("changed on disk"), "{err}");
}

#[tokio::test]
async fn edit_noop_is_rejected() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "hello").unwrap();
    let edit = EditTool::new(ws(&dir));
    let err = exec(&edit, json!({"path": "f.txt", "old_string": "hello", "new_string": "hello"}))
        .await
        .unwrap_err();
    assert!(err.contains("identical"), "{err}");
}

// -- Glob ------------------------------------------------------------------

#[tokio::test]
async fn glob_matches_and_respects_gitignore() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("target/junk.rs"), "ignored").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();

    let glob = GlobTool::new(ws(&dir));
    let out = exec(&glob, json!({"pattern": "**/*.rs"})).await.unwrap();
    assert!(out.contains("src/main.rs"), "{out}");
    assert!(!out.contains("junk.rs"), "{out}");
}

#[tokio::test]
async fn glob_no_matches_says_so() {
    let dir = TempDir::new().unwrap();
    let glob = GlobTool::new(ws(&dir));
    let out = exec(&glob, json!({"pattern": "**/*.zig"})).await.unwrap();
    assert!(out.contains("No files match"));
}

// -- Grep ------------------------------------------------------------------

#[tokio::test]
async fn grep_modes_and_glob_filter() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "fn gamma() {}\n").unwrap();

    let grep = GrepTool::new(ws(&dir));
    // files_with_matches default
    let out = exec(&grep, json!({"pattern": "fn \\w+"})).await.unwrap();
    assert!(out.contains("a.rs") && out.contains("b.txt"));

    // content mode with line numbers
    let out = exec(&grep, json!({"pattern": "beta", "output_mode": "content"})).await.unwrap();
    assert!(out.contains("a.rs:2:fn beta() {}"), "{out}");

    // count mode + glob filter
    let out =
        exec(&grep, json!({"pattern": "fn", "output_mode": "count", "glob": "*.rs"})).await.unwrap();
    assert!(out.contains("a.rs:2"), "{out}");
    assert!(!out.contains("b.txt"), "{out}");
}

#[tokio::test]
async fn grep_no_matches() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let grep = GrepTool::new(ws(&dir));
    let out = exec(&grep, json!({"pattern": "zzz_nothing"})).await.unwrap();
    assert!(out.contains("No matches"));
}

// -- Bash ------------------------------------------------------------------

fn bash(dir: &TempDir) -> BashTool {
    BashTool::new(ws(dir), JobRegistry::new())
}

#[tokio::test]
async fn bash_runs_in_workspace_and_combines_streams() {
    let dir = TempDir::new().unwrap();
    let bash = bash(&dir);
    let out = exec(&bash, json!({"command": "pwd && echo err >&2", "description": "print cwd"}))
        .await
        .unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    assert!(out.contains(&canonical.display().to_string()), "{out}");
    assert!(out.contains("err"));
    assert!(out.contains("[exit code: 0]"), "{out}");
}

#[tokio::test]
async fn bash_requires_description() {
    let dir = TempDir::new().unwrap();
    let bash = bash(&dir);
    let err = exec(&bash, json!({"command": "true"})).await.unwrap_err();
    assert!(err.contains("description"), "{err}");
}

#[tokio::test]
async fn bash_nonzero_exit_is_a_result_not_an_error() {
    let dir = TempDir::new().unwrap();
    let bash = bash(&dir);
    let out = exec(&bash, json!({"command": "echo oops; exit 3", "description": "fail on purpose"}))
        .await
        .unwrap();
    assert!(out.contains("[exit code: 3]"), "{out}");
    assert!(out.contains("oops"));
}

#[tokio::test]
async fn bash_timeout_kills_command() {
    let dir = TempDir::new().unwrap();
    let bash = bash(&dir);
    let err = exec(
        &bash,
        json!({"command": "sleep 5", "description": "sleep", "timeout_ms": 100}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("timed out"));
}

#[tokio::test]
async fn bash_input_prompt_receives_eof() {
    let dir = TempDir::new().unwrap();
    let out = exec(&bash(&dir), json!({
        "command": "printf 'Password: '; read -r password", "description": "Check closed input", "timeout_ms": 1000
    })).await.unwrap();
    assert!(out.contains("Password:"), "{out}");
    assert!(out.contains("[exit code: 1]"), "{out}");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_has_no_controlling_terminal() {
    let dir = TempDir::new().unwrap();
    let out = exec(&bash(&dir), json!({
        "command": "python3 -c 'import os; assert os.getsid(0) == os.getpgrp(); assert os.getsid(0) != os.getsid(os.getppid()); os.open(\"/dev/tty\", os.O_RDONLY)'",
        "description": "Check terminal isolation", "timeout_ms": 2000
    })).await.unwrap();
    assert!(out.contains("OSError"), "{out}");
    assert!(out.contains("[exit code: 1]"), "{out}");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_stops_descendants() {
    let dir = TempDir::new().unwrap();
    let err = exec(&bash(&dir), json!({
        "command": "(sleep 0.4; touch survived) & wait", "description": "Check descendant cleanup", "timeout_ms": 100
    })).await.unwrap_err();
    assert!(err.contains("timed out"), "{err}");
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(!dir.path().join("survived").exists());
}

// -- Background jobs -------------------------------------------------------

#[tokio::test]
async fn background_bash_requires_effective_job_controls_before_starting() {
    use rness_engine::tools::{ToolCall, ToolRegistry};
    let dir = TempDir::new().unwrap();
    let jobs = JobRegistry::new();
    let tools = ToolRegistry::default();
    tools.register(Arc::new(BashTool::new(ws(&dir), jobs.clone())));
    tools.register(Arc::new(JobOutputTool::new(jobs.clone())));
    tools.register(Arc::new(JobListTool::new(jobs.clone())));
    tools.register(Arc::new(JobKillTool::new(jobs.clone())));
    let session = "child".to_string();
    let call = ToolCall {
        call: "bash-call".into(), name: "Bash".into(),
        args: json!({"command":"touch started", "description":"Check background admission", "run_in_background":true}),
    };
    for missing in ["job_output", "job_list", "job_kill"] {
        // Model the inherited ceiling followed by the role's own allowlist.
        let ceiling = tools.restricted(&["Bash", "job_output", "job_list", "job_kill"]
            .map(str::to_owned));
        let allowed = ["Bash", "job_output", "job_list", "job_kill"]
            .into_iter().filter(|name| *name != missing).map(str::to_owned).collect::<Vec<_>>();
        let restricted = ceiling.restricted(&allowed);
        let results = restricted.dispatch(&session, std::slice::from_ref(&call), 1, &Default::default()).await;
        assert!(results[0].is_error);
        assert!(results[0].output.contains(missing), "{}", results[0].output);
        assert_eq!(results[0].presentation.as_ref().unwrap()["outcome"], "background_jobs_unavailable");
        assert!(jobs.list(&session).is_empty());
        assert!(!dir.path().join("started").exists());
    }

    let restricted = Arc::new(tools.restricted(&["Bash".into()]));
    // PTC must enforce the same admission check as native dispatch.
    let (_, nested) = rness_engine::tools::exposure::program(
        restricted.clone(), session.clone(), ToolCall {
            call: "ptc".into(), name: "run_code".into(),
            args: json!({"code":"return tools.call('Bash', {command='touch started', description='Check PTC admission', run_in_background=true})"}),
        }, Default::default(), None,
    ).await;
    assert_eq!(nested.len(), 1);
    assert!(nested[0].1.is_error);
    assert!(nested[0].1.output.contains("background jobs unavailable"));
    assert!(jobs.list(&session).is_empty());
    assert!(!dir.path().join("started").exists());

    for background in [None, Some(false)] {
        let mut foreground = call.clone();
        foreground.args["command"] = json!("echo foreground");
        if let Some(value) = background {
            foreground.args["run_in_background"] = json!(value);
        } else {
            foreground.args.as_object_mut().unwrap().remove("run_in_background");
        }
        let results = restricted.dispatch(&session, &[foreground], 1, &Default::default()).await;
        assert!(!results[0].is_error, "{}", results[0].output);
        assert!(results[0].output.contains("foreground"));
    }

    // Hidden-but-discoverable controls are still permitted, and a restricted
    // child must not change the original registry's ability to start jobs.
    tools.defer(["job_output", "job_list", "job_kill"].map(str::to_owned));
    let results = tools.dispatch(&session, &[call], 1, &Default::default()).await;
    assert!(!results[0].is_error, "{}", results[0].output);
    let id = results[0].presentation.as_ref().unwrap()["job_id"].as_str().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while jobs.count(&session) != 0 { tokio::task::yield_now().await; }
    }).await.unwrap();
    assert!(dir.path().join("started").exists());
    let output = tools.dispatch(&session, &[ToolCall {
        call: "output".into(), name: "job_output".into(), args: json!({"job_id":id}),
    }], 1, &Default::default()).await;
    assert!(!output[0].is_error);
    assert!(output[0].output.contains("[status: exited, code 0]"));
}

#[tokio::test]
async fn background_job_streams_output_and_settles() {
    let dir = TempDir::new().unwrap();
    let jobs = JobRegistry::new();
    let bash = BashTool::new(ws(&dir), jobs.clone());
    let output = JobOutputTool::new(jobs.clone());
    let list = JobListTool::new(jobs);

    let out = exec(
        &bash,
        json!({
            "command": "echo one; echo two",
            "description": "emit two lines",
            "run_in_background": true,
        }),
    )
    .await
    .unwrap();
    assert!(out.contains("started background job j1"), "{out}");

    // Wait for output + settlement.
    let out = exec(&output, json!({"job_id": "j1", "wait": true, "timeout_ms": 5000}))
        .await
        .unwrap();
    assert!(out.contains("one"), "{out}");

    // Drain until exited (settlement may lag the first output read).
    let mut status_line = String::new();
    for _ in 0..50 {
        let out = exec(&output, json!({"job_id": "j1", "wait": true, "timeout_ms": 200}))
            .await
            .unwrap();
        if out.contains("exited") {
            status_line = out;
            break;
        }
    }
    assert!(status_line.contains("[status: exited, code 0]"), "{status_line}");

    let out = exec(&list, json!({})).await.unwrap();
    assert!(out.contains("j1 [bash]"), "{out}");
}

#[tokio::test]
async fn job_output_is_incremental() {
    let dir = TempDir::new().unwrap();
    let jobs = JobRegistry::new();
    let bash = BashTool::new(ws(&dir), jobs.clone());
    let output = JobOutputTool::new(jobs);

    exec(
        &bash,
        json!({"command": "echo first", "description": "one line", "run_in_background": true}),
    )
    .await
    .unwrap();
    let first = exec(&output, json!({"job_id": "j1", "wait": true, "timeout_ms": 5000}))
        .await
        .unwrap();
    assert!(first.contains("first"), "{first}");

    // A second read returns only NEW output — none.
    let second = exec(&output, json!({"job_id": "j1"})).await.unwrap();
    assert!(second.contains("(no new output)"), "{second}");
    assert!(!second.contains("first"), "{second}");
}

#[tokio::test]
async fn job_kill_stops_a_running_job() {
    let dir = TempDir::new().unwrap();
    let jobs = JobRegistry::new();
    let bash = BashTool::new(ws(&dir), jobs.clone());
    let output = JobOutputTool::new(jobs.clone());
    let kill = JobKillTool::new(jobs);

    exec(
        &bash,
        json!({"command": "sleep 30", "description": "long sleep", "run_in_background": true}),
    )
    .await
    .unwrap();
    let out = exec(&kill, json!({"job_id": "j1"})).await.unwrap();
    assert!(out.contains("cancellation requested"), "{out}");

    let out = exec(&output, json!({"job_id": "j1", "wait": true, "timeout_ms": 5000}))
        .await
        .unwrap();
    assert!(out.contains("[status: killed]"), "{out}");
}

#[tokio::test]
async fn unknown_job_id_is_an_error() {
    let output = JobOutputTool::new(JobRegistry::new());
    let err = exec(&output, json!({"job_id": "j99"})).await.unwrap_err();
    assert!(err.contains("no job 'j99'"), "{err}");
}
