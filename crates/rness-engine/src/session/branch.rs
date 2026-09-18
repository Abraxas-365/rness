//! Branch operations over the session store: fork, ancestry, children,
//! and full-history assembly across the fork chain.
//!
//! A fork is a NEW session whose header carries `{parent, at}`. History
//! before the fork point is read from ancestors at query time — replayed,
//! never copied (parents stay immutable, invariant #3). Lineage is
//! acyclic by construction: a fork can only reference an event already
//! committed in the parent (invariant #4), which existed before the child.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use rness_protocol::branch::{AncestryHop, ChildRef, Delegation, ForkRef};
use rness_protocol::events::{Envelope, EventId, Header, SessionEvent, SessionId, FORMAT_VERSION};

use super::log::{LogError, SessionLog};

#[derive(Debug, thiserror::Error)]
pub enum BranchError {
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("fork point '{at}' not found in session '{session}'")]
    ForkPointNotFound { session: SessionId, at: EventId },
    #[error("cannot fork at the header of session '{0}'")]
    ForkAtHeader(SessionId),
}

/// Root directory holding one subdirectory per session.
pub struct SessionStore {
    root: PathBuf,
    readers: std::sync::Mutex<std::collections::HashMap<SessionId, super::log::SessionReader>>,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            readers: Default::default(),
        }
    }

    fn read_session(&self, session: &SessionId) -> Result<Vec<Envelope>, LogError> {
        let mut readers = self.readers.lock().unwrap();
        if readers.len() >= 8 && !readers.contains_key(session) {
            if let Some(oldest) = readers.keys().next().cloned() {
                readers.remove(&oldest);
            }
        }
        readers
            .entry(session.clone())
            .or_default()
            .read(&self.root, session)
    }

    /// Read through the first committed event only, without touching the
    /// history cache. Like SessionReader, skip blank lines and ignore torn tails.
    fn read_header(&self, session: &SessionId) -> Result<Header, LogError> {
        let path = super::log::log_file(&self.root.join(session));
        let file = std::fs::File::open(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                LogError::NotFound(session.clone())
            } else {
                LogError::Io(error)
            }
        })?;
        let mut reader = std::io::BufReader::new(file);
        let mut bytes = Vec::new();
        let mut line = 0;
        loop {
            line += 1;
            bytes.clear();
            if reader.read_until(b'\n', &mut bytes)? == 0 || !bytes.ends_with(b"\n") {
                return Err(LogError::Corrupt {
                    line: 0,
                    reason: "empty log".into(),
                });
            }
            if bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let envelope: Envelope =
                serde_json::from_slice(&bytes).map_err(|error| LogError::Corrupt {
                    line,
                    reason: error.to_string(),
                })?;
            let SessionEvent::Header(header) = envelope.event else {
                return Err(LogError::Corrupt {
                    line: 1,
                    reason: "first event is not session/header".into(),
                });
            };
            if header.version != FORMAT_VERSION {
                return Err(LogError::Corrupt {
                    line,
                    reason: format!("unsupported session header version {}", header.version),
                });
            }
            if &header.session != session {
                return Err(LogError::Corrupt {
                    line,
                    reason: format!(
                        "session header identity '{}' does not match '{session}'",
                        header.session
                    ),
                });
            }
            return Ok(header);
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create a root session (no parent).
    pub fn create(&self, workspace: Option<String>) -> Result<SessionLog, BranchError> {
        let sid = ulid::Ulid::new().to_string();
        Ok(SessionLog::create(&self.root, &sid, workspace, None, None)?)
    }

    /// Create a session on behalf of a delegating agent (subagent spawn):
    /// fresh history, delegation lineage stamped in the header.
    pub fn create_delegated(
        &self,
        workspace: Option<String>,
        delegation: Delegation,
    ) -> Result<SessionLog, BranchError> {
        let sid = ulid::Ulid::new().to_string();
        Ok(SessionLog::create(
            &self.root,
            &sid,
            workspace,
            None,
            Some(delegation),
        )?)
    }

    pub fn workspace(&self, session: &SessionId) -> Result<Option<String>, BranchError> {
        Ok(self.read_header(session)?.workspace)
    }

    pub fn open(&self, session: &SessionId) -> Result<SessionLog, BranchError> {
        Ok(SessionLog::open(&self.root, session)?)
    }

    /// Fork `session` at event `at` (or its tip when `None`). Returns the
    /// child's writable log. Validates the fork point exists and is not
    /// the header — forking "before any content" is just a new session.
    pub fn fork(
        &self,
        session: &SessionId,
        at: Option<EventId>,
    ) -> Result<SessionLog, BranchError> {
        self.fork_inner(session, at, None)
    }

    /// Fork on behalf of a delegating agent (subagent fork): seeded
    /// history AND delegation lineage.
    pub fn fork_delegated(
        &self,
        session: &SessionId,
        at: Option<EventId>,
        delegation: Delegation,
    ) -> Result<SessionLog, BranchError> {
        self.fork_inner(session, at, Some(delegation))
    }

    fn fork_inner(
        &self,
        session: &SessionId,
        at: Option<EventId>,
        delegation: Option<Delegation>,
    ) -> Result<SessionLog, BranchError> {
        let events = self.read_session(session)?;
        let at = match at {
            Some(id) => {
                let pos = events.iter().position(|e| e.id == id).ok_or_else(|| {
                    BranchError::ForkPointNotFound {
                        session: session.clone(),
                        at: id.clone(),
                    }
                })?;
                if pos == 0 {
                    return Err(BranchError::ForkAtHeader(session.clone()));
                }
                id
            }
            None => events.last().expect("log has header").id.clone(),
        };
        // Workspace is inherited from the parent header.
        let workspace = match &events[0].event {
            SessionEvent::Header(h) => h.workspace.clone(),
            _ => unreachable!("read_session guarantees header first"),
        };
        let child = ulid::Ulid::new().to_string();
        let fork = ForkRef {
            session: session.clone(),
            at,
        };
        Ok(SessionLog::create(
            &self.root,
            &child,
            workspace,
            Some(fork),
            delegation,
        )?)
    }

    /// Delegation lineage of a session, if an agent created it.
    pub fn delegation(&self, session: &SessionId) -> Result<Option<Delegation>, BranchError> {
        Ok(self.read_header(session)?.delegation)
    }

    /// Parent reference of a session, if it is a fork.
    pub fn parent(&self, session: &SessionId) -> Result<Option<ForkRef>, BranchError> {
        Ok(self.read_header(session)?.parent)
    }

    /// Ancestry chain, root first, queried session last. Each hop's
    /// `forked_at` is the fork point INTO the next hop.
    pub fn ancestry(&self, session: &SessionId) -> Result<Vec<AncestryHop>, BranchError> {
        let mut chain = vec![AncestryHop {
            session: session.clone(),
            forked_at: None,
        }];
        let mut cur = session.clone();
        while let Some(fork) = self.parent(&cur)? {
            chain.push(AncestryHop {
                session: fork.session.clone(),
                forked_at: Some(fork.at.clone()),
            });
            cur = fork.session;
        }
        chain.reverse();
        Ok(chain)
    }

    /// Direct children of `session`, optionally only those forked at `at`.
    /// Scans the store — fine at CLI scale; an index can replace this
    /// later (invariant #11: indexes stay disposable).
    pub fn children(
        &self,
        session: &SessionId,
        at: Option<&EventId>,
    ) -> Result<Vec<ChildRef>, BranchError> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(LogError::from)? {
            let entry = entry.map_err(LogError::from)?;
            if !entry.file_type().map_err(LogError::from)?.is_dir() {
                continue;
            }
            let child_id: SessionId = entry.file_name().to_string_lossy().into_owned();
            let Ok(Some(fork)) = self.parent(&child_id) else {
                continue;
            };
            if &fork.session == session && at.is_none_or(|a| a == &fork.at) {
                out.push(ChildRef {
                    session: child_id,
                    at: fork.at,
                });
            }
        }
        out.sort();
        Ok(out)
    }

    /// The session's complete history: ancestor prefixes up to each fork
    /// point, then its own events. Header envelopes of ancestors are
    /// dropped; the queried session's own header leads.
    pub fn history(&self, session: &SessionId) -> Result<Vec<Envelope>, BranchError> {
        let chain = self.ancestry(session)?;
        let mut out: Vec<Envelope> = Vec::new();
        for (i, hop) in chain.iter().enumerate() {
            let events = self.read_session(&hop.session)?;
            // This hop's prefix is bounded by where the next hop forked
            // from it — which is this hop's own `forked_at`.
            let bound = hop.forked_at.clone();
            let is_queried = i == chain.len() - 1;
            for env in events {
                let is_header = matches!(env.event, SessionEvent::Header(_));
                if is_header && !is_queried {
                    continue; // ancestor headers don't belong to this history
                }
                let id = env.id.clone();
                if is_queried && is_header {
                    // Queried session's header leads the history.
                    out.insert(0, env);
                } else {
                    out.push(env);
                }
                if let Some(b) = &bound {
                    if &id == b {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    /// Resolve search scope from a trusted execution session and authorize an
    /// optional model-selected target. Reject paths before touching the store.
    pub fn search_workspace(
        &self,
        caller: &SessionId,
        target: Option<&SessionId>,
    ) -> Result<String, BranchError> {
        for id in std::iter::once(caller).chain(target) {
            if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\']) {
                return Err(LogError::Corrupt {
                    line: 0,
                    reason: "invalid search session ID".into(),
                }
                .into());
            }
        }
        let workspace = self
            .workspace(caller)?
            .filter(|value| !value.is_empty())
            .ok_or_else(|| LogError::Corrupt {
                line: 0,
                reason: "session search requires a caller workspace".into(),
            })?;
        if let Some(target) = target {
            if self.workspace(target)?.as_deref() != Some(workspace.as_str()) {
                return Err(LogError::Corrupt {
                    line: 0,
                    reason: "target session is outside the caller workspace".into(),
                }
                .into());
            }
        }
        Ok(workspace)
    }

    /// Local committed events, authorized against the header in the same read.
    pub fn search_events(
        &self,
        caller: &SessionId,
        target: &SessionId,
    ) -> Result<Vec<Envelope>, BranchError> {
        let workspace = self.search_workspace(caller, Some(target))?;
        // Changed files may be replacements, not appends. Bypass the engine's
        // immutable-prefix cache for authoritative search refresh/read.
        let events = super::log::read_envelopes(&super::log::log_file(&self.root.join(target)))?;
        if !matches!(events.first().map(|event| &event.event),
            Some(SessionEvent::Header(header)) if header.session == *target
                && header.workspace.as_deref() == Some(workspace.as_str()))
        {
            return Err(LogError::Corrupt {
                line: 1,
                reason: "search session identity changed".into(),
            }
            .into());
        }
        Ok(events)
    }

    /// Cheap per-session revision. Includes replacement identity on Unix; never
    /// opens a body. Call before reading so concurrent appends refresh next time.
    pub fn search_revision(&self, session: &SessionId) -> Result<String, BranchError> {
        let directory = self.root.join(session);
        let path = super::log::log_file(&directory);
        if std::fs::symlink_metadata(&directory)
            .map_err(LogError::from)?
            .file_type()
            .is_symlink()
            || std::fs::symlink_metadata(&path)
                .map_err(LogError::from)?
                .file_type()
                .is_symlink()
        {
            return Err(LogError::Corrupt {
                line: 0,
                reason: "search refuses symlinked logs".into(),
            }
            .into());
        }
        let meta = std::fs::metadata(path).map_err(LogError::from)?;
        let revision = format!(
            "{}:{:?}",
            meta.len(),
            meta.modified().map_err(LogError::from)?
        );
        #[cfg(unix)]
        let revision = {
            use std::os::unix::fs::MetadataExt;
            format!(
                "{revision}:{}:{}:{}:{}",
                meta.dev(),
                meta.ino(),
                meta.ctime(),
                meta.ctime_nsec()
            )
        };
        Ok(revision)
    }

    /// Read searchable text from its original local JSONL event, without SQLite.
    /// Ancestor events must be addressed using their source session ID.
    pub fn search_event_read(
        &self,
        caller: &SessionId,
        target: &SessionId,
        event_ref: &str,
    ) -> Result<Option<crate::session_search::SearchDocument>, BranchError> {
        use rness_protocol::events::ContentPart;
        let workspace = self.search_workspace(caller, Some(target))?;
        // Changed files may be replacements, not appends. Bypass the engine's
        // immutable-prefix cache for authoritative search refresh/read.
        let events = super::log::read_envelopes(&super::log::log_file(&self.root.join(target)))?;
        if !matches!(events.first().map(|event| &event.event),
            Some(SessionEvent::Header(header)) if header.session == *target
                && header.workspace.as_deref() == Some(workspace.as_str()))
        {
            return Err(LogError::Corrupt {
                line: 1,
                reason: "session identity or workspace changed during read".into(),
            }
            .into());
        }
        for envelope in events {
            if envelope.id != event_ref {
                continue;
            }
            let content = match envelope.event {
                SessionEvent::UserMessage(message) => message.content,
                SessionEvent::AssistantMessage(message) => message.content,
                _ => return Ok(None),
            };
            let text = content
                .into_iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(Some(crate::session_search::SearchDocument {
                session_id: target.clone(),
                event_ref: envelope.id,
                text,
            }));
        }
        Ok(None)
    }

    /// Build a derived search snapshot on demand. This never opens a writable
    /// log or changes JSONL. Index local messages once, under their source
    /// session, rather than duplicating ancestor messages for every fork.
    /// The caller must supply a trusted workspace, not model-provided scope.
    pub fn search_snapshot(
        &self,
        workspace: &str,
        previous_revision: Option<&str>,
    ) -> Result<Option<crate::session_search::SearchSnapshot>, BranchError> {
        use crate::session_search::{SearchDocument, SearchSnapshot};
        use rness_protocol::events::ContentPart;
        use sha2::{Digest, Sha256};

        let mut documents = Vec::new();
        let mut digest = Sha256::new();
        digest.update(b"session-message-search-v1");
        digest.update((workspace.len() as u64).to_le_bytes());
        digest.update(workspace.as_bytes());
        for session_id in self.list()? {
            if self.workspace(&session_id)?.as_deref() != Some(workspace) {
                continue;
            }
            let events = self.read_session(&session_id)?;
            // Authorize again using the same observation as the indexed body.
            if !matches!(events.first().map(|event| &event.event),
                Some(SessionEvent::Header(header))
                    if header.session == session_id
                        && header.workspace.as_deref() == Some(workspace))
            {
                return Err(LogError::Corrupt {
                    line: 1,
                    reason: "session identity or workspace changed during search".into(),
                }
                .into());
            }
            digest.update((session_id.len() as u64).to_le_bytes());
            digest.update(session_id.as_bytes());
            for envelope in events {
                let content = match &envelope.event {
                    SessionEvent::UserMessage(message) => &message.content,
                    SessionEvent::AssistantMessage(message) => &message.content,
                    _ => continue,
                };
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    continue;
                }
                for value in [envelope.id.as_str(), text.as_str()] {
                    digest.update((value.len() as u64).to_le_bytes());
                    digest.update(value.as_bytes());
                }
                documents.push(SearchDocument {
                    session_id: session_id.clone(),
                    event_ref: envelope.id,
                    text,
                });
            }
        }
        let revision = format!("{:x}", digest.finalize());
        if previous_revision == Some(revision.as_str()) {
            return Ok(None);
        }
        Ok(Some(SearchSnapshot {
            revision,
            documents,
        }))
    }

    /// Return only new local envelopes. An ancestor cursor requires a full
    /// history reload; ancestor prefixes are immutable after a fork.
    pub fn history_after(
        &self,
        session: &SessionId,
        after: &str,
    ) -> Result<Option<Vec<Envelope>>, BranchError> {
        let mut readers = self.readers.lock().unwrap();
        if readers.len() >= 8 && !readers.contains_key(session) {
            if let Some(key) = readers.keys().next().cloned() {
                readers.remove(&key);
            }
        }
        Ok(readers
            .entry(session.clone())
            .or_default()
            .read_after(&self.root, session, after)?)
    }

    /// Session ids with a canonical log and a committed header. Auxiliary
    /// directories (notably persisted jobs) are not sessions. Inspect only the
    /// header, not full histories; corrupt later events remain visible on read.
    pub fn list(&self) -> Result<Vec<SessionId>, BranchError> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(LogError::from)? {
            let entry = entry.map_err(LogError::from)?;
            if !entry.file_type().map_err(LogError::from)?.is_dir() {
                continue;
            }
            let path = super::log::log_file(&entry.path());
            if !path.is_file() {
                continue;
            }
            let file = match std::fs::File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(LogError::from(error).into()),
            };
            let mut header = Vec::new();
            std::io::BufReader::new(file)
                .read_until(b'\n', &mut header)
                .map_err(LogError::from)?;
            if !header.ends_with(b"\n") {
                continue;
            }
            if matches!(
                serde_json::from_slice::<Envelope>(&header),
                Ok(Envelope {
                    event: SessionEvent::Header(_),
                    ..
                })
            ) {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rness_protocol::events::{ContentPart, TurnOutcome, UserIntent, UserMessage};

    fn msg(text: &str) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text: text.into() }],
            source: None,
        })
    }

    fn text_of(env: &Envelope) -> Option<&str> {
        match &env.event {
            SessionEvent::UserMessage(m) => match m.content.first() {
                Some(ContentPart::Text { text }) => Some(text),
                _ => None,
            },
            _ => None,
        }
    }

    #[test]
    fn header_metadata_ignores_corrupt_body_without_touching_cache() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut root = store.create(Some("/w".into())).unwrap();
        let root_id = root.session().clone();
        let at = root.append(&msg("base")).unwrap().id;
        assert_eq!(store.delegation(&root_id).unwrap(), None);
        assert_eq!(store.parent(&root_id).unwrap(), None);
        assert_eq!(store.workspace(&root_id).unwrap(), Some("/w".into()));
        assert!(store.readers.lock().unwrap().is_empty());

        // Fill the cache: metadata discovery must neither insert nor evict.
        store.history(&root_id).unwrap();
        for _ in 0..7 {
            let log = store.create(None).unwrap();
            store.history(log.session()).unwrap();
        }
        let cached: std::collections::HashSet<_> =
            store.readers.lock().unwrap().keys().cloned().collect();
        assert_eq!(cached.len(), 8);
        let delegation = Delegation {
            parent: root_id.clone(),
            call: Some("call-1".into()),
            depth: 1,
            mode: Default::default(),
        };
        let parent = ForkRef {
            session: root_id,
            at,
        };
        let sid = "metadata".to_string();
        let log = SessionLog::create(
            dir.path(),
            &sid,
            Some("/w".into()),
            Some(parent.clone()),
            Some(delegation.clone()),
        )
        .unwrap();
        let path = log.path().to_path_buf();
        let header = std::fs::read(&path).unwrap();
        drop(log);

        for body in [
            b"invalid JSON\n".to_vec(),
            b"{\"id\":\"future\",\"at\":\"now\",\"type\":\"future/event\"}\n".to_vec(),
            vec![b'x'; 1024 * 1024].into_iter().chain([b'\n']).collect(),
        ] {
            let mut contents = header.clone();
            contents.extend(body);
            std::fs::write(&path, &contents).unwrap();
            assert_eq!(store.delegation(&sid).unwrap(), Some(delegation.clone()));
            assert_eq!(store.parent(&sid).unwrap(), Some(parent.clone()));
            assert_eq!(store.workspace(&sid).unwrap(), Some("/w".into()));
            assert_eq!(
                store
                    .readers
                    .lock()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<std::collections::HashSet<_>>(),
                cached
            );
            // A fresh full-history reader still reports the corrupt body.
            assert!(matches!(
                SessionStore::new(dir.path()).history(&sid),
                Err(BranchError::Log(LogError::Corrupt { line: 2, .. }))
            ));
        }
    }

    #[test]
    fn header_metadata_rejects_malformed_headers_without_caching() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let log = store.create(None).unwrap();
        let sid = log.session().clone();
        let path = log.path().to_path_buf();
        let header = std::fs::read_to_string(&path).unwrap();
        drop(log);
        let value: serde_json::Value = serde_json::from_str(&header).unwrap();
        let mut wrong_version = value.clone();
        wrong_version["version"] = serde_json::json!(FORMAT_VERSION + 1);
        let mut wrong_session = value.clone();
        wrong_session["session"] = serde_json::json!("different-session");
        let mut missing_version = value.clone();
        missing_version.as_object_mut().unwrap().remove("version");
        let mut missing_session = value;
        missing_session.as_object_mut().unwrap().remove("session");
        for (contents, expected_line) in [
            (String::new(), 0),
            (" \t\n\n".into(), 0),
            (header.trim_end().to_string(), 0),
            ("not json\n".into(), 1),
            ("\nnot json\n".into(), 2),
            (
                "{\"id\":\"e\",\"at\":\"now\",\"type\":\"turn/started\",\"turn\":1}\n".into(),
                1,
            ),
            (format!("{wrong_version}\n"), 1),
            (format!("{wrong_session}\n"), 1),
            (format!("{missing_version}\n"), 1),
            (format!("{missing_session}\n"), 1),
        ] {
            std::fs::write(&path, &contents).unwrap();
            for result in [
                store.delegation(&sid).map(|_| ()),
                store.parent(&sid).map(|_| ()),
                store.workspace(&sid).map(|_| ()),
            ] {
                assert!(
                    matches!(result, Err(BranchError::Log(LogError::Corrupt { line, .. }))
                    if line == expected_line),
                    "contents: {contents:?}"
                );
            }
            assert!(store.readers.lock().unwrap().is_empty());
        }
        let missing = "missing".to_string();
        for result in [
            store.delegation(&missing).map(|_| ()),
            store.parent(&missing).map(|_| ()),
            store.workspace(&missing).map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(BranchError::Log(LogError::NotFound(id))) if id == missing)
            );
        }
        assert!(store.readers.lock().unwrap().is_empty());
    }

    #[test]
    fn header_metadata_accepts_blank_prefix_and_torn_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let log = store.create(None).unwrap();
        let sid = log.session().clone();
        let path = log.path().to_path_buf();
        let header = std::fs::read_to_string(&path).unwrap();
        drop(log);
        std::fs::write(&path, format!(" \t\n\r\n{header}torn body")).unwrap();
        assert_eq!(store.delegation(&sid).unwrap(), None);
        assert_eq!(store.parent(&sid).unwrap(), None);
        assert_eq!(store.workspace(&sid).unwrap(), None);
        assert!(store.readers.lock().unwrap().is_empty());
        assert_eq!(store.history(&sid).unwrap().len(), 1);
    }

    #[test]
    fn list_excludes_auxiliary_directories_without_session_logs() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let first = store.create(None).unwrap();
        let second = store.create(None).unwrap();
        let mut expected = vec![first.session().clone(), second.session().clone()];
        expected.sort();

        // Job persistence shares the session root; neither it nor other
        // directories/files should be mistaken for a session.
        std::fs::create_dir_all(dir.path().join("jobs/job-1")).unwrap();
        std::fs::write(dir.path().join("jobs/job-1/state.json"), "{}").unwrap();
        std::fs::create_dir(dir.path().join("cache")).unwrap();
        std::fs::create_dir(dir.path().join(ulid::Ulid::new().to_string())).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a session").unwrap();
        // A directory named like the log is not a log file either.
        std::fs::create_dir_all(dir.path().join("not-a-log/session.v1.jsonl")).unwrap();

        assert_eq!(store.list().unwrap(), expected);
        assert!(dir.path().join("jobs/job-1/state.json").is_file());
    }

    #[test]
    fn list_skips_empty_invalid_and_non_header_logs() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let real = store.create(None).unwrap();
        let sid = real.session().clone();
        let header =
            std::fs::read_to_string(super::super::log::log_file(&dir.path().join(&sid))).unwrap();
        for (name, contents) in [
            ("empty", ""),
            ("invalid-json", "not json\n"),
            (
                "non-header",
                "{\"id\":\"e\",\"at\":\"now\",\"type\":\"turn/started\",\"turn\":1}\n",
            ),
            ("uncommitted-header", header.trim_end()),
        ] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(super::super::log::log_file(&path), contents).unwrap();
        }
        // Discovery validates only the header, not an entire conversation.
        // A later corruption must still surface when that session is read.
        drop(real);
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(super::super::log::log_file(&dir.path().join(&sid)))
            .unwrap()
            .write_all(b"invalid later event\n")
            .unwrap();
        assert_eq!(store.list().unwrap(), vec![sid.clone()]);
        assert!(store.history(&sid).is_err());
    }

    #[test]
    fn delta_history_excludes_prefix_and_respects_forks() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut root = store.create(None).unwrap();
        let sid = root.session().clone();
        let first = root.append(&msg("first")).unwrap();
        store.history(&sid).unwrap();
        assert!(store
            .history_after(&sid, &first.id)
            .unwrap()
            .unwrap()
            .is_empty());
        let second = root.append(&msg("second")).unwrap();
        assert_eq!(
            store.history_after(&sid, &first.id).unwrap(),
            Some(vec![second.clone()])
        );
        let mut child = store.fork(&sid, Some(first.id.clone())).unwrap();
        let child_id = child.session().clone();
        assert!(store.history_after(&child_id, &first.id).unwrap().is_none());
        let own = child.append(&msg("child")).unwrap();
        assert!(store
            .history_after(&child_id, &own.id)
            .unwrap()
            .unwrap()
            .is_empty());
        assert!(store.history_after(&sid, "missing").unwrap().is_none());
    }

    #[test]
    fn fork_and_history_across_three_generations() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());

        // Root: a, b, c
        let mut root = store.create(Some("/w".into())).unwrap();
        let root_id = root.session().clone();
        let _a = root.append(&msg("a")).unwrap();
        let b = root.append(&msg("b")).unwrap();
        let _c = root.append(&msg("c")).unwrap();
        drop(root);

        // Child forks at b (drops c), adds d.
        let mut child = store.fork(&root_id, Some(b.id.clone())).unwrap();
        let child_id = child.session().clone();
        let d = child.append(&msg("d")).unwrap();
        drop(child);

        // Grandchild forks at child's tip, adds e.
        let mut grand = store.fork(&child_id, None).unwrap();
        let grand_id = grand.session().clone();
        grand.append(&msg("e")).unwrap();
        drop(grand);

        // Histories.
        let texts = |sid: &SessionId| -> Vec<String> {
            store
                .history(sid)
                .unwrap()
                .iter()
                .filter_map(text_of)
                .map(String::from)
                .collect()
        };
        assert_eq!(texts(&root_id), vec!["a", "b", "c"]);
        assert_eq!(texts(&child_id), vec!["a", "b", "d"]);
        assert_eq!(texts(&grand_id), vec!["a", "b", "d", "e"]);

        // Parent stayed byte-identical conceptually: c still in root history.
        // Ancestry chains.
        let anc = store.ancestry(&grand_id).unwrap();
        assert_eq!(anc.len(), 3);
        assert_eq!(anc[0].session, root_id);
        assert_eq!(anc[0].forked_at, Some(b.id.clone()));
        assert_eq!(anc[1].session, child_id);
        assert_eq!(anc[1].forked_at, Some(d.id.clone()));
        assert_eq!(anc[2].session, grand_id);
        assert_eq!(anc[2].forked_at, None);

        // Children queries.
        let kids = store.children(&root_id, None).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].session, child_id);
        assert_eq!(kids[0].at, b.id);
        assert_eq!(store.children(&root_id, Some(&b.id)).unwrap().len(), 1);
        assert!(store.children(&root_id, Some(&d.id)).unwrap().is_empty());
    }

    #[test]
    fn fork_point_must_exist_and_not_be_header() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut root = store.create(None).unwrap();
        let root_id = root.session().clone();
        root.append(&msg("a")).unwrap();
        let header_id = root.read_all().unwrap()[0].id.clone();
        drop(root);

        assert!(matches!(
            store.fork(&root_id, Some("01NOPE".into())),
            Err(BranchError::ForkPointNotFound { .. })
        ));
        assert!(matches!(
            store.fork(&root_id, Some(header_id)),
            Err(BranchError::ForkAtHeader(_))
        ));
    }

    #[test]
    fn sibling_forks_at_same_point() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut root = store.create(None).unwrap();
        let root_id = root.session().clone();
        let a = root.append(&msg("a")).unwrap();
        // Root can keep appending AFTER forks exist — parent never blocked.
        let s1 = store
            .fork(&root_id, Some(a.id.clone()))
            .unwrap()
            .session()
            .clone();
        let s2 = store
            .fork(&root_id, Some(a.id.clone()))
            .unwrap()
            .session()
            .clone();
        root.append(&msg("b")).unwrap();
        drop(root);

        let mut kids: Vec<_> = store
            .children(&root_id, Some(&a.id))
            .unwrap()
            .into_iter()
            .map(|c| c.session)
            .collect();
        kids.sort();
        let mut expect = vec![s1, s2];
        expect.sort();
        assert_eq!(kids, expect);

        // Turn events also flow into history assembly unharmed.
        let mut root = store.open(&root_id).unwrap();
        root.append(&SessionEvent::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed,
        })
        .unwrap();
    }
}
