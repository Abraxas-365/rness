//! LLM provider adapters, each a leaf plugin registering into the engine's
//! provider seam. We own the provider trait — no LLM abstraction crates
//! (they always lag providers; we need exact streaming/tool_use control).

pub mod anthropic;
pub mod auth;
pub mod headers;
pub mod ollama;
pub mod openai;
pub mod responses;
pub mod routes;
pub mod sse;

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
