//! WP-5 auth E2E: OAuth refresh race across "processes" (independent
//! CredentialSource instances sharing one credentials file, so separate
//! in-process locks, as two rness processes would have) and credential
//! file atomicity. Fake token endpoint with refresh-token rotation.
//!   cargo test -p rness-providers --test auth_e2e -- --nocapture
//!   ... -- --ignored --nocapture   (refresh latency perf)

use rness_providers::auth::{
    Credential, CredentialSource, CredentialStore, OAuthClient, OAuthConfig, Tokens,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Rotating token endpoint: each refresh_token is single-use (like
/// Anthropic/OpenAI). Returns (url, grants, rejected).
async fn token_server(delay_ms: u64) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    let grants = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(AtomicUsize::new(0));
    let valid = Arc::new(Mutex::new("rt0".to_string()));
    let (g, r) = (grants.clone(), rejected.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let (g, r, valid) = (g.clone(), r.clone(), valid.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    let n = match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            buf = buf[i + 4..i + 4 + len].to_vec();
                            break;
                        }
                    }
                }
                let body: serde_json::Value = serde_json::from_slice(&buf).unwrap_or_default();
                let rt = body["refresh_token"].as_str().unwrap_or("").to_string();
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let (status, resp) = {
                    let mut v = valid.lock().unwrap();
                    if *v == rt {
                        let n = g.fetch_add(1, Ordering::SeqCst) + 1;
                        *v = format!("rt{n}");
                        (
                            "200 OK",
                            json!({"access_token": format!("at{n}"), "refresh_token": format!("rt{n}"), "expires_in": 3600}),
                        )
                    } else {
                        r.fetch_add(1, Ordering::SeqCst);
                        (
                            "400 Bad Request",
                            json!({"error":"invalid_grant","error_description":"refresh token already used"}),
                        )
                    }
                };
                let s = resp.to_string();
                let _ = sock
                    .write_all(format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{s}", s.len()).as_bytes())
                    .await;
            });
        }
    });
    (url, grants, rejected)
}

fn expired_tokens() -> Tokens {
    Tokens {
        access_token: "at0".into(),
        refresh_token: "rt0".into(),
        expires_at: Some(
            (jiff::Timestamp::now() - jiff::SignedDuration::from_secs(60)).to_string(),
        ),
        scopes: vec!["user:inference".into()],
        ..Default::default()
    }
}

fn source(path: &std::path::Path, url: &str) -> CredentialSource {
    let client = OAuthClient::new(OAuthConfig {
        token_url: url.into(),
        ..Default::default()
    });
    CredentialSource::new(CredentialStore::new(path.to_path_buf()))
        .with_oauth_client(client)
        .oauth_only("anthropic".into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_process_concurrent_refresh_is_single_flight() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    CredentialStore::new(path.clone())
        .save_tokens("anthropic", &expired_tokens())
        .unwrap();
    let (url, grants, rejected) = token_server(100).await;
    let src = Arc::new(source(&path, &url));
    let mut hs = Vec::new();
    for _ in 0..8 {
        let s = src.clone();
        hs.push(tokio::spawn(async move { s.resolve().await }));
    }
    let mut ok = 0;
    for h in hs {
        if matches!(h.await.unwrap(), Ok(Credential::OAuth(_))) {
            ok += 1;
        }
    }
    println!(
        "WP5 auth.single_process 8 tasks ok={ok} grants={} rejected={}",
        grants.load(Ordering::SeqCst),
        rejected.load(Ordering::SeqCst)
    );
    assert_eq!(ok, 8);
    assert_eq!(grants.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_process_refresh_race_with_rotating_refresh_tokens() {
    // Two sources = two processes (separate refresh_lock), same file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    CredentialStore::new(path.clone())
        .save_tokens("anthropic", &expired_tokens())
        .unwrap();
    let (url, grants, rejected) = token_server(200).await;
    let a = Arc::new(source(&path, &url));
    let b = Arc::new(source(&path, &url));
    let (ra, rb) = tokio::join!(a.resolve(), b.resolve());
    let stored = CredentialStore::new(path.clone())
        .tokens("anthropic")
        .unwrap()
        .unwrap();
    println!(
        "WP5 auth.cross_process a_ok={} b_ok={} grants={} rejected={} stored_rt={} a_err={:?} b_err={:?}",
        ra.is_ok(),
        rb.is_ok(),
        grants.load(Ordering::SeqCst),
        rejected.load(Ordering::SeqCst),
        stored.refresh_token,
        ra.as_ref().err().map(|e| e.to_string()),
        rb.as_ref().err().map(|e| e.to_string())
    );
    let loser_failed = ra.is_err() || rb.is_err();
    if loser_failed {
        println!("WP5 BUG auth.cross_process: one process fails with invalid_grant instead of re-reading the store");
        // Does the loser recover on its next call (it re-reads the file)?
        let again = if ra.is_err() {
            a.resolve().await
        } else {
            b.resolve().await
        };
        println!("WP5 auth.cross_process loser_retry_ok={}", again.is_ok());
    }
    // The stored refresh token must be the latest valid one; otherwise the
    // user is logged out for good.
    let next = source(&path, &url).handle_unauthorized().await;
    println!(
        "WP5 auth.cross_process stored_token_still_refreshable={}",
        next.is_ok()
    );
    assert!(
        next.is_ok(),
        "stored refresh token was clobbered by a stale one: {stored:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_save_of_different_keys_loses_updates() {
    // Two processes logging into different providers at the same time:
    // save_tokens is read-modify-write without a file lock.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let n = 200;
    let p1 = path.clone();
    let p2 = path.clone();
    let lost = Arc::new(AtomicUsize::new(0));
    let t1 = std::thread::spawn(move || {
        let s = CredentialStore::new(p1);
        let mut errs = 0;
        for i in 0..n {
            let t = Tokens {
                access_token: format!("a{i}"),
                ..Default::default()
            };
            if s.save_tokens("prov-a", &t).is_err() {
                errs += 1;
            }
        }
        errs
    });
    let t2 = std::thread::spawn(move || {
        let s = CredentialStore::new(p2);
        let mut errs = 0;
        for i in 0..n {
            let t = Tokens {
                access_token: format!("b{i}"),
                ..Default::default()
            };
            if s.save_tokens("prov-b", &t).is_err() {
                errs += 1;
            }
        }
        errs
    });
    let (e1, e2) = (t1.join().unwrap(), t2.join().unwrap());
    let s = CredentialStore::new(path.clone());
    let final_ok = s.tokens("prov-a").is_ok();
    let a = s.tokens("prov-a").ok().flatten().map(|t| t.access_token);
    let b = s.tokens("prov-b").ok().flatten().map(|t| t.access_token);
    if a.as_deref() != Some("a199") || b.as_deref() != Some("b199") {
        lost.fetch_add(1, Ordering::SeqCst);
    }
    let raw = std::fs::read(&path).unwrap();
    let parses = serde_json::from_slice::<serde_json::Value>(&raw).is_ok();
    println!(
        "WP5 auth.concurrent_save write_errors={} final_parse_ok={final_ok} file_json_valid={parses} len={} tail={:?} a={a:?} b={b:?}",
        e1 + e2,
        raw.len(),
        String::from_utf8_lossy(&raw[raw.len().saturating_sub(60)..])
    );
    if !parses {
        println!("WP5 BUG auth.concurrent_save: credentials file left PERMANENTLY corrupt (every later read fails)");
    }
}

#[test]
fn reader_during_write_sees_truncated_file() {
    // write() truncates then writes in place: a concurrent reader (another
    // process resolving credentials) can observe an empty/partial file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let mut big = Tokens {
        access_token: "x".into(),
        ..Default::default()
    };
    big.extra
        .insert("profile".into(), json!("p".repeat(256 * 1024)));
    CredentialStore::new(path.clone())
        .save_tokens("anthropic", &big)
        .unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (p, s, b) = (path.clone(), stop.clone(), big.clone());
    let writer = std::thread::spawn(move || {
        let st = CredentialStore::new(p);
        let mut n = 0;
        while !s.load(Ordering::Relaxed) {
            st.save_tokens("anthropic", &b).unwrap();
            n += 1;
        }
        n
    });
    let st = CredentialStore::new(path.clone());
    let (mut reads, mut errors, mut missing) = (0, 0, 0);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(2) {
        match st.tokens("anthropic") {
            Ok(Some(_)) => {}
            Ok(None) => missing += 1,
            Err(_) => errors += 1,
        }
        reads += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let writes = writer.join().unwrap();
    println!(
        "WP5 auth.torn_read writes={writes} reads={reads} parse_errors={errors} missing={missing}"
    );
    if errors + missing > 0 {
        println!(
            "WP5 BUG auth.torn_read: {} of {reads} reads saw a torn credentials file",
            errors + missing
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn perf_oauth_refresh_latency() {
    let n: usize = std::env::var("RNESS_BENCH_REFRESH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let (url, _, _) = token_server(0).await;
    let mut lat = Vec::new();
    let store = CredentialStore::new(path.clone());
    let mut t = expired_tokens();
    for _ in 0..n {
        store.save_tokens("anthropic", &t).unwrap();
        let src = source(&path, &url); // fresh client each time = cold connection, like a new process
        let t0 = Instant::now();
        src.resolve().await.unwrap();
        lat.push(t0.elapsed().as_secs_f64() * 1000.0);
        t = store.tokens("anthropic").unwrap().unwrap();
        t.expires_at = expired_tokens().expires_at;
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| lat[((lat.len() - 1) as f64 * p) as usize];
    println!(
        "{}",
        json!({"probe":"oauth_refresh_latency","n":n,"median_ms":q(0.5),"p99_ms":q(0.99),"note":"loopback fake endpoint; includes file read+write"})
    );
}
