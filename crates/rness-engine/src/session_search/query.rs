//! Incremental query API. Metadata is checked on search; only changed session
//! bodies are read/reindexed. Cursors are bound to scope, filters and revisions.
use super::SqliteSessionSearch;
use crate::session::{branch::SessionStore, projection::search_surfaces};
use rness_protocol::events::{Envelope, SessionEvent};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn invalid(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message).into()
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryRequest {
    pub query: String,
    pub session_id: Option<String>,
    pub event_ref: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub event_types: Vec<String>,
    pub surfaces: Vec<String>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
}

fn digest(value: impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}
fn offset(request: &QueryRequest, generation: &str) -> Result<usize> {
    let Some(cursor) = &request.cursor else { return Ok(0) };
    let (token, position) = cursor.split_once(':').ok_or_else(|| invalid("invalid cursor"))?;
    if token != generation { return Err(invalid("stale cursor or changed query; restart without cursor")); }
    let offset: usize = position.parse()?;
    if offset > i64::MAX as usize { return Err(invalid("cursor out of range")); }
    Ok(offset)
}
fn generation(caller: &str, workspace: &str, operation: &str, request: &QueryRequest, revision: &str) -> Result<String> {
    let mut request = request.clone();
    request.cursor = None;
    digest((caller, workspace, operation, request, revision))
}
fn page(items: Vec<Value>, more: bool, next: usize, generation: &str) -> Value {
    json!({"items": items, "next_cursor": more.then(|| format!("{generation}:{next}"))})
}
fn timestamp(value: &str) -> Result<i64> {
    Ok(value.parse::<jiff::Timestamp>()?.as_millisecond())
}

/// Extract human/searchable event data, excluding encoded images and duplicated
/// streaming chunks. Full event reads remain available separately.
fn searchable(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(text) => output.push(text.clone()),
        Value::Array(values) => for value in values { searchable(value, output); },
        Value::Object(fields) => for (key, value) in fields {
            if !matches!(key.as_str(), "chunks" | "data" | "base64" | "bytes" | "url") {
                searchable(value, output);
            }
        },
        _ => {},
    }
}

pub(super) struct SearchPage {
    scope: String,
    items: Vec<Value>,
    created: std::time::Instant,
    truncated: bool,
}

impl SqliteSessionSearch {
    fn cached_page(&self, store: &SessionStore, caller: &String, token: &str, start: usize, limit: usize, scope: &str) -> Result<Value> {
        let snapshot = self.pages.get(token).ok_or_else(|| invalid("search cursor expired; restart without cursor"))?;
        if snapshot.scope != scope || snapshot.created.elapsed() > std::time::Duration::from_secs(600) {
            return Err(invalid("search cursor expired or query changed"));
        }
        if start > snapshot.items.len() { return Err(invalid("cursor out of range")); }
        let items: Vec<_> = snapshot.items.iter().skip(start).take(limit).cloned().collect();
        for item in &items {
            let target = item["session_id"].as_str().ok_or_else(|| invalid("invalid cached result"))?.to_string();
            store.search_workspace(caller, Some(&target))?;
        }
        let mut result = page(items, snapshot.items.len() > start + limit, start + limit, token);
        result["snapshot_truncated"] = json!(snapshot.truncated);
        Ok(result)
    }

    fn query_connection(&mut self) -> Result<&mut Connection> {
        if self.connection.is_none() {
            let mut connection = Connection::open(&self.path)?;
            connection.busy_timeout(std::time::Duration::from_secs(5))?;
            let app: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
            let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            let tables: i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'", [], |r| r.get(0))?;
            if (app != 0x524e5351 || !matches!(version, 2 | 3)) && !(app == 0 && version == 0 && tables == 0) {
                return Err(invalid("unrecognized search database; choose a new derived index path"));
            }
            let tx = connection.transaction()?;
            tx.execute_batch("CREATE TABLE IF NOT EXISTS revisions (
                workspace TEXT NOT NULL, session TEXT NOT NULL, revision TEXT NOT NULL,
                PRIMARY KEY(workspace, session));
                CREATE VIRTUAL TABLE IF NOT EXISTS events USING fts5(
                    workspace UNINDEXED, session UNINDEXED, event UNINDEXED,
                    kind UNINDEXED, surface UNINDEXED, time UNINDEXED, body);
                PRAGMA application_id = 1380864849;
                PRAGMA user_version = 2;")?;
            if version < 3 {
                // Preserve the existing FTS rowids. Indexed metadata supports
                // scoped lookup/deletion without scanning FTS content columns.
                tx.execute_batch("CREATE TABLE event_scope (
                    rowid INTEGER PRIMARY KEY, workspace TEXT NOT NULL, session TEXT NOT NULL);
                    CREATE INDEX event_scope_session ON event_scope(workspace,session);
                    INSERT INTO event_scope SELECT rowid,workspace,session FROM events;
                    PRAGMA user_version = 3;")?;
            } else {
                tx.execute_batch("PRAGMA user_version = 3;")?;
            }
            tx.commit()?;
            self.connection = Some(connection);
        }
        Ok(self.connection.as_mut().unwrap())
    }

    fn refresh(&mut self, store: &SessionStore, caller: &String, workspace: &str) -> Result<()> {
        let connection = self.query_connection()?;
        let old = connection.prepare("SELECT session, revision FROM revisions WHERE workspace=?1")?
            .query_map([workspace], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        let mut revisions = Vec::new();
        for session in store.list()? {
            let revision = store.search_revision(&session)?;
            if store.workspace(&session)?.as_deref() == Some(workspace) {
                revisions.push((session, revision));
            }
        }
        let present: std::collections::HashSet<_> = revisions.iter().map(|(session, _)| session).collect();
        let tx = connection.transaction()?;
        for session in old.keys().filter(|id| !present.contains(id)) {
            tx.execute("DELETE FROM events WHERE rowid IN (SELECT rowid FROM event_scope WHERE workspace=?1 AND session=?2)", params![workspace, session])?;
            tx.execute("DELETE FROM event_scope WHERE workspace=?1 AND session=?2", params![workspace, session])?;
            tx.execute("DELETE FROM revisions WHERE workspace=?1 AND session=?2", params![workspace, session])?;
        }
        for (session, revision) in &revisions {
            if old.get(session) == Some(revision) { continue; }
            let events = store.search_events(caller, session)?;
            let surfaces = search_surfaces(&events);
            tx.execute("DELETE FROM events WHERE rowid IN (SELECT rowid FROM event_scope WHERE workspace=?1 AND session=?2)", params![workspace, session])?;
            tx.execute("DELETE FROM event_scope WHERE workspace=?1 AND session=?2", params![workspace, session])?;
            let mut scope_insert = tx.prepare("INSERT INTO event_scope(rowid,workspace,session) VALUES(?1,?2,?3)")?;
            let mut insert = tx.prepare("INSERT INTO events(workspace,session,event,kind,surface,time,body) VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
            for event in events {
                let value = serde_json::to_value(&event.event)?;
                let mut parts = Vec::new();
                searchable(&value, &mut parts);
                insert.execute(params![workspace, session, event.id, value["type"].as_str(),
                    surfaces[&event.id], timestamp(&event.at)?, parts.join("\n")])?;
                scope_insert.execute(params![tx.last_insert_rowid(), workspace, session])?;
            }
            drop(insert);
            tx.execute("INSERT INTO revisions VALUES(?1,?2,?3) ON CONFLICT(workspace,session) DO UPDATE SET revision=excluded.revision",
                params![workspace, session, revision])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Called on a blocking worker, never on the Lua actor. Authorization occurs
    /// on every operation, before opening SQLite or reading the target body.
    pub fn execute(&mut self, store: &SessionStore, caller: &String, operation: &str, request: QueryRequest) -> Result<Value> {
        let workspace = store.search_workspace(caller, request.session_id.as_ref())?;
        let limit = request.limit.unwrap_or(20);
        if !(1..=100).contains(&limit) { return Err(invalid("limit must be 1..100")); }
        if request.event_types.len() > 32 || request.surfaces.len() > 3 { return Err(invalid("too many filters")); }
        if request.surfaces.iter().any(|s| !matches!(s.as_str(), "current" | "shadowed" | "log-only")) {
            return Err(invalid("unknown event surface"));
        }
        if request.cursor.as_ref().is_some_and(|v| v.len() > 128) { return Err(invalid("invalid cursor")); }
        let from = request.time_from.as_deref().map(timestamp).transpose()?;
        let to = request.time_to.as_deref().map(timestamp).transpose()?;
        if from.zip(to).is_some_and(|(a,b)| a > b) { return Err(invalid("time_from exceeds time_to")); }
        match operation {
            "session_search" | "session_event_search" => {
                if request.query.trim().is_empty() || request.query.len() > 4096 { return Err(invalid("query must contain 1..4096 bytes of text")); }
                let scope = generation(caller, &workspace, operation, &request, "snapshot")?;
                if let Some(cursor) = &request.cursor {
                    let (token, start) = cursor.split_once(':').ok_or_else(|| invalid("invalid cursor"))?;
                    return self.cached_page(store, caller, token, start.parse()?, limit, &scope);
                }
                self.refresh(store, caller, &workspace)?;
                let session = if operation == "session_event_search" { Some(request.session_id.as_ref().unwrap_or(caller)) } else { request.session_id.as_ref() };
                let phrase = format!("\"{}\"", request.query.trim().replace('"', "\"\""));
                let connection = self.query_connection()?;
                // Ranking and snippets must observe the same rows if another
                // process refreshes this shared derived index concurrently.
                let read = connection.transaction()?;
                // Rank narrow rows first. Snippets are evaluated separately for
                // the bounded winners, never for the full matching corpus.
                let source = if session.is_some() {
                    "events CROSS JOIN event_scope AS scope ON scope.rowid=events.rowid
                     WHERE events MATCH ?1 AND scope.workspace=?2 AND scope.session=?3"
                } else {
                    "events WHERE events MATCH ?1 AND workspace=?2 AND (?3 IS NULL OR session=?3)"
                };
                let selection = if operation == "session_search" {
                    "SELECT * FROM (SELECT *, row_number() OVER (PARTITION BY session ORDER BY score,event) AS n FROM matches) WHERE n=1"
                } else {
                    "SELECT * FROM matches"
                };
                let sql = format!("WITH matches AS MATERIALIZED (
                    SELECT events.rowid AS id,events.session,events.event,kind,surface,time,rank AS score
                    FROM {source}
                        AND (?4='[]' OR kind IN (SELECT value FROM json_each(?4)))
                        AND (?5='[]' OR surface IN (SELECT value FROM json_each(?5)))
                        AND (?6 IS NULL OR CAST(time AS INTEGER)>=?6)
                        AND (?7 IS NULL OR CAST(time AS INTEGER)<=?7)
                ), selected AS ({selection})
                SELECT session,event,kind,surface,time,id FROM selected
                ORDER BY score,session,event LIMIT 1001");
                let mut statement = read.prepare(&sql)?;
                let candidates = statement.query_map(params![phrase, workspace, session,
                    serde_json::to_string(&request.event_types)?, serde_json::to_string(&request.surfaces)?,
                    from, to], |row| {
                    Ok((row.get::<_,i64>(5)?, json!({"session_id": row.get::<_,String>(0)?, "event_ref": row.get::<_,String>(1)?,
                        "event_type": row.get::<_,String>(2)?, "surface": row.get::<_,String>(3)?,
                        "time_ms": row.get::<_,i64>(4)?})))
                })?.collect::<rusqlite::Result<Vec<_>>>()?;
                let truncated = candidates.len() > 1000;
                let rowids: Vec<_> = candidates.iter().take(1000).map(|(id, _)| *id).collect();
                // One FTS cursor for all selected snippets. Reopening MATCH once
                // per row is expensive for frequent terms even with rowid bounds.
                let mut snippet = read.prepare("SELECT rowid,substr(snippet(events,6,'','',' … ',32),1,512) FROM events WHERE events MATCH ?1 AND rowid IN (SELECT value FROM json_each(?2))")?;
                let mut excerpts = snippet.query_map(params![phrase, serde_json::to_string(&rowids)?], |row| {
                    Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?))
                })?.collect::<rusqlite::Result<HashMap<_,_>>>()?;
                let mut items = Vec::with_capacity(candidates.len().min(1000));
                for (rowid, mut item) in candidates.into_iter().take(1000) {
                    item["snippet"] = json!(excerpts.remove(&rowid).ok_or_else(|| invalid("search index changed while reading snippets; retry"))?);
                    items.push(item);
                }
                drop(snippet);
                drop(statement);
                read.commit()?;
                self.pages.retain(|_, page| page.created.elapsed() < std::time::Duration::from_secs(600));
                if self.pages.len() >= 8 {
                    if let Some(oldest) = self.pages.iter().min_by_key(|(_, page)| page.created).map(|(id, _)| id.clone()) {
                        self.pages.remove(&oldest);
                    }
                }
                let token = ulid::Ulid::new().to_string();
                self.pages.insert(token.clone(), SearchPage { scope: scope.clone(), items, created: std::time::Instant::now(), truncated });
                self.cached_page(store, caller, &token, 0, limit, &scope)
            }
            "session_event_read" | "session_event_trace" | "session_trace" => {
                let target = request.session_id.as_ref().unwrap_or(caller);
                store.search_revision(target)?; // refuse symlinked paths
                let events = store.search_events(caller, target)?;
                if operation == "session_event_read" {
                    let id = request.event_ref.as_ref().ok_or_else(|| invalid("event_ref is required"))?;
                    let event = events.iter().find(|e| &e.id == id).ok_or_else(|| invalid("event not found"))?;
                    let generation = generation(caller, &workspace, operation, &request, &digest(event)?)?;
                    let start = offset(&request, &generation)?;
                    let text = serde_json::to_string(event)?;
                    // Character offsets cannot split UTF-8. Every read returns at
                    // most 8192 characters (<=32KiB) plus its continuation token.
                    let mut chars = text.chars().skip(start);
                    let chunk: String = chars.by_ref().take(8192).collect();
                    let next = start + chunk.chars().count();
                    Ok(json!({"session_id": target, "event_ref": id, "encoding":"json",
                        "chunk":chunk,"next_cursor":chars.next().map(|_| format!("{generation}:{next}"))}))
                } else {
                    let edges = trace(&events, request.event_ref.as_deref(), operation == "session_trace")?;
                    let generation = generation(caller, &workspace, operation, &request, &digest(&edges)?)?;
                    let start = offset(&request, &generation)?;
                    let more = edges.len() > start.saturating_add(limit);
                    Ok(page(edges.into_iter().skip(start).take(limit).collect(), more, start + limit, &generation))
                }
            }
            _ => Err(invalid("unknown session query operation")),
        }
    }
}

/// Direct durable relationships only; no inferred fuzzy provenance. Target
/// session authorization does not grant access to parent-session content.
fn trace(events: &[Envelope], event_ref: Option<&str>, session_trace: bool) -> Result<Vec<Value>> {
    if !session_trace && !events.iter().any(|e| Some(e.id.as_str()) == event_ref) {
        return Err(invalid("event_ref not found"));
    }
    let mut edges = Vec::new();
    for event in events {
        let mut add = |kind: &str, target: &str| {
            if session_trace || event_ref == Some(event.id.as_str()) || event_ref == Some(target) {
                edges.push(json!({"from":event.id,"relationship":kind,"to":target}));
            }
        };
        match &event.event {
            SessionEvent::Compaction(c) => for target in &c.replaces { add("replaces", target); },
            SessionEvent::Prune(p) => add("replaces", &p.replaces),
            SessionEvent::CompactionStarted { sources, .. } => for target in sources { add("source", target); },
            SessionEvent::CompactionRequest { started, .. } | SessionEvent::CompactionFinished { started, .. } => add("started", started),
            SessionEvent::Header(header) if session_trace => {
                if let Some(parent) = &header.parent {
                    edges.push(json!({"relationship":"fork","parent":parent}));
                }
            },
            _ => {},
        }
    }
    Ok(edges)
}
