//! The append-only JSONL session log — the source of truth.
//!
//! One session = one directory: `<root>/<session-id>/session.v1.jsonl`.
//! Writers append a line and fsync; committed lines are never touched
//! (invariant #2). A torn tail (crash mid-write) is detected on open and
//! truncated away — the only mutation ever performed, and it only removes
//! bytes that were never acknowledged as committed.
//!
//! The file carries an OS advisory lock (std `File::try_lock`) for the
//! writer's lifetime: one writer per session, any number of readers.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rness_protocol::branch::{Delegation, ForkRef};
use rness_protocol::events::{Envelope, EventId, Header, SessionEvent, SessionId, FORMAT_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("session '{0}' already exists")]
    AlreadyExists(SessionId),
    #[error("session '{0}' not found")]
    NotFound(SessionId),
    #[error("session '{0}' is locked by another writer")]
    Locked(SessionId),
    #[error("corrupt log at line {line}: {reason}")]
    Corrupt { line: usize, reason: String },
}

/// Current-generation filename.
pub(super) fn log_file(dir: &Path) -> PathBuf {
    dir.join(format!("session.v{FORMAT_VERSION}.jsonl"))
}

/// An open, writable session log. Holds the OS writer lock for its lifetime.
pub struct SessionLog {
    session: SessionId,
    file: File,
    path: PathBuf,
}

impl SessionLog {
    /// Create a fresh session under `root`. Writes the header line.
    pub fn create(
        root: &Path,
        session: &SessionId,
        workspace: Option<String>,
        parent: Option<ForkRef>,
        delegation: Option<Delegation>,
    ) -> Result<Self, LogError> {
        let dir = root.join(session);
        if log_file(&dir).exists() {
            return Err(LogError::AlreadyExists(session.clone()));
        }
        fs::create_dir_all(&dir)?;
        let path = log_file(&dir);
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let mut log = Self::lock(session, file, path)?;
        let header = SessionEvent::Header(Header {
            version: FORMAT_VERSION,
            session: session.clone(),
            parent,
            delegation,
            workspace,
        });
        log.append(&header)?;
        Ok(log)
    }

    /// Open an existing session for appending. Detects and truncates a
    /// torn tail before returning.
    pub fn open(root: &Path, session: &SessionId) -> Result<Self, LogError> {
        let path = log_file(&root.join(session));
        if !path.exists() {
            return Err(LogError::NotFound(session.clone()));
        }
        let file = OpenOptions::new().append(true).read(true).open(&path)?;
        let mut log = Self::lock(session, file, path)?;
        log.heal_torn_tail()?;
        Ok(log)
    }

    fn lock(session: &SessionId, file: File, path: PathBuf) -> Result<Self, LogError> {
        match file.try_lock() {
            Ok(()) => Ok(Self {
                session: session.clone(),
                file,
                path,
            }),
            Err(_) => Err(LogError::Locked(session.clone())),
        }
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event and fsync. Returns the committed envelope.
    pub fn append(&mut self, event: &SessionEvent) -> Result<Envelope, LogError> {
        let envelope = Envelope {
            id: ulid::Ulid::new().to_string(),
            at: now_rfc3339(),
            event: event.clone(),
        };
        let mut line = serde_json::to_string(&envelope)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;
        Ok(envelope)
    }

    /// Read every committed envelope (header included), in order.
    pub fn read_all(&self) -> Result<Vec<Envelope>, LogError> {
        read_envelopes(&self.path)
    }

    /// Truncate an unterminated last line (crash artifact). A
    /// complete-but-invalid line is corruption and surfaces on read instead.
    fn heal_torn_tail(&mut self) -> Result<(), LogError> {
        let mut content = Vec::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_end(&mut content)?;
        if content.is_empty() || content.ends_with(b"\n") {
            return Ok(());
        }
        let keep = content
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        tracing::warn!(
            session = %self.session,
            dropped = content.len() - keep,
            "healing torn tail"
        );
        self.file.set_len(keep as u64)?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.sync_data()?;
        Ok(())
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Read a session's committed envelopes without taking the writer lock.
pub fn read_session(root: &Path, session: &SessionId) -> Result<Vec<Envelope>, LogError> {
    let path = log_file(&root.join(session));
    if !path.exists() {
        return Err(LogError::NotFound(session.clone()));
    }
    read_envelopes(&path)
}

pub(super) fn read_envelopes(path: &Path) -> Result<Vec<Envelope>, LogError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut line_no = 0usize;
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        // An unterminated final line is a torn tail: the committed prefix
        // ends at the previous line.
        if !buf.ends_with('\n') {
            break;
        }
        let trimmed = buf.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let envelope: Envelope = serde_json::from_str(trimmed).map_err(|e| LogError::Corrupt {
            line: line_no,
            reason: e.to_string(),
        })?;
        out.push(envelope);
    }
    // First line must be a header.
    match out.first() {
        Some(Envelope {
            event: SessionEvent::Header(_),
            ..
        }) => Ok(out),
        Some(_) => Err(LogError::Corrupt {
            line: 1,
            reason: "first event is not session/header".into(),
        }),
        None => Err(LogError::Corrupt {
            line: 0,
            reason: "empty log".into(),
        }),
    }
}

/// Incremental reader for an immutable committed prefix. Incomplete tails are
/// retried from their starting offset, never cached as committed events.
#[derive(Default)]
pub(super) struct SessionReader {
    offset: u64,
    line: usize,
    events: Vec<Envelope>,
    positions: std::collections::HashMap<EventId, usize>,
}

impl SessionReader {
    pub(super) fn read(
        &mut self,
        root: &Path,
        session: &SessionId,
    ) -> Result<Vec<Envelope>, LogError> {
        self.refresh(root, session)?;
        Ok(self.events.clone())
    }

    pub(super) fn read_after(
        &mut self,
        root: &Path,
        session: &SessionId,
        after: &str,
    ) -> Result<Option<Vec<Envelope>>, LogError> {
        self.refresh(root, session)?;
        Ok(self
            .positions
            .get(after)
            .map(|index| self.events[index + 1..].to_vec()))
    }

    fn refresh(&mut self, root: &Path, session: &SessionId) -> Result<(), LogError> {
        let path = log_file(&root.join(session));
        let mut file = File::open(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                LogError::NotFound(session.clone())
            } else {
                LogError::Io(error)
            }
        })?;
        if file.metadata()?.len() < self.offset {
            *self = Self::default();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            let n = reader.read_until(b'\n', &mut bytes)?;
            if n == 0 || !bytes.ends_with(b"\n") {
                break;
            }
            if !bytes.iter().all(u8::is_ascii_whitespace) {
                let event: Envelope =
                    serde_json::from_slice(&bytes).map_err(|error| LogError::Corrupt {
                        line: self.line + 1,
                        reason: error.to_string(),
                    })?;
                if self.events.is_empty() && !matches!(event.event, SessionEvent::Header(_)) {
                    return Err(LogError::Corrupt {
                        line: 1,
                        reason: "first event is not session/header".into(),
                    });
                }
                self.positions.insert(event.id.clone(), self.events.len());
                self.events.push(event);
            }
            self.offset += n as u64;
            self.line += 1;
        }
        if self.events.is_empty() {
            return Err(LogError::Corrupt {
                line: 0,
                reason: "empty log".into(),
            });
        }
        Ok(())
    }
}

/// Commit timestamp (RFC 3339, ms precision, UTC).
fn now_rfc3339() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// Last event id of a session — the default fork point.
pub fn tip(root: &Path, session: &SessionId) -> Result<EventId, LogError> {
    let events = read_session(root, session)?;
    Ok(events
        .last()
        .expect("read_session guarantees header")
        .id
        .clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rness_protocol::events::{ContentPart, UserIntent, UserMessage};

    fn user_msg(text: &str) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text: text.into() }],
            source: None,
        })
    }

    #[test]
    fn compaction_lifecycle_recovers_at_every_torn_byte_boundary() {
        use rness_protocol::events::{Compaction, Usage};
        let root = tempfile::tempdir().unwrap();
        let sid = "crash-matrix".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        log.append(&user_msg("original 日本語")).unwrap();
        let prefix = fs::read(log.path()).unwrap();
        let events = [
            SessionEvent::CompactionStarted {
                model: "test".into(),
                sources: vec![],
                estimated_input: 100,
                request: serde_json::json!({"text":"日本語"}),
            },
            SessionEvent::CompactionRequest {
                started: "start".into(),
                body: "{\"text\":\"日本語\"}".into(),
            },
            SessionEvent::Compaction(Compaction {
                replaces: vec![],
                summary: "summary".into(),
                model: "test".into(),
            }),
            SessionEvent::CompactionFinished {
                started: "start".into(),
                outcome: "committed".into(),
                usage: Usage::default(),
                chunks: vec![],
            },
        ];
        let mut durable = prefix;
        let path = log.path().to_path_buf();
        drop(log);
        for (stage, event) in events.into_iter().enumerate() {
            let envelope = Envelope {
                id: if stage == 0 {
                    "start".into()
                } else {
                    format!("event-{stage}")
                },
                at: now_rfc3339(),
                event,
            };
            let mut bytes = serde_json::to_vec(&envelope).unwrap();
            bytes.push(b'\n');
            for cut in 0..=bytes.len() {
                let mut crash = durable.clone();
                crash.extend_from_slice(&bytes[..cut]);
                fs::write(&path, &crash).unwrap();
                let mut reopened = SessionLog::open(root.path(), &sid).unwrap();
                assert_eq!(
                    fs::read(&path).unwrap(),
                    if cut == bytes.len() {
                        crash
                    } else {
                        durable.clone()
                    }
                );
                crate::turn::compaction::recover(&mut reopened).unwrap();
                let history = reopened.read_all().unwrap();
                let has_start = stage > 0 || cut == bytes.len();
                let has_checkpoint = stage > 2 || (stage == 2 && cut == bytes.len());
                let finishes: Vec<_> = history
                    .iter()
                    .filter_map(|e| match &e.event {
                        SessionEvent::CompactionFinished {
                            started, outcome, ..
                        } => Some((started.as_str(), outcome.as_str())),
                        _ => None,
                    })
                    .collect();
                if has_start {
                    assert_eq!(
                        finishes,
                        vec![(
                            "start",
                            if stage == 3 && cut == bytes.len() {
                                "committed"
                            } else if has_checkpoint {
                                "committed_before_interruption"
                            } else {
                                "interrupted"
                            }
                        )]
                    );
                } else {
                    assert!(finishes.is_empty());
                }
                drop(reopened);
                let mut reopened = SessionLog::open(root.path(), &sid).unwrap();
                crate::turn::compaction::recover(&mut reopened).unwrap();
                assert_eq!(history, reopened.read_all().unwrap());
            }
            durable.extend_from_slice(&bytes);
        }
    }

    #[test]
    fn failed_disk_write_does_not_acknowledge_or_change_committed_prefix() {
        let root = tempfile::tempdir().unwrap();
        let sid = "read-only-writer".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let before = log.read_all().unwrap();
        // A real OS write error, independent of permissions or running as root.
        log.file = File::open(log.path()).unwrap();
        let result = log.append(&SessionEvent::CompactionRequest {
            started: "start".into(),
            body: "{}".into(),
        });
        assert!(matches!(result, Err(LogError::Io(_))));
        assert_eq!(log.read_all().unwrap(), before);
        drop(log);
        assert_eq!(
            SessionLog::open(root.path(), &sid)
                .unwrap()
                .read_all()
                .unwrap(),
            before
        );
    }

    #[test]
    fn incremental_reader_retries_torn_tail_without_rereading_prefix() {
        let root = tempfile::tempdir().unwrap();
        let sid = "incremental".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let mut reader = SessionReader::default();
        assert_eq!(reader.read(root.path(), &sid).unwrap().len(), 1);
        let offset = reader.offset;
        assert_eq!(reader.read(root.path(), &sid).unwrap().len(), 1);
        assert_eq!(reader.offset, offset);
        log.append(&user_msg("hello")).unwrap();
        assert_eq!(reader.read(root.path(), &sid).unwrap().len(), 2);
        let offset = reader.offset;
        let event = Envelope {
            id: "tail".into(),
            at: now_rfc3339(),
            event: user_msg("é"),
        };
        let mut bytes = serde_json::to_vec(&event).unwrap();
        bytes.push(b'\n');
        let split = bytes.iter().position(|b| *b == 0xc3).unwrap() + 1;
        log.file.write_all(&bytes[..split]).unwrap();
        assert_eq!(reader.read(root.path(), &sid).unwrap().len(), 2);
        assert_eq!(reader.offset, offset);
        log.file.write_all(&bytes[split..]).unwrap();
        assert_eq!(reader.read(root.path(), &sid).unwrap().len(), 3);
        assert_eq!(
            reader.read(root.path(), &sid).unwrap(),
            log.read_all().unwrap()
        );
    }

    #[test]
    fn create_append_read_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let sid: SessionId = ulid::Ulid::new().to_string();
        let mut log = SessionLog::create(root.path(), &sid, Some("/w".into()), None, None).unwrap();
        log.append(&user_msg("hello")).unwrap();
        log.append(&user_msg("world")).unwrap();

        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0].event, SessionEvent::Header(_)));
        // ULIDs sort in commit order.
        let mut ids: Vec<_> = events.iter().map(|e| e.id.clone()).collect();
        let sorted = ids.clone();
        ids.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn create_twice_fails_and_open_missing_fails() {
        let root = tempfile::tempdir().unwrap();
        let sid: SessionId = "01TEST".into();
        let log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        drop(log);
        assert!(matches!(
            SessionLog::create(root.path(), &sid, None, None, None),
            Err(LogError::AlreadyExists(_))
        ));
        assert!(matches!(
            SessionLog::open(root.path(), &"nope".to_string()),
            Err(LogError::NotFound(_))
        ));
    }

    #[test]
    fn writer_lock_is_exclusive_but_readers_pass() {
        let root = tempfile::tempdir().unwrap();
        let sid: SessionId = "01LOCK".into();
        let log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        assert!(matches!(
            SessionLog::open(root.path(), &sid),
            Err(LogError::Locked(_))
        ));
        // Lockless read works while the writer holds the lock.
        assert_eq!(read_session(root.path(), &sid).unwrap().len(), 1);
        drop(log);
        // Lock released on drop -> second writer can open.
        assert!(SessionLog::open(root.path(), &sid).is_ok());
    }

    #[test]
    fn torn_tail_is_healed_on_open_and_skipped_on_read() {
        let root = tempfile::tempdir().unwrap();
        let sid: SessionId = "01TORN".into();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        log.append(&user_msg("committed")).unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        // Simulate a crash mid-write: garbage without trailing newline.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":\"01X\",\"at\":\"t\",\"type\":\"user/mess")
            .unwrap();
        drop(f);

        // Lockless read tolerates the torn tail (prefix only).
        assert_eq!(read_session(root.path(), &sid).unwrap().len(), 2);

        // Open heals it: the file shrinks back to the committed prefix.
        let log = SessionLog::open(root.path(), &sid).unwrap();
        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 2);
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.ends_with(b"\n"));
    }

    #[test]
    fn complete_invalid_line_is_corruption_not_healed() {
        let root = tempfile::tempdir().unwrap();
        let sid: SessionId = "01BAD".into();
        let log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let path = log.path().to_path_buf();
        drop(log);

        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"not json at all\n").unwrap();
        drop(f);

        assert!(matches!(
            read_session(root.path(), &sid),
            Err(LogError::Corrupt { line: 2, .. })
        ));
    }

    #[test]
    fn header_must_be_first() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("01NOHDR");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            log_file(&dir),
            b"{\"id\":\"01A\",\"at\":\"t\",\"type\":\"turn/started\",\"turn\":1}\n",
        )
        .unwrap();
        assert!(matches!(
            read_session(root.path(), &"01NOHDR".to_string()),
            Err(LogError::Corrupt { line: 1, .. })
        ));
    }

    #[test]
    fn fork_header_carries_parent_ref() {
        let root = tempfile::tempdir().unwrap();
        let parent_sid: SessionId = "01PARENT".into();
        let mut parent = SessionLog::create(root.path(), &parent_sid, None, None, None).unwrap();
        let committed = parent.append(&user_msg("base")).unwrap();
        drop(parent);

        let child_sid: SessionId = "01CHILD".into();
        let fork = ForkRef {
            session: parent_sid.clone(),
            at: committed.id.clone(),
        };
        let child =
            SessionLog::create(root.path(), &child_sid, None, Some(fork.clone()), None).unwrap();
        let events = child.read_all().unwrap();
        match &events[0].event {
            SessionEvent::Header(h) => assert_eq!(h.parent.as_ref(), Some(&fork)),
            other => panic!("expected header, got {other:?}"),
        }
    }
}
