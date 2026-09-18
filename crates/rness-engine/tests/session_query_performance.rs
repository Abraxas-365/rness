//! Reproducible synthetic performance probe; never accesses real sessions.
//! cargo test --release -p rness-engine --test session_query_performance -- --ignored --nocapture
//! Override corpus with RNESS_BENCH_SESSIONS / RNESS_BENCH_EVENTS (per session).
use rness_engine::{
    session::branch::SessionStore,
    session_search::{QueryRequest, SqliteSessionSearch},
};
use rness_protocol::events::*;
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    time::Instant,
};

fn timed(label: &str, repeats: usize, mut run: impl FnMut() -> Value) -> Value {
    let mut samples = Vec::new();
    let mut result = Value::Null;
    for _ in 0..repeats {
        let start = Instant::now();
        result = std::hint::black_box(run());
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{label}: n={repeats} min_ms={:.2} median_ms={:.2} max_ms={:.2}",
        samples[0],
        samples[repeats / 2],
        samples[repeats - 1]
    );
    result
}
fn execute(
    provider: &mut SqliteSessionSearch,
    store: &SessionStore,
    caller: &String,
    operation: &str,
    args: Value,
) -> Value {
    let request: QueryRequest = serde_json::from_value(args).unwrap();
    provider.execute(store, caller, operation, request).unwrap()
}
fn event(session: usize, index: usize, text: String) -> Envelope {
    Envelope {
        id: format!("event-{session:06}-{index:09}"),
        at: "2026-01-01T00:00:00Z".into(),
        event: SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text }],
            source: None,
        }),
    }
}

#[test]
#[ignore = "synthetic release-mode performance measurement"]
fn session_query_performance() {
    let sessions: usize = std::env::var("RNESS_BENCH_SESSIONS")
        .unwrap_or("100".into())
        .parse()
        .unwrap();
    let events: usize = std::env::var("RNESS_BENCH_EVENTS")
        .unwrap_or("1000".into())
        .parse()
        .unwrap();
    assert!(sessions > 0 && events > 0);
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut ids = Vec::new();
    let mut paths = Vec::new();
    let mut bytes = 0;
    // Buffered fixture writes deliberately exclude per-event fsync from timing.
    // Every record is a valid committed envelope; no index is prebuilt.
    for session in 0..sessions {
        let log = store.create(Some("/benchmark".into())).unwrap();
        ids.push(log.session().clone());
        paths.push(log.path().to_owned());
        drop(log);
        let mut writer = BufWriter::new(
            OpenOptions::new()
                .append(true)
                .open(&paths[session])
                .unwrap(),
        );
        for index in 0..events {
            let mut text = format!("commonterm session{session} message{index} ");
            for word in 0..80 {
                text.push_str(&format!(
                    "token{} ",
                    (index * 31 + word * 7 + session) % 4096
                ));
            }
            if index == 0 {
                text.push_str("rareterm ");
            }
            if session == 0 && index == events - 1 {
                text.push_str(&"large event payload ".repeat(2000));
            }
            serde_json::to_writer(&mut writer, &event(session, index, text)).unwrap();
            writer.write_all(b"\n").unwrap();
        }
        writer.flush().unwrap();
        bytes += std::fs::metadata(&paths[session]).unwrap().len();
    }
    let index_path = dir.path().join("index.sqlite3");
    let mut provider = SqliteSessionSearch::new(index_path.clone());
    println!(
        "corpus: sessions={sessions} events_per_session={events} total_events={} jsonl_mib={:.2}",
        sessions * events,
        bytes as f64 / 1048576.0
    );
    println!(
        "build: debug_assertions={} arch={} os={}",
        cfg!(debug_assertions),
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    timed("initial_index_and_rare_search", 1, || {
        execute(
            &mut provider,
            &store,
            &ids[0],
            "session_search",
            json!({"query":"rareterm"}),
        )
    });
    println!(
        "index_mib={:.2}",
        std::fs::metadata(&index_path).unwrap().len() as f64 / 1048576.0
    );
    timed("warm_no_match_refresh", 10, || {
        execute(
            &mut provider,
            &store,
            &ids[0],
            "session_search",
            json!({"query":"doesnotexistanywhere"}),
        )
    });
    timed("warm_rare_grouped", 10, || {
        execute(
            &mut provider,
            &store,
            &ids[0],
            "session_search",
            json!({"query":"rareterm"}),
        )
    });
    let page = timed("warm_common_grouped", 10, || {
        execute(
            &mut provider,
            &store,
            &ids[0],
            "session_search",
            json!({"query":"commonterm","limit":1}),
        )
    });
    if page["next_cursor"].is_string() {
        timed("cached_next_page", 10, || {
            execute(
                &mut provider,
                &store,
                &ids[0],
                "session_search",
                json!({"query":"commonterm","limit":1,"cursor":page["next_cursor"]}),
            )
        });
    }
    timed("warm_common_single_session", 10, || {
        execute(
            &mut provider,
            &store,
            &ids[0],
            "session_event_search",
            json!({"query":"commonterm"}),
        )
    });
    timed("append_one_and_refresh", 5, || {
        let mut writer = OpenOptions::new().append(true).open(&paths[0]).unwrap();
        let id = ulid::Ulid::new().to_string();
        let mut record = event(0, events, "freshappend commonterm".into());
        record.id = id;
        serde_json::to_writer(&mut writer, &record).unwrap();
        writer.write_all(b"\n").unwrap();
        execute(
            &mut provider,
            &store,
            &ids[0],
            "session_search",
            json!({"query":"freshappend"}),
        )
    });
    let event_ref = format!("event-{:06}-{:09}", 0, events - 1);
    timed("full_large_event_read_all_pages", 5, || {
        let mut args = json!({"event_ref":event_ref});
        let mut total = 0;
        loop {
            let page = execute(
                &mut provider,
                &store,
                &ids[0],
                "session_event_read",
                args.clone(),
            );
            total += page["chunk"].as_str().unwrap().len();
            if page["next_cursor"].is_null() {
                break;
            }
            args["cursor"] = page["next_cursor"].clone();
        }
        json!({"bytes":total})
    });
    // Ensure the fixture stayed isolated and source logs remained present.
    assert!(File::open(&paths[0]).is_ok());
}
