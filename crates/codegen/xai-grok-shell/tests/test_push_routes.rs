//! Server-side coverage for the authenticated Web Push routes added alongside
//! `/ws` and the PWA static routes on the `grok agent serve` router (see
//! `crates/codegen/xai-grok-shell/src/agent/server.rs`).
//!
//! Single `#[test]` for the same edition-2024 `set_var` safety reason as
//! `test_webui_routes.rs`: the process environment (`GROK_HOME`, proxy vars)
//! must be configured before any runtime threads exist. `GROK_HOME` points at
//! a tempdir so VAPID key generation and the subscription store stay isolated.

use std::net::SocketAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tempfile::TempDir;
use tokio::net::TcpListener;
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::agent::run_agent_server_on;

const SECRET: &str = "push-routes-test-secret";

#[test]
fn push_routes_require_auth_and_manage_subscriptions() {
    let grok_home = TempDir::new().expect("grok home tempdir");

    // SAFETY: no other threads exist yet in this test binary.
    unsafe {
        std::env::set_var("GROK_HOME", grok_home.path());
        std::env::set_var("GROK_TELEMETRY_ENABLED", "false");
        std::env::set_var("GROK_FEEDBACK_ENABLED", "false");
        std::env::set_var("GROK_TRACE_UPLOAD", "false");
        std::env::remove_var("HTTPS_PROXY");
        std::env::remove_var("https_proxy");
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("http_proxy");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");

    rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind :0");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let agent_config = AgentConfig::default();
        tokio::spawn(async move {
            let _ = run_agent_server_on(listener, SECRET.to_string(), agent_config).await;
        });

        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        vapid_public_key_requires_auth(&client, &base).await;
        subscribe_requires_auth_and_stores(&client, &base).await;
        unsubscribe_removes(&client, &base).await;
    });
}

/// `GET /push/vapid-public-key` must 401 without the secret and, with it,
/// return a base64url key that decodes to an uncompressed P-256 point (65 bytes).
async fn vapid_public_key_requires_auth(client: &reqwest::Client, base: &str) {
    let unauth = client
        .get(format!("{base}/push/vapid-public-key"))
        .send()
        .await
        .expect("GET vapid-public-key (unauth)");
    assert_eq!(unauth.status(), 401, "must require auth");

    let ok = client
        .get(format!("{base}/push/vapid-public-key?server-key={SECRET}"))
        .send()
        .await
        .expect("GET vapid-public-key (auth)");
    assert_eq!(ok.status(), 200);
    let body: serde_json::Value = ok.json().await.expect("json body");
    let key = body["publicKey"].as_str().expect("publicKey string");
    let decoded = URL_SAFE_NO_PAD.decode(key).expect("base64url key");
    assert_eq!(decoded.len(), 65, "uncompressed SEC1 P-256 point");
}

/// `POST /push/subscribe` must 401 without the secret; with it, a valid body
/// returns 204 and the subscription is stored (a duplicate subscribe stays
/// idempotent — still 204).
async fn subscribe_requires_auth_and_stores(client: &reqwest::Client, base: &str) {
    let sub = serde_json::json!({
        "endpoint": "https://push.example/abc",
        "p256dh": "test-p256dh",
        "auth": "test-auth",
    });

    let unauth = client
        .post(format!("{base}/push/subscribe"))
        .json(&sub)
        .send()
        .await
        .expect("POST subscribe (unauth)");
    assert_eq!(unauth.status(), 401, "must require auth");

    let ok = client
        .post(format!("{base}/push/subscribe?server-key={SECRET}"))
        .json(&sub)
        .send()
        .await
        .expect("POST subscribe (auth)");
    assert_eq!(ok.status(), 204);

    // Malformed body under a valid secret is a 400, not a panic/500.
    let bad = client
        .post(format!("{base}/push/subscribe?server-key={SECRET}"))
        .body("not json")
        .send()
        .await
        .expect("POST subscribe (bad body)");
    assert_eq!(bad.status(), 400);
}

/// `POST /push/unsubscribe` must 401 without the secret; with it, it removes
/// the endpoint and returns 204.
async fn unsubscribe_removes(client: &reqwest::Client, base: &str) {
    let body = serde_json::json!({ "endpoint": "https://push.example/abc" });

    let unauth = client
        .post(format!("{base}/push/unsubscribe"))
        .json(&body)
        .send()
        .await
        .expect("POST unsubscribe (unauth)");
    assert_eq!(unauth.status(), 401, "must require auth");

    let ok = client
        .post(format!("{base}/push/unsubscribe?server-key={SECRET}"))
        .json(&body)
        .send()
        .await
        .expect("POST unsubscribe (auth)");
    assert_eq!(ok.status(), 204);
}
