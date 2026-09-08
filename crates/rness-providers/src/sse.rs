//! Shared SSE plumbing for streaming provider adapters.
//!
//! Wraps a reqwest byte stream in `eventsource-stream` and exposes a
//! simple `next_event` pull with cancellation folded in at the seam —
//! adapters see one of three things: an event, the end, or "cancelled".

use eventsource_stream::{Event as SseEvent, Eventsource};
use futures_core::Stream;
use tokio_util::sync::CancellationToken;

pub struct SseReader<S> {
    inner: std::pin::Pin<Box<S>>,
}

pub enum SsePull {
    /// A parsed `event:`/`data:` pair.
    Event { event: String, data: String },
    /// Stream closed normally.
    Done,
    /// Cancellation fired first.
    Cancelled,
    /// Transport/parse error mid-stream.
    Error(String),
}

impl<S, E> SseReader<eventsource_stream::EventStream<S>>
where
    S: Stream<Item = Result<bytes::Bytes, E>>,
    E: std::error::Error + Send + Sync + Sized + 'static,
{
    pub fn new(bytes: S) -> Self {
        Self { inner: Box::pin(bytes.eventsource()) }
    }

    pub async fn pull(&mut self, cancel: &CancellationToken) -> SsePull {
        use futures_util::StreamExt;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => SsePull::Cancelled,
            next = self.inner.next() => match next {
                None => SsePull::Done,
                Some(Err(e)) => SsePull::Error(e.to_string()),
                Some(Ok(SseEvent { event, data, .. })) => SsePull::Event { event, data },
            },
        }
    }
}
