//! WP-1 E2E / adversarial tests for the session log, store, replay and
//! compaction (see ~/.rness/plans/rness/e2e-and-perf-test-plan.md, WP-1).
//!
//!   cargo test -p rness-engine --test session_store_e2e -- --nocapture
//!   # desired-behaviour tests for confirmed bugs (expected to FAIL today):
//!   cargo test -p rness-engine --test session_store_e2e -- --ignored --nocapture
//!
//! Tests prefixed `bug_` are `#[ignore]`d: they assert the behaviour we
//! want and fail on the current code (findings B1-n in e2e-findings/wp-1.md).
//! Everything else documents/pins current behaviour and must pass.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rness_engine::session::branch::{BranchError, SessionStore};
use rness_engine::session::log::{read_session, LogError, SessionLog};
use rness_engine::session::projection::{model_context, ModelTurn};
use rness_engine::session::replay::{replay, ReplayError};
use rness_engine::tools::{Tool, ToolRegistry};
use rness_engine::turn::compaction::{self, Policy};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_engine::turn::{run_turn, TurnConfig};
use rness_protocol::events::*;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- helpers

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: text.into() }],
        source: None,
    })
}

fn assistant(text: &str, calls: &[(&str, &str)]) -> AssistantMessage {
    let mut content = vec![ContentPart::Text { text: text.into() }];
    for (call, name) in calls {
        content.push(ContentPart::ToolUse {
            call: (*call).into(),
            name: (*name).into(),
            args: serde_json::json!({}),
        });
    }
    AssistantMessage {
        model: "fake-1".into(),
        content,
        stop: if calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        },
        // Zero usage: no calibration ratio (a tiny fake usage would clamp
        // the calibrated pressure to 0.25x and starve compaction).
        usage: Usage::default(),
        estimated_input: 0,
        chunks: vec![],
    }
}

fn result(call: &str, output: String) -> SessionEvent {
    SessionEvent::ToolResult(ToolResult {
        call: call.into(),
        name: "Big".into(),
        content: vec![],
        output,
        is_error: false,
        duration_ms: 1,
        tasks: None,
        plan_review: None,
        presentation: None,
    })
}

fn log_path(root: &Path, sid: &str) -> PathBuf {
    root.join(sid).join("session.v1.jsonl")
}

fn append_raw(path: &Path, bytes: &[u8]) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// A small valid session: header + one full tool turn.
fn seeded(root: &Path) -> (SessionStore, String) {
    let store = SessionStore::new(root);
    let mut log = store.create(Some("/w".into())).unwrap();
    log.append(&SessionEvent::TurnStarted { turn: 1 }).unwrap();
    log.append(&user("hello")).unwrap();
    log.append(&SessionEvent::AssistantMessage(assistant(
        "t",
        &[("c1", "Big")],
    )))
    .unwrap();
    log.append(&result("c1", "out".into())).unwrap();
    log.append(&SessionEvent::AssistantMessage(assistant("done", &[])))
        .unwrap();
    log.append(&SessionEvent::TurnEnded {
        turn: 1,
        outcome: TurnOutcome::Completed,
    })
    .unwrap();
    let sid = log.session().clone();
    (store, sid)
}

fn describe<T>(r: &Result<T, impl std::fmt::Display>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => format!("err: {e}"),
    }
}

fn is_corrupt_replay(r: &Result<rness_engine::session::replay::Replayed, ReplayError>) -> bool {
    matches!(
        r,
        Err(ReplayError::Branch(BranchError::Log(
            LogError::Corrupt { .. }
        )))
    )
}

/// Check tool_use / tool_result pairing in a derived context.
fn pairs_intact(turns: &[ModelTurn]) -> Result<(), String> {
    for (i, t) in turns.iter().enumerate() {
        if let ModelTurn::Assistant { content } = t {
            let calls: HashSet<&str> = content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolUse { call, .. } => Some(call.as_str()),
                    _ => None,
                })
                .collect();
            if calls.is_empty() {
                continue;
            }
            match turns.get(i + 1) {
                Some(ModelTurn::ToolResults { results }) => {
                    let got: HashSet<&str> = results.iter().map(|r| r.call.as_str()).collect();
                    if got != calls {
                        return Err(format!("turn {i}: calls {calls:?} results {got:?}"));
                    }
                }
                other => return Err(format!("turn {i}: calls {calls:?} followed by {other:?}")),
            }
        }
        if let ModelTurn::ToolResults { .. } = t {
            if !matches!(
                turns.get(i.wrapping_sub(1)),
                Some(ModelTurn::Assistant { .. })
            ) {
                return Err(format!("turn {i}: orphan tool results"));
            }
        }
    }
    Ok(())
}

// ------------------------------------------------- 3. corruption matrix

struct Outcome {
    case: &'static str,
    open: String,
    replay: String,
    read_session: String,
    listed: bool,
    append_then_replay: String,
}

fn probe(case: &'static str, mutate: impl FnOnce(&Path)) -> Outcome {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    drop(store);
    let path = log_path(dir.path(), &sid);
    mutate(&path);
    let store = SessionStore::new(dir.path());
    let listed = store.list().unwrap().contains(&sid);
    let read = read_session(dir.path(), &sid);
    let rep = replay(&store, &sid);
    let opened = store.open(&sid);
    let open = describe(&opened);
    let append_then_replay = match opened {
        Ok(mut log) => {
            let a = log.append(&user("after"));
            drop(log);
            let store2 = SessionStore::new(dir.path());
            format!(
                "append {} / replay {}",
                describe(&a),
                describe(&replay(&store2, &sid))
            )
        }
        Err(_) => "-".into(),
    };
    Outcome {
        case,
        open,
        replay: describe(&rep),
        read_session: describe(&read),
        listed,
        append_then_replay,
    }
}

#[test]
fn corruption_matrix_reports_current_behaviour() {
    let header_line = |p: &Path| -> String {
        fs::read_to_string(p)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string()
    };
    let rows = vec![
        probe("trailing complete invalid JSON line", |p| {
            append_raw(p, b"{\"id\":\"zz\",\"at\":\"x\",\"type\":\"user/mess\n")
        }),
        probe("trailing valid JSON, unknown event type", |p| {
            append_raw(
                p,
                b"{\"id\":\"zz\",\"at\":\"2026-01-01T00:00:00.000Z\",\"type\":\"future/event\"}\n",
            )
        }),
        probe("invalid UTF-8 inside a committed line", |p| {
            let text = fs::read(p).unwrap();
            let mut lines: Vec<Vec<u8>> = text.split(|b| *b == b'\n').map(|l| l.to_vec()).collect();
            // corrupt the user message line ("hello" -> "he\xFFlo")
            for l in lines.iter_mut() {
                if let Some(pos) = l.windows(5).position(|w| w == b"hello") {
                    l[pos + 2] = 0xFF;
                }
            }
            fs::write(p, lines.join(&b'\n')).unwrap();
        }),
        probe("invalid UTF-8 in torn tail", |p| {
            append_raw(p, b"{\"id\":\"zz\",\"at\":\"\xFF\xFE")
        }),
        probe("NUL bytes as torn tail (zero-filled extent)", |p| {
            append_raw(p, &[0u8; 4096])
        }),
        probe("NUL bytes then newline (zero-filled + later line)", |p| {
            let mut b = vec![0u8; 512];
            b.push(b'\n');
            append_raw(p, &b)
        }),
        probe("CRLF line endings", |p| {
            let text = fs::read_to_string(p).unwrap();
            fs::write(p, text.replace('\n', "\r\n")).unwrap();
        }),
        probe("blank lines between events", |p| {
            let text = fs::read_to_string(p).unwrap();
            fs::write(p, text.replace('\n', "\n\n   \n")).unwrap();
        }),
        probe("0-byte file", |p| fs::write(p, b"").unwrap()),
        probe("torn header (crash during create)", |p| {
            let h = header_line(p);
            fs::write(p, &h.as_bytes()[..h.len() / 2]).unwrap();
        }),
        probe("header missing (first line removed)", |p| {
            let text = fs::read_to_string(p).unwrap();
            let rest: Vec<&str> = text.lines().skip(1).collect();
            fs::write(p, rest.join("\n") + "\n").unwrap();
        }),
        probe("header version FORMAT_VERSION+1", |p| {
            let text = fs::read_to_string(p).unwrap();
            fs::write(p, text.replacen("\"version\":1", "\"version\":2", 1)).unwrap();
        }),
        probe("header version FORMAT_VERSION-1 (0)", |p| {
            let text = fs::read_to_string(p).unwrap();
            fs::write(p, text.replacen("\"version\":1", "\"version\":0", 1)).unwrap();
        }),
        probe("duplicate event id (last line repeated)", |p| {
            let text = fs::read_to_string(p).unwrap();
            let last = text.lines().last().unwrap().to_string();
            append_raw(p, format!("{last}\n").as_bytes());
        }),
        probe("event ids out of ULID order", |p| {
            let text = fs::read_to_string(p).unwrap();
            let mut lines: Vec<String> = text.lines().map(String::from).collect();
            // swap ids of lines 2 and 3 (user + assistant) — same ms, reversed order
            let id = |l: &str| l[7..33].to_string();
            let (a, b) = (id(&lines[2]), id(&lines[3]));
            lines[2] = lines[2].replacen(&a, &b, 1);
            lines[3] = lines[3].replacen(&b, &a, 1);
            fs::write(p, lines.join("\n") + "\n").unwrap();
        }),
        probe("header session id mismatch (copied dir)", |p| {
            let text = fs::read_to_string(p).unwrap();
            let sid = p.parent().unwrap().file_name().unwrap().to_str().unwrap();
            fs::write(p, text.replacen(sid, "01OTHERSESSIONID0000000000", 1)).unwrap();
        }),
    ];
    println!("\n| case | open | replay | read_session | listed | append→replay |");
    println!("|---|---|---|---|---|---|");
    for r in &rows {
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            r.case, r.open, r.replay, r.read_session, r.listed, r.append_then_replay
        );
    }
    let get = |c: &str| rows.iter().find(|r| r.case == c).unwrap();
    // Hard invariants the code claims today.
    assert_eq!(get("invalid UTF-8 in torn tail").replay, "ok");
    assert_eq!(
        get("NUL bytes as torn tail (zero-filled extent)").replay,
        "ok"
    );
    assert_eq!(get("CRLF line endings").replay, "ok");
    assert_eq!(get("blank lines between events").replay, "ok");
    assert_eq!(get("event ids out of ULID order").replay, "ok");
    let newer = get("header version FORMAT_VERSION+1");
    assert!(newer.replay.contains("format v2") && newer.replay.contains("upgrade"));
    assert!(
        newer.open.contains("format v2"),
        "open refuses a newer format"
    );
    // A complete invalid trailing line: lockless readers report it, the
    // writer quarantines it on open and later appends are reachable (B1-1).
    let tail = get("trailing complete invalid JSON line");
    assert!(tail.replay.contains("corrupt"));
    assert_eq!(tail.append_then_replay, "append ok / replay ok");
    let nul = get("NUL bytes then newline (zero-filled + later line)");
    assert_eq!(nul.append_then_replay, "append ok / replay ok");
    // Unknown (newer) event type: readable data (B1-2).
    let unknown = get("trailing valid JSON, unknown event type");
    assert_eq!(unknown.replay, "ok");
    assert_eq!(unknown.read_session, "ok");
    // Headerless / empty files: the writer refuses instead of appending
    // headerless events (B1-5).
    for case in [
        "0-byte file",
        "torn header (crash during create)",
        "header missing (first line removed)",
    ] {
        assert!(get(case).open.starts_with("err: corrupt"), "{case}");
    }
}

/// The open/heal path must never touch committed bytes (only the torn tail).
#[test]
fn heal_only_removes_unterminated_tail() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    let path = log_path(dir.path(), &sid);
    let committed = fs::read(&path).unwrap();
    append_raw(&path, b"{\"id\":\"torn\",\"at\":");
    let log = store.open(&sid).unwrap();
    assert_eq!(fs::read(&path).unwrap(), committed);
    drop(log);
    // Corrupt-but-terminated tail: open moves it to a sidecar (never drops
    // it) and records a repair event; the committed prefix is untouched.
    append_raw(&path, b"garbage\n");
    let log = store.open(&sid).unwrap();
    drop(log);
    let after = fs::read(&path).unwrap();
    assert!(after.starts_with(&committed));
    let events = read_session(dir.path(), &sid).unwrap();
    let SessionEvent::Repair(repair) = &events.last().unwrap().event else {
        panic!("expected session/repair, got {:?}", events.last());
    };
    assert_eq!((repair.bytes, repair.lines), (8, 1));
    let sidecar = path.parent().unwrap().join(&repair.sidecar);
    assert_eq!(fs::read(sidecar).unwrap(), b"garbage\n");
    // A terminated line that LOOKS like an event (string id + type) but does
    // not parse may be a real event with a schema problem: never moved.
    append_raw(
        &path,
        b"{\"id\":\"x\",\"at\":\"t\",\"type\":\"user/message\"}\n",
    );
    let before = fs::read(&path).unwrap();
    let _ = store.open(&sid);
    assert_eq!(fs::read(&path).unwrap(), before);
}

/// B1: a complete-but-invalid trailing line (e.g. a crash on a filesystem
/// that persisted the newline but not the full payload, or a stray editor
/// write) bricks the session forever: every replay fails, the writer still
/// opens and appends AFTER the bad line, so nothing ever recovers.
#[test]
fn bug_invalid_trailing_line_is_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    drop(store);
    append_raw(
        &log_path(dir.path(), &sid),
        b"{\"id\":\"zz\",\"at\":\"x\",\"type\":\"user/mess\n",
    );
    let store = SessionStore::new(dir.path());
    let mut log = store.open(&sid).unwrap();
    log.append(&user("continue")).unwrap();
    assert!(replay(&store, &sid).is_ok(), "session must stay usable");
}

/// A corrupt trailing line is quarantined on open (sidecar bytes == the bad
/// line), so events appended afterwards stay reachable (B1-1).
#[test]
fn open_succeeds_and_appends_after_corrupt_line() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    append_raw(&log_path(dir.path(), &sid), b"not json\n");
    let mut log = store.open(&sid).unwrap();
    assert!(log.append(&user("kept")).is_ok());
    drop(log);
    let fresh = SessionStore::new(dir.path());
    let replayed = replay(&fresh, &sid).unwrap();
    let rendered = serde_json::to_string(&replayed.context.turns).unwrap();
    assert!(rendered.contains("kept"));
    let repair = replayed
        .history
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::Repair(r) => Some(r.clone()),
            _ => None,
        })
        .expect("repair audit event");
    let sidecar = log_path(dir.path(), &sid)
        .parent()
        .unwrap()
        .join(&repair.sidecar);
    assert_eq!(fs::read(sidecar).unwrap(), b"not json\n");
    // The sidecar is not a session.
    assert_eq!(fresh.list().unwrap(), vec![sid]);
}

/// Mid-file corruption (valid lines after the bad one, e.g. a session
/// bricked by an older rness that appended past a bad line) is never
/// repaired automatically: open leaves the file alone, replay says where.
#[test]
fn mid_file_corruption_is_reported_with_offset() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    drop(store);
    let path = log_path(dir.path(), &sid);
    let text = fs::read_to_string(&path).unwrap();
    let last = text.lines().last().unwrap().to_string();
    let bad_at = fs::metadata(&path).unwrap().len();
    append_raw(&path, b"\x00\x00garbage\n");
    append_raw(&path, format!("{last}\n").as_bytes());
    let before = fs::read(&path).unwrap();
    let store = SessionStore::new(dir.path());
    let log = store.open(&sid).unwrap();
    drop(log);
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "open never touches mid-file damage"
    );
    let message = replay(&store, &sid).err().unwrap().to_string();
    assert!(message.contains("corrupt log at line 8"), "{message}");
    assert!(
        message.contains(&format!("byte offset {bad_at}")),
        "{message}"
    );
}

/// Invalid UTF-8 inside a committed line: every reader reports the same
/// `Corrupt{line}` (B1-3; they used to differ: io error without a line).
#[test]
fn invalid_utf8_error_shape_differs_between_readers() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    drop(store);
    let path = log_path(dir.path(), &sid);
    let mut bytes = fs::read(&path).unwrap();
    let pos = bytes.windows(5).position(|w| w == b"hello").unwrap();
    bytes[pos + 2] = 0xFF;
    fs::write(&path, bytes).unwrap();
    let store = SessionStore::new(dir.path());
    let cached = replay(&store, &sid);
    let raw = read_session(dir.path(), &sid);
    let mut log = store.open(&sid).unwrap();
    let recovered = compaction::recover(&mut log);
    println!("cached: {}", describe(&cached));
    println!("raw read_session: {}", describe(&raw));
    println!("recover(): {}", describe(&recovered));
    assert!(is_corrupt_replay(&cached));
    assert!(matches!(raw, Err(LogError::Corrupt { line: 3, .. })));
    assert!(matches!(recovered, Err(LogError::Corrupt { line: 3, .. })));
}

/// Version mismatch: reported as "written by a newer rness, upgrade" (not as
/// a corrupt log), and the writer refuses to append v1 events into it (B1-6).
#[test]
fn newer_format_version_message() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    drop(store);
    let p = log_path(dir.path(), &sid);
    let text = fs::read_to_string(&p).unwrap();
    fs::write(&p, text.replacen("\"version\":1", "\"version\":2", 1)).unwrap();
    let store = SessionStore::new(dir.path());
    let msg = describe(&replay(&store, &sid));
    println!("v2 replay: {msg}");
    // list() does not validate the version: the session shows up in the
    // picker and only fails on resume.
    assert!(store.list().unwrap().contains(&sid));
    assert!(
        msg.contains("format v2") && msg.contains("upgrade rness"),
        "{msg}"
    );
    assert!(!msg.contains("corrupt"), "{msg}");
    let before = fs::read(&p).unwrap();
    let opened = store.open(&sid);
    println!("v2 open for append: {}", describe(&opened));
    assert!(matches!(
        opened,
        Err(BranchError::Log(LogError::UnsupportedVersion {
            found: 2,
            supported: 1
        }))
    ));
    assert_eq!(fs::read(&p).unwrap(), before);
}

/// An event of a type this build does not know (newer rness) is data:
/// read/list/transcript work, its bytes survive untouched (also through a
/// fork), and a model turn is refused before anything is logged (B1-2).
#[tokio::test]
async fn unknown_event_type_is_readable_and_refuses_turns() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    drop(store);
    let path = log_path(dir.path(), &sid);
    let future = b"{\"id\":\"01FUTURE000000000000000000\",\"at\":\"2026-01-01T00:00:00.000Z\",\"type\":\"future/event\",\"payload\":{\"x\":[1,2]}}\n";
    // In the middle: one more valid line after it.
    append_raw(&path, future);
    let store = SessionStore::new(dir.path());
    let mut log = store.open(&sid).unwrap();
    log.append(&user("after future")).unwrap();
    let on_disk = fs::read(&path).unwrap();
    assert!(on_disk.windows(future.len()).any(|w| w == future));

    let replayed = replay(&store, &sid).unwrap();
    assert_eq!(replayed.unknown, vec!["future/event".to_string()]);
    let unknown = replayed
        .history
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::Unknown(u) => Some(u.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(unknown.raw["payload"]["x"][1], 2);
    assert!(store.list().unwrap().contains(&sid));
    let _ = rness_engine::session::projection::transcript(&replayed.history);
    // Serializing the envelope reproduces the line (fields reordered at most).
    let env = replayed
        .history
        .iter()
        .find(|e| e.id == "01FUTURE000000000000000000")
        .unwrap();
    let back: serde_json::Value = serde_json::to_value(&**env).unwrap();
    let orig: serde_json::Value = serde_json::from_slice(&future[..future.len() - 1]).unwrap();
    assert_eq!(back, orig);

    // A fork reads the parent's bytes through, never rewrites them.
    let tip = replayed.history.last().unwrap().id.clone();
    let child = store.fork(&sid, Some(tip)).unwrap();
    let child_id = child.session().clone();
    drop(child);
    assert_eq!(
        replay(&store, &child_id).unwrap().unknown,
        vec!["future/event".to_string()]
    );
    assert_eq!(fs::read(&path).unwrap(), on_disk);

    // The turn path refuses: no turn/started, no request.
    let tools = ToolRegistry::default();
    let before = fs::read(&path).unwrap();
    let err = run_turn(
        &store,
        &mut log,
        &Refuser,
        &tools,
        &TurnConfig::default(),
        &CancellationToken::new(),
        &mut Vec::new,
        2,
        &|_| {},
        None,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("newer rness (future/event)"),
        "{err}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

struct Refuser;
#[async_trait]
impl Provider for Refuser {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, _: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        panic!("no model request may be built for a session with unknown events");
    }
}

/// 0-byte, torn-header and headerless files: open refuses and leaves the
/// file byte-for-byte unchanged (no headerless appends, B1-5).
#[test]
fn headerless_open_refuses_and_leaves_file_unchanged() {
    for contents in [
        &b""[..],
        b"{\"id\":\"01A\",\"at\":\"t\",\"type\":\"session/hea",
        b"{\"id\":\"01A\",\"at\":\"t\",\"type\":\"turn/started\",\"turn\":1}\n",
        b"\x00\x00\x00\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (store, sid) = seeded(dir.path());
        let path = log_path(dir.path(), &sid);
        fs::write(&path, contents).unwrap();
        let opened = store.open(&sid);
        assert!(
            matches!(opened, Err(BranchError::Log(LogError::Corrupt { .. }))),
            "{contents:?}: {}",
            describe(&opened)
        );
        assert_eq!(fs::read(&path).unwrap(), contents);
    }
}

/// Duplicate event ids: a checkpoint replacing a duplicated id anchors
/// its summary at EVERY occurrence (anchors are keyed by id).
#[test]
fn duplicate_ids_under_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    let a = log.append(&user("first")).unwrap();
    log.append(&user("second")).unwrap();
    drop(log);
    // Re-append the first line verbatim (duplicate id), then a checkpoint over it.
    let p = log_path(dir.path(), &sid);
    let line = fs::read_to_string(&p)
        .unwrap()
        .lines()
        .nth(1)
        .unwrap()
        .to_string();
    append_raw(&p, format!("{line}\n").as_bytes());
    let mut log = store.open(&sid).unwrap();
    log.append(&SessionEvent::Compaction(Compaction {
        replaces: vec![a.id.clone()],
        summary: "SUM".into(),
        model: "m".into(),
    }))
    .unwrap();
    let ctx = replay(&store, &sid).unwrap().context;
    let rendered = serde_json::to_string(&ctx.turns).unwrap();
    let summaries = rendered.matches("SUM").count();
    println!(
        "duplicate-id checkpoint: {} turns, summary replayed {summaries}x",
        ctx.turns.len()
    );
    assert!(summaries >= 1);
}

// --------------------------------------------------------- 1. writer lock

#[test]
fn second_writer_is_locked_and_released_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    let holder = store.open(&sid).unwrap();
    // Same process, second open file description: flock conflicts.
    match store.open(&sid) {
        Err(BranchError::Log(LogError::Locked(s))) => assert_eq!(s, sid),
        other => panic!("expected Locked, got {}", describe(&other)),
    }
    let other_store = SessionStore::new(dir.path());
    assert!(matches!(
        other_store.open(&sid),
        Err(BranchError::Log(LogError::Locked(_)))
    ));
    // Readers and forks are not blocked by the writer.
    assert!(replay(&other_store, &sid).is_ok());
    let child = other_store.fork(&sid, None).unwrap();
    drop(child);
    drop(holder);
    assert!(store.open(&sid).is_ok());
}

/// A torn tail present while another writer holds the lock: the second
/// open fails BEFORE healing (lock first), so it never truncates under a
/// live writer.
#[test]
fn locked_open_never_heals_under_live_writer() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    let holder = store.open(&sid).unwrap();
    let path = log_path(dir.path(), &sid);
    append_raw(&path, b"{\"partial\":");
    let size = fs::metadata(&path).unwrap().len();
    assert!(store.open(&sid).is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), size);
    drop(holder);
}

// --------------------------------------------------------- 5. fork chains

fn build_chain(store: &SessionStore, depth: usize, per_hop: usize) -> Vec<String> {
    let mut log = store.create(Some("/w".into())).unwrap();
    let mut ids = vec![log.session().clone()];
    for i in 0..per_hop {
        log.append(&user(&format!("hop0-msg{i}"))).unwrap();
    }
    drop(log);
    for hop in 1..=depth {
        let mut child = store.fork(ids.last().unwrap(), None).unwrap();
        for i in 0..per_hop {
            child.append(&user(&format!("hop{hop}-msg{i}"))).unwrap();
        }
        ids.push(child.session().clone());
    }
    ids
}

#[test]
fn deep_fork_chains_replay_in_order() {
    for depth in [10usize, 50] {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let ids = build_chain(&store, depth, 3);
        let leaf = ids.last().unwrap();
        let replayed = replay(&store, leaf).unwrap();
        let texts: Vec<String> = replayed
            .context
            .turns
            .iter()
            .map(|t| match t {
                ModelTurn::User { content } => match &content[0] {
                    ContentPart::Text { text } => text.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            })
            .collect();
        let expected: Vec<String> = (0..=depth)
            .flat_map(|h| (0..3).map(move |i| format!("hop{h}-msg{i}")))
            .collect();
        assert_eq!(texts, expected, "depth {depth}");
        // Only the leaf's header leads the history.
        let headers = replayed
            .history
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::Header(_)))
            .count();
        assert_eq!(headers, 1);
        // Resume the leaf through a fresh store (cold cache) and continue.
        let fresh = SessionStore::new(dir.path());
        let mut log = fresh.open(leaf).unwrap();
        log.append(&user("leaf-continue")).unwrap();
        assert_eq!(
            replay(&fresh, leaf).unwrap().context.turns.len(),
            expected.len() + 1
        );
        assert_eq!(fresh.ancestry(leaf).unwrap().len(), depth + 1);
    }
}

/// Fork a fork at an id that is NOT in its visible history (unknown, or an
/// ancestor event past the point the fork branched off) stays
/// ForkPointNotFound.
#[test]
fn fork_at_unknown_or_invisible_event_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut root = store.create(None).unwrap();
    root.append(&user("root message")).unwrap();
    let root_id = root.session().clone();
    let child = store.fork(&root_id, None).unwrap().session().clone();
    // Appended to the root AFTER the fork: not visible in the child.
    let later = root.append(&user("after the fork")).unwrap();
    drop(root);
    for at in [later.id, "01NOPE".to_string()] {
        let r = store.fork(&child, Some(at));
        println!("fork child at invisible event: {}", describe(&r));
        assert!(matches!(r, Err(BranchError::ForkPointNotFound { .. })));
    }
}

/// B1-4: forking a fork at an event inherited from its parent (an id the
/// user sees in the leaf's transcript) forks from the owning ancestor.
#[test]
fn bug_fork_at_inherited_event_works() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut root = store.create(None).unwrap();
    let inherited = root.append(&user("root message")).unwrap();
    let root_id = root.session().clone();
    drop(root);
    let child = store.fork(&root_id, None).unwrap().session().clone();
    let grand = store.fork(&child, Some(inherited.id.clone())).unwrap();
    let ctx = replay(&store, grand.session()).unwrap().context;
    assert_eq!(ctx.turns.len(), 1);
    // Same visible prefix as forking the owner directly: parent = ancestor.
    assert_eq!(
        store.parent(grand.session()).unwrap().unwrap().session,
        root_id
    );
}

/// B1-4, deeper chain: a grandchild forked at a mid-root event sees exactly
/// the root prefix up to that event, not the child's or root's later events.
#[test]
fn fork_at_inherited_event_two_levels_keeps_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut root = store.create(None).unwrap();
    let a = root.append(&user("a")).unwrap();
    root.append(&user("b")).unwrap();
    let root_id = root.session().clone();
    drop(root);
    let mut child = store.fork(&root_id, None).unwrap();
    child.append(&user("c")).unwrap();
    let child_id = child.session().clone();
    drop(child);
    let mid = store.fork(&child_id, None).unwrap().session().clone();
    let leaf = store.fork(&mid, Some(a.id.clone())).unwrap();
    let ctx = replay(&store, leaf.session()).unwrap().context;
    assert_eq!(ctx.turns.len(), 1);
    let ids: Vec<_> = store
        .history(leaf.session())
        .unwrap()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    assert_eq!(ids.last(), Some(&a.id));
}

/// Fork at an assistant tool_use whose result is not in the prefix: the
/// child's projection must drop the dangling call (repair_tool_pairs) and
/// stay usable.
#[test]
fn fork_between_tool_use_and_result_is_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    log.append(&user("go")).unwrap();
    let call = log
        .append(&SessionEvent::AssistantMessage(assistant(
            "calling",
            &[("c1", "Big")],
        )))
        .unwrap();
    log.append(&result("c1", "r".into())).unwrap();
    let sid = log.session().clone();
    drop(log);
    let child = store.fork(&sid, Some(call.id)).unwrap();
    let ctx = replay(&store, child.session()).unwrap().context;
    pairs_intact(&ctx.turns).unwrap();
    assert_eq!(ctx.turns.len(), 2); // user + assistant text only
}

/// A deleted ancestor makes every descendant unreadable (NotFound).
#[test]
fn deleted_ancestor_breaks_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let ids = build_chain(&store, 2, 1);
    fs::remove_dir_all(dir.path().join(&ids[0])).unwrap();
    let fresh = SessionStore::new(dir.path());
    let r = replay(&fresh, &ids[2]);
    println!("leaf after root deleted: {}", describe(&r));
    assert!(r.is_err());
}

// ------------------------------------------------- 4. compaction stacking

const SUMMARY_SYSTEM: &str = "WP1 summarizer";

/// Main steps: odd step of a turn calls `Big`, even step ends the turn.
/// Summary steps return a numbered summary. Every main request is checked
/// against an independent read of the log: no shadowed marker may leak and
/// tool pairs must be intact.
struct Stacker {
    root: PathBuf,
    sid: Mutex<Option<String>>,
    summaries: Mutex<usize>,
    main_calls: Mutex<usize>,
    violations: Mutex<Vec<String>>,
    max_context_turns: Mutex<usize>,
}

#[async_trait]
impl Provider for Stacker {
    fn model(&self) -> &str {
        "fake-1"
    }
    async fn step(&self, request: StepRequest<'_>, _: &CancellationToken) -> StepOutcome {
        if request.system == SUMMARY_SYSTEM {
            let mut n = self.summaries.lock().unwrap();
            *n += 1;
            return StepOutcome::Committed(assistant(&format!("SUMMARY-{}", *n), &[]));
        }
        let mut calls = self.main_calls.lock().unwrap();
        *calls += 1;
        let ctx = request.context;
        {
            let mut m = self.max_context_turns.lock().unwrap();
            *m = (*m).max(ctx.turns.len());
        }
        if let Err(e) = pairs_intact(&ctx.turns) {
            self.violations.lock().unwrap().push(format!("pairs: {e}"));
        }
        // Independent check: which markers are shadowed per the log now?
        if let Some(sid) = self.sid.lock().unwrap().clone() {
            let history = read_session(&self.root, &sid).unwrap();
            let shadows = shadowed_by_checkpoints(&history);
            let shadowed: HashSet<&str> = shadows
                .iter()
                .flat_map(|s| s.shadowed.iter().map(String::as_str))
                .collect();
            let rendered = serde_json::to_string(&ctx.turns).unwrap();
            for env in &history {
                if !shadowed.contains(env.id.as_str()) {
                    continue;
                }
                let marker = match &env.event {
                    SessionEvent::ToolResult(r) => r.output[..17].to_string(),
                    SessionEvent::UserMessage(m) => match &m.content[0] {
                        ContentPart::Text { text } if text.starts_with("USER-") => text.clone(),
                        _ => continue,
                    },
                    _ => continue,
                };
                if rendered.contains(&format!("\"{marker}")) {
                    let msg = format!("shadowed marker {marker} leaked");
                    let mut v = self.violations.lock().unwrap();
                    if !v.contains(&msg) {
                        v.push(msg);
                    }
                }
            }
            let dup: HashSet<&String> = ctx.sources.iter().collect();
            if dup.len() != ctx.sources.len() {
                self.violations
                    .lock()
                    .unwrap()
                    .push("duplicate sources".into());
            }
        }
        let last_is_results = matches!(ctx.turns.last(), Some(ModelTurn::ToolResults { .. }));
        if last_is_results {
            StepOutcome::Committed(assistant("done", &[]))
        } else {
            StepOutcome::Committed(assistant(
                "calling",
                &[(&format!("call-{}", *calls), "Big")],
            ))
        }
    }
}

struct Big {
    n: Mutex<usize>,
}
#[async_trait]
impl Tool for Big {
    fn name(&self) -> &str {
        "Big"
    }
    async fn execute(&self, _: serde_json::Value) -> Result<String, String> {
        let mut n = self.n.lock().unwrap();
        *n += 1;
        // 17-char unique prefix marker, then ~20 KB of filler.
        Ok(format!("RESULT-{:010}", *n) + &"x".repeat(20 * 1024))
    }
}

fn stack_policy() -> Policy {
    Policy {
        meter: Default::default(),
        summary_profile: None,
        threshold_tokens: 12_000,
        retain_tokens: 6_000,
        summary_tokens: 256,
        system_prompt: SUMMARY_SYSTEM.into(),
        prompt: "Summarize.".into(),
        max_overflow_retries: 1,
        max_compactions: 2,
        prune_threshold: 1_000_000, // pruning off: exercise checkpoints only
        prune_head: 1000,
        prune_tail: 1000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_stacking_200_checkpoints() {
    let target: usize = std::env::var("RNESS_WP1_COMPACTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    let sid = log.session().clone();
    let provider = Stacker {
        root: dir.path().to_path_buf(),
        sid: Mutex::new(Some(sid.clone())),
        summaries: Mutex::new(0),
        main_calls: Mutex::new(0),
        violations: Mutex::new(vec![]),
        max_context_turns: Mutex::new(0),
    };
    let tools = ToolRegistry::default();
    tools.register(Arc::new(Big { n: Mutex::new(0) }));
    let config = TurnConfig {
        compaction: [("default".to_string(), stack_policy())].into(),
        ..Default::default()
    };
    let mut turn = 0u32;
    let mut turn_ms = Vec::new();
    let checkpoints = |store: &SessionStore| {
        read_session(store.root(), &sid)
            .unwrap()
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::Compaction(_)))
            .count()
    };
    while checkpoints(&store) < target && turn < (target as u32) * 3 {
        turn += 1;
        log.append(&user(&format!("USER-{turn:06}"))).unwrap();
        let t0 = std::time::Instant::now();
        let outcome = run_turn(
            &store,
            &mut log,
            &provider,
            &tools,
            &config,
            &CancellationToken::new(),
            &mut Vec::new,
            turn,
            &|_| {},
            None,
        )
        .await
        .unwrap();
        turn_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(outcome, TurnOutcome::Completed);
    }
    let n_ckpt = checkpoints(&store);
    let violations = provider.violations.lock().unwrap().clone();
    let size = fs::metadata(log.path()).unwrap().len();
    println!(
        "{{\"probe\":\"wp1-compaction-stacking\",\"turns\":{turn},\"checkpoints\":{n_ckpt},\"log_mb\":{:.1},\"turn1_ms\":{:.2},\"last_turn_ms\":{:.2},\"max_context_turns\":{}}}",
        size as f64 / 1e6,
        turn_ms[0],
        turn_ms.last().unwrap(),
        provider.max_context_turns.lock().unwrap()
    );
    assert!(violations.is_empty(), "{violations:?}");
    assert!(
        n_ckpt >= target,
        "only {n_ckpt} checkpoints in {turn} turns"
    );
    // Context stays bounded no matter how many checkpoints stacked.
    assert!(*provider.max_context_turns.lock().unwrap() < 20);

    // Exactly one live checkpoint; its shadow covers every earlier user turn.
    let history = read_session(dir.path(), &sid).unwrap();
    let shadows = shadowed_by_checkpoints(&history);
    assert_eq!(shadows.len(), 1, "one live checkpoint after stacking");

    // Resume from a cold store, recover(), replay: same context.
    drop(log);
    let fresh = SessionStore::new(dir.path());
    let before = replay(&store, &sid).unwrap().context;
    let mut log = fresh.open(&sid).unwrap();
    compaction::recover(&mut log).unwrap();
    let after = replay(&fresh, &sid).unwrap().context;
    assert_eq!(before.turns, after.turns);
    pairs_intact(&after.turns).unwrap();

    // Prune a result that is already folded into the checkpoint: inert.
    let folded = history
        .iter()
        .find(|e| {
            matches!(e.event, SessionEvent::ToolResult(_)) && shadows[0].shadowed.contains(&e.id)
        })
        .unwrap();
    if let SessionEvent::ToolResult(r) = &folded.event {
        let mut short = r.clone();
        short.output = "PRUNED-RESURRECTED".into();
        log.append(&SessionEvent::Prune(Prune {
            replaces: folded.id.clone(),
            result: short,
        }))
        .unwrap();
    }
    let with_prune = replay(&fresh, &sid).unwrap().context;
    assert!(!serde_json::to_string(&with_prune.turns)
        .unwrap()
        .contains("PRUNED-RESURRECTED"));
    assert_eq!(with_prune.turns, after.turns);

    // Fork at a shadowed user message: the child sees the ORIGINAL history
    // up to that point (no checkpoint yet), and stays consistent.
    let shadowed_user = history
        .iter()
        .find(|e| {
            matches!(&e.event, SessionEvent::UserMessage(m) if matches!(&m.content[0], ContentPart::Text{text} if text == "USER-000005"))
        })
        .unwrap();
    let child = fresh.fork(&sid, Some(shadowed_user.id.clone())).unwrap();
    let cctx = replay(&fresh, child.session()).unwrap().context;
    pairs_intact(&cctx.turns).unwrap();
    let rendered = serde_json::to_string(&cctx.turns).unwrap();
    assert!(rendered.contains("USER-000005"));
    assert!(!rendered.contains("USER-000006"));

    // Fork AT the live checkpoint: summary + retained tail before it.
    let ckpt = shadows[0].checkpoint.clone();
    let child2 = fresh.fork(&sid, Some(ckpt)).unwrap();
    let c2 = replay(&fresh, child2.session()).unwrap().context;
    pairs_intact(&c2.turns).unwrap();
    assert!(serde_json::to_string(&c2.turns[0])
        .unwrap()
        .contains("SUMMARY-"));
    drop(child);
    drop(child2);
}

/// reduce()'s prune pass must skip results that carry images.
#[tokio::test]
async fn prune_pass_skips_image_results() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    for i in 0..4 {
        log.append(&user(&format!("u{i}"))).unwrap();
        log.append(&SessionEvent::AssistantMessage(assistant(
            "c",
            &[(&format!("c{i}"), "Big")],
        )))
        .unwrap();
        let mut r = ToolResult {
            call: format!("c{i}"),
            name: "Big".into(),
            content: vec![],
            output: "y".repeat(40_000),
            is_error: false,
            duration_ms: 1,
            tasks: None,
            plan_review: None,
            presentation: None,
        };
        if i == 0 {
            r.content = vec![
                ToolResultContentPart::Text {
                    text: r.output.clone(),
                },
                ToolResultContentPart::Image {
                    attachment: ImageRef {
                        id: "sha256-deadbeef".into(),
                        media_type: "image/png".into(),
                        bytes: 10,
                        width: 1,
                        height: 1,
                    },
                },
            ];
        }
        log.append(&SessionEvent::ToolResult(r)).unwrap();
        log.append(&SessionEvent::AssistantMessage(assistant("ok", &[])))
            .unwrap();
    }
    let mut policy = stack_policy();
    policy.prune_threshold = 8192;
    policy.prune_head = 2000;
    policy.prune_tail = 500;
    policy.threshold_tokens = 1_000_000; // only the prune pass
    policy.retain_tokens = 100;
    let provider = Stacker {
        root: dir.path().to_path_buf(),
        sid: Mutex::new(None),
        summaries: Mutex::new(0),
        main_calls: Mutex::new(0),
        violations: Mutex::new(vec![]),
        max_context_turns: Mutex::new(0),
    };
    let changed = compaction::reduce(
        &store,
        &mut log,
        &provider,
        "",
        &[],
        &policy,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(changed);
    let history = read_session(dir.path(), &sid).unwrap();
    let prunes: Vec<&Prune> = history
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::Prune(p) => Some(p),
            _ => None,
        })
        .collect();
    assert!(
        prunes.iter().all(|p| p.result.call != "c0"),
        "image result was pruned"
    );
    assert!(!prunes.is_empty());
    // A second reduce must not re-prune (idempotent).
    let again = compaction::reduce(
        &store,
        &mut log,
        &provider,
        "",
        &[],
        &policy,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!again, "second prune pass re-pruned already-pruned results");
}

/// The prune pass commits its prunes in batches (plan P6): 1100 oversized
/// results -> 1100 Prune events in history order, 3 syncs (512 cap), and
/// the projection replaces every pruned result.
#[tokio::test]
async fn prune_pass_is_batched_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    let mut batch = Vec::new();
    for i in 0..1100 {
        batch.push(user(&format!("u{i}")));
        batch.push(SessionEvent::AssistantMessage(assistant(
            "c",
            &[(&format!("c{i}"), "Big")],
        )));
        batch.push(SessionEvent::ToolResult(ToolResult {
            call: format!("c{i}"),
            name: "Big".into(),
            content: vec![],
            output: format!("{i:05}").repeat(2000),
            is_error: false,
            duration_ms: 1,
            tasks: None,
            plan_review: None,
            presentation: None,
        }));
        batch.push(SessionEvent::AssistantMessage(assistant("ok", &[])));
    }
    log.append_batch(&batch).unwrap();
    let mut policy = stack_policy();
    policy.prune_threshold = 8192;
    policy.prune_head = 100;
    policy.prune_tail = 100;
    policy.threshold_tokens = 100_000_000; // only the prune pass
    policy.retain_tokens = 100;
    let provider = Refuser;
    let syncs = log.sync_count();
    let changed = compaction::reduce(
        &store,
        &mut log,
        &provider,
        "",
        &[],
        &policy,
        false,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(changed);
    assert_eq!(
        log.sync_count() - syncs,
        3,
        "1100 prunes in 512-event batches"
    );
    let history = read_session(dir.path(), &sid).unwrap();
    let calls: Vec<String> = history
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::Prune(p) => Some(p.result.call.clone()),
            _ => None,
        })
        .collect();
    let pruned = calls.len();
    assert!(pruned >= 1090, "{pruned}");
    let expected: Vec<String> = (0..pruned).map(|i| format!("c{i}")).collect();
    assert_eq!(calls, expected, "prunes follow history order");
    let replayed = replay(&store, &sid).unwrap();
    let rendered = serde_json::to_string(&replayed.context.turns).unwrap();
    assert_eq!(
        rendered.matches("[tool result middle pruned]").count(),
        pruned
    );
}

/// B1-9: a process killed mid-turn leaves `turn/started N` unclosed; recover()
/// closes only that trailing turn, as `failed`, once (idempotent), and the
/// context is unchanged.
#[test]
fn recover_closes_trailing_unclosed_turn() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    for turn in 1..=2u32 {
        log.append(&SessionEvent::TurnStarted { turn }).unwrap();
        log.append(&user("q")).unwrap();
        if turn == 1 {
            log.append(&SessionEvent::TurnEnded {
                turn,
                outcome: TurnOutcome::Completed,
            })
            .unwrap();
        }
    }
    let before = replay(&store, &sid).unwrap().context;
    drop(log);
    let mut log = store.open(&sid).unwrap();
    compaction::recover(&mut log).unwrap();
    compaction::recover(&mut log).unwrap(); // idempotent
    let ended: Vec<(u32, TurnOutcome)> = read_session(dir.path(), &sid)
        .unwrap()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::TurnEnded { turn, outcome } => Some((*turn, *outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended,
        vec![(1, TurnOutcome::Completed), (2, TurnOutcome::Failed)]
    );
    assert_eq!(replay(&store, &sid).unwrap().context, before);
}

/// recover(): a compaction/started with a later checkpoint but no finished
/// is closed as committed_before_interruption; a bare one as interrupted.
#[test]
fn recover_closes_interrupted_compaction_spans() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(None).unwrap();
    let sid = log.session().clone();
    let u = log.append(&user("a")).unwrap();
    let s1 = log
        .append(&SessionEvent::CompactionStarted {
            model: "m".into(),
            sources: vec![u.id.clone()],
            estimated_input: 1,
            request: serde_json::Value::Null,
        })
        .unwrap();
    log.append(&SessionEvent::Compaction(Compaction {
        replaces: vec![u.id.clone()],
        summary: "s".into(),
        model: "m".into(),
    }))
    .unwrap();
    let s2 = log
        .append(&SessionEvent::CompactionStarted {
            model: "m".into(),
            sources: vec![],
            estimated_input: 1,
            request: serde_json::Value::Null,
        })
        .unwrap();
    drop(log);
    let mut log = store.open(&sid).unwrap();
    compaction::recover(&mut log).unwrap();
    compaction::recover(&mut log).unwrap(); // idempotent
    let finished: Vec<(String, String)> = read_session(dir.path(), &sid)
        .unwrap()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::CompactionFinished {
                started, outcome, ..
            } => Some((started.clone(), outcome.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![
            (s1.id, "committed_before_interruption".into()),
            (s2.id, "interrupted".into())
        ]
    );
}

// ------------------------------------------ 6. instruction re-injection

mod instructions_e2e {
    use super::*;
    use rness_engine::instructions::{ensure, render, InstructionsConfig};

    fn cfg(cwd: &Path, max: usize) -> InstructionsConfig {
        InstructionsConfig {
            cwd: cwd.to_path_buf(),
            candidates: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
            max_bytes: max,
        }
    }

    fn baselines(store: &SessionStore, sid: &str) -> Vec<String> {
        replay(store, &sid.to_string())
            .unwrap()
            .history
            .iter()
            .filter_map(|e| match &e.event {
                SessionEvent::UserMessage(UserMessage {
                    source: Some(MessageSource::Instructions { identity }),
                    ..
                }) => Some(identity.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn truncation_at_multibyte_boundary() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "é".repeat(100)).unwrap(); // 200 bytes
        for max in [1usize, 2, 3, 99, 101, 199] {
            let b = render(&cfg(dir.path(), max)).unwrap();
            assert!(b.text.contains("truncated"), "max {max}");
        }
        let full = render(&cfg(dir.path(), 200)).unwrap();
        assert!(!full.text.contains("truncated"));
    }

    /// The byte budget counts only file bytes, not the rendered wrapper /
    /// per-file headers: the injected text exceeds `max_bytes`.
    #[test]
    fn budget_excludes_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "a".repeat(1000)).unwrap();
        let b = render(&cfg(dir.path(), 1000)).unwrap();
        println!("max_bytes=1000 rendered={} bytes", b.text.len());
        assert!(b.text.len() > 1000);
    }

    /// AGENTS.md with invalid UTF-8 is injected lossily (B1-7): the first
    /// candidate still wins, invalid bytes become U+FFFD, and a warning is
    /// logged instead of silently falling through to CLAUDE.md.
    #[test]
    fn invalid_utf8_agents_md_is_injected_lossily() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("AGENTS.md"), b"rules \xFF\xFE here").unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "claude rules").unwrap();
        let b = render(&cfg(dir.path(), 65536)).unwrap();
        println!("rendered: {}", b.text.replace('\n', " | "));
        assert!(b.text.contains("rules \u{FFFD}\u{FFFD} here"), "{}", b.text);
        assert!(!b.text.contains("claude rules"), "AGENTS.md must win");
    }

    /// Changing AGENTS.md between turns appends a new baseline and the old
    /// one is superseded (B1-8): both stay in the append-only log, but only
    /// the current version is model-visible — across cold replay, forks and
    /// compaction-style prune/fold of the log.
    #[test]
    fn changed_file_supersedes_previous_baseline() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let agents = dir.path().join("AGENTS.md");
        fs::write(&agents, "version one").unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let mut log = store.create(None).unwrap();
        let sid = log.session().clone();
        let c = cfg(dir.path(), 65536);
        assert!(ensure(&store, &mut log, &c).unwrap());
        assert!(
            !ensure(&store, &mut log, &c).unwrap(),
            "re-injected unchanged baseline"
        );
        fs::write(&agents, "version two").unwrap();
        assert!(ensure(&store, &mut log, &c).unwrap());
        let ids = baselines(&store, &sid);
        let ctx = replay(&store, &sid).unwrap().context;
        let rendered = serde_json::to_string(&ctx.turns).unwrap();
        println!(
            "baselines={} v1_visible={} v2_visible={}",
            ids.len(),
            rendered.contains("version one"),
            rendered.contains("version two")
        );
        // Both baselines are durable history, one per identity...
        assert_eq!(ids.len(), 2);
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
        // ...but only the current one reaches the model.
        assert!(rendered.contains("version two"));
        assert!(!rendered.contains("version one"), "stale baseline visible");
        // Same through a cold store and through a fork of the session.
        let cold = SessionStore::new(dir.path().join("sessions"));
        let fork = cold.fork(&sid, None).unwrap();
        for session in [&sid, fork.session()] {
            let rendered =
                serde_json::to_string(&replay(&cold, session).unwrap().context.turns).unwrap();
            assert!(rendered.contains("version two") && !rendered.contains("version one"));
        }
        // Reverting the file re-injects a fresh baseline for the old identity
        // (it is superseded, hence not visible) and hides version two.
        fs::write(&agents, "version one").unwrap();
        assert!(ensure(&store, &mut log, &c).unwrap());
        let rendered = serde_json::to_string(&replay(&store, &sid).unwrap().context.turns).unwrap();
        assert!(rendered.contains("version one") && !rendered.contains("version two"));
    }

    /// After a checkpoint folds the baseline, ensure re-injects exactly once.
    #[test]
    fn reinjected_once_after_fold() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "rules").unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let mut log = store.create(None).unwrap();
        let sid = log.session().clone();
        let c = cfg(dir.path(), 65536);
        ensure(&store, &mut log, &c).unwrap();
        let u = log.append(&user("work")).unwrap();
        let ctx = replay(&store, &sid).unwrap().context;
        log.append(&SessionEvent::Compaction(Compaction {
            replaces: ctx.sources.clone(),
            summary: "folded".into(),
            model: "m".into(),
        }))
        .unwrap();
        let _ = u;
        assert!(ensure(&store, &mut log, &c).unwrap());
        assert!(!ensure(&store, &mut log, &c).unwrap());
        let ids = baselines(&store, &sid);
        assert_eq!(ids.len(), 2);
        let visible = replay(&store, &sid)
            .unwrap()
            .context
            .turns
            .iter()
            .filter(|t| {
                serde_json::to_string(t)
                    .unwrap()
                    .contains("Workspace instructions")
            })
            .count();
        assert_eq!(visible, 1);
    }
}

// ---------------------------------------------------- misc invariants

/// The model context derived via the store equals the one derived from a
/// raw read (cache vs disk) on a session mixing every event kind.
#[test]
fn cached_and_raw_projection_agree() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    let cached = replay(&store, &sid).unwrap().context;
    let raw = model_context(&read_session(dir.path(), &sid).unwrap());
    assert_eq!(cached.turns, raw.turns);
    assert_eq!(cached.sources, raw.sources);
}

/// Appending through one store while another store's cached reader is
/// warm: the warm reader picks up the new events.
#[test]
fn warm_reader_sees_appends_from_other_writer() {
    let dir = tempfile::tempdir().unwrap();
    let (store, sid) = seeded(dir.path());
    let n0 = replay(&store, &sid).unwrap().history.len();
    let other = SessionStore::new(dir.path());
    let mut log = other.open(&sid).unwrap();
    log.append(&user("x")).unwrap();
    assert_eq!(replay(&store, &sid).unwrap().history.len(), n0 + 1);
    let _ = SessionLog::open; // keep import used
}
