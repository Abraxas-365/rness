//! The timed record of one model step's stream, shared by the adapters.
//!
//! Every delta goes to the live sink (`StepRequest::on_delta`) exactly as it
//! arrived. The retained record coalesces adjacent deltas of the same kind
//! (and the same tool call) that arrive within [`COALESCE_MS`] of the
//! piece that opened the chunk, up to [`COALESCE_BYTES`] per chunk: a
//! 10 MiB reply in 16-byte deltas is ~1 k chunks instead of 655 k. The
//! text is byte-identical; only the timing granularity of the record drops
//! to ~20 fps.
//!
//! Failed and cancelled attempts keep the full record (it is their only
//! trace). A committed message already owns its tool arguments, so when a
//! step's tool-argument chunks exceed [`COMMITTED_TOOL_ARGS_LIMIT`] they are
//! dropped from the committed record (which is only persisted with
//! `rness.record_stream` on).

use rness_engine::turn::provider::DeltaSink;
use rness_protocol::events::{ChunkDelta, TimedChunk, ToolCallId};

/// A retained chunk absorbs later deltas for this long after its first one.
pub const COALESCE_MS: u64 = 50;
/// A retained chunk stops absorbing deltas at this size.
pub const COALESCE_BYTES: usize = 16 * 1024;
/// Above this many bytes of tool-argument chunks, a committed message's
/// record keeps only its text and thinking chunks.
pub const COMMITTED_TOOL_ARGS_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy)]
enum Kind<'c> {
    Text,
    Thinking,
    ToolArgs(&'c ToolCallId),
}

impl Kind<'_> {
    fn delta(self, t: String) -> ChunkDelta {
        match self {
            Kind::Text => ChunkDelta::Text { t },
            Kind::Thinking => ChunkDelta::Thinking { t },
            Kind::ToolArgs(call) => ChunkDelta::ToolArgs {
                call: call.clone(),
                t,
            },
        }
    }

    /// The text of `delta` if it is the same kind (and tool call).
    fn open(self, delta: &mut ChunkDelta) -> Option<&mut String> {
        match (self, delta) {
            (Kind::Text, ChunkDelta::Text { t }) | (Kind::Thinking, ChunkDelta::Thinking { t }) => {
                Some(t)
            }
            (Kind::ToolArgs(call), ChunkDelta::ToolArgs { call: last, t }) if last == call => {
                Some(t)
            }
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct ChunkLog<'a> {
    chunks: Vec<TimedChunk>,
    sink: Option<DeltaSink<'a>>,
    /// Reused buffer for the delta handed to the sink (no allocation per
    /// delta on the hot path).
    scratch: String,
    tool_arg_bytes: usize,
}

impl<'a> ChunkLog<'a> {
    pub(crate) fn new(sink: Option<DeltaSink<'a>>) -> Self {
        Self {
            sink,
            ..Default::default()
        }
    }

    pub(crate) fn text(&mut self, t: &str, at_ms: u64) {
        self.push(Kind::Text, t, at_ms);
    }

    pub(crate) fn thinking(&mut self, t: &str, at_ms: u64) {
        self.push(Kind::Thinking, t, at_ms);
    }

    pub(crate) fn tool_args(&mut self, call: &ToolCallId, t: &str, at_ms: u64) {
        self.tool_arg_bytes += t.len();
        self.push(Kind::ToolArgs(call), t, at_ms);
    }

    fn push(&mut self, kind: Kind<'_>, t: &str, at_ms: u64) {
        if t.is_empty() {
            return;
        }
        if let Some(sink) = self.sink {
            let mut scratch = std::mem::take(&mut self.scratch);
            scratch.clear();
            scratch.push_str(t);
            let delta = kind.delta(scratch);
            sink(&delta);
            self.scratch = match delta {
                ChunkDelta::Text { t }
                | ChunkDelta::Thinking { t }
                | ChunkDelta::ToolArgs { t, .. } => t,
            };
        }
        if let Some(last) = self.chunks.last_mut() {
            let fresh = at_ms.saturating_sub(last.ms) < COALESCE_MS;
            if let Some(open) = kind.open(&mut last.delta).filter(|_| fresh) {
                if open.len() + t.len() <= COALESCE_BYTES {
                    open.push_str(t);
                    return;
                }
            }
        }
        self.chunks.push(TimedChunk {
            ms: at_ms,
            delta: kind.delta(t.to_string()),
        });
    }

    /// The full record, for failed and cancelled attempts.
    pub(crate) fn into_partial(self) -> Vec<TimedChunk> {
        self.chunks
    }

    /// The record for a committed message (see the module docs).
    pub(crate) fn into_committed(mut self) -> Vec<TimedChunk> {
        if self.tool_arg_bytes > COMMITTED_TOOL_ARGS_LIMIT {
            self.chunks
                .retain(|c| !matches!(c.delta, ChunkDelta::ToolArgs { .. }));
        }
        self.chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn text(c: &TimedChunk) -> (u64, &str) {
        match &c.delta {
            ChunkDelta::Text { t }
            | ChunkDelta::Thinking { t }
            | ChunkDelta::ToolArgs { t, .. } => (c.ms, t),
        }
    }

    #[test]
    fn coalesces_same_kind_within_window_and_size() {
        let seen = Mutex::new(Vec::new());
        let sink = |d: &ChunkDelta| seen.lock().unwrap().push(d.clone());
        let mut log = ChunkLog::new(Some(&sink));
        let a: ToolCallId = "a".into();
        let b: ToolCallId = "b".into();
        log.text("he", 0);
        log.text("llo", 49); // within 50 ms of the chunk's first piece
        log.text("!", 50); // window over: new chunk
        log.thinking("hm", 51); // other kind: new chunk
        log.text("", 52); // empty: ignored entirely
        log.tool_args(&a, "{", 60);
        log.tool_args(&b, "{", 61); // other call: new chunk
        log.tool_args(&b, "}", 62);
        let big = "x".repeat(COALESCE_BYTES);
        log.text(&big, 200);
        log.text("y", 201); // would exceed the size cap: new chunk
        assert_eq!(seen.lock().unwrap().len(), 9, "the sink sees every delta");
        let chunks = log.into_partial();
        let got: Vec<_> = chunks.iter().map(text).collect();
        assert_eq!(
            got,
            vec![
                (0, "hello"),
                (50, "!"),
                (51, "hm"),
                (60, "{"),
                (61, "{}"),
                (200, big.as_str()),
                (201, "y")
            ]
        );
    }

    #[test]
    fn committed_record_drops_tool_args_only_above_the_limit() {
        let call: ToolCallId = "c".into();
        let mut small = ChunkLog::new(None);
        small.text("t", 0);
        small.tool_args(&call, "{}", 0);
        assert_eq!(small.into_committed().len(), 2);

        let mut big = ChunkLog::new(None);
        big.text("t", 0);
        let piece = "x".repeat(64 * 1024);
        for i in 0..=(COMMITTED_TOOL_ARGS_LIMIT / piece.len()) as u64 {
            big.tool_args(&call, &piece, i * 100);
        }
        let mut partial = ChunkLog::new(None);
        partial.tool_args(&call, &"x".repeat(COMMITTED_TOOL_ARGS_LIMIT + 1), 0);
        assert_eq!(
            partial.into_partial().len(),
            1,
            "failed attempts keep everything"
        );
        let committed = big.into_committed();
        assert_eq!(committed.len(), 1);
        assert!(matches!(committed[0].delta, ChunkDelta::Text { .. }));
    }
}
