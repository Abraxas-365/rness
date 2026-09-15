//! Lazy, derived session search index. JSONL remains the source of truth.
//!
//! The opt-in plugin owns this provider. Constructing it performs no I/O;
//! only `search` opens SQLite and requests source content.

use std::path::PathBuf;

use rusqlite::{Connection, params};

mod query;
pub use query::QueryRequest;

/// One searchable message, with a stable reference into its source session.
#[derive(Debug, serde::Serialize)]
pub struct SearchDocument {
    pub session_id: String,
    pub event_ref: String,
    pub text: String,
}

/// A workspace snapshot. The adapter must change `revision` whenever any
/// searchable content or session membership changes, including deletions.
pub struct SearchSnapshot {
    pub revision: String,
    pub documents: Vec<SearchDocument>,
}

#[derive(Debug, serde::Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub event_ref: String,
    pub snippet: String,
}

pub struct SqliteSessionSearch {
    path: PathBuf,
    connection: Option<Connection>,
    pages: std::collections::HashMap<String, query::SearchPage>,
}

impl SqliteSessionSearch {
    /// Merely retains a path: does not create directories, open a database,
    /// or enumerate session logs. The parent directory must already exist.
    pub fn new(path: PathBuf) -> Self {
        Self { path, connection: None, pages: Default::default() }
    }

    fn connection(&mut self) -> rusqlite::Result<&mut Connection> {
        if self.connection.is_none() {
            let connection = Connection::open(&self.path)?;
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS workspace_revisions (
                    workspace TEXT PRIMARY KEY,
                    revision TEXT NOT NULL
                );
                CREATE VIRTUAL TABLE IF NOT EXISTS session_messages USING fts5(
                    workspace UNINDEXED,
                    session_id UNINDEXED,
                    event_ref UNINDEXED,
                    body
                );",
            )?;
            self.connection = Some(connection);
        }
        Ok(self.connection.as_mut().expect("connection initialized"))
    }

    /// Searches a literal phrase, not arbitrary FTS syntax.
    ///
    /// `workspace` must come from the trusted caller identity, not tool input.
    /// `refresh` receives the last indexed revision and returns `None` when
    /// unchanged, avoiding index writes on repeated searches. A new
    /// snapshot replaces this workspace atomically, also removing stale rows.
    /// Source failures leave the old revision intact and fail the search.
    pub fn search<E>(
        &mut self,
        workspace: &str,
        query: &str,
        limit: usize,
        refresh: impl FnOnce(Option<&str>) -> Result<Option<SearchSnapshot>, E>,
    ) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        self.search_in_session(workspace, None, query, limit, refresh)
    }

    /// As `search`, with an optional source-session filter applied before LIMIT.
    /// The caller must authorize the target session before calling this method.
    pub fn search_in_session<E>(
        &mut self,
        workspace: &str,
        session_id: Option<&str>,
        query: &str,
        limit: usize,
        refresh: impl FnOnce(Option<&str>) -> Result<Option<SearchSnapshot>, E>,
    ) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        use rusqlite::OptionalExtension;

        if workspace.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "session search requires a caller workspace",
            ).into());
        }
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let connection = self.connection()?;
        let revision: Option<String> = connection.query_row(
            "SELECT revision FROM workspace_revisions WHERE workspace = ?1",
            [workspace],
            |row| row.get(0),
        ).optional()?;
        if let Some(snapshot) = refresh(revision.as_deref())? {
            let transaction = connection.transaction()?;
            transaction.execute(
                "DELETE FROM session_messages WHERE workspace = ?1",
                [workspace],
            )?;
            {
                let mut insert = transaction.prepare(
                    "INSERT INTO session_messages (workspace, session_id, event_ref, body)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for document in snapshot.documents {
                    insert.execute(params![
                        workspace, document.session_id, document.event_ref, document.text,
                    ])?;
                }
            }
            transaction.execute(
                "INSERT INTO workspace_revisions (workspace, revision) VALUES (?1, ?2)
                 ON CONFLICT(workspace) DO UPDATE SET revision = excluded.revision",
                params![workspace, snapshot.revision],
            )?;
            transaction.commit()?;
        } else if revision.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session search source omitted the initial snapshot",
            ).into());
        }

        let phrase = format!("\"{}\"", query.trim().replace('"', "\"\""));
        let mut statement = connection.prepare(
            "SELECT session_id, event_ref,
                    snippet(session_messages, 3, '', '', ' … ', 32)
             FROM session_messages
             WHERE session_messages MATCH ?1 AND workspace = ?2
               AND (?4 IS NULL OR session_id = ?4)
             ORDER BY bm25(session_messages), session_id, event_ref
             LIMIT ?3",
        )?;
        let hits = statement.query_map(params![phrase, workspace, limit.min(100) as i64, session_id], |row| {
            Ok(SearchHit {
                session_id: row.get(0)?,
                event_ref: row.get(1)?,
                snippet: row.get(2)?,
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(hits)
    }
}
