//! Lua tool cards: a shared cache of pre-rendered card lines, keyed by
//! tool call id. The HOST evaluates Lua renderers when tool results
//! land (render loop never waits on the VM — same stance as the
//! statusline poll) and publishes here; chat consumes hits and falls
//! back to the built-in card on miss. TUI stays Lua-agnostic: a line is
//! text plus a THEME STYLE NAME resolved at render time.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rness_protocol::events::ToolCallId;

pub use rness_kernel::presentation::StyledLine as CardLine;

/// Shared handle: host writes, chat reads every frame.
#[derive(Clone, Default)]
pub struct CardCache {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    generation: u64,
    revision: u64,
    revisions: HashMap<ToolCallId, u64>,
    changes: std::collections::VecDeque<(u64, ToolCallId)>,
    cards: HashMap<ToolCallId, Arc<Vec<CardLine>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_one_card_does_not_invalidate_other_calls() {
        let cache = CardCache::default();
        cache.insert("a".into(), vec![]);
        let first = cache.call_revision(&"a".into());
        cache.insert("b".into(), vec![]);
        assert_eq!(cache.call_revision(&"a".into()), first);
        assert!(cache.call_revision(&"b".into()) > first);
        cache.invalidate();
        assert_eq!(cache.call_revision(&"a".into()), 0);
    }

    #[test]
    fn reads_share_immutable_card_storage() {
        let cache = CardCache::default();
        cache.insert("call".into(), vec![CardLine { text:"old".into(), ..Default::default() }]);
        let first = cache.get(&"call".into()).unwrap();
        let second = cache.get(&"call".into()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        cache.insert("call".into(), vec![CardLine {text:"new".into(), ..Default::default() }]);
        assert_eq!(first[0].text, "old");
        assert_eq!(cache.get(&"call".into()).unwrap()[0].text, "new");
    }

    #[test]
    fn invalidation_rejects_in_flight_cards() {
        let cache = CardCache::default();
        let generation = cache.generation();
        assert!(cache.insert_if_current(generation, "call".into(), vec![]));
        cache.invalidate();
        assert!(!cache.contains(&"call".into()));
        assert!(!cache.insert_if_current(generation, "call".into(), vec![]));
        assert!(cache.insert_if_current(cache.generation(), "call".into(), vec![]));
    }
}

impl CardCache {
    pub fn changes_since(&self, revision: u64) -> Option<Vec<ToolCallId>> {
        let inner = self.inner.read().expect("card cache lock");
        if revision == inner.revision { return Some(Vec::new()); }
        if inner.changes.front().is_none_or(|(first, _)| revision.saturating_add(1) < *first) { return None; }
        Some(inner.changes.iter().filter(|(r, _)| *r > revision).map(|(_, call)| call.clone()).collect())
    }

    pub fn revision(&self) -> u64 { self.inner.read().expect("card cache lock").revision }

    pub fn call_revision(&self, call: &ToolCallId) -> u64 {
        self.inner.read().expect("card cache lock").revisions.get(call).copied().unwrap_or(0)
    }

    pub fn generation(&self) -> u64 {
        self.inner.read().expect("card cache lock").generation
    }

    pub fn invalidate(&self) {
        let mut inner = self.inner.write().expect("card cache lock");
        inner.generation = inner.generation.checked_add(1).expect("card generation exhausted");
        inner.cards.clear();
        inner.revisions.clear();
        inner.changes.clear();
        inner.revision += 1;
    }

    pub fn insert_if_current(&self, generation: u64, call: ToolCallId, lines: Vec<CardLine>) -> bool {
        let mut inner = self.inner.write().expect("card cache lock");
        if inner.generation != generation { return false; }
        inner.revision += 1;
        let revision = inner.revision;
        inner.revisions.insert(call.clone(), revision);
        inner.changes.push_back((revision, call.clone()));
        if inner.changes.len() > 1024 { inner.changes.pop_front(); }
        inner.cards.insert(call, Arc::new(lines));
        true
    }

    /// Remove a stale live card when a completed renderer declines; readers
    /// must invalidate cached rows and use the built-in completed-result card.
    pub fn remove_if_current(&self, generation: u64, call: &ToolCallId) -> bool {
        let mut inner = self.inner.write().expect("card cache lock");
        if inner.generation != generation { return false; }
        if inner.cards.remove(call).is_some() {
            inner.revision += 1;
            let revision = inner.revision;
            inner.revisions.insert(call.clone(), revision);
            inner.changes.push_back((revision, call.clone()));
            if inner.changes.len() > 1024 { inner.changes.pop_front(); }
        }
        true
    }

    pub fn insert(&self, call: ToolCallId, lines: Vec<CardLine>) {
        let mut inner = self.inner.write().expect("card cache lock");
        inner.revision += 1;
        let revision = inner.revision;
        inner.revisions.insert(call.clone(), revision);
        inner.changes.push_back((revision, call.clone()));
        if inner.changes.len() > 1024 { inner.changes.pop_front(); }
        inner.cards.insert(call, Arc::new(lines));
    }

    pub fn get(&self, call: &ToolCallId) -> Option<Arc<Vec<CardLine>>> {
        self.inner.read().expect("card cache lock").cards.get(call).cloned()
    }

    pub fn contains(&self, call: &ToolCallId) -> bool {
        self.inner.read().expect("card cache lock").cards.contains_key(call)
    }
}
