use rness_engine::{
    session::branch::SessionStore,
    session_search::{QueryRequest, SqliteSessionSearch},
};
use rness_protocol::events::*;
use serde_json::{json, Value};
fn message(text: &str) -> SessionEvent {
    SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: text.into() }],
        source: None,
    })
}
fn request(value: Value) -> QueryRequest {
    serde_json::from_value(value).unwrap()
}

#[test]
fn incremental_grouped_pages_filters_reads_and_traces() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut a = store.create(Some("/w".into())).unwrap();
    let mut b = store.create(Some("/w".into())).unwrap();
    let mut private = store.create(Some("/private".into())).unwrap();
    let source = a.append(&message("needle original")).unwrap();
    a.append(&message("needle second")).unwrap();
    b.append(&message("needle elsewhere")).unwrap();
    private.append(&message("needle secret")).unwrap();
    let path = dir.path().join("index.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    assert!(!path.exists());
    let first = provider
        .execute(
            &store,
            a.session(),
            "session_search",
            request(json!({"query":"needle","limit":1})),
        )
        .unwrap();
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert!(first["next_cursor"].is_string());
    let second = provider
        .execute(
            &store,
            a.session(),
            "session_search",
            request(json!({"query":"needle","limit":1,"cursor":first["next_cursor"]})),
        )
        .unwrap();
    assert_ne!(
        first["items"][0]["session_id"],
        second["items"][0]["session_id"]
    );
    assert!(second["next_cursor"].is_null());
    let connection = rusqlite::Connection::open(&path).unwrap();
    // A trigger makes touching unchanged rows a hard failure, verifying that
    // refresh is truly per session, not just an unchanged global digest.
    connection.execute_batch(&format!("CREATE TRIGGER unchanged BEFORE DELETE ON revisions WHEN old.session='{}' BEGIN SELECT RAISE(FAIL,'unchanged session rewritten'); END;", b.session())).unwrap();
    let before: i64 = connection
        .query_row(
            "SELECT rowid FROM events WHERE session=?1 LIMIT 1",
            [b.session()],
            |r| r.get(0),
        )
        .unwrap();
    a.append(&message("freshly appended")).unwrap();
    let fresh = provider
        .execute(
            &store,
            a.session(),
            "session_event_search",
            request(json!({"query":"freshly"})),
        )
        .unwrap();
    assert_eq!(fresh["items"].as_array().unwrap().len(), 1);
    let after: i64 = connection
        .query_row(
            "SELECT rowid FROM events WHERE session=?1 LIMIT 1",
            [b.session()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
    assert!(provider
        .execute(
            &store,
            a.session(),
            "session_search",
            request(json!({"query":"needle","limit":1,"cursor":first["next_cursor"]}))
        )
        .is_ok());
    assert!(provider
        .execute(
            &store,
            a.session(),
            "session_search",
            request(json!({"query":"different","limit":1,"cursor":first["next_cursor"]}))
        )
        .is_err());
    let checkpoint = a
        .append(&SessionEvent::Compaction(Compaction {
            replaces: vec![source.id.clone()],
            summary: "short summary".into(),
            model: "test".into(),
        }))
        .unwrap();
    let shadowed = provider
        .execute(
            &store,
            a.session(),
            "session_event_search",
            request(
                json!({"query":"needle","surfaces":["shadowed"],"event_types":["user/message"]}),
            ),
        )
        .unwrap();
    assert_eq!(shadowed["items"].as_array().unwrap().len(), 1);
    assert_eq!(shadowed["items"][0]["event_ref"], source.id);
    let trace = provider
        .execute(
            &store,
            a.session(),
            "session_event_trace",
            request(json!({"event_ref":source.id})),
        )
        .unwrap();
    assert_eq!(trace["items"][0]["from"], checkpoint.id);
    let large = a.append(&message(&"🦀".repeat(12000))).unwrap();
    let mut args = json!({"event_ref":large.id});
    let mut text = String::new();
    loop {
        let part = provider
            .execute(
                &store,
                a.session(),
                "session_event_read",
                request(args.clone()),
            )
            .unwrap();
        let chunk = part["chunk"].as_str().unwrap();
        assert!(chunk.chars().count() <= 8192);
        text.push_str(chunk);
        if part["next_cursor"].is_null() {
            break;
        }
        args["cursor"] = part["next_cursor"].clone();
        a.append(&message("intervening tool round")).unwrap();
    }
    assert_eq!(serde_json::from_str::<Envelope>(&text).unwrap(), large);
    for operation in [
        "session_event_search",
        "session_event_read",
        "session_trace",
    ] {
        assert!(provider
            .execute(
                &store,
                a.session(),
                operation,
                request(
                    json!({"query":"needle","event_ref":source.id,"session_id":private.session()})
                )
            )
            .is_err());
    }
    assert!(provider
        .execute(
            &store,
            a.session(),
            "session_search",
            request(json!({"query":"needle","time_from":"yesterday"}))
        )
        .is_err());
}

#[test]
fn scope_metadata_migrates_and_tracks_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    log.append(&message("needle original")).unwrap();
    let path = dir.path().join("index.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    let initial = provider
        .execute(
            &store,
            log.session(),
            "session_event_search",
            request(json!({"query":"needle"})),
        )
        .unwrap();
    drop(provider);
    // Reproduce a shipped v3 index (surface inside the FTS row, no cursors)
    // holding a stale row; the v4 upgrade discards it and rebuilds.
    std::fs::remove_file(&path).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE revisions (workspace TEXT NOT NULL, session TEXT NOT NULL,
               revision TEXT NOT NULL, PRIMARY KEY(workspace, session));
             CREATE VIRTUAL TABLE events USING fts5(workspace UNINDEXED, session UNINDEXED,
               event UNINDEXED, kind UNINDEXED, surface UNINDEXED, time UNINDEXED, body);
             CREATE TABLE event_scope (rowid INTEGER PRIMARY KEY, workspace TEXT NOT NULL,
               session TEXT NOT NULL);
             INSERT INTO events VALUES('/w','gone','e','user/message','current',0,'needle stale');
             INSERT INTO event_scope SELECT rowid,workspace,session FROM events;
             PRAGMA application_id = 1380864849; PRAGMA user_version=3;",
        )
        .unwrap();
    let mut provider = SqliteSessionSearch::new(path.clone());
    let migrated = provider
        .execute(
            &store,
            log.session(),
            "session_event_search",
            request(json!({"query":"needle"})),
        )
        .unwrap();
    assert_eq!(initial["items"], migrated["items"]);
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        4
    );
    log.append(&message("needle appended")).unwrap();
    let refreshed = provider
        .execute(
            &store,
            log.session(),
            "session_event_search",
            request(json!({"query":"needle"})),
        )
        .unwrap();
    assert_eq!(refreshed["items"].as_array().unwrap().len(), 2);
    let mismatches: i64 = connection.query_row("SELECT count(*) FROM events LEFT JOIN event_scope s ON events.rowid=s.rowid WHERE s.rowid IS NULL OR events.session!=s.session OR events.workspace!=s.workspace",[],|row|row.get(0)).unwrap();
    assert_eq!(mismatches, 0);
    let orphaned: i64 = connection.query_row("SELECT count(*) FROM event_scope s LEFT JOIN events ON events.rowid=s.rowid WHERE events.rowid IS NULL",[],|row|row.get(0)).unwrap();
    assert_eq!(orphaned, 0);
}

#[test]
fn read_is_lazy_and_foreign_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    let event = log.append(&message("saved")).unwrap();
    let path = dir.path().join("index.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    provider
        .execute(
            &store,
            log.session(),
            "session_event_read",
            request(json!({"event_ref":event.id})),
        )
        .unwrap();
    assert!(!path.exists());
    // Replacement after the engine cache was warmed must not preserve old text.
    store.history(log.session()).unwrap();
    let original = std::fs::read_to_string(log.path()).unwrap();
    std::fs::write(log.path(), original.replace("saved", "newer")).unwrap();
    let read = provider
        .execute(
            &store,
            log.session(),
            "session_event_read",
            request(json!({"event_ref":event.id})),
        )
        .unwrap();
    assert!(read["chunk"].as_str().unwrap().contains("newer"));
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated(value TEXT);")
        .unwrap();
    assert!(provider
        .execute(
            &store,
            log.session(),
            "session_search",
            request(json!({"query":"saved"}))
        )
        .is_err());
}

#[test]
fn cancelled_search_stops_refresh_and_keeps_committed_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    log.append(&message("needle")).unwrap();
    let path = dir.path().join("index.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let error = provider
        .execute_cancellable(
            &store,
            log.session(),
            "session_search",
            request(json!({"query":"needle"})),
            &cancel,
        )
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    // The provider stays usable; a fresh token finishes the refresh.
    let found = provider
        .execute(
            &store,
            log.session(),
            "session_search",
            request(json!({"query":"needle"})),
        )
        .unwrap();
    assert_eq!(found["items"].as_array().unwrap().len(), 1);
}

/// Every indexed row as (session, event, kind, surface, body), sorted.
fn index_rows(path: &std::path::Path) -> Vec<(String, String, String, String, String)> {
    let connection = rusqlite::Connection::open(path).unwrap();
    let mut rows: Vec<_> = connection
        .prepare(
            "SELECT events.session,events.event,events.kind,s.surface,events.body
             FROM events JOIN event_scope s ON s.rowid=events.rowid",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.sort();
    rows
}

fn search(
    provider: &mut SqliteSessionSearch,
    store: &SessionStore,
    caller: &String,
    query: &str,
) -> Value {
    provider
        .execute(
            store,
            caller,
            "session_event_search",
            request(json!({"query": query, "limit": 100})),
        )
        .unwrap()
}

/// Plan P4: appends index only the delta (no row of the session is
/// deleted or rewritten), a delta with a Prune/Compaction rebuilds the
/// session with re-labelled surfaces, and in every state the index equals
/// a from-scratch index of the same logs.
#[test]
fn appends_index_incrementally_and_match_a_fresh_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    let caller = log.session().clone();
    let first = log.append(&message("needle one")).unwrap();
    let path = dir.path().join("index.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    search(&mut provider, &store, &caller, "needle");
    let fresh_matches = |provider_path: &std::path::Path| {
        let fresh = dir.path().join("fresh.sqlite3");
        let _ = std::fs::remove_file(&fresh);
        search(
            &mut SqliteSessionSearch::new(fresh.clone()),
            &store,
            &caller,
            "needle",
        );
        assert_eq!(index_rows(provider_path), index_rows(&fresh));
    };
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER no_delete BEFORE DELETE ON event_scope
             BEGIN SELECT RAISE(FAIL,'append rewrote indexed rows'); END;",
        )
        .unwrap();
    for i in 0..3 {
        log.append(&message(&format!("needle appended {i}")))
            .unwrap();
        log.append(&SessionEvent::TurnEnded {
            turn: i,
            outcome: TurnOutcome::Completed,
        })
        .unwrap();
        let found = search(&mut provider, &store, &caller, "needle");
        assert_eq!(found["items"].as_array().unwrap().len(), 2 + i as usize);
    }
    fresh_matches(&path);
    // A torn (unterminated) tail is not indexed, and is picked up once the
    // line completes.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(log.path())
        .unwrap();
    let torn = serde_json::to_string(&Envelope {
        id: "01ZZZZZZZZZZZZZZZZZZZZZZZZ".into(),
        at: "2026-01-01T00:00:00Z".into(),
        event: message("needle torn"),
    })
    .unwrap();
    use std::io::Write;
    file.write_all(&torn.as_bytes()[..20]).unwrap();
    assert_eq!(
        search(&mut provider, &store, &caller, "torn")["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    file.write_all(&torn.as_bytes()[20..]).unwrap();
    file.write_all(b"\n").unwrap();
    assert_eq!(
        search(&mut provider, &store, &caller, "torn")["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fresh_matches(&path);
    // Fold events re-label earlier rows: rebuild of this session.
    connection.execute_batch("DROP TRIGGER no_delete;").unwrap();
    log.append(&SessionEvent::Compaction(Compaction {
        replaces: vec![first.id.clone()],
        summary: "summary".into(),
        model: "m".into(),
    }))
    .unwrap();
    let shadowed = provider
        .execute(
            &store,
            &caller,
            "session_event_search",
            request(json!({"query":"needle","surfaces":["shadowed"]})),
        )
        .unwrap();
    assert_eq!(shadowed["items"][0]["event_ref"], first.id);
    fresh_matches(&path);
}

/// Plan P4: a rewritten or replaced log (not an append) is re-indexed from
/// scratch, and an unreadable session is skipped with its rows dropped
/// instead of failing the whole workspace search.
#[test]
fn rewrites_rebuild_and_corrupt_sessions_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    let mut other = store.create(Some("/w".into())).unwrap();
    let caller = log.session().clone();
    log.append(&message("needle original text")).unwrap();
    other.append(&message("needle in other")).unwrap();
    let path = dir.path().join("index.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    let workspace_search = |provider: &mut SqliteSessionSearch, query: &str| {
        provider
            .execute(
                &store,
                &caller,
                "session_search",
                request(json!({"query": query, "limit": 100})),
            )
            .unwrap()
    };
    assert_eq!(
        workspace_search(&mut provider, "needle")["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Same-length in-place rewrite of the last indexed line plus growth:
    // the last-line check catches it.
    let original = std::fs::read_to_string(log.path()).unwrap();
    let rewritten = original.replace("needle original text", "needle replaced text");
    std::fs::write(log.path(), &rewritten).unwrap();
    log.append(&message("needle later")).unwrap();
    let found = search(&mut provider, &store, &caller, "original");
    assert_eq!(found["items"].as_array().unwrap().len(), 0, "{found}");
    assert_eq!(
        search(&mut provider, &store, &caller, "replaced")["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Garbage in another session's committed prefix: skipped, not fatal.
    let bytes = std::fs::read(other.path()).unwrap();
    let mut broken = bytes.clone();
    let second_line = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
    broken[second_line] = b'#';
    std::fs::write(other.path(), &broken).unwrap();
    let found = workspace_search(&mut provider, "needle");
    let sessions: Vec<_> = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(|s| s == &caller), "{sessions:?}");
    // Repaired: indexed again.
    std::fs::write(other.path(), &bytes).unwrap();
    let found = workspace_search(&mut provider, "other");
    assert_eq!(found["items"].as_array().unwrap().len(), 1);
}

/// The standard index file is new per schema: an existing v2/v3 file (which
/// older rness processes may still be using) is neither opened nor changed.
#[test]
fn v4_index_gets_a_new_file_and_leaves_older_indexes_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    log.append(&message("needle original")).unwrap();

    let v2 = dir.path().join("session-search-v2.sqlite3");
    {
        let connection = rusqlite::Connection::open(&v2).unwrap();
        connection
            .execute_batch(
                "CREATE VIRTUAL TABLE events USING fts5(workspace UNINDEXED, session UNINDEXED,
                   event UNINDEXED, kind UNINDEXED, surface UNINDEXED, time UNINDEXED, body);
                 INSERT INTO events VALUES('/w','old','e','user/message','current',0,'needle stale');
                 PRAGMA application_id = 1380864849; PRAGMA user_version=2;",
            )
            .unwrap();
    }
    let proto = dir.path().join("session-search.sqlite3");
    std::fs::write(&proto, b"prototype bytes").unwrap();
    let (v2_before, proto_before) = (std::fs::read(&v2).unwrap(), std::fs::read(&proto).unwrap());

    let path = dir
        .path()
        .join(rness_engine::session_search::INDEX_FILE_NAME);
    assert_eq!(path.file_name().unwrap(), "session-search-v4.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    let found = provider
        .execute(
            &store,
            log.session(),
            "session_event_search",
            request(json!({"query":"needle"})),
        )
        .unwrap();
    assert_eq!(found["items"].as_array().unwrap().len(), 1);

    let connection = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    drop(connection);
    drop(provider);
    assert_eq!(std::fs::read(&v2).unwrap(), v2_before, "v2 index rewritten");
    assert_eq!(std::fs::read(&proto).unwrap(), proto_before);
    let v2_version: i64 = rusqlite::Connection::open(&v2)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(v2_version, 2);
}

/// A session whose header cannot be read is recorded with its log revision,
/// so it is skipped (and warned about) once per change, not on every search.
#[test]
fn unreadable_header_records_a_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut log = store.create(Some("/w".into())).unwrap();
    let mut broken = store.create(Some("/w".into())).unwrap();
    let caller = log.session().clone();
    log.append(&message("needle ok")).unwrap();
    broken.append(&message("needle broken")).unwrap();
    let broken_id = broken.session().clone();
    let path = dir.path().join("index.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    let run = |provider: &mut SqliteSessionSearch| {
        provider
            .execute(
                &store,
                &caller,
                "session_search",
                request(json!({"query":"needle","limit":100})),
            )
            .unwrap()
    };
    assert_eq!(run(&mut provider)["items"].as_array().unwrap().len(), 2);
    // Fresh store handle: the first store cached the readable header.
    // A newer-format header still lists, but its header cannot be read.
    let text = std::fs::read_to_string(broken.path()).unwrap();
    assert!(text.contains("\"version\":1"));
    std::fs::write(
        broken.path(),
        text.replacen("\"version\":1", "\"version\":99", 1),
    )
    .unwrap();
    let store2 = SessionStore::new(dir.path());
    let found = provider
        .execute(
            &store2,
            &caller,
            "session_search",
            request(json!({"query":"needle","limit":100})),
        )
        .unwrap();
    assert_eq!(found["items"].as_array().unwrap().len(), 1, "{found}");
    let connection = rusqlite::Connection::open(&path).unwrap();
    let recorded: i64 = connection
        .query_row(
            "SELECT count(*) FROM revisions WHERE session=?1",
            [&broken_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1);
    let rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM event_scope WHERE session=?1",
            [&broken_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "nothing stale served from it");
}
