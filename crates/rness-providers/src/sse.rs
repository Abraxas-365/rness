//! SSE iterator with a cancelable, per-pull inactivity watchdog.
use eventsource_stream::{Event as SseEvent, Eventsource};
use futures_core::Stream;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use std::{sync::Arc, time::Duration};

pub const DEFAULT_IDLE_TIMEOUT: Option<Duration> = Some(Duration::from_secs(300));

pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() { return Some(Duration::from_secs(seconds.min(86400))); }
    let date = jiff::civil::DateTime::strptime("%a, %d %b %Y %H:%M:%S GMT", value).ok()?.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp();
    Some(Duration::from_secs(date.as_second().saturating_sub(jiff::Timestamp::now().as_second()).clamp(0, 86400) as u64))
}

pub async fn idle_deadline(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

pub struct SseReader {
    inner: std::pin::Pin<Box<dyn Stream<Item = Result<SseEvent, String>> + Send>>,
    activity: Arc<tokio::sync::Notify>,
    timeout: Option<Duration>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(3)));
        headers.insert(reqwest::header::RETRY_AFTER, "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::ZERO));
        headers.insert(reqwest::header::RETRY_AFTER, "invalid".parse().unwrap());
        assert_eq!(retry_after(&headers), None);
    }

    #[tokio::test]
    async fn default_matches_dsh_and_disabled_can_be_cancelled() {
        assert_eq!(DEFAULT_IDLE_TIMEOUT, Some(Duration::from_secs(300)));
        let mut reader = SseReader::new(futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>(), None);
        let cancel = CancellationToken::new();
        assert!(tokio::time::timeout(Duration::from_millis(30), reader.pull(&cancel)).await.is_err());
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
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), reader.pull(&CancellationToken::new())).await.unwrap(), SsePull::Timeout));
    }

    #[tokio::test]
    async fn comments_reset_watchdog_and_consumer_time_is_excluded() {
        let stream = futures_util::stream::unfold(0, |n| async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let data = if n < 10 { b": alive\n\n".as_slice() } else { b"data: hello\n\n".as_slice() };
            Some((Ok::<_, std::io::Error>(bytes::Bytes::from_static(data)), n + 1))
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
        let activity = Arc::new(tokio::sync::Notify::new());
        let pulse = activity.clone();
        // Observe complete comment lines without buffering arbitrary provider data.
        // Fragmentary bytes are not progress. CR, LF and CRLF are SSE line endings.
        let mut start = true;
        let mut comment = false;
        let bytes = bytes.inspect(move |item| {
            if let Ok(bytes) = item {
                for byte in bytes {
                    if *byte == b'\r' || *byte == b'\n' {
                        if comment { pulse.notify_one(); }
                        start = true;
                        comment = false;
                    } else if start {
                        comment = *byte == b':';
                        start = false;
                    }
                }
            }
        });
        Self { inner: Box::pin(bytes.eventsource().map(|result| result.map_err(|e| e.to_string()))), activity, timeout }
    }

    pub async fn pull(&mut self, cancel: &CancellationToken) -> SsePull {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return SsePull::Cancelled,
                next = self.inner.next() => return match next {
                    None => SsePull::Done,
                    Some(Err(e)) => SsePull::Error(e),
                    Some(Ok(SseEvent { event, data, .. })) => SsePull::Event { event, data },
                },
                _ = self.activity.notified() => continue,
                _ = idle_deadline(self.timeout) => return SsePull::Timeout,
            }
        }
    }
}
