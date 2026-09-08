//! Branch operations over the session store: fork, ancestry, children,
//! and full-history assembly across the fork chain.
//!
//! A fork is a NEW session whose header carries `{parent, at}`. History
//! before the fork point is read from ancestors at query time — replayed,
//! never copied (parents stay immutable, invariant #3). Lineage is
//! acyclic by construction: a fork can only reference an event already
//! committed in the parent (invariant #4), which existed before the child.

use std::path::{Path, PathBuf};

use rness_protocol::branch::{AncestryHop, ChildRef, Delegation, ForkRef};
use rness_protocol::events::{Envelope, EventId, SessionEvent, SessionId};

use super::log::{read_session, LogError, SessionLog};

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
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
        Ok(SessionLog::create(&self.root, &sid, workspace, None, Some(delegation))?)
    }

    pub fn workspace(&self, session: &SessionId) -> Result<Option<String>, BranchError> {
        let events = read_session(&self.root, session)?;
        match &events[0].event {
            SessionEvent::Header(header) => Ok(header.workspace.clone()),
            _ => unreachable!("read_session guarantees header first"),
        }
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
        let events = read_session(&self.root, session)?;
        let at = match at {
            Some(id) => {
                let pos = events.iter().position(|e| e.id == id).ok_or_else(|| {
                    BranchError::ForkPointNotFound { session: session.clone(), at: id.clone() }
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
        let fork = ForkRef { session: session.clone(), at };
        Ok(SessionLog::create(&self.root, &child, workspace, Some(fork), delegation)?)
    }

    /// Delegation lineage of a session, if an agent created it.
    pub fn delegation(&self, session: &SessionId) -> Result<Option<Delegation>, BranchError> {
        let events = read_session(&self.root, session)?;
        match &events[0].event {
            SessionEvent::Header(h) => Ok(h.delegation.clone()),
            _ => unreachable!(),
        }
    }

    /// Parent reference of a session, if it is a fork.
    pub fn parent(&self, session: &SessionId) -> Result<Option<ForkRef>, BranchError> {
        let events = read_session(&self.root, session)?;
        match &events[0].event {
            SessionEvent::Header(h) => Ok(h.parent.clone()),
            _ => unreachable!(),
        }
    }

    /// Ancestry chain, root first, queried session last. Each hop's
    /// `forked_at` is the fork point INTO the next hop.
    pub fn ancestry(&self, session: &SessionId) -> Result<Vec<AncestryHop>, BranchError> {
        let mut chain = vec![AncestryHop { session: session.clone(), forked_at: None }];
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
            let Ok(Some(fork)) = self.parent(&child_id) else { continue };
            if &fork.session == session && at.is_none_or(|a| a == &fork.at) {
                out.push(ChildRef { session: child_id, at: fork.at });
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
            let events = read_session(&self.root, &hop.session)?;
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

    /// All session ids in the store (unordered scan).
    pub fn list(&self) -> Result<Vec<SessionId>, BranchError> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(LogError::from)? {
            let entry = entry.map_err(LogError::from)?;
            if entry.file_type().map_err(LogError::from)?.is_dir() {
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
        let s1 = store.fork(&root_id, Some(a.id.clone())).unwrap().session().clone();
        let s2 = store.fork(&root_id, Some(a.id.clone())).unwrap().session().clone();
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
        root.append(&SessionEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed }).unwrap();
    }
}
