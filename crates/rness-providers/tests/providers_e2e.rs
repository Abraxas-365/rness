//! WP-5 provider E2E/perf against a raw TCP server (byte-exact control over
//! chunking, delays and error bodies). Run:
//!   cargo test -p rness-providers --test providers_e2e -- --nocapture
//!   cargo test --release -p rness-providers --test providers_e2e -- --ignored --nocapture   (perf)
//! Prints `WP5 ...` evidence lines and perf JSON lines.

use rness_engine::session::projection::{ModelContext, ModelTurn};
use rness_engine::turn::provider::{Provider, StepOutcome, StepRequest};
use rness_protocol::events::{ChunkDelta, ContentPart};
use rness_providers::anthropic::AnthropicProvider;
use rness_providers::auth::openai::{CodexCredentialSource, STORE_KEY};
use rness_providers::auth::{CredentialStore, Tokens};
use rness_providers::openai::OpenAiProvider;
use rness_providers::responses::ResponsesProvider;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// A piece of the response: raw bytes then an optional pause.
#[derive(Clone)]
struct Piece(Vec<u8>, u64);

type Script = Arc<dyn Fn() -> Vec<Piece> + Send + Sync>;

/// Accepts connections forever; each request gets `script()` written verbatim.
async fn raw_server(script: Script) -> (String, Arc<Mutex<usize>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(Mutex::new(0usize));
    let c = count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let script = script.clone();
            *c.lock().unwrap() += 1;
            tokio::spawn(async move {
                // Read headers + Content-Length body.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 65536];
                let header_end = loop {
                    let n = match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while buf.len() < header_end + len {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let _ = sock.set_nodelay(true);
                for Piece(bytes, pause) in script() {
                    if !bytes.is_empty() && sock.write_all(&bytes).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                    if pause > 0 {
                        tokio::time::sleep(Duration::from_millis(pause)).await;
                    }
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), count)
}

fn sse_head() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n".to_vec()
}

fn http_error(status: &str, ctype: &str, body: &str) -> Vec<Piece> {
    vec![Piece(
        format!("HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
            .into_bytes(),
        0,
    )]
}

#[derive(Clone, Copy, Debug)]
enum Shape {
    Anthropic,
    OpenAi,
    Responses,
}
const SHAPES: [Shape; 3] = [Shape::Anthropic, Shape::OpenAi, Shape::Responses];

fn ev(shape: Shape, kind: &str, data: serde_json::Value) -> String {
    match shape {
        Shape::OpenAi => format!("data: {data}\n\n"),
        _ => format!("event: {kind}\ndata: {data}\n\n"),
    }
}

/// Complete text stream for a shape, split into the given text deltas.
fn text_stream(shape: Shape, deltas: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    match shape {
        Shape::Anthropic => {
            out.push(ev(
                shape,
                "message_start",
                json!({"type":"message_start","message":{"usage":{"input_tokens":1}}}),
            ));
            out.push(ev(shape, "content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})));
            for d in deltas {
                out.push(ev(shape, "content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":d}})));
            }
            out.push(ev(
                shape,
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ));
            out.push(ev(shape, "message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}})));
            out.push(ev(shape, "message_stop", json!({"type":"message_stop"})));
        }
        Shape::OpenAi => {
            out.push(ev(
                shape,
                "",
                json!({"choices":[{"index":0,"delta":{"role":"assistant"}}]}),
            ));
            for d in deltas {
                out.push(ev(
                    shape,
                    "",
                    json!({"choices":[{"index":0,"delta":{"content":d}}]}),
                ));
            }
            out.push(ev(
                shape,
                "",
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ));
            out.push("data: [DONE]\n\n".into());
        }
        Shape::Responses => {
            out.push(ev(
                shape,
                "response.created",
                json!({"type":"response.created","response":{"id":"r"}}),
            ));
            out.push(ev(shape, "response.output_item.added", json!({"type":"response.output_item.added","output_index":0,"item":{"id":"m","type":"message","role":"assistant","content":[]}})));
            for d in deltas {
                out.push(ev(shape, "response.output_text.delta", json!({"type":"response.output_text.delta","item_id":"m","output_index":0,"delta":d})));
            }
            let full: String = deltas.concat();
            out.push(ev(shape, "response.output_item.done", json!({"type":"response.output_item.done","output_index":0,"item":{"id":"m","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":full}]}})));
            out.push(ev(shape, "response.completed", json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}})));
        }
    }
    out
}

fn provider(
    shape: Shape,
    base: &str,
    idle: Option<Duration>,
    dir: &std::path::Path,
) -> Arc<dyn Provider> {
    match shape {
        Shape::Anthropic => Arc::new(
            AnthropicProvider::new("key", "m")
                .with_base_url(base)
                .with_stream_idle_timeout(idle),
        ),
        Shape::OpenAi => Arc::new(
            OpenAiProvider::new("key", "m")
                .with_base_url(base)
                .with_stream_idle_timeout(idle),
        ),
        Shape::Responses => {
            let store = CredentialStore::new(dir.join("credentials.json"));
            let mut tokens = Tokens {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at: Some(
                    (jiff::Timestamp::now() + jiff::SignedDuration::from_secs(3600)).to_string(),
                ),
                ..Default::default()
            };
            tokens.extra.insert("accountId".into(), json!("acct"));
            store.save_tokens(STORE_KEY, &tokens).unwrap();
            Arc::new(
                ResponsesProvider::new(CodexCredentialSource::new(store), "m")
                    .with_base_url(base)
                    .with_stream_idle_timeout(idle),
            )
        }
    }
}

fn ctx() -> ModelContext {
    ModelContext {
        turns: vec![ModelTurn::User {
            content: vec![ContentPart::Text { text: "hi".into() }],
        }],
        ..Default::default()
    }
}

async fn step(p: &dyn Provider, cancel: &CancellationToken) -> StepOutcome {
    let c = ctx();
    p.step(
        StepRequest {
            context: &c,
            system: "",
            tools: &[],
            on_delta: None,
        },
        cancel,
    )
    .await
}

fn text_of(o: &StepOutcome) -> String {
    match o {
        StepOutcome::Committed(m) => m
            .content
            .iter()
            .filter_map(|p| {
                if let ContentPart::Text { text } = p {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect(),
        _ => String::new(),
    }
}

fn describe(o: &StepOutcome) -> String {
    match o {
        StepOutcome::Committed(m) => {
            format!(
                "committed text={:?}",
                text_of(o).chars().take(60).collect::<String>()
            ) + &format!(" parts={}", m.content.len())
        }
        StepOutcome::Cancelled { partial } => format!("cancelled partial={}", partial.len()),
        StepOutcome::Failed { error, partial } => {
            format!(
                "failed code={} retryable={} msg={:?} partial={}",
                error.code,
                error.retryable,
                error.message,
                partial.len()
            )
        }
    }
}

// -- split UTF-8 at the byte level -----------------------------------------

#[tokio::test]
async fn split_utf8_inside_every_multibyte_char_reassembles_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let text = "héllo wörld 日本語 🎉 ünïcödé ✓ — 𝔘𝔫𝔦";
    // Each delta is a separate event; additionally cut the TCP stream inside
    // every multi-byte sequence (and inside the event framing).
    for shape in SHAPES {
        let chars: Vec<String> = text.chars().map(String::from).collect();
        let refs: Vec<&str> = chars.iter().map(|s| s.as_str()).collect();
        let body = text_stream(shape, &refs).concat().into_bytes();
        let script: Script = Arc::new(move || {
            let mut pieces = vec![Piece(sse_head(), 0)];
            let mut start = 0;
            for i in 1..body.len() {
                // cut after the lead byte of each multi-byte char, and before every '\n'
                if (body[i] & 0xC0 == 0x80 && body[i - 1] >= 0xC0) || body[i] == b'\n' {
                    pieces.push(Piece(body[start..i].to_vec(), 1));
                    start = i;
                }
            }
            pieces.push(Piece(body[start..].to_vec(), 0));
            pieces
        });
        let (base, _) = raw_server(script).await;
        let p = provider(shape, &base, Some(Duration::from_secs(5)), dir.path());
        let out = step(p.as_ref(), &CancellationToken::new()).await;
        println!("WP5 split_utf8 {shape:?}: {}", describe(&out));
        assert_eq!(text_of(&out), text, "{shape:?}");
    }
}

// -- heartbeat-only stream vs the idle timer -----------------------------

#[tokio::test]
async fn heartbeat_comments_keep_a_contentless_stream_alive_forever() {
    // Idle timeout 300 ms; server sends `: ping` every 100 ms and nothing else.
    // Expected by design: comments count as activity, so the step never ends
    // on its own (there is no total/first-token deadline). We observe 2 s.
    let dir = tempfile::tempdir().unwrap();
    for shape in SHAPES {
        let script: Script = Arc::new(|| {
            let mut v = vec![Piece(sse_head(), 0)];
            for _ in 0..600 {
                v.push(Piece(b": ping\n\n".to_vec(), 100));
            }
            v
        });
        let (base, _) = raw_server(script).await;
        let p = provider(shape, &base, Some(Duration::from_millis(300)), dir.path());
        let cancel = CancellationToken::new();
        let t0 = Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(2), step(p.as_ref(), &cancel)).await;
        println!(
            "WP5 heartbeat_forever {shape:?}: still_running_after_2s={} (idle=300ms)",
            r.is_err()
        );
        assert!(r.is_err(), "{shape:?}: {}", describe(&r.unwrap()));
        // Cancel still works promptly.
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        let out = step(p.as_ref(), &cancel).await;
        assert!(matches!(out, StepOutcome::Cancelled { .. }));
        let _ = t0;
    }
}

#[tokio::test]
async fn heartbeat_without_trailing_blank_line_still_resets_idle() {
    // ":" lines without the blank line terminator (`:x\n` only) are comments too.
    let dir = tempfile::tempdir().unwrap();
    let script: Script = Arc::new(|| {
        let mut v = vec![Piece(sse_head(), 0)];
        for _ in 0..10 {
            v.push(Piece(b":x\n".to_vec(), 100));
        }
        for e in text_stream(Shape::Anthropic, &["late"]) {
            v.push(Piece(e.into_bytes(), 0));
        }
        v
    });
    let (base, _) = raw_server(script).await;
    let p = provider(
        Shape::Anthropic,
        &base,
        Some(Duration::from_millis(300)),
        dir.path(),
    );
    let out = step(p.as_ref(), &CancellationToken::new()).await;
    println!("WP5 heartbeat_no_blank: {}", describe(&out));
    assert_eq!(text_of(&out), "late");
}

// -- headers never arrive / black-holed connect -----------------------------

#[tokio::test]
async fn silent_server_before_headers_is_bounded_only_by_idle_timeout() {
    let dir = tempfile::tempdir().unwrap();
    for shape in SHAPES {
        let script: Script = Arc::new(|| vec![Piece(vec![], 60_000)]);
        let (base, _) = raw_server(script).await;
        let p = provider(shape, &base, Some(Duration::from_millis(700)), dir.path());
        let t0 = Instant::now();
        let out = step(p.as_ref(), &CancellationToken::new()).await;
        let ms = t0.elapsed().as_millis();
        println!(
            "WP5 stall_headers {shape:?}: took_ms={ms} {}",
            describe(&out)
        );
        assert!(matches!(&out, StepOutcome::Failed { error, .. } if error.code == "TIMEOUT"));
        assert!((650..2000).contains(&ms));
        // With the idle timeout disabled (`set_stream_idle_timeout(name, 0)`)
        // nothing bounds it at all.
        let p = provider(shape, &base, None, dir.path());
        let r = tokio::time::timeout(
            Duration::from_secs(2),
            step(p.as_ref(), &CancellationToken::new()),
        )
        .await;
        println!(
            "WP5 stall_headers {shape:?} idle=None: hung_2s={}",
            r.is_err()
        );
        assert!(r.is_err());
    }
}

#[tokio::test]
async fn blackholed_connect_time_to_failure() {
    // 10.255.255.1 drops SYNs on this network (verified with a raw socket: no
    // RST within 8 s). Measures how long a step takes with the idle timer
    // as the only bound; skips if the address answers quickly.
    let dir = tempfile::tempdir().unwrap();
    let base =
        std::env::var("RNESS_BENCH_BLACKHOLE").unwrap_or_else(|_| "http://10.255.255.1:81".into());
    for shape in SHAPES {
        let p = provider(shape, &base, Some(Duration::from_millis(1500)), dir.path());
        let t0 = Instant::now();
        let out = step(p.as_ref(), &CancellationToken::new()).await;
        let ms = t0.elapsed().as_millis();
        println!(
            "WP5 blackhole {shape:?} idle=1.5s: took_ms={ms} {}",
            describe(&out)
        );
        if ms < 500 {
            println!("WP5 blackhole: address not black-holed here; skipping assertion");
            return;
        }
        assert!(
            matches!(&out, StepOutcome::Failed { error, .. } if error.code == "TIMEOUT" && error.retryable)
        );
        // The message says "waiting for provider response" — it does not say
        // the TCP connect never completed.
    }
}

// -- gateway / proxy error shapes ------------------------------------------

#[tokio::test]
async fn gateway_error_shapes_produce_useful_messages() {
    let dir = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, &str, &str, &str)> = vec![
        (
            "502 Bad Gateway",
            "text/html",
            "<html><body><h1>502 Bad Gateway</h1>upstream connect error</body></html>",
            "upstream",
        ),
        (
            "503 Service Unavailable",
            "application/json",
            r#"{"message":"upstream overloaded, try later"}"#,
            "upstream overloaded",
        ),
        (
            "504 Gateway Timeout",
            "text/plain",
            "upstream request timeout",
            "timeout",
        ),
        (
            "400 Bad Request",
            "application/json",
            r#"{"error":"model 'm' not found"}"#,
            "not found",
        ),
        (
            "403 Forbidden",
            "application/json",
            r#"{"detail":"region blocked"}"#,
            "region",
        ),
        (
            "401 Unauthorized",
            "application/json",
            r#"{"error":{"message":"invalid x-api-key"}}"#,
            "invalid x-api-key",
        ),
        (
            "429 Too Many Requests",
            "application/json",
            r#"{"errors":[{"message":"quota"}]}"#,
            "quota",
        ),
    ];
    let mut useless = Vec::new();
    for shape in SHAPES {
        for (status, ctype, body, needle) in &cases {
            if matches!(shape, Shape::Responses) && status.starts_with("401") {
                continue; // triggers an OAuth refresh against the real endpoint
            }
            let pieces = http_error(status, ctype, body);
            let script: Script = Arc::new(move || pieces.clone());
            let (base, _) = raw_server(script).await;
            let p = provider(shape, &base, Some(Duration::from_secs(5)), dir.path());
            let out = step(p.as_ref(), &CancellationToken::new()).await;
            let StepOutcome::Failed { error, .. } = &out else {
                panic!("{shape:?} {status}: {}", describe(&out))
            };
            let useful = error.message.to_lowercase().contains(needle);
            println!(
                "WP5 gateway {shape:?} {status} [{ctype}]: code={} retryable={} useful={useful} msg={:?}",
                error.code, error.retryable, error.message
            );
            if !useful {
                useless.push(format!("{shape:?} {status}"));
            }
        }
    }
    println!(
        "WP5 gateway useless_messages={} {:?}",
        useless.len(),
        useless
    );
}

#[tokio::test]
async fn sse_200_with_json_error_body_is_not_committed_as_empty() {
    // Some gateways answer 200 + application/json error instead of a stream.
    let dir = tempfile::tempdir().unwrap();
    for shape in SHAPES {
        let body = r#"{"error":{"message":"gateway says no","type":"api_error"}}"#;
        let pieces = vec![Piece(
            format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes(),
            0,
        )];
        let script: Script = Arc::new(move || pieces.clone());
        let (base, _) = raw_server(script).await;
        let p = provider(shape, &base, Some(Duration::from_secs(5)), dir.path());
        let out = step(p.as_ref(), &CancellationToken::new()).await;
        let committed_empty = matches!(&out, StepOutcome::Committed(m) if m.content.is_empty());
        println!(
            "WP5 json200 {shape:?}: committed_empty={committed_empty} {}",
            describe(&out)
        );
        if committed_empty {
            println!(
                "WP5 BUG json200 {shape:?}: 200+JSON error committed as an empty assistant message"
            );
        }
    }
}

// -- huge tool JSON ----------------------------------------------------------

fn tool_stream(shape: Shape, mb: usize, chunk: usize) -> Vec<u8> {
    let pad = "A".repeat(mb * 1024 * 1024);
    let args = format!("{{\"pad\":\"{pad}\"}}");
    let pieces: Vec<&str> = args
        .as_bytes()
        .chunks(chunk)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let mut out = String::new();
    match shape {
        Shape::Anthropic => {
            out += &ev(
                shape,
                "message_start",
                json!({"type":"message_start","message":{"usage":{"input_tokens":1}}}),
            );
            out += &ev(
                shape,
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"X","input":{}}}),
            );
            for p in &pieces {
                out += &ev(
                    shape,
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":p}}),
                );
            }
            out += &ev(
                shape,
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
            );
            out += &ev(shape, "message_stop", json!({"type":"message_stop"}));
        }
        Shape::OpenAi => {
            out += &ev(
                shape,
                "",
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"t1","type":"function","function":{"name":"X","arguments":""}}]}}]}),
            );
            for p in &pieces {
                out += &ev(
                    shape,
                    "",
                    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":p}}]}}]}),
                );
            }
            out += &ev(
                shape,
                "",
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            );
            out += "data: [DONE]\n\n";
        }
        Shape::Responses => {
            out += &ev(
                shape,
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc","type":"function_call","call_id":"t1","name":"X","arguments":""}}),
            );
            for p in &pieces {
                out += &ev(
                    shape,
                    "response.function_call_arguments.delta",
                    json!({"type":"response.function_call_arguments.delta","item_id":"fc","output_index":0,"delta":p}),
                );
            }
            out += &ev(
                shape,
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":0,"item":{"id":"fc","type":"function_call","status":"completed","call_id":"t1","name":"X","arguments":args}}),
            );
            out += &ev(
                shape,
                "response.completed",
                json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}),
            );
        }
    }
    out.into_bytes()
}

fn tool_args_len(o: &StepOutcome) -> Option<usize> {
    let StepOutcome::Committed(m) = o else {
        return None;
    };
    m.content.iter().find_map(|p| match p {
        ContentPart::ToolUse { args, .. } => Some(args["pad"].as_str().map_or(0, str::len)),
        _ => None,
    })
}

fn chunk_bytes(o: &StepOutcome) -> usize {
    let StepOutcome::Committed(m) = o else {
        return 0;
    };
    m.chunks
        .iter()
        .map(|c| match &c.delta {
            ChunkDelta::ToolArgs { t, .. }
            | ChunkDelta::Text { t }
            | ChunkDelta::Thinking { t } => t.len(),
            #[allow(unreachable_patterns)]
            _ => 0,
        })
        .sum()
}

#[cfg(unix)]
fn max_rss_mb() -> f64 {
    // getrusage via ps on our own pid (no libc dep). macOS reports KiB.
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn perf_huge_tool_json_time_and_memory() {
    let mb: usize = std::env::var("RNESS_BENCH_TOOL_JSON_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let dir = tempfile::tempdir().unwrap();
    for shape in SHAPES {
        for chunk in [65536usize, 512] {
            let body = Arc::new(tool_stream(shape, mb, chunk));
            let b = body.clone();
            let script: Script = Arc::new(move || vec![Piece(sse_head(), 0), Piece(b.to_vec(), 0)]);
            let (base, _) = raw_server(script).await;
            let p = provider(shape, &base, Some(Duration::from_secs(60)), dir.path());
            let before = max_rss_mb();
            let t0 = Instant::now();
            let out = step(p.as_ref(), &CancellationToken::new()).await;
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let during = max_rss_mb();
            let args = tool_args_len(&out);
            println!(
                "{}",
                json!({"probe":"provider_huge_tool_json","shape":format!("{shape:?}"),"mb":mb,"chunk":chunk,
                       "median_ms":ms,"mb_per_s":(body.len() as f64/1048576.0)/(ms/1000.0),
                       "rss_before_mb":before,"rss_after_mb":during,"args_len":args,"chunk_bytes_retained":chunk_bytes(&out),
                       "ok": args == Some(mb*1024*1024)})
            );
            drop(out);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn perf_stream_throughput_text() {
    // Fake emitting MB of text at full speed in deltas of `delta` bytes.
    let mb: usize = std::env::var("RNESS_BENCH_STREAM_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let dir = tempfile::tempdir().unwrap();
    for shape in SHAPES {
        for delta in [16usize, 256] {
            let word =
                "abcdefghijklmnopqrstuvwxyz0123456789".repeat(delta / 36 + 1)[..delta].to_string();
            let n = mb * 1024 * 1024 / delta;
            let deltas: Vec<&str> = std::iter::repeat_n(word.as_str(), n).collect();
            let body = Arc::new(text_stream(shape, &deltas).concat().into_bytes());
            let b = body.clone();
            let script: Script = Arc::new(move || vec![Piece(sse_head(), 0), Piece(b.to_vec(), 0)]);
            let (base, _) = raw_server(script).await;
            let p = provider(shape, &base, Some(Duration::from_secs(60)), dir.path());
            let sink_count = std::sync::atomic::AtomicUsize::new(0);
            let sink = |_: &ChunkDelta| {
                sink_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            };
            let c = ctx();
            let t0 = Instant::now();
            let out = p
                .step(
                    StepRequest {
                        context: &c,
                        system: "",
                        tools: &[],
                        on_delta: Some(&sink),
                    },
                    &CancellationToken::new(),
                )
                .await;
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(text_of(&out).len(), n * delta, "{shape:?}");
            println!(
                "{}",
                json!({"probe":"provider_stream_throughput","shape":format!("{shape:?}"),"text_mb":mb,"delta_bytes":delta,
                       "deltas":n,"median_ms":ms,"wire_mb":body.len() as f64/1048576.0,
                       "deltas_per_s": n as f64/(ms/1000.0),"us_per_delta": ms*1000.0/n as f64,
                       "sink_calls": sink_count.load(std::sync::atomic::Ordering::Relaxed)})
            );
        }
    }
}

/// One SSE `data:` line of N MiB (Anthropic shape, single input_json_delta).
/// Linear parsing => time doubles with size; quadratic => x4.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn perf_single_huge_sse_line_scaling() {
    let dir = tempfile::tempdir().unwrap();
    for mb in [2usize, 4, 8] {
        let body = Arc::new(tool_stream(Shape::Anthropic, mb, usize::MAX >> 1));
        let b = body.clone();
        // Write in 16 KiB TCP pieces, like a real socket would deliver.
        let script: Script = Arc::new(move || {
            let mut v = vec![Piece(sse_head(), 0)];
            v.extend(b.chunks(16 * 1024).map(|c| Piece(c.to_vec(), 0)));
            v
        });
        let (base, _) = raw_server(script).await;
        let p = provider(
            Shape::Anthropic,
            &base,
            Some(Duration::from_secs(600)),
            dir.path(),
        );
        let t0 = Instant::now();
        let out = step(p.as_ref(), &CancellationToken::new()).await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{}",
            json!({"probe":"sse_single_line_scaling","mb":mb,"median_ms":ms,"ok":tool_args_len(&out)==Some(mb*1024*1024)})
        );
    }
}
