//! End-to-end loopback test for remote connect: real `run_agent_server_on`
//! (the `grok agent serve` core) + real `connect_remote_agent` (the
//! `grok --remote` transport).
//!
//! Structured as a single `#[test]` so the process environment can be set up
//! before any runtime threads exist (edition-2024 `set_var` safety), then all
//! scenarios run sequentially against one server:
//!
//! 1. A wrong secret is rejected at the WebSocket handshake (HTTP 401).
//! 2. A correct secret connects, reads the server's hello frame, and
//!    completes an ACP `initialize` JSON-RPC round trip through the WS
//!    transport to the persistent `MvpAgent`.
//! 3. Reconnecting to the SAME server process yields the SAME
//!    `agent_instance_id` and `RemoteWsReconnector` reports no replay is
//!    needed.

use std::time::Duration;

use agent_client_protocol as acp;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::agent::{
    RemoteAgentConfig, RemoteWsReconnector, connect_remote_agent, run_agent_server_on,
};
use xai_grok_shell::leader::ReconnectPolicy;

const SECRET: &str = "e2e-test-secret";

#[test]
fn remote_agent_server_round_trip() {
    let grok_home = TempDir::new().expect("grok home tempdir");

    // SAFETY: no other threads exist yet in this test binary; the runtime
    // (and the server's persistent agent thread) start below.
    unsafe {
        std::env::set_var("GROK_HOME", grok_home.path());
        std::env::set_var("GROK_TELEMETRY_ENABLED", "false");
        std::env::set_var("GROK_FEEDBACK_ENABLED", "false");
        std::env::set_var("GROK_TRACE_UPLOAD", "false");
        // The remote client must not pick up a proxy from the CI environment
        // for a 127.0.0.1 dial.
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
        let ws_url = format!("ws://{addr}/ws");

        wrong_secret_is_rejected(&ws_url).await;
        let first_instance_id = initialize_round_trip(&ws_url).await;
        reconnect_same_server_needs_no_replay(&ws_url, &first_instance_id).await;
    });
}

/// A bad secret must fail the handshake with a clear 401 hint, not connect.
async fn wrong_secret_is_rejected(ws_url: &str) {
    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        connect_remote_agent(
            RemoteAgentConfig {
                ws_url: ws_url.to_string(),
                secret: "wrong-secret".to_string(),
            },
            cancel,
        ),
    )
    .await
    .expect("connect attempt timed out");

    let err = result.expect_err("wrong secret must be rejected");
    assert!(
        err.to_string().contains("401"),
        "error should surface the 401 rejection: {err}"
    );
}

/// The full client path: dial, authenticate, read the hello frame, send an
/// ACP `initialize` JSON-RPC request as a text frame, and get the agent's
/// response back. Returns the hello's `agent_instance_id` for the caller to
/// compare against a later reconnect.
async fn initialize_round_trip(ws_url: &str) -> String {
    let cancel = CancellationToken::new();
    let (hello, tx, mut rx) = tokio::time::timeout(
        Duration::from_secs(30),
        connect_remote_agent(
            RemoteAgentConfig {
                ws_url: ws_url.to_string(),
                secret: SECRET.to_string(),
            },
            cancel.clone(),
        ),
    )
    .await
    .expect("connect timed out")
    .expect("connect with correct secret");

    assert!(
        !hello.agent_instance_id.is_empty(),
        "hello must carry a non-empty agent_instance_id"
    );
    assert_eq!(hello.protocol_version, xai_grok_shell::agent::REMOTE_PROTOCOL_VERSION);
    assert!(
        !hello.binary_version.is_empty(),
        "hello must carry the server's binary_version"
    );

    let params = serde_json::to_value(
        acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new())
                .terminal(false),
        ),
    )
    .expect("serialize initialize params");
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": params,
    });
    tx.send(request.to_string()).expect("send initialize");

    // The first frame with id=1 is the initialize response (the agent may
    // also emit notifications, which have no id).
    let response = loop {
        let line = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("initialize response timed out")
            .expect("connection closed before initialize response");
        let json: serde_json::Value = serde_json::from_str(&line).expect("response is JSON");
        if json.get("id").and_then(|v| v.as_i64()) == Some(1) {
            break json;
        }
    };

    assert!(
        response.get("error").is_none(),
        "initialize must not error: {response}"
    );
    let result = response
        .get("result")
        .unwrap_or_else(|| panic!("initialize response has no result: {response}"));
    assert!(
        result.get("protocolVersion").is_some(),
        "initialize result missing protocolVersion: {result}"
    );

    cancel.cancel();
    hello.agent_instance_id
}

/// The server process didn't restart between connections, so a reconnect via
/// `RemoteWsReconnector` must observe the SAME `agent_instance_id` and report
/// `needs_replay = false` — no `initialize`/`session/load` replay, just
/// resume pumping (the persistent `MvpAgent` never went away).
async fn reconnect_same_server_needs_no_replay(ws_url: &str, first_instance_id: &str) {
    let config = RemoteAgentConfig {
        ws_url: ws_url.to_string(),
        secret: SECRET.to_string(),
    };
    let reconnector = RemoteWsReconnector::new(config, first_instance_id.to_string());
    let cancel = CancellationToken::new();

    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        reconnector.reconnect(ReconnectPolicy::bounded(), &cancel),
    )
    .await
    .expect("reconnect timed out")
    .expect("reconnect to the still-running server must succeed");

    assert_eq!(
        outcome.hello.agent_instance_id, first_instance_id,
        "the server process did not restart; the instance id must be identical"
    );
    assert!(
        !outcome.needs_replay,
        "same agent_instance_id must not require an ACP replay"
    );

    cancel.cancel();
}
