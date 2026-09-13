//! Small, disposable completion hints, not transcript projections.
//!
//! Each reader retains only a byte cursor and hint strings. Polls parse newly
//! committed local lines, never clone/replay the accumulated conversation.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

use rness_protocol::events::{ContentPart, Envelope, SessionEvent, SessionId};

use super::branch::{BranchError, SessionStore};
use super::log::{LogError, log_file};

/// Maximum task hint length in Unicode scalar values (not bytes).
pub const TASK_HINT_CHARS: usize = 512;

fn safe_char(c: char) -> char {
    if c.is_control() { ' ' } else { c }
}

/// Incremental hints for one session. Use a separate reader per session.
#[derive(Default)]
pub struct AgentHintReader {
    offset: u64,
    line: usize,
    initialized: bool,
    has_local_config: bool,
    name: Option<String>,
    task: Option<String>,
}

impl AgentHintReader {
    /// Current role (or `subagent`) and the original local, unsourced prompt.
    /// A fork's inherited prompts and engine-injected instructions never count
    /// as its assigned task. Empty task means no such prompt is committed yet.
    pub fn read<'a>(
        &'a mut self,
        store: &SessionStore,
        session: &SessionId,
    ) -> Result<(&'a str, &'a str), BranchError> {
        let mut file = File::open(log_file(&store.root().join(session))).map_err(LogError::from)?;
        if file.metadata().map_err(LogError::from)?.len() < self.offset {
            *self = Self::default();
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(LogError::from)?;
        let mut reader = BufReader::new(file);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            let n = reader
                .read_until(b'\n', &mut bytes)
                .map_err(LogError::from)?;
            // Do not advance over torn tails: retry when the writer commits.
            if n == 0 || !bytes.ends_with(b"\n") {
                break;
            }
            if !bytes.iter().all(u8::is_ascii_whitespace) {
                let envelope: Envelope =
                    serde_json::from_slice(&bytes).map_err(|error| LogError::Corrupt {
                        line: self.line + 1,
                        reason: error.to_string(),
                    })?;
                match envelope.event {
                    SessionEvent::RequestConfig(config) => {
                        // None is an explicit clearing of an inherited role.
                        self.name = config
                            .agent
                            .map(|agent| agent.name.chars().map(safe_char).collect());
                        self.has_local_config = true;
                    }
                    SessionEvent::UserMessage(message)
                        if self.task.is_none() && message.source.is_none() =>
                    {
                        self.task = Some(
                            message
                                .content
                                .iter()
                                .filter_map(|part| match part {
                                    ContentPart::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .enumerate()
                                .flat_map(|(i, text)| {
                                    (i > 0).then_some(' ').into_iter().chain(text.chars())
                                })
                                .take(TASK_HINT_CHARS)
                                .map(safe_char)
                                .collect(),
                        );
                    }
                    _ => {}
                }
            }
            self.offset += n as u64;
            self.line += 1;
        }
        if !self.initialized {
            // Delegation normally stamps a local config before sending its
            // prompt. Older/bare forks may not: resolve their frozen inherited
            // config once, respecting the fork point. Never inherit their task.
            if !self.has_local_config && store.parent(session)?.is_some() {
                self.name = store
                    .history(session)?
                    .into_iter()
                    .rev()
                    .find_map(|entry| match entry.event {
                        SessionEvent::RequestConfig(config) => Some(
                            config
                                .agent
                                .map(|agent| agent.name.chars().map(safe_char).collect()),
                        ),
                        _ => None,
                    })
                    .flatten();
            }
            self.initialized = true;
        }
        Ok((
            self.name.as_deref().unwrap_or("subagent"),
            self.task.as_deref().unwrap_or(""),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rness_protocol::branch::{Delegation, DelegationMode};
    use rness_protocol::events::{
        AgentSnapshot, CallConfig, MessageSource, UserIntent, UserMessage,
    };

    fn config(name: Option<&str>) -> SessionEvent {
        SessionEvent::RequestConfig(CallConfig {
            agent: name.map(|name| AgentSnapshot {
                name: name.into(),
                instructions: "role instructions".into(),
                tools: None,
            }),
            ..Default::default()
        })
    }

    fn prompt(text: &str, source: Option<MessageSource>) -> SessionEvent {
        SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![ContentPart::Text { text: text.into() }],
            source,
        })
    }

    #[test]
    fn text_parts_are_separated_and_role_controls_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut log = store.create(None).unwrap();
        log.append(&config(Some("scout\x1b\n"))).unwrap();
        log.append(&SessionEvent::UserMessage(UserMessage {
            intent: UserIntent::Followup,
            content: vec![
                ContentPart::Text {
                    text: "first".into(),
                },
                ContentPart::Text {
                    text: "second".into(),
                },
            ],
            source: None,
        }))
        .unwrap();
        assert_eq!(
            AgentHintReader::default()
                .read(&store, log.session())
                .unwrap(),
            ("scout  ", "first second")
        );
    }

    #[test]
    fn fork_boundary_role_clearing_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut parent = store.create(None).unwrap();
        parent.append(&config(Some("reviewer"))).unwrap();
        let boundary = parent
            .append(&prompt("inherited user prompt", None))
            .unwrap();
        let mut child = store
            .fork_delegated(
                parent.session(),
                Some(boundary.id),
                Delegation {
                    parent: parent.session().clone(),
                    call: None,
                    depth: 1,
                    mode: DelegationMode::Continuable,
                },
            )
            .unwrap();
        parent.append(&config(Some("later parent role"))).unwrap();
        child
            .append(&prompt(
                "workspace instructions",
                Some(MessageSource::Instructions {
                    identity: "i".into(),
                }),
            ))
            .unwrap();
        let mut hints = AgentHintReader::default();
        assert_eq!(
            hints.read(&store, child.session()).unwrap(),
            ("reviewer", "")
        );
        child.append(&config(None)).unwrap();
        child.append(&prompt("assigned task", None)).unwrap();
        child
            .append(&prompt("followup, not the task", None))
            .unwrap();
        assert_eq!(
            hints.read(&store, child.session()).unwrap(),
            ("subagent", "assigned task")
        );
        child.append(&config(Some("scout"))).unwrap();
        assert_eq!(
            hints.read(&store, child.session()).unwrap(),
            ("scout", "assigned task")
        );
        // Fresh store and reader: all metadata is reconstructed from the log.
        let store = SessionStore::new(dir.path());
        assert_eq!(
            AgentHintReader::default()
                .read(&store, child.session())
                .unwrap(),
            ("scout", "assigned task")
        );
        // Explicit None must also win over inheritance on a cold read.
        child.append(&config(None)).unwrap();
        assert_eq!(
            AgentHintReader::default()
                .read(&store, child.session())
                .unwrap(),
            ("subagent", "assigned task")
        );
    }

    #[test]
    fn spawn_bounded_sanitized_task_and_incremental_torn_tail() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut log = store.create(None).unwrap();
        let mut hints = AgentHintReader::default();
        assert_eq!(hints.read(&store, log.session()).unwrap(), ("subagent", ""));
        log.append(&prompt(
            "job output",
            Some(MessageSource::JobCompletion { id: "j".into() }),
        ))
        .unwrap();
        let text = format!("\x1b\0\n{}", "界".repeat(600));
        log.append(&prompt(&text, None)).unwrap();
        let expected = format!("   {}", "界".repeat(TASK_HINT_CHARS - 3));
        assert_eq!(
            hints.read(&store, log.session()).unwrap(),
            ("subagent", expected.as_str())
        );
        let offset = hints.offset;
        let line = hints.line;
        assert_eq!(
            hints.read(&store, log.session()).unwrap(),
            ("subagent", expected.as_str())
        );
        assert_eq!((hints.offset, hints.line), (offset, line));

        let envelope = Envelope {
            id: "new-config".into(),
            at: "now".into(),
            event: config(Some("reviewer")),
        };
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let split = bytes.len() / 2;
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(log.path())
            .unwrap();
        writer.write_all(&bytes[..split]).unwrap();
        assert_eq!(
            hints.read(&store, log.session()).unwrap(),
            ("subagent", expected.as_str())
        );
        assert_eq!(hints.offset, offset);
        writer.write_all(&bytes[split..]).unwrap();
        writer.write_all(b"\n").unwrap();
        assert_eq!(
            hints.read(&store, log.session()).unwrap(),
            ("reviewer", expected.as_str())
        );
        assert_eq!(hints.line, line + 1);
    }
}
