use rness_server::auth::{authorize, Auth};
#[tokio::test]
async fn tokenless_loopback_rejects_rebinding_hosts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "private" }))
        .layer(axum::middleware::from_fn_with_state(
            Auth::for_bind(address, None).unwrap(),
            authorize,
        ));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/");
    for host in ["localhost", "127.0.0.1", "[::1]"] {
        assert_eq!(
            client
                .get(&url)
                .header("Host", host)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    for host in [
        "attacker.example",
        "localhost.attacker.example",
        "localhost@attacker.example",
        "127.0.0.1.attacker.example",
    ] {
        assert_eq!(
            client
                .get(&url)
                .header("Host", host)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    server.abort();
}

#[tokio::test]
async fn bearer_and_browser_origin_checks_cover_routes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let token = "test-secret-".repeat(4);
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "private" }))
        .layer(axum::middleware::from_fn_with_state(
            Auth::for_bind(address, Some(token.clone())).unwrap(),
            authorize,
        ));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/");
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&token)
            .header("Origin", "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&token)
            .header("Sec-Fetch-Site", "cross-site")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    server.abort();
}
