//! Opt-in local transport. Wire v1: handshake + one bounded NDJSON submission.
use crate::control_journal::{Journal, Request};
use anyhow::{bail, Context};
use rness_engine::service::SessionService;
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
const MAX_REQUEST: usize = 1024 * 1024;

pub struct ControlSocket {
    task: tokio::task::JoinHandle<()>,
    #[cfg(unix)]
    owned: (PathBuf, u64, u64),
}
impl ControlSocket {
    pub async fn shutdown(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}
impl Drop for ControlSocket {
    fn drop(&mut self) {
        self.task.abort();
        #[cfg(unix)]
        if let Ok(meta) = std::fs::symlink_metadata(&self.owned.0) {
            if meta.dev() == self.owned.1 && meta.ino() == self.owned.2 {
                let _ = std::fs::remove_file(&self.owned.0);
            }
        }
    }
}
#[cfg(unix)]
fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    let parent = path
        .parent()
        .context("socket requires a private parent directory")?;
    let directory = std::fs::symlink_metadata(parent)?;
    if !directory.is_dir() || directory.permissions().mode() & 0o077 != 0 {
        bail!("socket directory must be private (0700)");
    }
    let listener =
        UnixListener::bind(path).context("bind socket; existing paths are never overwritten")?;
    let meta = std::fs::symlink_metadata(path)?;
    if directory.uid() != meta.uid() {
        std::fs::remove_file(path)?;
        bail!("socket directory must belong to current user");
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    active: &Arc<RwLock<String>>,
    journal: &mut Journal,
) -> anyhow::Result<()> {
    let session = active.read().unwrap().clone();
    let (read, mut write) = tokio::io::split(stream);
    tokio::time::timeout(
        Duration::from_secs(3),
        write.write_all(
            format!(
                "{}\n",
                json!({"version":1,"session":session,"durable":true})
            )
            .as_bytes(),
        ),
    )
    .await??;
    let mut line = Vec::new();
    let mut reader = BufReader::new(read).take((MAX_REQUEST + 1) as u64);
    tokio::time::timeout(Duration::from_secs(3), reader.read_until(b'\n', &mut line)).await??;
    if line.is_empty() {
        return Ok(());
    } // Handshake-only discovery.
    let reply = if line.len() > MAX_REQUEST || line.last() != Some(&b'\n') {
        json!({"error":"request must be newline-delimited JSON, at most 1 MiB"})
    } else {
        match serde_json::from_slice::<Request>(&line) {
            Ok(request) => {
                let current = active.read().unwrap();
                if request.session != session || *current != session {
                    json!({"error":"active session changed; reconnect and verify the target"})
                } else if request.id.is_empty()
                    || request.id.len() > 128
                    || request.text.trim().is_empty()
                    || request.text.trim_start().starts_with('/')
                {
                    json!({"error":"requires ID (1–128 bytes) and nonempty chat text; slash commands are not supported"})
                } else {
                    let id = request.id.clone();
                    match journal.accept(request) {
                        Ok(()) => {
                            json!({"id":id,"session":session,"status":"accepted","durable":true})
                        }
                        Err(error) => json!({"error":error.to_string()}),
                    }
                }
            }
            Err(_) => json!({"error":"expected only id, session, text"}),
        }
    };
    tokio::time::timeout(
        Duration::from_secs(3),
        write.write_all(format!("{reply}\n").as_bytes()),
    )
    .await??;
    Ok(())
}
pub fn start(
    path: &Path,
    sessions: Arc<SessionService>,
    active: Arc<RwLock<String>>,
    notices: tokio::sync::mpsc::UnboundedSender<rness_tui::app::Action>,
) -> anyhow::Result<ControlSocket> {
    let mut journal = Journal::open(sessions.store().root())?;
    #[cfg(unix)]
    let listener = bind(path)?;
    #[cfg(unix)]
    let meta = std::fs::symlink_metadata(path)?;
    #[cfg(windows)]
    let mut listener = crate::control_windows::server(path, true)?;
    let task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                accepted = async {
                    #[cfg(unix)] { listener.accept().await.map(|(stream, _)| stream) }
                    #[cfg(windows)] { listener.connect().await }
                } => {
                    #[cfg(unix)] match accepted {
                        Ok(stream) => { let _ = exchange(stream, &active, &mut journal).await; }
                        Err(_) => break,
                    }
                    #[cfg(windows)] {
                        if accepted.is_err() { break; }
                        let _ = exchange(&mut listener, &active, &mut journal).await;
                        // DisconnectNamedPipe discards unread replies. Give the
                        // client time to consume the reply and close first.
                        let mut byte = [0u8];
                        let _ = tokio::time::timeout(Duration::from_secs(3), listener.read(&mut byte)).await;
                        let _ = listener.disconnect();
                    }
                },
                _ = tick.tick() => {
                    let current = active.read().unwrap();
                    if let Err(error) = journal.drain(&sessions, &current) {
                        let _ = notices.send(rness_tui::app::Action::Notice(format!("External prompt queue blocked: {error}. Fix configuration and restart to retry; queued text is preserved.")));
                    }
                }
            }
        }
    });
    Ok(ControlSocket {
        task,
        #[cfg(unix)]
        owned: (path.into(), meta.dev(), meta.ino()),
    })
}

#[derive(clap::Args)]
pub struct SendArgs {
    /// Unix socket path or Windows named pipe path.
    #[arg(long)]
    pub socket: PathBuf,
    /// Expected displayed session (required to prevent targeting the wrong chat).
    #[arg(long)]
    pub session: String,
    /// Stable unique ID. Reuse with identical text after an uncertain acknowledgment.
    #[arg(long)]
    pub id: String,
    /// Prompt text; omit to read UTF-8 text from stdin.
    pub text: Option<String>,
}
pub async fn send(args: SendArgs) -> anyhow::Result<()> {
    let text = if let Some(text) = args.text {
        text
    } else {
        let mut bytes = Vec::new();
        tokio::io::stdin()
            .take(MAX_REQUEST as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        String::from_utf8(bytes)?
    };
    let wire = serde_json::to_vec(&Request {
        id: args.id.clone(),
        session: args.session.clone(),
        text,
    })?;
    if wire.len() + 1 > MAX_REQUEST {
        bail!("request exceeds 1 MiB");
    }
    #[cfg(unix)]
    let stream = tokio::time::timeout(Duration::from_secs(10), UnixStream::connect(&args.socket))
        .await
        .context("connect timeout; nothing sent")??;
    #[cfg(windows)]
    let stream = {
        crate::control_windows::validate(&args.socket)?;
        tokio::net::windows::named_pipe::ClientOptions::new().open(&args.socket)?
    };
    let mut stream = BufReader::new(stream);
    let hello = response(&mut stream).await?;
    if hello["version"] != 1 || hello["session"] != args.session || hello["durable"] != true {
        bail!("unexpected session/handshake; nothing sent");
    }
    eprintln!(
        "submission ID: {} (reuse with same session/text if acknowledgment is lost)",
        args.id
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        stream.get_mut().write_all(&wire).await?;
        stream.get_mut().write_all(b"\n").await
    })
    .await
    .context("write timeout; delivery unknown, retry same ID/session/text")??;
    let reply = response(&mut stream).await?;
    if reply["durable"] != true
        || reply["status"] != "accepted"
        || reply["id"] != args.id
        || reply["session"] != args.session
    {
        bail!("submission not acknowledged: {reply}");
    }
    println!("{reply}");
    Ok(())
}
async fn response<S: AsyncRead + Unpin>(stream: &mut BufReader<S>) -> anyhow::Result<Value> {
    let mut line = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.take(65537).read_until(b'\n', &mut line),
    )
    .await??;
    if line.len() > 65536 || line.last() != Some(&b'\n') {
        bail!("invalid control response");
    }
    Ok(serde_json::from_slice(&line)?)
}
#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    #[test]
    fn cli_modes() {
        crate::Cli::command().debug_assert();
        assert!(
            crate::Cli::try_parse_from(["rness", "--control-socket", "x", "-p", "hello"]).is_err()
        );
        assert!(crate::Cli::try_parse_from([
            "rness",
            "send",
            "--socket",
            "x",
            "--session",
            "s",
            "--id",
            "i",
            "hello"
        ])
        .is_ok());
    }
    #[tokio::test]
    async fn socket_submission_does_not_answer_pending_questions_or_plan_reviews() {
        use rness_engine::{
            questions::{AskUser, Questions},
            tools::Tool,
        };
        for plan in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = Arc::new(rness_engine::session::branch::SessionStore::new(
                root.path(),
            ));
            let mut log = store.create(None).unwrap();
            if plan {
                log.append(&rness_protocol::events::SessionEvent::PlanMode { active: true })
                    .unwrap();
            }
            let session = log.session().clone();
            drop(log);
            let questions = Arc::new(Questions::default());
            questions.set_available(true);
            let mut events = questions.subscribe();
            let q = questions.clone();
            let target = session.clone();
            let waiter = tokio::spawn(async move {
                let cancel = tokio_util::sync::CancellationToken::new();
                if plan {
                    rness_engine::plan::ExitPlan {
                        config: Default::default(),
                        store,
                        questions: q,
                        alive: cancel.clone(),
                    }
                    .review(&target, "call", json!({"plan":"# Plan\nDo work"}), &cancel)
                    .await
                    .map(|(text, _)| text)
                } else {
                    AskUser(q)
                        .execute_call(
                            &target,
                            "call",
                            json!({"questions":[{
                                "id":"question", "question":"Approve?"
                            }]}),
                            &cancel,
                        )
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let mut journal = Journal::open(dir.path()).unwrap();
            let active = Arc::new(RwLock::new(session.clone()));
            let (client, server) = tokio::io::duplex(4096);
            let client = async {
                let mut stream = BufReader::new(client);
                response(&mut stream).await.unwrap();
                stream
                    .get_mut()
                    .write_all(
                        format!(
                            "{}\n",
                            json!({"id":"i","session":session,"text":"New prompt, not an answer"})
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response(&mut stream).await.unwrap()["status"], "accepted");
            };
            let (result, _) = tokio::join!(exchange(server, &active, &mut journal), client);
            result.unwrap();
            assert_eq!(questions.pending().len(), 1);
            assert!(!waiter.is_finished());
            questions.dismiss(&session, "call");
            let _ = waiter.await;
        }
    }
    #[tokio::test]
    async fn protocol_session_switch_and_unsupported_actions() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let active = Arc::new(RwLock::new("s".into()));
        for (wire, switch, valid) in [
            (
                json!({"id":"one","session":"s","text":"hello"}),
                false,
                true,
            ),
            (
                json!({"id":"one","session":"s","text":"hello"}),
                false,
                true,
            ),
            (
                json!({"id":"two","session":"s","text":"hello"}),
                true,
                false,
            ),
            (
                json!({"id":"a","session":"s","text":"hi","approve":true}),
                false,
                false,
            ),
            (
                json!({"id":"b","session":"s","text":"/plan on"}),
                false,
                false,
            ),
        ] {
            *active.write().unwrap() = "s".into();
            let (client, server) = tokio::io::duplex(4096);
            let switched = active.clone();
            let client = async move {
                let mut stream = BufReader::new(client);
                assert_eq!(response(&mut stream).await.unwrap()["session"], "s");
                if switch {
                    *switched.write().unwrap() = "other".into();
                }
                stream
                    .get_mut()
                    .write_all(format!("{wire}\n").as_bytes())
                    .await
                    .unwrap();
                let reply = response(&mut stream).await.unwrap();
                assert_eq!(reply["status"] == "accepted", valid);
            };
            let (result, _) = tokio::join!(exchange(server, &active, &mut journal), client);
            result.unwrap();
        }
    }
}
