//! SSE iterator with a cancelable, per-pull inactivity watchdog, over an
//! incremental `text/event-stream` decoder that is linear in the input
//! (every byte is scanned once, however the lines are split across chunks).
use futures_core::Stream;
use futures_util::StreamExt;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_IDLE_TIMEOUT: Option<Duration> = Some(Duration::from_secs(300));

pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds.min(86400)));
    }
    let date = jiff::civil::DateTime::strptime("%a, %d %b %Y %H:%M:%S GMT", value)
        .ok()?
        .to_zoned(jiff::tz::TimeZone::UTC)
        .ok()?
        .timestamp();
    Some(Duration::from_secs(
        date.as_second()
            .saturating_sub(jiff::Timestamp::now().as_second())
            .clamp(0, 86400) as u64,
    ))
}

pub async fn idle_deadline(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

/// Largest single event (its data plus the line being read) the decoder
/// buffers before failing the stream. Tool arguments may legitimately be tens
/// of MiB; this bounds memory against a broken or hostile server.
pub const MAX_EVENT_BYTES: usize = 128 * 1024 * 1024;

pub struct SseReader {
    inner: Pin<Box<dyn Stream<Item = Result<Frame, String>> + Send>>,
    timeout: Option<Duration>,
}

/// One decoded unit: a dispatched event, or a complete comment line (a
/// keep-alive: progress for the idle watchdog, nothing for the consumer).
#[derive(Debug, PartialEq)]
enum Frame {
    Event { event: String, data: String },
    Activity,
}

/// Incremental SSE decoder (WHATWG `text/event-stream` parsing): LF, CRLF and
/// lone CR line endings (a CR ending one chunk and an LF starting the next
/// are one terminator), one optional space after the colon, comment lines,
/// a leading BOM, `event` defaulting to `message`, `id`/`retry` ignored.
/// Only the incomplete current line is buffered, and only new bytes are
/// scanned, so a line split into k chunks costs O(line), not O(k·line).
/// Lines are UTF-8-decoded only once complete, so a multibyte character
/// split across chunks is never decoded early. An unterminated tail at EOF
/// is dropped. All state lives in the struct: a `next()` future dropped by
/// `select!` loses nothing.
struct SseDecoder<S> {
    bytes: Pin<Box<S>>,
    /// The current chunk and how much of it is consumed.
    chunk: bytes::Bytes,
    pos: usize,
    /// Bytes of the incomplete current line from earlier chunks.
    line: Vec<u8>,
    /// The previous line ended with CR at a chunk end: skip one leading LF.
    after_cr: bool,
    first_line: bool,
    event: String,
    /// Data lines, each followed by `\n` (the last one is removed on dispatch).
    data: String,
    ready: VecDeque<Frame>,
    max_event_bytes: usize,
    finished: bool,
}

impl<S> SseDecoder<S> {
    fn new(bytes: S, max_event_bytes: usize) -> Self {
        Self {
            bytes: Box::pin(bytes),
            chunk: bytes::Bytes::new(),
            pos: 0,
            line: Vec::new(),
            after_cr: false,
            first_line: true,
            event: String::new(),
            data: String::new(),
            ready: VecDeque::new(),
            max_event_bytes,
            finished: false,
        }
    }

    /// Scan the rest of the current chunk until it is consumed or a frame
    /// is ready.
    fn scan(&mut self) -> Result<(), String> {
        while self.pos < self.chunk.len() && self.ready.is_empty() {
            let chunk = self.chunk.clone();
            let rest = &chunk[self.pos..];
            if std::mem::take(&mut self.after_cr) && rest[0] == b'\n' {
                self.pos += 1;
                continue;
            }
            match memchr::memchr2(b'\r', b'\n', rest) {
                Some(end) => {
                    let mut consumed = end + 1;
                    if rest[end] == b'\r' {
                        match rest.get(end + 1) {
                            Some(b'\n') => consumed += 1,
                            Some(_) => {}
                            None => self.after_cr = true,
                        }
                    }
                    self.pos += consumed;
                    if self.line.is_empty() {
                        self.check_size(end)?;
                        self.line_done(&rest[..end])?;
                    } else {
                        self.check_size(end)?;
                        let mut line = std::mem::take(&mut self.line);
                        line.extend_from_slice(&rest[..end]);
                        self.line_done(&line)?;
                        line.clear();
                        self.line = line; // keep the allocation
                    }
                }
                None => {
                    self.line.extend_from_slice(rest);
                    self.pos = self.chunk.len();
                    self.check_size(0)?;
                }
            }
        }
        Ok(())
    }

    fn check_size(&self, pending: usize) -> Result<(), String> {
        let size = self.data.len() + self.line.len() + pending;
        if size > self.max_event_bytes {
            return Err(format!("sse event exceeds {} bytes", self.max_event_bytes));
        }
        Ok(())
    }

    fn line_done(&mut self, line: &[u8]) -> Result<(), String> {
        let mut line = std::str::from_utf8(line).map_err(|e| format!("UTF8 error: {e}"))?;
        if std::mem::take(&mut self.first_line) {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        if line.is_empty() {
            let mut data = std::mem::take(&mut self.data);
            let event = std::mem::take(&mut self.event);
            if !data.is_empty() {
                data.pop(); // the `\n` after the last data line
                let event = if event.is_empty() {
                    "message".into()
                } else {
                    event
                };
                self.ready.push_back(Frame::Event { event, data });
            }
            return Ok(());
        }
        if line.starts_with(':') {
            self.ready.push_back(Frame::Activity);
            return Ok(());
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            "event" => self.event = value.to_string(),
            _ => {} // id, retry and unknown fields are not used
        }
        Ok(())
    }
}

impl<S, E> Stream for SseDecoder<S>
where
    S: Stream<Item = Result<bytes::Bytes, E>>,
    E: std::fmt::Display,
{
    type Item = Result<Frame, String>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `SseDecoder` is Unpin: the byte stream is boxed.
        let this = self.get_mut();
        loop {
            if let Some(frame) = this.ready.pop_front() {
                return Poll::Ready(Some(Ok(frame)));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            if this.pos < this.chunk.len() {
                if let Err(e) = this.scan() {
                    this.finished = true;
                    return Poll::Ready(Some(Err(e)));
                }
                continue;
            }
            match this.bytes.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => this.finished = true,
                Poll::Ready(Some(Err(e))) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(format!("Transport error: {e}"))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    this.chunk = chunk;
                    this.pos = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(3)));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&headers), Some(Duration::ZERO));
        headers.insert(reqwest::header::RETRY_AFTER, "invalid".parse().unwrap());
        assert_eq!(retry_after(&headers), None);
    }

    #[tokio::test]
    async fn default_matches_dsh_and_disabled_can_be_cancelled() {
        assert_eq!(DEFAULT_IDLE_TIMEOUT, Some(Duration::from_secs(300)));
        let mut reader = SseReader::new(
            futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>(),
            None,
        );
        let cancel = CancellationToken::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), reader.pull(&cancel))
                .await
                .is_err()
        );
        cancel.cancel();
        assert!(matches!(reader.pull(&cancel).await, SsePull::Cancelled));
    }

    #[tokio::test]
    async fn partial_bytes_do_not_reset_watchdog() {
        let stream = futures_util::stream::unfold((), |_| async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some((Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"x")), ()))
        });
        let mut reader = SseReader::new(stream, Some(Duration::from_millis(30)));
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(1),
                reader.pull(&CancellationToken::new())
            )
            .await
            .unwrap(),
            SsePull::Timeout
        ));
    }

    // -- decoder ---------------------------------------------------------------

    fn ev(event: &str, data: &str) -> Frame {
        Frame::Event {
            event: event.into(),
            data: data.into(),
        }
    }

    /// Decode `chunks` to completion (frames, then an optional final error).
    fn decode_chunks(chunks: Vec<Vec<u8>>, cap: usize) -> (Vec<Frame>, Option<String>) {
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::io::Error>(bytes::Bytes::from(c))),
        );
        let mut decoder = SseDecoder::new(stream, cap);
        let mut frames = Vec::new();
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            match Pin::new(&mut decoder).poll_next(&mut cx) {
                Poll::Ready(Some(Ok(frame))) => frames.push(frame),
                Poll::Ready(Some(Err(e))) => return (frames, Some(e)),
                Poll::Ready(None) => return (frames, None),
                Poll::Pending => unreachable!("iter streams are always ready"),
            }
        }
    }

    /// Every split point, every pair of split points, and one byte at a
    /// time all decode `input` exactly like the unsplit input.
    fn assert_split_invariant(input: &[u8], expected: &[Frame]) {
        let whole = decode_chunks(vec![input.to_vec()], MAX_EVENT_BYTES);
        assert_eq!(
            whole.0,
            expected,
            "unsplit {:?}",
            String::from_utf8_lossy(input)
        );
        assert_eq!(whole.1, None);
        for i in 0..=input.len() {
            for j in i..=input.len() {
                let chunks = vec![
                    input[..i].to_vec(),
                    input[i..j].to_vec(),
                    input[j..].to_vec(),
                ];
                let got = decode_chunks(chunks, MAX_EVENT_BYTES);
                assert_eq!(
                    got,
                    whole,
                    "split at {i},{j} of {:?}",
                    String::from_utf8_lossy(input)
                );
            }
        }
        let bytes = input.iter().map(|b| vec![*b]).collect();
        assert_eq!(
            decode_chunks(bytes, MAX_EVENT_BYTES),
            whole,
            "byte at a time"
        );
    }

    #[test]
    fn decoder_line_endings_lf_crlf_cr_and_mixed() {
        for input in [
            "event: a\ndata: 1\n\ndata: 2\n\n",
            "event: a\r\ndata: 1\r\n\r\ndata: 2\r\n\r\n",
            "event: a\rdata: 1\r\rdata: 2\r\r",
            "event: a\r\ndata: 1\n\rdata: 2\r\n\n",
        ] {
            assert_split_invariant(input.as_bytes(), &[ev("a", "1"), ev("message", "2")]);
        }
    }

    #[test]
    fn decoder_crlf_split_across_chunks_is_one_terminator() {
        // A blank line would dispatch `data: 1` early and drop `event: b`'s pairing.
        let got = decode_chunks(
            vec![
                b"data: 1\r".to_vec(),
                b"\nevent: b\r".to_vec(),
                b"\n\r\n".to_vec(),
            ],
            MAX_EVENT_BYTES,
        );
        assert_eq!(got, (vec![ev("b", "1")], None));
    }

    #[test]
    fn decoder_fields_comments_bom_and_multiline_data() {
        let input =
            "\u{feff}: hello\ndata:x\ndata:  two spaces\ndata\nid: 7\nretry: 10\nfoo: bar\n\n";
        assert_split_invariant(
            input.as_bytes(),
            &[Frame::Activity, ev("message", "x\n two spaces\n")],
        );
        // BOM only counts at stream start.
        assert_split_invariant(
            "data: 1\n\n\u{feff}data: 2\n\n".as_bytes(),
            &[ev("message", "1")],
        );
    }

    #[test]
    fn decoder_event_without_data_is_not_dispatched_and_type_resets() {
        assert_split_invariant(
            b"event: lonely\n\ndata: next\n\n: ping\n",
            &[ev("message", "next"), Frame::Activity],
        );
        // An empty `data:` line is data (an empty line + LF): dispatched as "".
        assert_split_invariant(b"data:\n\n", &[ev("message", "")]);
    }

    #[test]
    fn decoder_many_events_in_one_chunk_and_unterminated_tail_dropped() {
        assert_split_invariant(
            b"data: a\n\ndata: b\n\ndata: c\n\ndata: tail-without-blank-line\n",
            &[ev("message", "a"), ev("message", "b"), ev("message", "c")],
        );
        assert_split_invariant(b"data: no newline", &[]);
    }

    #[test]
    fn decoder_utf8_split_inside_chars_and_invalid_utf8_errors() {
        assert_split_invariant(
            "data: héllo 日本 🎉\n\n".as_bytes(),
            &[ev("message", "héllo 日本 🎉")],
        );
        let (frames, err) = decode_chunks(
            vec![b"data: ok\n\ndata: \xff\n\n".to_vec()],
            MAX_EVENT_BYTES,
        );
        assert_eq!(frames, vec![ev("message", "ok")]);
        assert!(err.unwrap().starts_with("UTF8 error"));
    }

    #[test]
    fn decoder_event_size_cap() {
        // Data accumulated over many lines counts, as does an unterminated line.
        let (frames, err) = decode_chunks(vec![b"data: 12345\ndata: 67890\n".to_vec()], 12);
        assert!(frames.is_empty());
        assert_eq!(err.as_deref(), Some("sse event exceeds 12 bytes"));
        let (_, err) = decode_chunks(vec![b"data: 1234".to_vec(), b"567890".to_vec()], 12);
        assert_eq!(err.as_deref(), Some("sse event exceeds 12 bytes"));
        // At the cap is fine; the cap resets per event.
        let (frames, err) = decode_chunks(vec![b"data: 1234\n\ndata: 5678\n\n".to_vec()], 12);
        assert_eq!((frames.len(), err), (2, None));
    }

    #[test]
    fn decoder_huge_line_in_small_chunks_is_linear() {
        // 8 MiB in 1 KiB chunks: quadratic rescanning would take minutes.
        let payload = "A".repeat(8 << 20);
        let input = format!("data: {payload}\n\n").into_bytes();
        let chunks = input.chunks(1024).map(<[u8]>::to_vec).collect();
        let started = std::time::Instant::now();
        let (frames, err) = decode_chunks(chunks, MAX_EVENT_BYTES);
        assert_eq!(err, None);
        assert_eq!(frames, vec![ev("message", &payload)]);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn cancelled_pull_mid_line_loses_no_bytes() {
        let (tx, rx) =
            tokio::sync::mpsc::unbounded_channel::<Result<bytes::Bytes, std::io::Error>>();
        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let mut reader = SseReader::new(stream, None);
        tx.send(Ok(bytes::Bytes::from_static(b"event: x\ndata: hel")))
            .unwrap();
        // The pull is dropped (as `select!` does) while the line is incomplete.
        assert!(tokio::time::timeout(
            Duration::from_millis(30),
            reader.pull(&CancellationToken::new())
        )
        .await
        .is_err());
        tx.send(Ok(bytes::Bytes::from_static(b"lo\n\n"))).unwrap();
        drop(tx);
        match reader.pull(&CancellationToken::new()).await {
            SsePull::Event { event, data } => {
                assert_eq!((event.as_str(), data.as_str()), ("x", "hello"))
            }
            _ => panic!("expected the resumed event"),
        }
        assert!(matches!(
            reader.pull(&CancellationToken::new()).await,
            SsePull::Done
        ));
    }

    #[tokio::test]
    async fn transport_error_is_reported() {
        let stream = futures_util::stream::iter(vec![
            Ok(bytes::Bytes::from_static(b"data: a\n\n")),
            Err(std::io::Error::other("reset")),
        ]);
        let mut reader = SseReader::new(stream, None);
        let cancel = CancellationToken::new();
        assert!(matches!(reader.pull(&cancel).await, SsePull::Event { .. }));
        assert!(matches!(reader.pull(&cancel).await, SsePull::Error(e) if e.contains("reset")));
    }

    #[tokio::test]
    async fn comments_reset_watchdog_and_consumer_time_is_excluded() {
        let stream = futures_util::stream::unfold(0, |n| async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let data = if n < 10 {
                b": alive\n\n".as_slice()
            } else {
                b"data: hello\n\n".as_slice()
            };
            Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(data)),
                n + 1,
            ))
        });
        let mut reader = SseReader::new(stream, Some(Duration::from_millis(50)));
        let cancel = CancellationToken::new();
        assert!(matches!(reader.pull(&cancel).await, SsePull::Event { .. }));
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(matches!(reader.pull(&cancel).await, SsePull::Event { .. }));
    }
}

pub enum SsePull {
    Event { event: String, data: String },
    Done,
    Cancelled,
    Timeout,
    Error(String),
}

impl SseReader {
    pub fn new<S, E>(bytes: S, timeout: Option<Duration>) -> Self
    where
        S: Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        Self {
            inner: Box::pin(SseDecoder::new(bytes, MAX_EVENT_BYTES)),
            timeout,
        }
    }

    /// Next event. Each wait has a fresh idle deadline; a complete comment
    /// line (keep-alive) restarts it, fragmentary bytes do not.
    pub async fn pull(&mut self, cancel: &CancellationToken) -> SsePull {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return SsePull::Cancelled,
                next = self.inner.next() => return match next {
                    None => SsePull::Done,
                    Some(Err(e)) => SsePull::Error(e),
                    Some(Ok(Frame::Event { event, data })) => SsePull::Event { event, data },
                    Some(Ok(Frame::Activity)) => continue,
                },
                _ = idle_deadline(self.timeout) => return SsePull::Timeout,
            }
        }
    }
}
