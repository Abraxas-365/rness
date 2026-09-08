//! Property tests on log invariants: any sequence of appended events
//! survives a crash at any byte offset with the committed prefix intact
//! (append-only + torn-tail healing, invariants #2 and durability).

use std::fs;
use std::io::Write;

use rness_engine::session::log::{read_session, SessionLog};
use rness_protocol::events::{ContentPart, SessionEvent, UserIntent, UserMessage};
use proptest::prelude::*;

fn arb_event() -> impl Strategy<Value = SessionEvent> {
    // Text content exercising escaping: quotes, newlines, unicode, braces.
    "[ -~\n\"\\\\{}\u{00e9}\u{4e16}]{0,60}".prop_map(|text| {
        SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text }],
            source: None,
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever we append, we read back identically, in order.
    #[test]
    fn append_read_identity(events in prop::collection::vec(arb_event(), 0..20)) {
        let root = tempfile::tempdir().unwrap();
        let sid = "01PROP".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let mut committed = Vec::new();
        for ev in &events {
            committed.push(log.append(ev).unwrap());
        }
        let read = log.read_all().unwrap();
        prop_assert_eq!(read.len(), events.len() + 1); // + header
        for (env, orig) in read.iter().skip(1).zip(&committed) {
            prop_assert_eq!(env, orig);
        }
    }

    /// Truncate the file at ANY byte length, reopen: every fully-committed
    /// line before the cut survives; the log heals and stays appendable.
    #[test]
    fn crash_at_any_offset_preserves_committed_prefix(
        events in prop::collection::vec(arb_event(), 1..8),
        cut_ratio in 0.0f64..1.0,
    ) {
        let root = tempfile::tempdir().unwrap();
        let sid = "01CRASH".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        for ev in &events {
            log.append(ev).unwrap();
        }
        let path = log.path().to_path_buf();
        drop(log);

        let bytes = fs::read(&path).unwrap();
        // Cut somewhere in the file, but never before the header line —
        // a session whose header never hit disk was never created.
        let header_end = bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
        let cut = header_end + ((bytes.len() - header_end) as f64 * cut_ratio) as usize;
        let expect_lines = bytes[..cut].iter().filter(|&&b| b == b'\n').count();

        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(cut as u64).unwrap();
        drop(f);

        // Heal on open.
        let mut log = SessionLog::open(root.path(), &sid).unwrap();
        let read = log.read_all().unwrap();
        prop_assert_eq!(read.len(), expect_lines);

        // Still appendable after healing.
        log.append(&events[0]).unwrap();
        drop(log);
        prop_assert_eq!(read_session(root.path(), &sid).unwrap().len(), expect_lines + 1);
    }

    /// Appending never rewrites earlier bytes (append-only, byte-level).
    #[test]
    fn append_never_mutates_existing_bytes(events in prop::collection::vec(arb_event(), 1..10)) {
        let root = tempfile::tempdir().unwrap();
        let sid = "01APP".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let path = log.path().to_path_buf();
        let mut prev = fs::read(&path).unwrap();
        for ev in &events {
            log.append(ev).unwrap();
            let now = fs::read(&path).unwrap();
            prop_assert!(now.len() > prev.len());
            prop_assert_eq!(&now[..prev.len()], &prev[..]);
            prev = now;
        }
    }
}

#[test]
fn torn_tail_followed_by_append_yields_clean_log() {
    // Deterministic companion: heal, then append, then verify parse of the
    // WHOLE file (no gluing of healed boundary and new line).
    let root = tempfile::tempdir().unwrap();
    let sid = "01GLUE".to_string();
    let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
    log.append(&SessionEvent::TurnStarted { turn: 1 }).unwrap();
    let path = log.path().to_path_buf();
    drop(log);

    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"id\":\"torn").unwrap();
    drop(f);

    let mut log = SessionLog::open(root.path(), &sid).unwrap();
    log.append(&SessionEvent::TurnEnded { turn: 1, outcome: rness_protocol::events::TurnOutcome::Completed }).unwrap();
    let read = log.read_all().unwrap();
    assert_eq!(read.len(), 3);
}
