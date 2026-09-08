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
    cards: HashMap<ToolCallId, Vec<CardLine>>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    pub fn generation(&self) -> u64 {
        self.inner.read().expect("card cache lock").generation
    }

    pub fn invalidate(&self) {
        let mut inner = self.inner.write().expect("card cache lock");
        inner.generation = inner.generation.checked_add(1).expect("card generation exhausted");
        inner.cards.clear();
    }

    pub fn insert_if_current(&self, generation: u64, call: ToolCallId, lines: Vec<CardLine>) -> bool {
        let mut inner = self.inner.write().expect("card cache lock");
        if inner.generation != generation { return false; }
        inner.cards.insert(call, lines);
        true
    }

    pub fn insert(&self, call: ToolCallId, lines: Vec<CardLine>) {
        self.inner.write().expect("card cache lock").cards.insert(call, lines);
    }

    pub fn get(&self, call: &ToolCallId) -> Option<Vec<CardLine>> {
        self.inner.read().expect("card cache lock").cards.get(call).cloned()
    }

    pub fn contains(&self, call: &ToolCallId) -> bool {
        self.inner.read().expect("card cache lock").cards.contains_key(call)
    }
}
