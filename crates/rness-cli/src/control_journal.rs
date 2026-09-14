//! Durable admission journal. Its append-only records survive lost acknowledgments
//! and restart; engine provenance closes the journal/log two-phase crash window.
use anyhow::{Context, bail};
use rness_engine::service::SessionService;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: String,
    pub session: String,
    pub text: String,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Pending { request: Request },
    Delivered { id: String },
}
struct Entry {
    request: Request,
    delivered: bool,
}
const MAX_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 65536;
pub struct Journal {
    file: File,
    entries: BTreeMap<String, Entry>,
    order: Vec<String>,
    blocked: BTreeMap<String, String>,
}
impl Journal {
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        let directory = root.join("control");
        std::fs::create_dir_all(&directory)?;
        if !std::fs::symlink_metadata(&directory)?.file_type().is_dir() {
            bail!("control journal directory must not be a symlink");
        }
        let path = directory.join("submissions.jsonl");
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if !metadata.file_type().is_file() {
                bail!("control journal must be a regular file, not a symlink");
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.try_lock()
            .context("another control-enabled TUI owns this session root")?;
        #[cfg(unix)]
        {
            File::open(&directory)?.sync_all()?;
            File::open(root)?.sync_all()?;
        }
        if file.metadata()?.len() > MAX_JOURNAL_BYTES {
            bail!("control journal exceeds 128 MiB; archive with TUI stopped before proceeding");
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let committed = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
        if committed < bytes.len() {
            file.set_len(committed as u64)?;
            file.sync_all()?;
        }
        let mut journal = Self {
            file,
            entries: BTreeMap::new(),
            order: Vec::new(),
            blocked: BTreeMap::new(),
        };
        for line in bytes[..committed]
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
        {
            match serde_json::from_slice(line)
                .context("corrupt control journal; refusing to replay")?
            {
                Record::Pending { request } => {
                    if journal.entries.contains_key(&request.id) {
                        bail!("duplicate journal ID");
                    }
                    journal.order.push(request.id.clone());
                    journal.entries.insert(
                        request.id.clone(),
                        Entry {
                            request,
                            delivered: false,
                        },
                    );
                }
                Record::Delivered { id } => {
                    journal
                        .entries
                        .get_mut(&id)
                        .context("unknown delivered ID")?
                        .delivered = true
                }
            }
        }
        journal.order.retain(|id| !journal.entries[id].delivered);
        journal.file.seek(SeekFrom::End(0))?;
        Ok(journal)
    }
    fn append(&mut self, record: &Record) -> anyhow::Result<()> {
        let mut bytes = serde_json::to_vec(record)?;
        bytes.push(b'\n');
        let start = self.file.stream_position()?;
        if start + bytes.len() as u64 > MAX_JOURNAL_BYTES {
            bail!(
                "control journal full; stop TUI and archive it (archiving resets duplicate protection)"
            );
        }
        if let Err(error) = self
            .file
            .write_all(&bytes)
            .and_then(|_| self.file.sync_all())
        {
            // A failed append is not acknowledged. Restore a valid tail before retry.
            self.file.set_len(start)?;
            self.file.seek(SeekFrom::Start(start))?;
            self.file.sync_all()?;
            return Err(error.into());
        }
        Ok(())
    }
    pub fn accept(&mut self, request: Request) -> anyhow::Result<()> {
        if let Some(old) = self.entries.get(&request.id) {
            if old.request.session != request.session || old.request.text != request.text {
                bail!("submission ID reused with different session/text");
            }
            return Ok(());
        }
        if let Some(error) = self.blocked.get(&request.session) {
            bail!("queue blocked: {error}; fix session configuration then restart TUI to retry");
        }
        if self.entries.len() >= MAX_ENTRIES
            || self.file.metadata()?.len() + request.text.len() as u64 * 6 + 4096
                > MAX_JOURNAL_BYTES - (self.order.len() as u64 + 1) * 850 - 4096
        {
            bail!(
                "control journal admission limit reached; stop and archive before new IDs (duplicate protection resets)"
            );
        }
        self.append(&Record::Pending {
            request: request.clone(),
        })?;
        self.order.push(request.id.clone());
        self.entries.insert(
            request.id.clone(),
            Entry {
                request,
                delivered: false,
            },
        );
        Ok(())
    }
    pub fn drain(&mut self, sessions: &SessionService, active: &str) -> anyhow::Result<()> {
        // Only the displayed session is eligible. Switching does not lose queues
        // or unexpectedly wake a hidden session. FIFO within each session.
        if self.blocked.contains_key(active) {
            return Ok(());
        }
        let next = self
            .order
            .iter()
            .find(|id| {
                let entry = &self.entries[*id];
                !entry.delivered && entry.request.session == active
            })
            .cloned();
        if let Some(id) = next {
            let r = &self.entries[&id].request;
            let delivered = match sessions.deliver_external_once(&r.session, &r.id, r.text.clone())
            {
                Ok(delivered) => delivered,
                Err(rness_engine::service::ServiceError::Busy) => false,
                Err(error) => {
                    self.blocked.insert(active.into(), error.to_string());
                    return Err(error.into());
                }
            };
            if delivered {
                if let Err(error) = self.append(&Record::Delivered { id: id.clone() }) {
                    self.blocked.insert(active.into(), error.to_string());
                    return Err(error);
                }
                self.entries.get_mut(&id).unwrap().delivered = true;
                self.order.retain(|key| key != &id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_records_and_capacity_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        journal.file.set_len(MAX_JOURNAL_BYTES - 4096).unwrap();
        assert!(
            journal
                .accept(Request {
                    id: "i".into(),
                    session: "s".into(),
                    text: "hello".into()
                })
                .is_err()
        );
        assert!(journal.entries.is_empty());
        drop(journal);
        std::fs::write(dir.path().join("control/submissions.jsonl"), b"{invalid}\n").unwrap();
        assert!(Journal::open(dir.path()).is_err());
    }
    #[test]
    fn durable_ack_duplicate_payload_lock_and_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let request = Request {
            id: "id".into(),
            session: "s".into(),
            text: "hello".into(),
        };
        let mut journal = Journal::open(dir.path()).unwrap();
        journal.accept(request.clone()).unwrap();
        assert!(Journal::open(dir.path()).is_err());
        drop(journal);
        let path = dir.path().join("control/submissions.jsonl");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{torn")
            .unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        journal.accept(request.clone()).unwrap();
        assert_eq!(journal.entries.len(), 1);
        assert!(
            journal
                .accept(Request {
                    text: "changed".into(),
                    ..request
                })
                .is_err()
        );
        assert!(!std::fs::read_to_string(path).unwrap().contains("torn"));
    }
}
