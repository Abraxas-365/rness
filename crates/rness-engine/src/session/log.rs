//! The append-only JSONL session log — the source of truth.
//!
//! One session = one directory: `<root>/<session-id>/session.v1.jsonl`.
//! Writers append a line and fsync; committed lines are never touched
//! (invariant #2). On open the writer repairs only bytes that were never
//! acknowledged as committed, and never loses them silently:
//! - a torn tail (unterminated last line, crash mid-write) is truncated;
//! - a trailing region of terminated-but-unparseable lines (a crash that
//!   persisted the newline but not the payload, a NUL-filled extent, a stray
//!   editor write) is moved to a `quarantine-<ULID>.bin` sidecar and a
//!   `session/repair` audit event is appended. `append` always writes a
//!   valid envelope, so such a line can never hold a committed event.
//!
//! An invalid line FOLLOWED by valid lines is mid-file corruption: it is
//! never repaired automatically; readers report it with line and offset.
//! A failed append rolls the file back to its previous length, so a partial
//! write never glues onto the next event.
//!
//! The file carries an OS advisory lock (std `File::try_lock`) for the
//! writer's lifetime: one writer per session, any number of readers.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rness_protocol::branch::{Delegation, ForkRef};
use rness_protocol::events::{
    Envelope, EventId, Header, LogRepair, SessionEvent, SessionId, FORMAT_VERSION,
};

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
    /// `offset` is the byte offset of the offending line (0 when the error
    /// is not about one line).
    #[error("corrupt log at line {line}: {reason}{}", offset_note(.offset))]
    Corrupt {
        line: usize,
        offset: u64,
        reason: String,
    },
    /// The header names a format version this build cannot read (written by
    /// a newer — or a much older — rness). Not data loss: upgrade rness.
    #[error(
        "session log format v{found} is not supported by this rness (reads v{supported}); {}",
        version_advice(*.found, *.supported)
    )]
    UnsupportedVersion { found: u32, supported: u32 },
}

fn version_advice(found: u32, supported: u32) -> &'static str {
    if found > supported {
        "it was written by a newer rness — upgrade rness to open it"
    } else {
        "it was written by an older rness and has no migration"
    }
}

fn offset_note(offset: &u64) -> String {
    if *offset == 0 {
        String::new()
    } else {
        format!(
            " (byte offset {offset}; a damaged tail is quarantined when the session \
             is next opened for writing, damage before valid events needs a \
             restore from backup)"
        )
    }
}

/// Upper bound on the header line read by `open` and header probes.
const MAX_HEADER_BYTES: u64 = 4 << 20;
/// Bounds of the trailing invalid region `open` quarantines automatically.
/// Anything larger is left alone (reads report it as corruption).
const MAX_QUARANTINE_LINES: u64 = 1024;
const MAX_QUARANTINE_BYTES: u64 = 64 << 20;

/// Parse one committed log line (see [`Envelope::parse_line`]): unknown
/// event types are data, anything else unparseable is `Corrupt`.
pub(crate) fn parse_line(bytes: &[u8], line: usize, offset: u64) -> Result<Envelope, LogError> {
    Envelope::parse_line(bytes).map_err(|error| LogError::Corrupt {
        line,
        offset,
        reason: error.to_string(),
    })
}

/// Parse the first committed line, which must be a `session/header` of the
/// supported format version. A header of another version is reported as
/// [`LogError::UnsupportedVersion`] even if its shape no longer parses.
pub(crate) fn parse_header_line(bytes: &[u8], line: usize) -> Result<Envelope, LogError> {
    let version_of = |bytes: &[u8]| -> Option<u32> {
        let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        if value.get("type")?.as_str()? != "session/header" {
            return None;
        }
        u32::try_from(value.get("version")?.as_u64()?).ok()
    };
    let envelope = match parse_line(bytes, line, 0) {
        Ok(envelope) => envelope,
        Err(error) => {
            return Err(match version_of(bytes) {
                Some(found) if found != FORMAT_VERSION => LogError::UnsupportedVersion {
                    found,
                    supported: FORMAT_VERSION,
                },
                _ => error,
            })
        }
    };
    match &envelope.event {
        SessionEvent::Header(header) if header.version != FORMAT_VERSION => {
            Err(LogError::UnsupportedVersion {
                found: header.version,
                supported: FORMAT_VERSION,
            })
        }
        SessionEvent::Header(_) => Ok(envelope),
        _ => Err(LogError::Corrupt {
            line,
            offset: 0,
            reason: "first event is not session/header".into(),
        }),
    }
}

/// Current-generation filename.
pub(crate) fn log_file(dir: &Path) -> PathBuf {
    dir.join(format!("session.v{FORMAT_VERSION}.jsonl"))
}

/// An open, writable session log. Holds the OS writer lock for its lifetime.
pub struct SessionLog {
    session: SessionId,
    file: File,
    path: PathBuf,
    /// `rness.record_stream`: keep the exact timed stream of committed
    /// outputs. Off by default — see [`drop_committed_stream`].
    record_stream: bool,
    /// Event ids from this writer are strictly increasing, also within one
    /// millisecond and within a batch.
    ids: ulid::Generator,
    /// Durability syncs issued by appends (observability for batching).
    syncs: u64,
    /// Successful writes through this handle; see [`SessionLog::generation`].
    generation: u64,
    /// A failed write could not be rolled back: the tail may hold a partial
    /// line, so nothing more is appended through this handle.
    poisoned: bool,
}

/// Buffered bytes per `write_all` in [`SessionLog::append_batch`]; the
/// whole batch still gets a single sync.
const BATCH_WRITE_CHUNK: usize = 8 << 20;

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

    /// Open an existing session for appending. Validates the header
    /// (present, supported version, matching session id), then repairs the
    /// tail: a torn last line is truncated, a trailing run of complete but
    /// unparseable lines is quarantined to a sidecar (see module docs).
    /// O(header + tail), never a full read. A log that fails validation is
    /// left byte-for-byte unchanged.
    pub fn open(root: &Path, session: &SessionId) -> Result<Self, LogError> {
        let path = log_file(&root.join(session));
        if !path.exists() {
            return Err(LogError::NotFound(session.clone()));
        }
        let file = OpenOptions::new().append(true).read(true).open(&path)?;
        let mut log = Self::lock(session, file, path)?;
        let header_end = log.check_header()?;
        log.heal_tail(header_end)?;
        Ok(log)
    }

    fn lock(session: &SessionId, file: File, path: PathBuf) -> Result<Self, LogError> {
        match file.try_lock() {
            Ok(()) => Ok(Self {
                session: session.clone(),
                file,
                path,
                record_stream: false,
                ids: ulid::Generator::new(),
                syncs: 0,
                generation: 0,
                poisoned: false,
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

    /// Record the exact timed stream of committed outputs too (default off).
    pub fn set_record_stream(&mut self, on: bool) {
        self.record_stream = on;
    }

    /// Append one event and fsync. Returns the committed envelope.
    pub fn append(&mut self, event: &SessionEvent) -> Result<Envelope, LogError> {
        let mut committed = self.append_batch(std::slice::from_ref(event))?;
        Ok(committed.pop().expect("one event in, one envelope out"))
    }

    /// Append `events` in order with ONE durability sync (bytes are written
    /// in chunks of at most 8 MiB). Empty slice: no write, no sync.
    ///
    /// Not atomic: a crash mid-batch can leave any prefix of the batch (the
    /// last line possibly torn; `open` truncates that) — exactly what
    /// consecutive single appends could leave. Durability is only promised
    /// once this call returns `Ok`: bytes written before the sync may or may
    /// not survive a crash, and nothing is acknowledged to callers before it.
    /// On error the file is rolled back to its previous length (best effort;
    /// if that fails the handle is poisoned).
    pub fn append_batch(&mut self, events: &[SessionEvent]) -> Result<Vec<Envelope>, LogError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let at = now_rfc3339();
        let mut envelopes = Vec::with_capacity(events.len());
        let mut bytes = Vec::new();
        for event in events {
            let mut event = event.clone();
            if !self.record_stream {
                drop_committed_stream(&mut event);
            }
            let id = self.ids.generate().unwrap_or_else(|_| ulid::Ulid::new());
            let envelope = Envelope {
                id: id.to_string(),
                at: at.clone(),
                event,
            };
            let start = bytes.len();
            serde_json::to_writer(&mut bytes, &envelope)?;
            // Refuse what `Envelope::parse_line` could not read back: a
            // committed event must stay readable.
            if rness_protocol::events::json_depth(&bytes[start..])
                > rness_protocol::events::MAX_JSON_DEPTH
            {
                return Err(LogError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "event nests JSON deeper than {} levels; not logged",
                        rness_protocol::events::MAX_JSON_DEPTH
                    ),
                )));
            }
            bytes.push(b'\n');
            envelopes.push(envelope);
        }
        self.write_durable(&bytes)?;
        Ok(envelopes)
    }

    /// Durability syncs issued by appends through this handle so far.
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    /// Bumped by every successful write through this handle. The handle
    /// holds the writer lock, so an unchanged generation means the log has
    /// not changed — what [`crate::session::replay::ReplayCache`] relies on.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Write `bytes` (whole lines) at the end and fsync once. On any failure
    /// the file is rolled back to its previous length, so a partial write
    /// never glues onto the next append; if even that fails the handle is
    /// poisoned (later appends error instead of writing after garbage).
    pub(crate) fn write_durable(&mut self, bytes: &[u8]) -> Result<(), LogError> {
        self.write_durable_with(bytes, |file, bytes| {
            for chunk in bytes.chunks(BATCH_WRITE_CHUNK) {
                file.write_all(chunk)?;
            }
            file.sync_data()
        })
    }

    fn write_durable_with(
        &mut self,
        bytes: &[u8],
        write: impl FnOnce(&mut File, &[u8]) -> std::io::Result<()>,
    ) -> Result<(), LogError> {
        if self.poisoned {
            return Err(LogError::Io(std::io::Error::other(
                "session log handle is poisoned by an earlier failed write; reopen the session",
            )));
        }
        let before = self.file.metadata()?.len();
        let result = write(&mut self.file, bytes);
        self.syncs += 1;
        if let Err(error) = result {
            let rolled_back = self
                .file
                .set_len(before)
                .and_then(|()| self.file.sync_data());
            self.poisoned = rolled_back.is_err();
            tracing::warn!(
                session = %self.session,
                %error,
                rolled_back = rolled_back.is_ok(),
                "session append failed"
            );
            return Err(error.into());
        }
        self.generation += 1;
        Ok(())
    }

    /// Read every committed envelope (header included), in order.
    pub fn read_all(&self) -> Result<Vec<Envelope>, LogError> {
        read_envelopes(&self.path)
    }

    /// [`Self::read_all`] with [`elide_payloads`] applied while parsing, so
    /// a long log never holds its audit bodies in memory at once.
    pub fn read_all_elided(&self) -> Result<Vec<Envelope>, LogError> {
        read_envelopes_with(&self.path, true)
    }

    /// Validate the first committed line (bounded read). Returns the byte
    /// offset just past it.
    fn check_header(&mut self) -> Result<u64, LogError> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new((&self.file).take(MAX_HEADER_BYTES));
        let mut bytes = Vec::new();
        let mut offset = 0u64;
        let mut line = 0usize;
        loop {
            bytes.clear();
            let n = reader.read_until(b'\n', &mut bytes)?;
            line += 1;
            if n == 0 || !bytes.ends_with(b"\n") {
                return Err(LogError::Corrupt {
                    line: if offset == 0 && n == 0 { 0 } else { line },
                    offset,
                    reason: if offset == 0 && n == 0 {
                        "empty log (no session header)".into()
                    } else {
                        "session header line is incomplete".into()
                    },
                });
            }
            offset += n as u64;
            if bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let envelope = parse_header_line(&bytes, line)?;
            let SessionEvent::Header(header) = &envelope.event else {
                unreachable!("parse_header_line returns headers only");
            };
            if header.session != self.session {
                return Err(LogError::Corrupt {
                    line,
                    offset: 0,
                    reason: format!(
                        "session header identity '{}' does not match '{}'",
                        header.session, self.session
                    ),
                });
            }
            return Ok(offset);
        }
    }

    /// Repair the tail (writer lock held): truncate a torn last line, then
    /// quarantine a trailing run of terminated-but-invalid lines. Never
    /// reads more than the last valid line plus the repaired region.
    fn heal_tail(&mut self, header_end: u64) -> Result<(), LogError> {
        let len = self.file.metadata()?.len();
        let mut end = len;
        if end > header_end && read_byte(&mut self.file, end - 1)? != b'\n' {
            let keep = line_start_before(&mut self.file, end)?.max(header_end);
            tracing::warn!(
                session = %self.session,
                dropped = end - keep,
                "healing torn tail"
            );
            self.file.set_len(keep)?;
            self.file.sync_data()?;
            end = keep;
        }
        // Walk complete lines backwards from EOF while they are invalid.
        let mut region_start = None;
        let mut lines = 0u64;
        let mut cursor = end;
        while cursor > header_end {
            let start = line_start_before(&mut self.file, cursor - 1)?.max(header_end);
            let too_big = || {
                tracing::warn!(
                    session = %self.session,
                    "invalid log tail exceeds the quarantine bound; left in place"
                );
            };
            if end - start > MAX_QUARANTINE_BYTES {
                // Never load an oversized line just to classify it. It may be
                // a large valid event; quarantining only what follows it is
                // unsafe if it is garbage, so leave everything in place.
                if region_start.is_some() {
                    too_big();
                }
                return Ok(());
            }
            let bytes = read_range(&mut self.file, start, cursor)?;
            match classify_line(&bytes) {
                LineClass::Blank => {}
                LineClass::Valid | LineClass::Suspicious => break,
                LineClass::Garbage => {
                    region_start = Some(start);
                    lines += 1;
                    if lines > MAX_QUARANTINE_LINES {
                        too_big();
                        return Ok(());
                    }
                }
            }
            cursor = start;
        }
        let Some(start) = region_start else {
            return Ok(());
        };
        let region = read_range(&mut self.file, start, end)?;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let sidecar = format!("quarantine-{}.bin", ulid::Ulid::new());
        {
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dir.join(&sidecar))?;
            out.write_all(&region)?;
            out.sync_data()?;
        }
        sync_dir(dir)?;
        tracing::warn!(
            session = %self.session,
            bytes = region.len(),
            lines,
            sidecar = %sidecar,
            "quarantined invalid log tail"
        );
        self.file.set_len(start)?;
        self.file.sync_data()?;
        self.append(&SessionEvent::Repair(LogRepair {
            bytes: region.len() as u64,
            lines,
            sidecar,
            reason: "complete but unparseable trailing lines (never acknowledged appends)".into(),
        }))?;
        Ok(())
    }
}

enum LineClass {
    Blank,
    /// Parses (known or unknown event type).
    Valid,
    /// Envelope-shaped JSON (string `id` and `type`) that fails to parse:
    /// possibly a real event with a schema problem — never auto-repaired.
    Suspicious,
    /// Not JSON, or not envelope-shaped: cannot be an acknowledged append.
    Garbage,
}

fn classify_line(bytes: &[u8]) -> LineClass {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return LineClass::Blank;
    }
    if Envelope::parse_line(bytes).is_ok() {
        return LineClass::Valid;
    }
    // A committed line is always valid JSON (the writer serializes it
    // first), so a syntax error means it was never acknowledged — even if it
    // starts with `{"id":"` (a torn line whose newline persisted). The one
    // valid-JSON-but-unparseable case is serde_json's recursion limit
    // (deeper than `MAX_JSON_DEPTH`, e.g. from another writer): keep it.
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Err(error) if error.to_string().starts_with("recursion limit exceeded") => {
            LineClass::Suspicious
        }
        Ok(value)
            if value.get("id").is_some_and(serde_json::Value::is_string)
                && value.get("type").is_some_and(serde_json::Value::is_string) =>
        {
            LineClass::Suspicious
        }
        _ => LineClass::Garbage,
    }
}

fn read_byte(file: &mut File, at: u64) -> std::io::Result<u8> {
    let mut byte = [0u8];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn read_range(file: &mut File, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = vec![0u8; (end - start) as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Number of `\n`-terminated lines in `[0, before)`.
fn count_lines(path: &Path, before: u64) -> std::io::Result<usize> {
    let mut reader = BufReader::new(File::open(path)?.take(before));
    let mut buf = [0u8; 64 << 10];
    let mut lines = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(lines);
        }
        lines += buf[..n].iter().filter(|&&b| b == b'\n').count();
    }
}

/// Offset just past the last `\n` in `[0, before)`, or 0. Scans backwards
/// in 64 KiB chunks.
fn line_start_before(file: &mut File, before: u64) -> std::io::Result<u64> {
    const CHUNK: u64 = 64 << 10;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut end = before;
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let chunk = &mut buf[..(end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(chunk)?;
        if let Some(i) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// Make a new directory entry durable (no-op where directories can't be
/// opened for sync).
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
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

/// Write-side policy when `record_stream` is off: a committed output drops
/// its timed stream, because the result is already in the event
/// (`assistant/message.content`, the `compaction/summary` text). Failed,
/// cancelled and rejected requests keep theirs — it is the only record of
/// what the model sent, and they are rare and small. On a 12 h session the
/// dropped streams were 158 MB of 722 MB.
pub fn drop_committed_stream(event: &mut SessionEvent) {
    match event {
        SessionEvent::AssistantMessage(message) => message.chunks = Vec::new(),
        SessionEvent::CompactionFinished {
            outcome, chunks, ..
        } if outcome == "committed" => *chunks = Vec::new(),
        _ => {}
    }
}

/// Drop payloads that nothing reads once committed: legacy compaction audit
/// bodies (`compaction/started.request`, `compaction/request.body` — no longer
/// written, but present in older logs) and timed stream recordings
/// (`chunks`). They stay on disk, untouched; `SessionLog::read_all` and
/// `session_event_read` (which read the file directly) still return them.
/// On a 58k-event session this halves the parsed log (~1.08 GB -> ~0.5 GB).
pub fn elide_payloads(event: &mut SessionEvent) {
    match event {
        SessionEvent::AssistantMessage(message) => message.chunks = Vec::new(),
        SessionEvent::AssistantAttempt(attempt) => attempt.chunks = Vec::new(),
        SessionEvent::CompactionFinished { chunks, .. } => *chunks = Vec::new(),
        SessionEvent::CompactionStarted { request, .. } => *request = serde_json::Value::Null,
        SessionEvent::CompactionRequest { body, .. } => *body = String::new(),
        _ => {}
    }
}

pub(super) fn read_envelopes(path: &Path) -> Result<Vec<Envelope>, LogError> {
    read_envelopes_with(path, false)
}

/// Committed envelopes of `path` from byte `offset` (a line start), each
/// with its line's start offset, reading no further than byte `limit`.
/// Only `\n`-terminated lines are consumed: an unterminated last line (a
/// torn tail or an append in progress) is left for a later call. Returns
/// the offset just past the last consumed line. At `offset == 0` the first
/// line must be a supported header. Line numbers in errors are absolute
/// (the prefix before `offset` is only counted when an error is reported).
/// Payloads are not elided (search indexes full events).
pub(crate) fn read_envelopes_from(
    path: &Path,
    offset: u64,
    limit: u64,
) -> Result<(Vec<(Envelope, u64)>, u64), LogError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file.take(limit.saturating_sub(offset)));
    let mut out = Vec::new();
    let mut bytes = Vec::new();
    let mut position = offset;
    let mut line_no = 0usize;
    loop {
        bytes.clear();
        let n = reader.read_until(b'\n', &mut bytes)?;
        if n == 0 || !bytes.ends_with(b"\n") {
            break;
        }
        line_no += 1;
        let start = position;
        position += n as u64;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let parsed = if offset == 0 && out.is_empty() {
            parse_header_line(&bytes, line_no)
        } else {
            parse_line(&bytes, line_no, start)
        };
        let envelope = match parsed {
            Ok(envelope) => envelope,
            Err(LogError::Corrupt {
                line,
                offset: at,
                reason,
            }) if offset > 0 => {
                return Err(LogError::Corrupt {
                    line: line + count_lines(path, offset)?,
                    offset: at,
                    reason,
                })
            }
            Err(error) => return Err(error),
        };
        out.push((envelope, start));
    }
    if offset == 0 && out.is_empty() {
        return Err(LogError::Corrupt {
            line: 0,
            offset: 0,
            reason: "empty log".into(),
        });
    }
    Ok((out, position))
}

/// Bytes `[start, end)` of `path`; shorter if the file ends first.
pub(crate) fn read_path_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>, LogError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(end.saturating_sub(start))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The first line of `path` (the header), at most `MAX_HEADER_BYTES` bytes.
pub(crate) fn read_header_bytes(path: &Path) -> Result<Vec<u8>, LogError> {
    let mut bytes = Vec::new();
    BufReader::new(File::open(path)?.take(MAX_HEADER_BYTES)).read_until(b'\n', &mut bytes)?;
    if !bytes.ends_with(b"\n") {
        return Err(LogError::Corrupt {
            line: 1,
            offset: 0,
            reason: "missing or oversized header line".into(),
        });
    }
    Ok(bytes)
}

fn read_envelopes_with(path: &Path, elide: bool) -> Result<Vec<Envelope>, LogError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut bytes = Vec::new();
    let mut line_no = 0usize;
    let mut offset = 0u64;
    loop {
        bytes.clear();
        let n = reader.read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        // An unterminated final line is a torn tail: the committed prefix
        // ends at the previous line.
        if !bytes.ends_with(b"\n") {
            break;
        }
        let line_offset = offset;
        offset += n as u64;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut envelope = if out.is_empty() {
            parse_header_line(&bytes, line_no)?
        } else {
            parse_line(&bytes, line_no, line_offset)?
        };
        if elide {
            elide_payloads(&mut envelope.event);
        }
        out.push(envelope);
    }
    if out.is_empty() {
        return Err(LogError::Corrupt {
            line: 0,
            offset: 0,
            reason: "empty log".into(),
        });
    }
    Ok(out)
}

/// Incremental reader for an immutable committed prefix. Incomplete tails are
/// retried from their starting offset, never cached as committed events.
/// Cached events have [`elide_payloads`] applied.
///
/// The file handle is cached across refreshes so that a single reader never
/// re-opens the file. When the reader is evicted from the LRU cache in
/// `SessionStore`, the handle is closed automatically on drop.
pub(super) struct SessionReader {
    offset: u64,
    line: usize,
    /// Parsed once and shared: history reads hand out pointer copies, never
    /// payload copies (a long session's parsed log can exceed 1 GB).
    events: Vec<Arc<Envelope>>,
    positions: std::collections::HashMap<EventId, usize>,
    /// Cached read-only handle; lazily opened on first refresh.
    handle: Option<File>,
    /// Path to the session log (cached to avoid recomputing).
    path: PathBuf,
}

impl SessionReader {
    pub(super) fn new(root: &Path, session: &SessionId) -> Self {
        Self {
            offset: 0,
            line: 0,
            events: Vec::new(),
            positions: std::collections::HashMap::new(),
            handle: None,
            path: log_file(&root.join(session)),
        }
    }

    pub(super) fn read(&mut self, session: &SessionId) -> Result<Vec<Arc<Envelope>>, LogError> {
        self.refresh(session)?;
        Ok(self.events.clone())
    }

    pub(super) fn read_after(
        &mut self,
        session: &SessionId,
        after: &str,
    ) -> Result<Option<Vec<Arc<Envelope>>>, LogError> {
        self.refresh(session)?;
        Ok(self
            .positions
            .get(after)
            .map(|index| self.events[index + 1..].to_vec()))
    }

    fn refresh(&mut self, session: &SessionId) -> Result<(), LogError> {
        // Open or reuse the cached file handle.
        if self.handle.is_none() {
            let file = File::open(&self.path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    LogError::NotFound(session.clone())
                } else {
                    LogError::Io(error)
                }
            })?;
            self.handle = Some(file);
        }
        let file = self.handle.as_mut().unwrap();
        if file.metadata()?.len() < self.offset {
            // File was truncated (torn-tail heal); reset state but keep
            // the handle — just seek back to start.
            self.offset = 0;
            self.line = 0;
            self.events.clear();
            self.positions.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(&mut *file);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            let n = reader.read_until(b'\n', &mut bytes)?;
            if n == 0 || !bytes.ends_with(b"\n") {
                break;
            }
            if !bytes.iter().all(u8::is_ascii_whitespace) {
                let mut event = if self.events.is_empty() {
                    parse_header_line(&bytes, self.line + 1)?
                } else {
                    parse_line(&bytes, self.line + 1, self.offset)?
                };
                elide_payloads(&mut event.event);
                self.positions.insert(event.id.clone(), self.events.len());
                self.events.push(Arc::new(event));
            }
            self.offset += n as u64;
            self.line += 1;
        }
        if self.events.is_empty() {
            return Err(LogError::Corrupt {
                line: 0,
                offset: 0,
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
    use rness_protocol::events::{AssistantMessage, ContentPart, UserIntent, UserMessage};

    fn user_msg(text: &str) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text: text.into() }],
            source: None,
        })
    }

    #[test]
    fn cached_reads_elide_audit_payloads_but_disk_keeps_them() {
        use rness_protocol::events::{ChunkDelta, Compaction, TimedChunk};
        let root = tempfile::tempdir().unwrap();
        let sid = "elide".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        // Disk must hold every payload (as legacy logs do) to test elision.
        log.set_record_stream(true);
        log.append(&SessionEvent::CompactionStarted {
            model: "m".into(),
            sources: vec!["a".into()],
            estimated_input: 7,
            request: serde_json::json!({"big": "context"}),
        })
        .unwrap();
        log.append(&SessionEvent::CompactionRequest {
            started: "s".into(),
            body: "{\"big\":\"body\"}".into(),
        })
        .unwrap();
        log.append(&SessionEvent::CompactionFinished {
            started: "s".into(),
            outcome: "committed".into(),
            usage: Default::default(),
            chunks: vec![TimedChunk {
                ms: 1,
                delta: ChunkDelta::Text { t: "x".into() },
            }],
        })
        .unwrap();
        log.append(&SessionEvent::Compaction(Compaction {
            replaces: vec!["a".into()],
            summary: "kept".into(),
            model: "m".into(),
        }))
        .unwrap();
        let disk_before = fs::read(log.path()).unwrap();

        let mut reader = SessionReader::new(root.path(), &sid);
        let cached = reader.read(&sid).unwrap();
        let elided = log.read_all_elided().unwrap();
        for events in [
            cached.iter().map(|e| e.event.clone()).collect::<Vec<_>>(),
            elided.into_iter().map(|e| e.event).collect(),
        ] {
            assert!(events.iter().any(|e| matches!(e, SessionEvent::CompactionStarted {
                request: serde_json::Value::Null, estimated_input: 7, sources, .. } if sources.len() == 1)));
            assert!(events.iter().any(
                |e| matches!(e, SessionEvent::CompactionRequest { body, .. } if body.is_empty())
            ));
            assert!(events.iter().any(
                |e| matches!(e, SessionEvent::CompactionFinished { chunks, .. } if chunks.is_empty())
            ));
            assert!(events.iter().any(
                |e| matches!(e, SessionEvent::Compaction(c) if c.summary == "kept" && c.replaces.len() == 1)
            ));
        }
        // Full reads and the file itself keep every payload.
        let full = log.read_all().unwrap();
        assert!(full.iter().any(|e| matches!(&e.event,
            SessionEvent::CompactionRequest { body, .. } if body == "{\"big\":\"body\"}")));
        assert!(full.iter().any(|e| matches!(&e.event,
            SessionEvent::CompactionFinished { chunks, .. } if chunks.len() == 1)));
        assert_eq!(fs::read(log.path()).unwrap(), disk_before);
    }

    #[test]
    fn committed_streams_are_dropped_unless_recording_is_on() {
        use rness_protocol::events::{
            AssistantAttempt, AssistantMessage, AttemptOutcome, ChunkDelta, StopReason, TimedChunk,
        };
        let chunks = || {
            vec![TimedChunk {
                ms: 3,
                delta: ChunkDelta::Text { t: "hi".into() },
            }]
        };
        let finished = |outcome: &str| SessionEvent::CompactionFinished {
            started: "s".into(),
            outcome: outcome.into(),
            usage: Default::default(),
            chunks: chunks(),
        };
        let events = [
            SessionEvent::AssistantMessage(AssistantMessage {
                model: "m".into(),
                content: vec![rness_protocol::events::ContentPart::Text { text: "hi".into() }],
                stop: StopReason::EndTurn,
                usage: Default::default(),
                estimated_input: 0,
                chunks: chunks(),
            }),
            finished("committed"),
            // Failures/rejections keep their only record of the reply.
            SessionEvent::AssistantAttempt(AssistantAttempt {
                model: "m".into(),
                outcome: AttemptOutcome::Cancelled,
                chunks: chunks(),
            }),
            finished("non_shrinking"),
            finished("failed: TIMEOUT: x"),
        ];
        let kept = |e: &SessionEvent| match e {
            SessionEvent::AssistantMessage(m) => m.chunks.len(),
            SessionEvent::AssistantAttempt(a) => a.chunks.len(),
            SessionEvent::CompactionFinished { chunks, .. } => chunks.len(),
            _ => unreachable!(),
        };
        for (record, expected) in [(false, [0, 0, 1, 1, 1]), (true, [1, 1, 1, 1, 1])] {
            let root = tempfile::tempdir().unwrap();
            let sid = "stream".to_string();
            let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
            log.set_record_stream(record);
            let returned: Vec<_> = events.iter().map(|e| log.append(e).unwrap()).collect();
            let disk = log.read_all().unwrap();
            for (index, want) in expected.into_iter().enumerate() {
                assert_eq!(
                    kept(&disk[index + 1].event),
                    want,
                    "record={record} #{index}"
                );
                // The returned envelope is exactly what was committed.
                assert_eq!(returned[index], disk[index + 1]);
            }
            // The result itself is never dropped.
            assert!(
                matches!(&disk[1].event, SessionEvent::AssistantMessage(m) if m.content.len() == 1)
            );
        }
    }

    #[test]
    fn compaction_started_omits_request_and_legacy_lines_still_parse() {
        let event = SessionEvent::CompactionStarted {
            model: "m".into(),
            sources: vec!["a".into()],
            estimated_input: 7,
            request: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("request"), "{json}");
        assert_eq!(serde_json::from_str::<SessionEvent>(&json).unwrap(), event);
        // Older logs: a full request, and the separate raw-body event.
        let legacy: SessionEvent = serde_json::from_str(
            r#"{"type":"compaction/started","model":"m","sources":[],"estimated_input":1,"request":{"system":"s"}}"#,
        )
        .unwrap();
        assert!(
            matches!(legacy, SessionEvent::CompactionStarted { request, .. } if request["system"] == "s")
        );
        let legacy: SessionEvent =
            serde_json::from_str(r#"{"type":"compaction/request","started":"x","body":"{}"}"#)
                .unwrap();
        assert!(matches!(legacy, SessionEvent::CompactionRequest { .. }));
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
        let mut reader = SessionReader::new(root.path(), &sid);
        assert_eq!(reader.read(&sid).unwrap().len(), 1);
        let offset = reader.offset;
        assert_eq!(reader.read(&sid).unwrap().len(), 1);
        assert_eq!(reader.offset, offset);
        log.append(&user_msg("hello")).unwrap();
        assert_eq!(reader.read(&sid).unwrap().len(), 2);
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
        assert_eq!(reader.read(&sid).unwrap().len(), 2);
        assert_eq!(reader.offset, offset);
        log.file.write_all(&bytes[split..]).unwrap();
        assert_eq!(reader.read(&sid).unwrap().len(), 3);
        assert_eq!(
            reader
                .read(&sid)
                .unwrap()
                .into_iter()
                .map(|event| (*event).clone())
                .collect::<Vec<_>>(),
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
        let committed = fs::read(&path).unwrap();

        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"not json at all\n").unwrap();
        drop(f);

        // Lockless readers report it (they must not guess)...
        assert!(matches!(
            read_session(root.path(), &sid),
            Err(LogError::Corrupt { line: 2, .. })
        ));
        // ...the writer quarantines it: bytes to a sidecar, audit event.
        let log = SessionLog::open(root.path(), &sid).unwrap();
        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 2);
        let SessionEvent::Repair(repair) = &events[1].event else {
            panic!("expected repair, got {:?}", events[1]);
        };
        assert!(fs::read(&path).unwrap().starts_with(&committed));
        assert_eq!(
            fs::read(path.parent().unwrap().join(&repair.sidecar)).unwrap(),
            b"not json at all\n"
        );
        // Idempotent: a second open has nothing to repair.
        drop(log);
        let len = fs::metadata(&path).unwrap().len();
        drop(SessionLog::open(root.path(), &sid).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), len);
    }

    fn deep_tool_call(depth: usize) -> SessionEvent {
        let mut args = serde_json::json!("leaf");
        for _ in 0..depth {
            args = serde_json::json!([args]);
        }
        SessionEvent::AssistantMessage(AssistantMessage {
            model: "m".into(),
            content: vec![ContentPart::ToolUse {
                call: "c1".into(),
                name: "t".into(),
                args,
            }],
            stop: rness_protocol::events::StopReason::EndTurn,
            usage: Default::default(),
            estimated_input: 0,
            chunks: vec![],
        })
    }

    #[test]
    fn deeply_nested_tool_args_survive_reopen_and_stay_readable() {
        for depth in [125, 126, 127, 200] {
            let root = tempfile::tempdir().unwrap();
            let sid: SessionId = "01DEEP".into();
            let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
            let event = deep_tool_call(depth);
            let committed = log.append(&event).unwrap();
            let path = log.path().to_path_buf();
            drop(log);
            let before = fs::read(&path).unwrap();
            // Reopen heals the tail: the deep event is the last line.
            let log = SessionLog::open(root.path(), &sid).unwrap();
            assert_eq!(
                fs::read(&path).unwrap(),
                before,
                "depth {depth}: file changed"
            );
            let events = log.read_all().unwrap();
            assert_eq!(events.len(), 2, "depth {depth}");
            assert_eq!(events[1], committed, "depth {depth}");
            assert!(read_session(root.path(), &sid).is_ok());
            let sidecars = fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("quarantine-")
                })
                .count();
            assert_eq!(sidecars, 0, "depth {depth}");
        }
    }

    #[test]
    fn nesting_too_deep_to_read_back_is_refused_at_append() {
        let root = tempfile::tempdir().unwrap();
        let sid: SessionId = "01TOODEEP".into();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let before = fs::metadata(log.path()).unwrap().len();
        let result = log.append(&deep_tool_call(rness_protocol::events::MAX_JSON_DEPTH));
        assert!(matches!(result, Err(LogError::Io(_))), "{result:?}");
        assert_eq!(fs::metadata(log.path()).unwrap().len(), before);
        // The handle is still usable.
        log.append(&user_msg("after")).unwrap();
        assert_eq!(log.read_all().unwrap().len(), 2);
    }

    #[test]
    fn recursion_limited_lines_are_never_garbage_but_syntax_errors_are() {
        let deep = format!("{}1{}", "[".repeat(300), "]".repeat(300));
        // Too deep even for the bounded retry, and not our writer's prefix.
        assert!(matches!(
            classify_line(deep.as_bytes()),
            LineClass::Suspicious
        ));
        // A torn (syntactically invalid) line is garbage even with our prefix.
        let torn = br#"{"id":"01X","at":"t","type":"user/mess"#;
        assert!(matches!(classify_line(torn), LineClass::Garbage));
        assert!(matches!(classify_line(b"not json"), LineClass::Garbage));
    }

    #[test]
    fn read_envelopes_from_reports_absolute_line_numbers() {
        let root = tempfile::tempdir().unwrap();
        let sid: SessionId = "01LINES".into();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        log.append(&user_msg("a")).unwrap();
        log.append(&user_msg("b")).unwrap();
        let path = log.path().to_path_buf();
        drop(log);
        let offset = fs::metadata(&path).unwrap().len();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":\"1\",\"at\":\"t\",\"type\":\"user/message\"}\n")
            .unwrap();
        drop(f);
        // header + 2 events precede `offset`; the bad line is line 4.
        let from_offset = read_envelopes_from(&path, offset, u64::MAX).unwrap_err();
        let from_start = read_envelopes_from(&path, 0, u64::MAX).unwrap_err();
        assert!(
            matches!(from_offset, LogError::Corrupt { line: 4, .. }),
            "{from_offset:?}"
        );
        assert!(
            matches!(from_start, LogError::Corrupt { line: 4, .. }),
            "{from_start:?}"
        );
    }

    #[test]
    fn nul_run_tail_and_multiple_garbage_lines_are_quarantined_together() {
        let root = tempfile::tempdir().unwrap();
        let sid: SessionId = "01NUL".into();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        log.append(&user_msg("kept")).unwrap();
        let path = log.path().to_path_buf();
        drop(log);
        let mut tail = vec![0u8; 4096];
        tail.extend_from_slice(b"\n\n  \nxx\n");
        tail.extend_from_slice(&[0u8; 100]); // torn: no newline
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&tail).unwrap();
        drop(f);
        let log = SessionLog::open(root.path(), &sid).unwrap();
        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 3);
        let SessionEvent::Repair(repair) = &events[2].event else {
            panic!("expected repair");
        };
        // The torn part is truncated; the terminated region is quarantined.
        assert_eq!(repair.bytes, 4096 + 8);
        assert_eq!(repair.lines, 2);
    }

    #[test]
    fn append_batch_writes_lines_in_order_with_one_sync() {
        let root = tempfile::tempdir().unwrap();
        let sid = "batch".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let syncs = log.sync_count();
        assert!(log.append_batch(&[]).unwrap().is_empty());
        assert_eq!(log.sync_count(), syncs, "empty batch: no write, no sync");
        let events: Vec<_> = (0..100).map(|i| user_msg(&format!("m{i}"))).collect();
        let committed = log.append_batch(&events).unwrap();
        assert_eq!(log.sync_count(), syncs + 1);
        let read = log.read_all().unwrap();
        assert_eq!(read.len(), 101);
        assert_eq!(&read[1..], &committed[..]);
        // Ids strictly increasing, also across the single-append path.
        let next = log.append(&user_msg("after")).unwrap();
        let ids: Vec<_> = log
            .read_all()
            .unwrap()
            .into_iter()
            .skip(1)
            .map(|e| e.id)
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
        assert_eq!(ids.last(), Some(&next.id));
    }

    #[test]
    fn append_batch_drops_committed_stream_unless_recording() {
        let root = tempfile::tempdir().unwrap();
        let sid = "batch-stream".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let message = SessionEvent::AssistantMessage(AssistantMessage {
            model: "m".into(),
            content: vec![],
            stop: rness_protocol::events::StopReason::EndTurn,
            usage: Default::default(),
            estimated_input: 0,
            chunks: vec![rness_protocol::events::TimedChunk {
                ms: 1,
                delta: rness_protocol::events::ChunkDelta::Text { t: "x".into() },
            }],
        });
        let off = log.append_batch(std::slice::from_ref(&message)).unwrap();
        log.set_record_stream(true);
        let on = log.append_batch(std::slice::from_ref(&message)).unwrap();
        let chunks = |e: &Envelope| match &e.event {
            SessionEvent::AssistantMessage(m) => m.chunks.len(),
            _ => unreachable!(),
        };
        assert_eq!((chunks(&off[0]), chunks(&on[0])), (0, 1));
    }

    #[test]
    fn batch_cut_at_every_byte_heals_to_a_whole_event_prefix() {
        let root = tempfile::tempdir().unwrap();
        let sid = "batch-cut".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let before = fs::metadata(log.path()).unwrap().len();
        let events: Vec<_> = (0..5)
            .map(|i| user_msg(&format!("event {i} \"q\"")))
            .collect();
        let committed = log.append_batch(&events).unwrap();
        let path = log.path().to_path_buf();
        drop(log);
        let full = fs::read(&path).unwrap();
        for cut in before..=full.len() as u64 {
            fs::write(&path, &full[..cut as usize]).unwrap();
            let log = SessionLog::open(root.path(), &sid).unwrap();
            let read = log.read_all().unwrap();
            let k = read.len() - 1;
            assert_eq!(&read[1..], &committed[..k], "cut {cut}");
            assert!(read
                .iter()
                .all(|e| !matches!(e.event, SessionEvent::Repair(_))));
        }
    }

    #[test]
    fn failed_batch_is_rolled_back_and_unrollbackable_failure_poisons() {
        let root = tempfile::tempdir().unwrap();
        let sid = "batch-fail".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let before = fs::read(log.path()).unwrap();
        let result = log.write_durable_with(b"{\"a\":1}\n{\"b\"", |file, bytes| {
            file.write_all(bytes)?;
            Err(std::io::Error::other("EIO"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(log.path()).unwrap(), before);
        assert!(!log.poisoned);
        log.append(&user_msg("fine")).unwrap();
        // Simulate a rollback that cannot happen.
        log.poisoned = true;
        assert!(log.append(&user_msg("refused")).is_err());
        assert_eq!(log.read_all().unwrap().len(), 2);
    }

    #[test]
    fn failed_partial_append_is_rolled_back() {
        let root = tempfile::tempdir().unwrap();
        let sid = "partial".to_string();
        let mut log = SessionLog::create(root.path(), &sid, None, None, None).unwrap();
        let before = fs::read(log.path()).unwrap();
        // A write that persists half the line, then fails (ENOSPC, EIO).
        let result = log.write_durable_with(b"{\"id\":\"half\",\"at\":\"t\"}\n", |file, bytes| {
            file.write_all(&bytes[..bytes.len() / 2])?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(matches!(result, Err(LogError::Io(_))));
        assert_eq!(fs::read(log.path()).unwrap(), before);
        // The next append starts on a clean line.
        log.append(&user_msg("next")).unwrap();
        assert_eq!(log.read_all().unwrap().len(), 2);
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
