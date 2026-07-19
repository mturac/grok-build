//! Server-side coverage for the mobile/browser PWA static asset routes added
//! alongside `/ws` on the `grok agent serve` router (see
//! `crates/codegen/xai-grok-shell/src/agent/webui.rs`).
//!
//! Structured as a single `#[test]` for the same edition-2024 `set_var`
//! safety reason as `test_remote_agent_e2e.rs`: the process environment must
//! be configured before any runtime threads exist. Reuses
//! `connect_remote_agent` (rather than a raw WS client) for the `/ws` checks
//! so "hello frame still first" and "still 401s without secret" are proven
//! by the exact same code path a real client uses, not a reimplementation of
//! it.

use std::net::SocketAddr;
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::agent::{RemoteAgentConfig, connect_remote_agent, run_agent_server_on};

const SECRET: &str = "webui-test-secret";

#[test]
fn webui_static_routes_and_ws_auth_untouched() {
    let grok_home = TempDir::new().expect("grok home tempdir");

    // SAFETY: no other threads exist yet in this test binary; the runtime
    // (and the server's persistent agent thread) start below.
    unsafe {
        std::env::set_var("GROK_HOME", grok_home.path());
        std::env::set_var("GROK_TELEMETRY_ENABLED", "false");
        std::env::set_var("GROK_FEEDBACK_ENABLED", "false");
        std::env::set_var("GROK_TRACE_UPLOAD", "false");
        // The reqwest/WS clients must not pick up a proxy from the CI
        // environment for a 127.0.0.1 dial.
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
        let addr = listener.local_addr().expect("local addr");
        let agent_config = AgentConfig::default();
        tokio::spawn(async move {
            let _ = run_agent_server_on(listener, SECRET.to_string(), agent_config).await;
        });

        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        index_returns_html_shell(&client, &base).await;
        manifest_has_correct_content_type(&client, &base).await;
        other_static_assets_serve(&client, &base).await;
        ws_still_401s_without_secret(&addr).await;
        hello_frame_still_first_on_ws(&addr).await;
    });
}

/// `GET /` must return the app shell with no auth required.
async fn index_returns_html_shell(client: &reqwest::Client, base: &str) {
    let resp = client.get(base).send().await.expect("GET /");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").expect("content-type"),
        "text/html; charset=utf-8"
    );
    assert_eq!(
        resp.headers().get("cache-control").expect("cache-control"),
        "no-cache"
    );
    let body = resp.text().await.expect("body");
    assert!(body.contains("<title>Grok Remote</title>"), "{body}");
    assert!(body.contains("id=\"app\""), "{body}");
}

/// `GET /manifest.webmanifest` must have the PWA manifest content type and
/// be valid, well-formed manifest JSON.
async fn manifest_has_correct_content_type(client: &reqwest::Client, base: &str) {
    let resp = client
        .get(format!("{base}/manifest.webmanifest"))
        .send()
        .await
        .expect("GET /manifest.webmanifest");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").expect("content-type"),
        "application/manifest+json"
    );
    assert_eq!(
        resp.headers().get("cache-control").expect("cache-control"),
        "no-cache"
    );
    let body = resp.text().await.expect("body");
    let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON manifest");
    assert_eq!(value["name"], "Grok Remote");
    assert_eq!(value["display"], "standalone");
}

/// The remaining static assets must serve with the right content type too.
async fn other_static_assets_serve(client: &reqwest::Client, base: &str) {
    for (path, content_type) in [
        ("/app.js", "text/javascript; charset=utf-8"),
        ("/style.css", "text/css; charset=utf-8"),
        ("/sw.js", "text/javascript; charset=utf-8"),
        ("/icon.svg", "image/svg+xml"),
    ] {
        let resp = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {path}: {e}"));
        assert_eq!(resp.status(), 200, "GET {path} status");
        assert_eq!(
            resp.headers().get("content-type").expect("content-type"),
            content_type,
            "GET {path} content-type"
        );
        assert_eq!(
            resp.headers().get("cache-control").expect("cache-control"),
            "no-cache",
            "GET {path} cache-control"
        );
    }
}

/// The static routes carry no auth, but `/ws` must still reject a wrong
/// secret with 401 exactly as before this change.
async fn ws_still_401s_without_secret(addr: &SocketAddr) {
    let ws_url = format!("ws://{addr}/ws");
    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        connect_remote_agent(
            RemoteAgentConfig {
                ws_url,
                secret: "not-the-real-secret".to_string(),
            },
            cancel,
        ),
    )
    .await
    .expect("connect attempt timed out");

    let err = result.expect_err("wrong secret must still be rejected");
    assert!(
        err.to_string().contains("401"),
        "error should surface the 401 rejection: {err}"
    );
}

/// `connect_remote_agent` internally errors out unless the hello frame is
/// the very first message on the socket (see `read_hello_frame`), so a
/// successful connect here IS the proof that the new static routes didn't
/// disturb hello-frame-first ordering on `/ws`.
async fn hello_frame_still_first_on_ws(addr: &SocketAddr) {
    let ws_url = format!("ws://{addr}/ws");
    let cancel = CancellationToken::new();
    let (hello, _tx, _rx) = tokio::time::timeout(
        Duration::from_secs(30),
        connect_remote_agent(
            RemoteAgentConfig {
                ws_url,
                secret: SECRET.to_string(),
            },
            cancel.clone(),
        ),
    )
    .await
    .expect("connect timed out")
    .expect("connect with correct secret");

    assert!(!hello.agent_instance_id.is_empty());
    assert_eq!(
        hello.protocol_version,
        xai_grok_shell::agent::REMOTE_PROTOCOL_VERSION
    );
    cancel.cancel();
}
