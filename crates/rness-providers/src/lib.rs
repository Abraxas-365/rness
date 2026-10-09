//! LLM provider adapters, each a leaf plugin registering into the engine's
//! provider seam. We own the provider trait — no LLM abstraction crates
//! (they always lag providers; we need exact streaming/tool_use control).

pub mod anthropic;
pub mod auth;
mod chunks;
pub mod headers;
pub mod ollama;
pub mod openai;
pub mod responses;
pub mod routes;
pub mod sse;

// -- stream completeness contract ---------------------------------------------
//
// An adapter's `step` returns `StepOutcome::Committed` only after it saw the
// wire shape's terminal marker: Anthropic `message_stop`; OpenAI chat
// `[DONE]` or a chunk with a non-null `finish_reason`; Responses
// `response.completed` / `response.incomplete`. A stream that ends (EOF)
// without it is a retryable `PROVIDER` failure carrying the partial chunks
// (`truncated`), never a committed message. A non-empty data payload that is
// not JSON is a retryable `PROVIDER` failure too; unknown event types are
// ignored. A 2xx response that is not an event stream is rejected before
// parsing (`reject_non_sse`).

/// The stream ended before `marker`: retryable, partial chunks kept as the
/// failed attempt's record.
fn truncated(
    shape: &str,
    marker: &str,
    partial: Vec<rness_protocol::events::TimedChunk>,
) -> rness_engine::turn::provider::StepOutcome {
    rness_engine::turn::provider::StepOutcome::Failed {
        error: rness_engine::turn::provider::ProviderError {
            code: "PROVIDER",
            retry_after: None,
            message: format!(
                "{shape}: stream ended before {marker} (connection closed or incomplete response)"
            ),
            retryable: true,
        },
        partial,
    }
}

/// `Content-Type` is `text/event-stream` (any parameters/case), or absent.
fn is_event_stream(headers: &reqwest::header::HeaderMap) -> bool {
    match headers.get(reqwest::header::CONTENT_TYPE) {
        None => true,
        Some(value) => value
            .to_str()
            .unwrap_or_default()
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("text/event-stream"),
    }
}

/// Upper bound on establishing a provider connection (TCP + TLS).
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The connect timeout for a stream idle timeout: [`CONNECT_TIMEOUT`], or
/// half the idle timeout when that is shorter, so a black-holed host is
/// reported as a connect failure rather than as a silent provider.
pub(crate) fn connect_timeout(idle: Option<std::time::Duration>) -> std::time::Duration {
    idle.map_or(CONNECT_TIMEOUT, |idle| (idle / 2).min(CONNECT_TIMEOUT))
        .max(std::time::Duration::from_millis(1))
}

/// An HTTP client whose connect phase is bounded by `connect`.
pub(crate) fn http_client(connect: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .build()
        // Same failure mode as `reqwest::Client::new()` (TLS backend init).
        .expect("http client")
}

/// A failed `send()`: a connect timeout is a retryable `TIMEOUT` naming the
/// host; anything else a retryable `PROVIDER` transport error.
pub(crate) fn transport_error(e: &reqwest::Error) -> rness_engine::turn::provider::ProviderError {
    use rness_engine::turn::provider::ProviderError;
    if e.is_connect() && e.is_timeout() {
        let host = e
            .url()
            .and_then(|u| u.host_str())
            .unwrap_or("provider")
            .to_string();
        return ProviderError {
            code: "TIMEOUT",
            retry_after: None,
            message: format!("TIMEOUT: connect timeout to {host}"),
            retryable: true,
        };
    }
    ProviderError {
        code: "PROVIDER",
        retry_after: None,
        message: format!("transport: {e}"),
        retryable: true,
    }
}

/// Characters of a non-JSON error body quoted in an error message.
const ERROR_TEXT_CHARS: usize = 200;

/// A useful one-line message from an HTTP error body, whatever its shape:
/// `error.message` → `error` (string) → `message` → `detail` →
/// `errors[0].message` → the body text (HTML tags stripped, whitespace
/// collapsed, first 200 characters) → `http {status}`.
pub(crate) fn error_message(body: &str, status: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        let found = [
            &v["error"]["message"],
            &v["error"],
            &v["message"],
            &v["detail"],
            &v["errors"][0]["message"],
        ]
        .into_iter()
        .filter_map(|m| m.as_str())
        .find(|m| !m.trim().is_empty());
        if let Some(message) = found {
            return message.to_string();
        }
    }
    let mut text = String::new();
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if in_tag => {}
            _ => text.push(c),
        }
    }
    let text: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(ERROR_TEXT_CHARS)
        .collect();
    if text.is_empty() {
        format!("http {status}")
    } else {
        text
    }
}

/// How much of a non-stream 2xx body is read to explain the failure.
const NON_SSE_BODY_LIMIT: usize = 64 * 1024;

/// A 2xx response that is not an event stream (gateways answering
/// `200 application/json {"error":…}`): read a bounded prefix of the body
/// and fail the step instead of parsing zero events into an empty message.
/// Retryable unless the body says context overflow. `Ok` = stream it.
async fn reject_non_sse(
    response: reqwest::Response,
    idle_timeout: Option<std::time::Duration>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<reqwest::Response, rness_engine::turn::provider::StepOutcome> {
    use rness_engine::turn::provider::{ProviderError, StepOutcome};
    if is_event_stream(response.headers()) {
        return Ok(response);
    }
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let read = async {
        let mut response = response;
        let mut body = Vec::new();
        while body.len() < NON_SSE_BODY_LIMIT {
            match response.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                _ => break,
            }
        }
        body.truncate(NON_SSE_BODY_LIMIT);
        String::from_utf8_lossy(&body).into_owned()
    };
    let body = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(StepOutcome::Cancelled { partial: vec![] }),
        _ = sse::idle_deadline(idle_timeout) => String::new(),
        body = read => body,
    };
    let value = serde_json::from_str::<serde_json::Value>(&body).ok();
    let overflow = value
        .as_ref()
        .is_some_and(|v| is_context_overflow(&v["error"]));
    let detail = error_message(&body, &status.to_string());
    Err(StepOutcome::Failed {
        error: ProviderError {
            code: if overflow { "CONTEXT_OVERFLOW" } else { "HTTP" },
            retry_after: None,
            message: format!(
                "provider returned {status} {content_type} instead of an event stream: {detail}"
            ),
            retryable: !overflow,
        },
        partial: vec![],
    })
}

fn request_error_code(status: u16, body: &str) -> &'static str {
    if !matches!(status, 400 | 413 | 422) {
        return "HTTP";
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return "HTTP";
    };
    if is_context_overflow(&value["error"]) {
        "CONTEXT_OVERFLOW"
    } else {
        "HTTP"
    }
}

fn is_context_overflow(error: &serde_json::Value) -> bool {
    let code = error["code"]
        .as_str()
        .or(error["type"].as_str())
        .unwrap_or("");
    let message = error["message"].as_str().unwrap_or("").to_lowercase();
    matches!(code, "context_length_exceeded" | "context_window_exceeded")
        || (code == "invalid_request_error" && message.starts_with("prompt is too long"))
}

fn stream_overflow(event: &str, data: &str) -> Option<rness_engine::turn::provider::ProviderError> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let kind = value["type"].as_str().unwrap_or(event);
    let error = if kind == "response.failed" {
        &value["response"]["error"]
    } else if value["error"].is_object() {
        &value["error"]
    } else if kind == "error" {
        &value
    } else {
        return None;
    };
    is_context_overflow(error).then(|| rness_engine::turn::provider::ProviderError {
        code: "CONTEXT_OVERFLOW",
        retry_after: None,
        retryable: false,
        message: error["message"]
            .as_str()
            .unwrap_or("provider context window exceeded")
            .into(),
    })
}

fn rejected_uploads(detail: &str, used: &[(String, String)]) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(detail) else {
        return Vec::new();
    };
    let detail = ["code", "type", "message"]
        .iter()
        .filter_map(|key| value["error"][key].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let lower = detail.to_lowercase();
    let stale = lower.contains("file")
        && [
            "expired",
            "not found",
            "not_found",
            "deleted",
            "does not exist",
            "do not exist",
            "not created under",
            "invalid file id",
            "invalid file_id",
            "file_id invalid",
        ]
        .iter()
        .any(|s| lower.contains(s));
    if !stale {
        return Vec::new();
    }
    let exact: Vec<_> = used
        .iter()
        .filter(|(_, id)| {
            detail
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
                .any(|token| token == id)
        })
        .cloned()
        .collect();
    if exact.is_empty() {
        used.to_vec()
    } else {
        exact
    }
}

fn upload_scope(endpoint: &str, credential: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(endpoint, credential)).expect("string tuple"))
    )
}

async fn upload_with_quota_recovery(
    store: &rness_engine::images::ImageStore,
    scope: &str,
    protected: &[String],
    upload: impl Fn() -> Result<reqwest::RequestBuilder, rness_engine::turn::provider::ProviderError>,
    delete: impl Fn(&str) -> reqwest::RequestBuilder,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, rness_engine::turn::provider::ProviderError> {
    for attempt in 0..2 {
        let request = upload()?;
        let mut response = tokio::select! {
            _ = cancel.cancelled() => return Err(image_error("image upload cancelled".into())),
            result = request.timeout(std::time::Duration::from_secs(60)).send() => result.map_err(|e| image_error(e.to_string()))?,
        };
        let status = response.status();
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(image_error("image upload cancelled".into())),
                result = response.chunk() => result.map_err(|e| image_error(e.to_string()))?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            if bytes.len() + chunk.len() > 64 * 1024 {
                return Err(image_error("upload response exceeds 64 KiB".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(|e| image_error(e.to_string()));
        }
        let detail = String::from_utf8_lossy(&bytes).to_lowercase();
        let quota = matches!(status.as_u16(), 400 | 403 | 409 | 413 | 429)
            && [
                "storage quota",
                "storage limit",
                "file quota",
                "file count",
                "too many files",
                "stored files",
                "storage_limit_exceeded",
                "file_quota_exceeded",
            ]
            .iter()
            .any(|s| detail.contains(s));
        if attempt != 0 || !quota {
            return Err(image_error(format!("image upload failed ({status})")));
        }
        let mut reclaimed = false;
        for id in store
            .oldest_owned_uploads(scope, protected)
            .map_err(image_error)?
        {
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(image_error("image cleanup cancelled".into())),
                result = delete(&id).timeout(std::time::Duration::from_secs(30)).send() => result.map_err(|e| image_error(e.to_string()))?,
            };
            if !response.status().is_success()
                && response.status() != reqwest::StatusCode::NOT_FOUND
            {
                return Err(image_error(format!(
                    "image quota cleanup failed ({})",
                    response.status()
                )));
            }
            store.forget_owned_upload(scope, &id).map_err(image_error)?;
            reclaimed = true;
        }
        if !reclaimed {
            return Err(image_error("image upload quota exceeded; no unprotected rness-owned uploads are safe to reclaim".into()));
        }
    }
    unreachable!("bounded upload attempts return")
}

#[cfg(test)]
mod error_message_tests {
    use super::error_message;

    #[test]
    fn connect_timeout_is_ten_seconds_or_half_the_idle_timeout() {
        use super::{connect_timeout, CONNECT_TIMEOUT};
        use std::time::Duration;
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(connect_timeout(None), CONNECT_TIMEOUT);
        assert_eq!(
            connect_timeout(Some(Duration::from_secs(300))),
            CONNECT_TIMEOUT
        );
        assert_eq!(
            connect_timeout(Some(Duration::from_secs(4))),
            Duration::from_secs(2)
        );
        assert_eq!(
            connect_timeout(Some(Duration::ZERO)),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn fallback_chain_covers_gateway_shapes() {
        let cases = [
            (r#"{"error":{"message":"invalid key"}}"#, "invalid key"),
            (r#"{"error":"model 'm' not found"}"#, "model 'm' not found"),
            (r#"{"message":"overloaded"}"#, "overloaded"),
            (r#"{"detail":"region blocked"}"#, "region blocked"),
            (r#"{"errors":[{"message":"quota"}]}"#, "quota"),
            (r#"{"error":{"message":""},"detail":"why"}"#, "why"),
            (
                "<html><body><h1>502 Bad Gateway</h1>upstream\n  connect error</body></html>",
                "502 Bad Gateway upstream connect error",
            ),
            ("upstream request timeout", "upstream request timeout"),
            (r#"{"unrelated":1}"#, r#"{"unrelated":1}"#),
            ("   ", "http 503 Service Unavailable"),
            ("<html></html>", "http 503 Service Unavailable"),
        ];
        for (body, want) in cases {
            assert_eq!(
                error_message(body, "503 Service Unavailable"),
                want,
                "{body}"
            );
        }
        let long = "x".repeat(1000);
        assert_eq!(error_message(&long, "500").len(), 200);
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[test]
    fn streaming_overflow_envelopes_are_classified_without_matching_normal_text() {
        for (event, data) in [
            (
                "",
                r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#,
            ),
            (
                "error",
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 100 > 50"}}"#,
            ),
            (
                "response.failed",
                r#"{"response":{"error":{"code":"context_window_exceeded","message":"too long"}}}"#,
            ),
            (
                "error",
                r#"{"type":"error","code":"context_length_exceeded","message":"too long"}"#,
            ),
        ] {
            let error = stream_overflow(event, data).unwrap();
            assert_eq!(error.code, "CONTEXT_OVERFLOW");
            assert!(!error.retryable);
        }
        for data in [
            "invalid JSON",
            r#"{"type":"response.output_text.delta","delta":"context_length_exceeded"}"#,
            r#"{"error":{"code":"rate_limit_exceeded","message":"too long"}}"#,
        ] {
            assert!(stream_overflow("", data).is_none());
        }
    }

    #[test]
    fn overflow_classification_requires_specific_provider_evidence() {
        for body in [
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 100 tokens > 50 maximum"}}"#,
        ] {
            assert_eq!(request_error_code(400, body), "CONTEXT_OVERFLOW");
            assert_eq!(request_error_code(500, body), "HTTP");
        }
        for body in [
            "context too large",
            r#"{"error":{"message":"quota exceeded"}}"#,
            r#"{"error":{"type":"invalid_request_error","message":"invalid tools"}}"#,
        ] {
            assert_eq!(request_error_code(400, body), "HTTP");
        }
    }

    #[tokio::test]
    async fn quota_recovery_reclaims_unexpired_owned_uploads_and_protects_selected_files() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let store =
            rness_engine::images::ImageStore::new(temp.path().into(), Default::default()).unwrap();
        let endpoint = format!("{}/files", server.uri());
        let scope = upload_scope(&endpoint, "key");
        store.record_owned_upload(&scope, "old", u64::MAX).unwrap();
        store
            .record_owned_upload(&scope, "active", u64::MAX)
            .unwrap();
        store
            .record_owned_upload(&upload_scope(&endpoint, "other-key"), "foreign", 1)
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        Mock::given(method("POST"))
            .and(path("/files"))
            .respond_with(move |_: &wiremock::Request| {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(413).set_body_json(
                        serde_json::json!({"error":{"message":"storage quota exceeded"}}),
                    )
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"new"}))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/files/old"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = upload_with_quota_recovery(
            &store,
            &scope,
            &["active".into()],
            || Ok(client.post(&endpoint)),
            |id| client.delete(format!("{endpoint}/{id}")),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(result["id"], "new");
        assert!(store
            .oldest_owned_uploads(&scope, &["active".into()])
            .unwrap()
            .is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        assert_eq!(
            store
                .oldest_owned_uploads(&upload_scope(&endpoint, "other-key"), &[])
                .unwrap(),
            vec!["foreign"]
        );
    }

    #[tokio::test]
    async fn non_quota_errors_and_repeated_quota_do_not_loop() {
        for (status, message, expected) in
            [(429, "rate limit", 1), (413, "storage quota exceeded", 2)]
        {
            let server = MockServer::start().await;
            let temp = tempfile::tempdir().unwrap();
            let store =
                rness_engine::images::ImageStore::new(temp.path().into(), Default::default())
                    .unwrap();
            let endpoint = format!("{}/files", server.uri());
            let scope = upload_scope(&endpoint, "key");
            store.record_owned_upload(&scope, "old", u64::MAX).unwrap();
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string(message))
                .expect(expected)
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(404))
                .expect(expected - 1)
                .mount(&server)
                .await;
            let client = reqwest::Client::new();
            assert!(upload_with_quota_recovery(
                &store,
                &scope,
                &["active".into()],
                || Ok(client.post(&endpoint)),
                |id| client.delete(format!("{endpoint}/{id}")),
                &tokio_util::sync::CancellationToken::new()
            )
            .await
            .is_err());
        }
    }

    #[test]
    fn stale_errors_select_exact_mappings_or_all_when_unspecified() {
        let used = vec![
            ("a".into(), "file_one".into()),
            ("b".into(), "file_one_more".into()),
        ];
        let error = |message| serde_json::json!({"error":{"message":message}}).to_string();
        assert_eq!(
            rejected_uploads(&error("file_one not found"), &used),
            vec![used[0].clone()]
        );
        assert_eq!(rejected_uploads(&error("file expired"), &used), used);
        assert!(rejected_uploads(&error("invalid image format"), &used).is_empty());
        assert!(rejected_uploads(&error("file upload storage quota exceeded"), &used).is_empty());
        assert!(rejected_uploads(&error("file expired"), &[]).is_empty());
    }

    #[tokio::test]
    async fn cancelled_upload_does_not_clean_up() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let store =
            rness_engine::images::ImageStore::new(temp.path().into(), Default::default()).unwrap();
        let scope = upload_scope(&server.uri(), "key");
        store.record_owned_upload(&scope, "old", u64::MAX).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let client = reqwest::Client::new();
        assert!(upload_with_quota_recovery(
            &store,
            &scope,
            &["active".into()],
            || Ok(client.post(server.uri())),
            |_| client.delete(server.uri()),
            &cancel
        )
        .await
        .is_err());
        assert_eq!(
            store
                .oldest_owned_uploads(&scope, &["active".into()])
                .unwrap(),
            vec!["old"]
        );
    }
}

fn image_error(message: String) -> rness_engine::turn::provider::ProviderError {
    rness_engine::turn::provider::ProviderError {
        code: "UNSUPPORTED_CONTENT",
        retry_after: None,
        message,
        retryable: false,
    }
}

fn validate_image_roles(
    context: &rness_engine::session::projection::ModelContext,
    configured: bool,
) -> Result<(), rness_engine::turn::provider::ProviderError> {
    use rness_engine::session::projection::ModelTurn;
    for turn in &context.turns {
        let (content, user) = match turn {
            ModelTurn::User { content } => (content, true),
            ModelTurn::Assistant { content } => (content, false),
            ModelTurn::ToolResults { results } => {
                if !configured
                    && results.iter().any(|r| {
                        r.content.iter().any(|p| {
                            matches!(
                                p,
                                rness_protocol::events::ToolResultContentPart::Image { .. }
                            )
                        })
                    })
                {
                    return Err(image_error(
                        "tool image attachment resolution is not configured for this provider"
                            .into(),
                    ));
                }
                continue;
            }
        };
        if content
            .iter()
            .any(|p| matches!(p, rness_protocol::events::ContentPart::Image { .. }))
        {
            if !configured {
                return Err(image_error(
                    "image attachment resolution is not configured for this provider".into(),
                ));
            }
            if !user {
                return Err(image_error(
                    "this provider cannot replay assistant image content".into(),
                ));
            }
        }
    }
    Ok(())
}
