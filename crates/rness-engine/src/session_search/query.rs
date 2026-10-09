//! Incremental query API. Metadata is checked on search; only changed session
//! bodies are read/reindexed. Cursors are bound to scope, filters and revisions.
use super::SqliteSessionSearch;
use crate::session::{
    branch::{BranchError, SearchProbe, SessionStore},
    projection::{fast_surface, search_surfaces},
};
use rness_protocol::events::{Envelope, SessionEvent};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Derived index layout. v4: `surface` moved from the FTS row to
/// `event_scope`, per-session `session_cursor` for append-only refresh.
///
/// The standard index lives in a NEW file ([`super::INDEX_FILE_NAME`]), so
/// processes running an older build keep using their own v2/v3 file and this
/// build never rewrites it. Only a custom path holding a v2/v3 index is
/// migrated in place (drop + rebuild, under the write lock).
const SCHEMA_VERSION: i64 = 4;
const APPLICATION_ID: i64 = 0x524e5351;

/// First build of a v4 index: say so if an earlier build's (possibly huge)
/// index file sits next to it. It is never deleted automatically.
fn note_legacy_indexes(path: &std::path::Path) {
    let Some(dir) = path.parent() else { return };
    for name in super::LEGACY_INDEX_FILE_NAMES {
        let legacy = dir.join(name);
        if let Ok(meta) = std::fs::metadata(&legacy) {
            tracing::info!(
                path = %legacy.display(),
                bytes = meta.len(),
                "created a new session search index; this older index is no longer used \
                 and can be deleted once no older rness is running"
            );
        }
    }
}
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
    let Some(cursor) = &request.cursor else {
        return Ok(0);
    };
    let (token, position) = cursor
        .split_once(':')
        .ok_or_else(|| invalid("invalid cursor"))?;
    if token != generation {
        return Err(invalid(
            "stale cursor or changed query; restart without cursor",
        ));
    }
    let offset: usize = position.parse()?;
    if offset > i64::MAX as usize {
        return Err(invalid("cursor out of range"));
    }
    Ok(offset)
}
fn generation(
    caller: &str,
    workspace: &str,
    operation: &str,
    request: &QueryRequest,
    revision: &str,
) -> Result<String> {
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
        Value::Array(values) => {
            for value in values {
                searchable(value, output);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                if !matches!(key.as_str(), "chunks" | "data" | "base64" | "bytes" | "url") {
                    searchable(value, output);
                }
            }
        }
        _ => {}
    }
}

pub(super) struct SearchPage {
    scope: String,
    items: Vec<Value>,
    created: std::time::Instant,
    truncated: bool,
}

impl SqliteSessionSearch {
    fn cached_page(
        &self,
        store: &SessionStore,
        caller: &String,
        token: &str,
        start: usize,
        limit: usize,
        scope: &str,
    ) -> Result<Value> {
        let snapshot = self
            .pages
            .get(token)
            .ok_or_else(|| invalid("search cursor expired; restart without cursor"))?;
        if snapshot.scope != scope
            || snapshot.created.elapsed() > std::time::Duration::from_secs(600)
        {
            return Err(invalid("search cursor expired or query changed"));
        }
        if start > snapshot.items.len() {
            return Err(invalid("cursor out of range"));
        }
        let items: Vec<_> = snapshot
            .items
            .iter()
            .skip(start)
            .take(limit)
            .cloned()
            .collect();
        for item in &items {
            let target = item["session_id"]
                .as_str()
                .ok_or_else(|| invalid("invalid cached result"))?
                .to_string();
            store.search_workspace(caller, Some(&target))?;
        }
        let mut result = page(
            items,
            snapshot.items.len() > start + limit,
            start + limit,
            token,
        );
        result["snapshot_truncated"] = json!(snapshot.truncated);
        Ok(result)
    }

    fn query_connection(&mut self) -> Result<&mut Connection> {
        if self.connection.is_none() {
            let mut connection = Connection::open(&self.path)?;
            connection.busy_timeout(std::time::Duration::from_secs(5))?;
            // Long FTS queries and index writes abort with SQLITE_INTERRUPT
            // once the current operation's token is cancelled.
            let cancel = std::sync::Arc::clone(&self.cancel);
            connection.progress_handler(
                10_000,
                Some(move || cancel.lock().is_ok_and(|token| token.is_cancelled())),
            );
            let header = |connection: &Connection| -> Result<(i64, i64, i64)> {
                Ok((
                    connection.query_row("PRAGMA application_id", [], |r| r.get(0))?,
                    connection.query_row("PRAGMA user_version", [], |r| r.get(0))?,
                    connection.query_row(
                        "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                        [],
                        |r| r.get(0),
                    )?,
                ))
            };
            let (app, version, _) = header(&connection)?;
            if app == APPLICATION_ID && version == SCHEMA_VERSION {
                // Current layout (the normal case): no write lock taken.
                self.connection = Some(connection);
                return Ok(self.connection.as_mut().unwrap());
            }
            // Anything else is decided under the write lock: another process
            // may be migrating or creating the same file right now, so the
            // header is read again inside the transaction.
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (app, version, tables) = header(&tx)?;
            if (app != APPLICATION_ID || !matches!(version, 2..=SCHEMA_VERSION))
                && !(app == 0 && version == 0 && tables == 0)
            {
                return Err(invalid(
                    "unrecognized search database; choose a new derived index path",
                ));
            }
            if app == 0 {
                note_legacy_indexes(&self.path);
            }
            if (2..SCHEMA_VERSION).contains(&version) {
                // Only reachable for a custom index path: the standard path
                // ([`INDEX_FILE_NAME`]) is a new file, so shipped v2/v3
                // indexes are never opened, let alone dropped, by this build.
                // v2/v3 kept `surface` inside the FTS row, so re-labelling an
                // event rewrote its body. The index is derived: drop it and
                // let the next refresh rebuild every session once.
                tx.execute_batch(
                    "DROP TABLE IF EXISTS events; DROP TABLE IF EXISTS event_scope;
                     DROP TABLE IF EXISTS revisions; DROP TABLE IF EXISTS session_cursor;",
                )?;
            }
            // `events` holds the searchable text only; mutable labels live in
            // `event_scope`, keyed by the FTS rowid. `session_cursor` records
            // how far each session log is indexed (byte offset, last line).
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS revisions (
                workspace TEXT NOT NULL, session TEXT NOT NULL, revision TEXT NOT NULL,
                PRIMARY KEY(workspace, session));
                CREATE VIRTUAL TABLE IF NOT EXISTS events USING fts5(
                    workspace UNINDEXED, session UNINDEXED, event UNINDEXED,
                    kind UNINDEXED, time UNINDEXED, body);
                CREATE TABLE IF NOT EXISTS event_scope (
                    rowid INTEGER PRIMARY KEY, workspace TEXT NOT NULL, session TEXT NOT NULL,
                    surface TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS event_scope_session ON event_scope(workspace,session);
                CREATE TABLE IF NOT EXISTS session_cursor (
                    workspace TEXT NOT NULL, session TEXT NOT NULL,
                    offset INTEGER NOT NULL, last_line_start INTEGER NOT NULL,
                    last_line_hash TEXT NOT NULL, header_hash TEXT NOT NULL,
                    identity TEXT NOT NULL, events INTEGER NOT NULL,
                    PRIMARY KEY(workspace, session));
                PRAGMA application_id = 1380864849;",
            )?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            self.connection = Some(connection);
        }
        Ok(self.connection.as_mut().unwrap())
    }

    /// Bring the index up to date with every session log of `workspace`.
    ///
    /// Per session (one SQLite transaction each, so cancelled refreshes keep
    /// their progress): unchanged revision → nothing; same file grown past
    /// its indexed prefix (identity, header and last indexed line verified)
    /// → index only the new complete lines; anything else, or a delta that
    /// holds a `Compaction`/`Prune` (they re-label earlier events) → rebuild
    /// that session. A session that cannot be read is dropped from the index
    /// with a warning instead of failing the whole search.
    fn refresh(
        &mut self,
        store: &SessionStore,
        caller: &String,
        workspace: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let cancelled = || -> Result<()> {
            if cancel.is_cancelled() {
                Err(invalid("session search cancelled"))
            } else {
                Ok(())
            }
        };
        let connection = self.query_connection()?;
        let old = connection
            .prepare("SELECT session, revision FROM revisions WHERE workspace=?1")?
            .query_map([workspace], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        let mut probes = Vec::new();
        for session in store.list()? {
            let in_workspace = store
                .workspace(&session)
                .map(|ws| ws.as_deref() == Some(workspace))
                .and_then(|member| member.then(|| store.search_probe(&session)).transpose());
            match in_workspace {
                Ok(Some(probe)) => probes.push((session, probe)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%session, %error, "session search skips an unreadable session")
                }
            }
        }
        let present: std::collections::HashSet<_> =
            probes.iter().map(|(session, _)| session).collect();
        let tx = connection.transaction()?;
        for session in old.keys().filter(|id| !present.contains(id)) {
            forget(&tx, workspace, session)?;
            tx.execute(
                "DELETE FROM revisions WHERE workspace=?1 AND session=?2",
                params![workspace, session],
            )?;
        }
        tx.commit()?;
        for (session, probe) in &probes {
            if old.get(session) == Some(&probe.revision) {
                continue;
            }
            cancelled()?;
            // Write lock first, then re-read this session's state: another
            // process sharing the index may have indexed it meanwhile, and
            // extending from a stale cursor would duplicate rows.
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let revision: Option<String> = tx
                .query_row(
                    "SELECT revision FROM revisions WHERE workspace=?1 AND session=?2",
                    params![workspace, session],
                    |row| row.get(0),
                )
                .optional()?;
            if revision.as_ref() == Some(&probe.revision) {
                continue;
            }
            let cursor = tx
                .query_row(
                    "SELECT offset,last_line_start,last_line_hash,header_hash,identity,events
                     FROM session_cursor WHERE workspace=?1 AND session=?2",
                    params![workspace, session],
                    |row| {
                        Ok(Cursor {
                            offset: row.get::<_, i64>(0)? as u64,
                            last_line_start: row.get::<_, i64>(1)? as u64,
                            last_line_hash: row.get(2)?,
                            header_hash: row.get(3)?,
                            identity: row.get(4)?,
                            events: row.get(5)?,
                        })
                    },
                )
                .optional()?;
            let outcome = match cursor {
                Some(cursor) if cursor.identity == probe.identity && probe.len >= cursor.offset => {
                    extend(
                        store, caller, &tx, workspace, session, probe, &cursor, &cancelled,
                    )
                }
                _ => Ok(Extended::Rebuild),
            };
            let outcome = match outcome {
                Ok(Extended::Rebuild) => {
                    rebuild(store, caller, &tx, workspace, session, probe, &cancelled)
                }
                other => other.map(|_| ()),
            };
            match outcome {
                Ok(()) => {}
                Err(error) if error.downcast_ref::<BranchError>().is_some() => {
                    // Unreadable now (corrupt, newer format, replaced by
                    // another session…): serve nothing stale from it, and do
                    // not re-read it until the file changes.
                    tracing::warn!(%session, %error, "session search skips an unreadable session");
                    drop(tx);
                    let tx = connection.transaction()?;
                    forget(&tx, workspace, session)?;
                    set_revision(&tx, workspace, session, &probe.revision)?;
                    tx.commit()?;
                    continue;
                }
                Err(error) => return Err(error),
            }
            set_revision(&tx, workspace, session, &probe.revision)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Called on a blocking worker, never on the Lua actor. Authorization occurs
    /// on every operation, before opening SQLite or reading the target body.
    pub fn execute(
        &mut self,
        store: &SessionStore,
        caller: &String,
        operation: &str,
        request: QueryRequest,
    ) -> Result<Value> {
        self.execute_cancellable(store, caller, operation, request, &CancellationToken::new())
    }

    /// As [`Self::execute`]; `cancel` stops an index refresh between rows
    /// (sessions already indexed stay indexed) and interrupts SQLite queries.
    pub fn execute_cancellable(
        &mut self,
        store: &SessionStore,
        caller: &String,
        operation: &str,
        request: QueryRequest,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        *self.cancel.lock().expect("search cancel lock") = cancel.clone();
        let result = self.execute_inner(store, caller, operation, request, cancel);
        *self.cancel.lock().expect("search cancel lock") = CancellationToken::new();
        result
    }

    fn execute_inner(
        &mut self,
        store: &SessionStore,
        caller: &String,
        operation: &str,
        request: QueryRequest,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let workspace = store.search_workspace(caller, request.session_id.as_ref())?;
        let limit = request.limit.unwrap_or(20);
        if !(1..=100).contains(&limit) {
            return Err(invalid("limit must be 1..100"));
        }
        if request.event_types.len() > 32 || request.surfaces.len() > 3 {
            return Err(invalid("too many filters"));
        }
        if request
            .surfaces
            .iter()
            .any(|s| !matches!(s.as_str(), "current" | "shadowed" | "log-only"))
        {
            return Err(invalid("unknown event surface"));
        }
        if request.cursor.as_ref().is_some_and(|v| v.len() > 128) {
            return Err(invalid("invalid cursor"));
        }
        let from = request.time_from.as_deref().map(timestamp).transpose()?;
        let to = request.time_to.as_deref().map(timestamp).transpose()?;
        if from.zip(to).is_some_and(|(a, b)| a > b) {
            return Err(invalid("time_from exceeds time_to"));
        }
        match operation {
            "session_search" | "session_event_search" => {
                if request.query.trim().is_empty() || request.query.len() > 4096 {
                    return Err(invalid("query must contain 1..4096 bytes of text"));
                }
                let scope = generation(caller, &workspace, operation, &request, "snapshot")?;
                if let Some(cursor) = &request.cursor {
                    let (token, start) = cursor
                        .split_once(':')
                        .ok_or_else(|| invalid("invalid cursor"))?;
                    return self.cached_page(store, caller, token, start.parse()?, limit, &scope);
                }
                self.refresh(store, caller, &workspace, cancel)?;
                let session = if operation == "session_event_search" {
                    Some(request.session_id.as_ref().unwrap_or(caller))
                } else {
                    request.session_id.as_ref()
                };
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
                    "events CROSS JOIN event_scope AS scope ON scope.rowid=events.rowid
                     WHERE events MATCH ?1 AND events.workspace=?2 AND (?3 IS NULL OR events.session=?3)"
                };
                let selection = if operation == "session_search" {
                    "SELECT * FROM (SELECT *, row_number() OVER (PARTITION BY session ORDER BY score,event) AS n FROM matches) WHERE n=1"
                } else {
                    "SELECT * FROM matches"
                };
                let sql = format!("WITH matches AS MATERIALIZED (
                    SELECT events.rowid AS id,events.session,events.event,kind,scope.surface,time,rank AS score
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
                let mut snippet = read.prepare("SELECT rowid,substr(snippet(events,5,'','',' … ',32),1,512) FROM events WHERE events MATCH ?1 AND rowid IN (SELECT value FROM json_each(?2))")?;
                let mut excerpts = snippet
                    .query_map(params![phrase, serde_json::to_string(&rowids)?], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<HashMap<_, _>>>()?;
                let mut items = Vec::with_capacity(candidates.len().min(1000));
                for (rowid, mut item) in candidates.into_iter().take(1000) {
                    item["snippet"] = json!(excerpts.remove(&rowid).ok_or_else(|| invalid(
                        "search index changed while reading snippets; retry"
                    ))?);
                    items.push(item);
                }
                drop(snippet);
                drop(statement);
                read.commit()?;
                self.pages
                    .retain(|_, page| page.created.elapsed() < std::time::Duration::from_secs(600));
                if self.pages.len() >= 8 {
                    if let Some(oldest) = self
                        .pages
                        .iter()
                        .min_by_key(|(_, page)| page.created)
                        .map(|(id, _)| id.clone())
                    {
                        self.pages.remove(&oldest);
                    }
                }
                let token = ulid::Ulid::new().to_string();
                self.pages.insert(
                    token.clone(),
                    SearchPage {
                        scope: scope.clone(),
                        items,
                        created: std::time::Instant::now(),
                        truncated,
                    },
                );
                self.cached_page(store, caller, &token, 0, limit, &scope)
            }
            "session_event_read" | "session_event_trace" | "session_trace" => {
                let target = request.session_id.as_ref().unwrap_or(caller);
                store.search_revision(target)?; // refuse symlinked paths
                let events = store.search_events(caller, target)?;
                if operation == "session_event_read" {
                    let id = request
                        .event_ref
                        .as_ref()
                        .ok_or_else(|| invalid("event_ref is required"))?;
                    let event = events
                        .iter()
                        .find(|e| &e.id == id)
                        .ok_or_else(|| invalid("event not found"))?;
                    let generation =
                        generation(caller, &workspace, operation, &request, &digest(event)?)?;
                    let start = offset(&request, &generation)?;
                    let text = serde_json::to_string(event)?;
                    // Character offsets cannot split UTF-8. Every read returns at
                    // most 8192 characters (<=32KiB) plus its continuation token.
                    let mut chars = text.chars().skip(start);
                    let chunk: String = chars.by_ref().take(8192).collect();
                    let next = start + chunk.chars().count();
                    Ok(
                        json!({"session_id": target, "event_ref": id, "encoding":"json",
                        "chunk":chunk,"next_cursor":chars.next().map(|_| format!("{generation}:{next}"))}),
                    )
                } else {
                    let edges = trace(
                        &events,
                        request.event_ref.as_deref(),
                        operation == "session_trace",
                    )?;
                    let generation =
                        generation(caller, &workspace, operation, &request, &digest(&edges)?)?;
                    let start = offset(&request, &generation)?;
                    let more = edges.len() > start.saturating_add(limit);
                    Ok(page(
                        edges.into_iter().skip(start).take(limit).collect(),
                        more,
                        start + limit,
                        &generation,
                    ))
                }
            }
            _ => Err(invalid("unknown session query operation")),
        }
    }
}

/// How far a session log is indexed. `last_line_hash` covers bytes
/// `[last_line_start, offset)` and `header_hash` the first line: both are
/// re-checked before trusting the indexed prefix of a grown file.
struct Cursor {
    offset: u64,
    last_line_start: u64,
    last_line_hash: String,
    header_hash: String,
    identity: String,
    events: i64,
}

enum Extended {
    Done,
    Rebuild,
}

fn forget(tx: &rusqlite::Transaction<'_>, workspace: &str, session: &str) -> Result<()> {
    tx.execute("DELETE FROM events WHERE rowid IN (SELECT rowid FROM event_scope WHERE workspace=?1 AND session=?2)", params![workspace, session])?;
    tx.execute(
        "DELETE FROM event_scope WHERE workspace=?1 AND session=?2",
        params![workspace, session],
    )?;
    tx.execute(
        "DELETE FROM session_cursor WHERE workspace=?1 AND session=?2",
        params![workspace, session],
    )?;
    Ok(())
}

fn set_revision(
    tx: &rusqlite::Transaction<'_>,
    workspace: &str,
    session: &str,
    revision: &str,
) -> Result<()> {
    tx.execute("INSERT INTO revisions VALUES(?1,?2,?3) ON CONFLICT(workspace,session) DO UPDATE SET revision=excluded.revision",
        params![workspace, session, revision])?;
    Ok(())
}

/// Insert `events` (with their surfaces) as new rows of `session`.
fn insert_rows<'e>(
    tx: &rusqlite::Transaction<'_>,
    workspace: &str,
    session: &str,
    events: impl Iterator<Item = (&'e Envelope, &'static str)>,
    cancelled: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let mut scope_insert = tx.prepare_cached(
        "INSERT INTO event_scope(rowid,workspace,session,surface) VALUES(?1,?2,?3,?4)",
    )?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO events(workspace,session,event,kind,time,body) VALUES(?1,?2,?3,?4,?5,?6)",
    )?;
    for (index, (event, surface)) in events.enumerate() {
        if index % 512 == 0 {
            cancelled()?;
        }
        let value = serde_json::to_value(&event.event)?;
        let mut parts = Vec::new();
        searchable(&value, &mut parts);
        insert.execute(params![
            workspace,
            session,
            event.id,
            value["type"].as_str(),
            timestamp(&event.at)?,
            parts.join("\n")
        ])?;
        scope_insert.execute(params![tx.last_insert_rowid(), workspace, session, surface])?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn save_cursor(
    store: &SessionStore,
    tx: &rusqlite::Transaction<'_>,
    workspace: &str,
    session: &String,
    probe: &SearchProbe,
    header_hash: String,
    last_line_start: u64,
    offset: u64,
    events: i64,
) -> Result<()> {
    let last_line_hash = store.search_range_digest(session, last_line_start, offset)?;
    tx.execute(
        "INSERT INTO session_cursor VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(workspace,session) DO UPDATE SET offset=excluded.offset,
         last_line_start=excluded.last_line_start, last_line_hash=excluded.last_line_hash,
         header_hash=excluded.header_hash, identity=excluded.identity, events=excluded.events",
        params![
            workspace,
            session,
            offset as i64,
            last_line_start as i64,
            last_line_hash,
            header_hash,
            probe.identity,
            events
        ],
    )?;
    Ok(())
}

/// Index only what was appended after `cursor`, if the indexed prefix is
/// verifiably unchanged and the delta cannot re-label earlier events.
#[allow(clippy::too_many_arguments)]
fn extend(
    store: &SessionStore,
    caller: &String,
    tx: &rusqlite::Transaction<'_>,
    workspace: &str,
    session: &String,
    probe: &SearchProbe,
    cursor: &Cursor,
    cancelled: &dyn Fn() -> Result<()>,
) -> Result<Extended> {
    // Same file, same first line, same last indexed line: the prefix is
    // the one indexed (an in-place edit elsewhere in the prefix would break
    // the append-only contract, invariants #2/#3, and is not detected).
    if store.search_header_digest(session)? != cursor.header_hash
        || store.search_range_digest(session, cursor.last_line_start, cursor.offset)?
            != cursor.last_line_hash
    {
        return Ok(Extended::Rebuild);
    }
    let (events, end) = store.search_events_from(caller, session, cursor.offset, probe.len)?;
    let mut labelled = Vec::with_capacity(events.len());
    for (event, _) in &events {
        match fast_surface(&event.event) {
            Some(surface) => labelled.push((event, surface)),
            None => return Ok(Extended::Rebuild),
        }
    }
    insert_rows(tx, workspace, session, labelled.into_iter(), cancelled)?;
    if let Some((_, last_start)) = events.last() {
        save_cursor(
            store,
            tx,
            workspace,
            session,
            probe,
            cursor.header_hash.clone(),
            *last_start,
            end,
            cursor.events + events.len() as i64,
        )?;
    }
    Ok(Extended::Done)
}

/// Re-index a whole session log.
fn rebuild(
    store: &SessionStore,
    caller: &String,
    tx: &rusqlite::Transaction<'_>,
    workspace: &str,
    session: &String,
    probe: &SearchProbe,
    cancelled: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let header_hash = store.search_header_digest(session)?;
    let (events, end) = store.search_events_from(caller, session, 0, probe.len)?;
    let envelopes: Vec<&Envelope> = events.iter().map(|(event, _)| event).collect();
    let surfaces = search_surfaces(&envelopes);
    forget(tx, workspace, session)?;
    insert_rows(
        tx,
        workspace,
        session,
        envelopes.iter().map(|event| (*event, surfaces[&event.id])),
        cancelled,
    )?;
    let last_start = events.last().map_or(0, |(_, start)| *start);
    save_cursor(
        store,
        tx,
        workspace,
        session,
        probe,
        header_hash,
        last_start,
        end,
        events.len() as i64,
    )
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
            SessionEvent::Compaction(c) => {
                for target in &c.replaces {
                    add("replaces", target);
                }
            }
            SessionEvent::Prune(p) => add("replaces", &p.replaces),
            SessionEvent::CompactionStarted { sources, .. } => {
                for target in sources {
                    add("source", target);
                }
            }
            SessionEvent::CompactionRequest { started, .. }
            | SessionEvent::CompactionFinished { started, .. } => add("started", started),
            SessionEvent::Header(header) if session_trace => {
                if let Some(parent) = &header.parent {
                    edges.push(json!({"relationship":"fork","parent":parent}));
                }
            }
            _ => {}
        }
    }
    Ok(edges)
}
