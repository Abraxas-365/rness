use rness_engine::session::branch::SessionStore;
use rness_engine::session_search::SqliteSessionSearch;
use rness_protocol::events::{ContentPart, SessionEvent, UserIntent, UserMessage};

fn message(text: &str) -> SessionEvent {
    SessionEvent::UserMessage(UserMessage {
        intent: UserIntent::Followup,
        content: vec![ContentPart::Text { text: text.into() }],
        source: None,
    })
}

#[test]
fn session_search_filters_before_limit_refreshes_and_stays_lazy() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut first = store.create(Some("/workspace".into())).unwrap();
    let mut second = store.create(Some("/workspace".into())).unwrap();
    let mut other = store.create(Some("/other".into())).unwrap();
    first.append(&message("search needle first")).unwrap();
    let event = second.append(&message("search needle second")).unwrap();
    other.append(&message("search needle private")).unwrap();
    let path = dir.path().join("search.sqlite3");
    let mut provider = SqliteSessionSearch::new(path.clone());
    assert!(!path.exists());
    let hits = provider.search_in_session(
        "/workspace", Some(second.session()), "search needle", 1,
        |revision| store.search_snapshot("/workspace", revision),
    ).unwrap();
    assert!(path.exists());
    assert_eq!(hits.len(), 1);
    assert_eq!(&hits[0].session_id, second.session());
    assert_eq!(hits[0].event_ref, event.id);
    let hits = provider.search("/workspace", "search needle", 100,
        |revision| store.search_snapshot("/workspace", revision)).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| &hit.session_id != other.session()));
    second.append(&message("fresh message")).unwrap();
    let hits = provider.search_in_session("/workspace", Some(second.session()), "fresh", 20,
        |revision| store.search_snapshot("/workspace", revision)).unwrap();
    assert_eq!(hits.len(), 1);
    let snapshot = store.search_snapshot("/workspace", None).unwrap().unwrap();
    assert!(store.search_snapshot("/workspace", Some(&snapshot.revision)).unwrap().is_none());
}

#[test]
fn session_event_read_authorizes_and_reads_original_local_text() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let caller = store.create(Some("/workspace".into())).unwrap();
    let mut target = store.create(Some("/workspace".into())).unwrap();
    let other = store.create(Some("/other".into())).unwrap();
    let no_workspace = store.create(None).unwrap();
    let event = target.append(&message("full original text 🦀")).unwrap();
    let document = store.search_event_read(caller.session(), target.session(), &event.id)
        .unwrap().unwrap();
    assert_eq!(document.text, "full original text 🦀");
    assert_eq!(document.event_ref, event.id);
    assert!(store.search_event_read(caller.session(), target.session(), "missing").unwrap().is_none());
    assert!(store.search_event_read(caller.session(), other.session(), &event.id).is_err());
    assert!(store.search_event_read(no_workspace.session(), target.session(), &event.id).is_err());
    assert!(store.search_workspace(caller.session(), Some(&"../escape".into())).is_err());
    assert!(store.search_workspace(caller.session(), Some(&"..\\escape".into())).is_err());
    assert!(!dir.path().join("session-search.sqlite3").exists());
}
