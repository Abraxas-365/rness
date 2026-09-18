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
    let connection = rusqlite::Connection::open(&path).unwrap();
    // Reproduce the shipped v2 schema, then upgrade without rereading bodies.
    connection
        .execute_batch("DROP TABLE event_scope; PRAGMA user_version=2;")
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
        3
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
