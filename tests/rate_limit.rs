//! Integration test for the rate-limiting middleware (GitHub issue #24).
//!
//! Spins up a real TCP listener so the governor layer's `PeerIpKeyExtractor`
//! can read `ConnectInfo<SocketAddr>`, then hammers `/health` with a burst of
//! requests and asserts that the server returns HTTP 429 with a `Retry-After`
//! header once the bucket is exhausted.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use ai_pod::runtime::{ContainerRuntime, RuntimeKind};
use ai_pod::server::{AppState, build_app};
use tempfile::TempDir;
use tokio::sync::Mutex;

fn make_state(config_dir: &std::path::Path) -> AppState {
    AppState {
        projects: Arc::new(Mutex::new(HashMap::new())),
        config_dir: config_dir.to_path_buf(),
        approval_lock: Arc::new(Mutex::new(())),
        daemons: Arc::new(Mutex::new(HashMap::new())),
        runtime: ContainerRuntime {
            kind: RuntimeKind::Podman,
            dry_run: false,
        },
    }
}

/// Drive the server with enough back-to-back requests to exhaust the burst
/// bucket, then assert that at least one response is a 429 carrying a
/// `Retry-After` header and that normal traffic inside the burst was allowed.
#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    let dir = TempDir::new().unwrap();
    let state = make_state(dir.path());
    let app = build_app(state);

    // Bind to an ephemeral port on localhost.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Serve with ConnectInfo so PeerIpKeyExtractor can identify the peer.
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let client = reqwest::Client::new();
    let url = format!("http://{}/health", addr);

    // Burst size is 50 in production config; fire more than that sequentially
    // so the GCRA bucket is guaranteed to exhaust within the same second.
    let mut statuses = Vec::with_capacity(80);
    let mut retry_after_on_reject: Option<String> = None;
    for _ in 0..80 {
        let resp = client.get(&url).send().await.expect("request failed");
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && retry_after_on_reject.is_none() {
            retry_after_on_reject = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
        }
        statuses.push(status);
    }

    // The first few responses must be 200 — legitimate traffic inside the
    // burst should be unaffected by rate limiting.
    assert_eq!(
        statuses[0],
        reqwest::StatusCode::OK,
        "first request in burst should succeed"
    );
    assert!(
        statuses.iter().take(10).all(|s| *s == reqwest::StatusCode::OK),
        "first 10 requests in burst should all succeed, got: {:?}",
        &statuses[..10]
    );

    // Once the burst is drained, at least one later response must be 429.
    assert!(
        statuses
            .iter()
            .any(|s| *s == reqwest::StatusCode::TOO_MANY_REQUESTS),
        "expected at least one HTTP 429 after the burst was exhausted, \
         got statuses: {:?}",
        statuses
    );

    // tower_governor always sets Retry-After on 429 responses.
    assert!(
        retry_after_on_reject.is_some(),
        "expected Retry-After header on the first 429 response"
    );
}
