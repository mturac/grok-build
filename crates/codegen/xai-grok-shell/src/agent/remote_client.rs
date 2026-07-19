//! WebSocket client for connecting to a remote agent server.
//!
//! This is the client-side counterpart of [`super::server`] (`grok agent
//! serve`): it dials the server's `/ws` endpoint, authenticates with the
//! shared secret, reads the server's one-time hello frame, and exposes the
//! connection as a pair of line-oriented string channels carrying raw ACP
//! JSON-RPC messages — the same shape the leader IPC transport produces, so
//! callers can bridge it into a typed ACP channel without caring about the
//! transport.
//!
//! Auto-reconnect: when the socket closes (server restart, network drop,
//! liveness timeout), [`RemoteWsReconnector`] re-dials and reports whether the
//! server's `agent_instance_id` changed — i.e. whether the server process
//! (and its `MvpAgent`) was replaced and ACP state needs replaying, or merely
//! reconnected to the same still-alive agent.

use super::proxy;
use super::relay::{SessionEndReason, is_handshake_unauthorized, run_websocket_session};
use futures_util::StreamExt as _;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{
    WebSocketStream, connect_async, tungstenite::Message, tungstenite::client::IntoClientRequest,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const CONNECT_TIMEOUT_SECS: u64 = 30;

/// Wire protocol version for the remote-agent WS transport (hello frame shape
/// + the ACP JSON-RPC framing carried over it). Bumped whenever a change
/// would break an older/newer peer; [`connect_remote_agent`] rejects a
/// mismatch instead of limping along.
pub const REMOTE_PROTOCOL_VERSION: u32 = 1;

/// One-time frame the server sends immediately after a successful WS
/// handshake, before any ACP traffic — see [`super::server`]'s
/// `handle_connection`.
///
/// `agent_instance_id` is generated once per server process (at `grok agent
/// serve` startup) and is the same for every connection that process
/// accepts. A client comparing it across a reconnect learns whether the
/// server process — and its persistent `MvpAgent` — survived (same id) or was
/// replaced (different id, e.g. a restart), which determines whether ACP
/// state needs replaying.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteHello {
    pub agent_instance_id: String,
    pub protocol_version: u32,
    pub binary_version: String,
}

/// Read and validate the hello frame that must be the first message on a
/// freshly connected remote-agent WebSocket.
///
/// Skips WS control frames (`Ping`/`Pong`/`Frame`) that can legitimately
/// arrive before the hello — e.g. a keepalive ping sent immediately after the
/// handshake — instead of bailing on the first non-hello frame (mirrors the
/// steady-state read loops in `relay.rs`/`server.rs`). Still bails on `Close`.
///
/// The whole wait (including any skipped control frames) is bounded by
/// [`CONNECT_TIMEOUT_SECS`] and cancellable, so a server that completes the WS
/// handshake but never sends a hello (e.g. an older, pre-hello `grok agent
/// serve`) fails fast instead of hanging forever.
async fn read_hello_frame<S>(
    ws: &mut WebSocketStream<S>,
    cancel: &CancellationToken,
) -> anyhow::Result<RemoteHello>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let read_until_hello = async {
        loop {
            let msg = ws.next().await.ok_or_else(|| {
                anyhow::anyhow!(
                    "remote agent server closed the connection before sending its hello frame"
                )
            })?;
            let msg = msg.map_err(|e| {
                anyhow::Error::from(e).context("error reading hello frame from remote agent server")
            })?;
            match msg {
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(_) => anyhow::bail!(
                    "remote agent server closed the connection before sending its hello frame"
                ),
                other => return Ok(other),
            }
        }
    };

    let msg = tokio::select! {
        _ = cancel.cancelled() => anyhow::bail!("remote agent connection cancelled"),
        result = tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), read_until_hello) => {
            match result {
                Ok(inner) => inner?,
                Err(_) => anyhow::bail!(
                    "remote agent server did not send a hello frame within {CONNECT_TIMEOUT_SECS} \
                     seconds — is it an older `grok agent serve`?"
                ),
            }
        }
    };

    let text = match msg {
        Message::Text(t) => t.to_string(),
        Message::Binary(b) => String::from_utf8(b.to_vec())
            .map_err(|_| anyhow::anyhow!("remote agent server sent a non-UTF8 hello frame"))?,
        other => anyhow::bail!("expected a hello frame from the remote agent server, got {other:?}"),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("malformed hello frame from remote agent server: {e}"))?;
    let frame_type = value.get("type").and_then(|v| v.as_str()).unwrap_or_default();
    if frame_type != "hello" {
        anyhow::bail!(
            "expected a hello frame from the remote agent server, got: {text}\n\
             (is --remote pointed at a `grok agent serve` endpoint?)"
        );
    }
    let hello: RemoteHello = serde_json::from_value(value)
        .map_err(|e| anyhow::anyhow!("malformed hello frame from remote agent server: {e}"))?;
    if hello.protocol_version != REMOTE_PROTOCOL_VERSION {
        return Err(anyhow::Error::new(PermanentConnectError).context(format!(
            "remote agent server speaks protocol version {}, but this client expects {}. \
             Update whichever of the client or `grok agent serve` is older so both match.",
            hello.protocol_version,
            REMOTE_PROTOCOL_VERSION,
        )));
    }
    Ok(hello)
}

/// Marker attached (via [`anyhow::Error::context`]) to a connect error that no
/// amount of retrying will fix — a rejected secret (HTTP 401) or a protocol
/// version mismatch. [`RemoteWsReconnector::reconnect`] checks the error
/// chain for this type and gives up immediately instead of retrying under an
/// unbounded policy, which would otherwise bury the actionable message under
/// an endless stream of warn logs.
#[derive(Debug)]
pub(crate) struct PermanentConnectError;

impl std::fmt::Display for PermanentConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "permanent remote agent connect error (will not be retried)")
    }
}

impl std::error::Error for PermanentConnectError {}

/// Configuration for connecting to a remote agent server.
#[derive(Debug, Clone)]
pub struct RemoteAgentConfig {
    /// WebSocket URL of the agent server, e.g. `wss://host:2419/ws`.
    pub ws_url: String,
    /// Shared secret, matching the server's `--secret` / `GROK_AGENT_SECRET`.
    pub secret: String,
}

/// Build the WebSocket handshake request with the bearer secret.
///
/// The secret goes in the `Authorization` header rather than the
/// `?server-key=` query parameter so it stays out of URLs (shell history,
/// proxy logs). The server accepts both.
fn build_remote_request(config: &RemoteAgentConfig) -> anyhow::Result<axum::http::Request<()>> {
    let mut req = config.ws_url.clone().into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        axum::http::header::HeaderValue::from_str(&format!("Bearer {}", config.secret))?,
    );
    req.headers_mut().insert(
        "x-grok-client-version",
        axum::http::header::HeaderValue::from_static(xai_grok_version::VERSION),
    );
    Ok(req)
}

/// Connect to a remote agent server and return its hello plus raw ACP string
/// channels.
///
/// Returns `(hello, to_server_tx, from_server_rx)`: `hello` is the server's
/// one-time greeting (read and validated before any ACP traffic); lines sent
/// on `to_server_tx` are forwarded to the server as WS text frames; inbound
/// frames arrive on `from_server_rx`. The underlying session task keeps the
/// connection alive with pings and tears it down on a read-liveness timeout
/// (half-open TCP), at which point both channels close.
pub async fn connect_remote_agent(
    config: RemoteAgentConfig,
    cancel: CancellationToken,
) -> anyhow::Result<(
    RemoteHello,
    mpsc::UnboundedSender<String>,
    mpsc::UnboundedReceiver<String>,
)> {
    let req = build_remote_request(&config)?;

    let target_host = req.uri().host().map(str::to_owned);
    let proxy_url = target_host
        .as_deref()
        .and_then(proxy::resolve_proxy_for_host);

    let connect_timeout = Duration::from_secs(CONNECT_TIMEOUT_SECS);
    let connect_result = tokio::select! {
        _ = cancel.cancelled() => anyhow::bail!("remote agent connection cancelled"),
        result = tokio::time::timeout(connect_timeout, async {
            if let Some(ref proxy_url) = proxy_url {
                let host = target_host
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("remote agent URL has no host"))?;
                let port = req.uri().port_u16().unwrap_or(443);
                let tunneled = proxy::connect_via_proxy(proxy_url, host, port).await?;
                let (ws, resp) = tokio_tungstenite::client_async(req, tunneled)
                    .await
                    .map_err(|e| {
                        anyhow::Error::from(e).context("WebSocket handshake via proxy failed")
                    })?;
                Ok((ws, resp))
            } else {
                connect_async(req)
                    .await
                    .map_err(|e| anyhow::Error::from(e).context("WebSocket connection failed"))
            }
        }) => match result {
            Ok(inner) => inner,
            Err(_) => anyhow::bail!(
                "connection to remote agent timed out after {CONNECT_TIMEOUT_SECS} seconds"
            ),
        },
    };

    let mut ws = match connect_result {
        Ok((ws, _resp)) => ws,
        Err(e) => {
            if is_handshake_unauthorized(&e) {
                return Err(anyhow::Error::new(PermanentConnectError).context(
                    "remote agent rejected the secret (HTTP 401). \
                     Check --remote-secret / GROK_AGENT_SECRET against the \
                     server's `grok agent serve --secret`.",
                ));
            }
            return Err(e.context(format!("failed to connect to {}", config.ws_url)));
        }
    };
    info!(ws_url = %config.ws_url, "connected to remote agent server");

    let hello = read_hello_frame(&mut ws, &cancel).await?;
    info!(
        agent_instance_id = %hello.agent_instance_id,
        binary_version = %hello.binary_version,
        "received remote agent hello frame"
    );

    let (to_client_tx, to_client_rx) = mpsc::unbounded_channel::<String>();
    let (from_client_tx, mut from_client_rx) = mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        match run_websocket_session(ws, &to_client_tx, &mut from_client_rx, &cancel).await {
            Ok(SessionEndReason::Normal) => {
                info!("remote agent WebSocket session ended");
            }
            Ok(SessionEndReason::AuthError) => {
                // Static-secret servers never emit -32000 auth errors, but a
                // future bearer-auth server might; surface it distinctly.
                warn!("remote agent session ended with an authentication error");
            }
            Err(e) => {
                warn!(error = ?e, "remote agent WebSocket session ended with error");
            }
        }
        // Dropping `to_client_tx` here closes the inbound channel, which the
        // caller's bridge observes as a disconnect.
    });

    Ok((hello, from_client_tx, to_client_rx))
}

/// Redials a remote agent server's `/ws` endpoint on disconnect.
///
/// Mirrors [`crate::leader::LeaderReconnector`]'s contract (a `reconnect`
/// method retried under a [`crate::leader::ReconnectPolicy`]) so
/// `xai_grok_pager::acp::leader_bridge::bridge_channels`'s generic reconnect
/// loop can drive either transport.
///
/// Unlike a leader process, a remote agent server can be restarted between
/// connections. A restarted server mints a fresh `agent_instance_id` at
/// startup (see [`RemoteHello`]), and its `MvpAgent` — along with every
/// session — is gone with it. `reconnect` compares the new hello's instance
/// id against the one observed on the previous (re)connection and reports
/// whether the caller must replay ACP state (`initialize` + `session/load`)
/// before resuming normal pumping, or whether the same agent process answered
/// and no replay is needed.
pub struct RemoteWsReconnector {
    config: RemoteAgentConfig,
    /// Instance id observed on the most recent (re)connection. `Mutex`
    /// because `reconnect` takes `&self`, matching `LeaderReconnector`.
    last_instance_id: tokio::sync::Mutex<String>,
}

/// Outcome of one successful [`RemoteWsReconnector::reconnect`] call.
pub struct RemoteReconnectOutcome {
    pub tx: mpsc::UnboundedSender<String>,
    pub rx: mpsc::UnboundedReceiver<String>,
    /// `true` when the server's `agent_instance_id` changed since the
    /// previous (re)connection — the server process was replaced (e.g.
    /// restarted) and the caller must replay ACP state. `false` means the
    /// same agent process answered and no replay is needed.
    pub needs_replay: bool,
    /// The hello observed on this (re)connection.
    pub hello: RemoteHello,
}

/// Whether a reconnect must replay ACP state (`initialize` + `session/load`)
/// before resuming normal pumping: `true` when the freshly observed instance
/// id differs from the previously observed one, meaning the server process
/// was replaced (e.g. restarted) and its `MvpAgent`/sessions are gone.
/// `false` means the same agent process answered and no replay is needed.
///
/// Pulled out of [`RemoteWsReconnector::reconnect`] so the decision itself is
/// unit-testable without dialing a real server.
fn instance_id_changed(previous: &str, observed: &str) -> bool {
    previous != observed
}

impl RemoteWsReconnector {
    /// `initial_instance_id` is the hello instance id observed on the
    /// connection that is about to be bridged — the baseline the first
    /// reconnect compares against.
    pub fn new(config: RemoteAgentConfig, initial_instance_id: String) -> Self {
        Self {
            config,
            last_instance_id: tokio::sync::Mutex::new(initial_instance_id),
        }
    }

    /// Attempt to reconnect to the remote agent server.
    ///
    /// Uses the same exponential backoff curve as `LeaderReconnector`: 1s →
    /// 2s → 4s → ... capped at 30s.
    ///
    /// Deliberately does NOT commit the freshly observed instance id as the
    /// new baseline — [`RemoteReconnectOutcome::needs_replay`] is computed
    /// against the *previous* baseline every time this is called, until the
    /// caller calls [`Self::confirm_instance`]. If a caller replayed ACP state
    /// after a `needs_replay` reconnect and the replay then failed partway
    /// through (socket drop, timeout), the caller must NOT confirm; the next
    /// `reconnect` call will then see the same stale baseline and report
    /// `needs_replay` again, re-arming the replay instead of silently
    /// stranding the client without its sessions.
    ///
    /// # Retry policy
    /// - [`crate::leader::ReconnectPolicy::Unbounded`]: retries until
    ///   `cancel` fires (interactive TUI).
    /// - [`crate::leader::ReconnectPolicy::Bounded`]: retries up to
    ///   `max_attempts`, then returns an error.
    ///
    /// # Permanent errors
    /// A connect error carrying [`PermanentConnectError`] in its chain (a
    /// rejected secret, a protocol version mismatch) is returned immediately
    /// regardless of `policy` — retrying it would never succeed and would
    /// just bury the actionable message under repeated warn logs.
    pub async fn reconnect(
        &self,
        policy: crate::leader::ReconnectPolicy,
        cancel: &CancellationToken,
    ) -> anyhow::Result<RemoteReconnectOutcome> {
        use crate::leader::{RECONNECT_BASE_DELAY, RECONNECT_MAX_DELAY, ReconnectPolicy};

        let mut attempt: u32 = 0;
        let mut delay = RECONNECT_BASE_DELAY;
        loop {
            if cancel.is_cancelled() {
                anyhow::bail!("remote agent reconnect cancelled");
            }
            attempt += 1;
            match connect_remote_agent(self.config.clone(), cancel.clone()).await {
                Ok((hello, tx, rx)) => {
                    let last = self.last_instance_id.lock().await;
                    let needs_replay = instance_id_changed(&last, &hello.agent_instance_id);
                    drop(last);
                    info!(
                        attempt,
                        needs_replay, "reconnected to remote agent server"
                    );
                    return Ok(RemoteReconnectOutcome {
                        tx,
                        rx,
                        needs_replay,
                        hello,
                    });
                }
                Err(e) => {
                    warn!(attempt, error = %e, "remote agent reconnect attempt failed");
                    if e.chain().any(|c| c.is::<PermanentConnectError>()) {
                        return Err(e.context(
                            "remote agent connect error is permanent; giving up instead of retrying",
                        ));
                    }
                    if let ReconnectPolicy::Bounded { max_attempts } = policy
                        && attempt >= max_attempts
                    {
                        return Err(
                            e.context(format!("failed to reconnect after {max_attempts} attempts"))
                        );
                    }
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => anyhow::bail!("remote agent reconnect cancelled"),
                _ = tokio::time::sleep(delay) => {}
            }
            delay = std::cmp::min(delay * 2, RECONNECT_MAX_DELAY);
        }
    }

    /// Confirm `id` as the new baseline for the NEXT `reconnect`'s
    /// `needs_replay` comparison. Callers must call this only after any
    /// replay required by the reconnect that observed `id` has actually
    /// succeeded (or wasn't needed) — see [`Self::reconnect`]'s docs.
    pub async fn confirm_instance(&self, id: &str) {
        *self.last_instance_id.lock().await = id.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_remote_request_sets_bearer_and_version() {
        let config = RemoteAgentConfig {
            ws_url: "ws://127.0.0.1:2419/ws".into(),
            secret: "s3cret".into(),
        };
        let req = build_remote_request(&config).unwrap();
        assert_eq!(
            req.headers().get("Authorization").unwrap(),
            "Bearer s3cret"
        );
        assert!(req.headers().contains_key("x-grok-client-version"));
        assert_eq!(req.uri().path(), "/ws");
    }

    #[test]
    fn build_remote_request_rejects_non_ws_url() {
        let config = RemoteAgentConfig {
            ws_url: "not a url".into(),
            secret: "s".into(),
        };
        assert!(build_remote_request(&config).is_err());
    }

    #[test]
    fn instance_id_changed_detects_same_and_different() {
        assert!(!instance_id_changed("abc", "abc"));
        assert!(instance_id_changed("abc", "def"));
        assert!(instance_id_changed("", "abc"));
    }

    /// In-memory WS pair (no network, no handshake) so hello-frame tests run
    /// without a real server — mirrors `relay::tests::ws_pair`.
    async fn ws_client_server_pair() -> (
        tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
        tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
    ) {
        use tokio_tungstenite::tungstenite::protocol::Role;
        let (client, server) = tokio::io::duplex(64 * 1024);
        let client_ws =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let server_ws =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        (client_ws, server_ws)
    }

    #[tokio::test]
    async fn read_hello_frame_parses_valid_hello() {
        use futures_util::SinkExt as _;
        let (mut client_ws, mut server_ws) = ws_client_server_pair().await;
        let hello = serde_json::json!({
            "type": "hello",
            "agent_instance_id": "instance-abc",
            "protocol_version": REMOTE_PROTOCOL_VERSION,
            "binary_version": "9.9.9",
        });
        tokio::spawn(async move {
            let _ = server_ws
                .send(Message::Text(hello.to_string().into()))
                .await;
        });
        let result = read_hello_frame(&mut client_ws, &CancellationToken::new())
            .await
            .expect("well-formed hello must parse");
        assert_eq!(result.agent_instance_id, "instance-abc");
        assert_eq!(result.protocol_version, REMOTE_PROTOCOL_VERSION);
        assert_eq!(result.binary_version, "9.9.9");
    }

    #[tokio::test]
    async fn read_hello_frame_rejects_protocol_version_mismatch() {
        use futures_util::SinkExt as _;
        let (mut client_ws, mut server_ws) = ws_client_server_pair().await;
        let hello = serde_json::json!({
            "type": "hello",
            "agent_instance_id": "instance-abc",
            "protocol_version": REMOTE_PROTOCOL_VERSION + 1,
            "binary_version": "9.9.9",
        });
        tokio::spawn(async move {
            let _ = server_ws
                .send(Message::Text(hello.to_string().into()))
                .await;
        });
        let err = read_hello_frame(&mut client_ws, &CancellationToken::new())
            .await
            .expect_err("a protocol version mismatch must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("protocol version"),
            "error should mention protocol version: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("update"),
            "error should tell the user to update: {msg}"
        );
        assert!(
            err.chain().any(|c| c.is::<PermanentConnectError>()),
            "a protocol version mismatch must be classified as permanent"
        );
    }

    #[tokio::test]
    async fn read_hello_frame_rejects_non_hello_first_message() {
        use futures_util::SinkExt as _;
        let (mut client_ws, mut server_ws) = ws_client_server_pair().await;
        tokio::spawn(async move {
            let acp_msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
            let _ = server_ws.send(Message::Text(acp_msg.into())).await;
        });
        let err = read_hello_frame(&mut client_ws, &CancellationToken::new())
            .await
            .expect_err("a non-hello first frame must be rejected");
        assert!(
            err.to_string().contains("hello"),
            "error should call out the missing hello frame: {err}"
        );
    }

    #[tokio::test]
    async fn read_hello_frame_rejects_closed_connection() {
        let (mut client_ws, server_ws) = ws_client_server_pair().await;
        drop(server_ws);
        let err = read_hello_frame(&mut client_ws, &CancellationToken::new())
            .await
            .expect_err("a closed connection before hello must be rejected");
        assert!(err.to_string().contains("hello"), "{err}");
    }

    /// A server that completes the WS handshake but never sends a hello frame
    /// (e.g. an older, pre-hello `grok agent serve`) must fail fast rather
    /// than hang forever — bounded by the same [`CONNECT_TIMEOUT_SECS`]
    /// budget as the connect phase.
    #[tokio::test(start_paused = true)]
    async fn read_hello_frame_times_out_when_server_stays_silent() {
        let (mut client_ws, server_ws) = ws_client_server_pair().await;
        // Keep the server side alive (but silent) for the duration of the
        // test so the failure is unambiguously a timeout, not a closed
        // connection.
        let _server_ws = server_ws;
        let result = tokio::time::timeout(
            Duration::from_secs(CONNECT_TIMEOUT_SECS + 5),
            read_hello_frame(&mut client_ws, &CancellationToken::new()),
        )
        .await
        .expect("read_hello_frame itself must time out, not hang past the outer timeout");
        let err = result.expect_err("a silent server must not be treated as a valid hello");
        let msg = err.to_string();
        assert!(
            msg.contains("did not send a hello frame"),
            "error should call out the missing hello frame: {msg}"
        );
    }

    /// Control frames (e.g. a keepalive `Ping`) arriving before the hello
    /// must be skipped, not treated as a protocol violation.
    #[tokio::test]
    async fn read_hello_frame_skips_ping_before_hello() {
        use futures_util::SinkExt as _;
        let (mut client_ws, mut server_ws) = ws_client_server_pair().await;
        let hello = serde_json::json!({
            "type": "hello",
            "agent_instance_id": "instance-after-ping",
            "protocol_version": REMOTE_PROTOCOL_VERSION,
            "binary_version": "9.9.9",
        });
        // Send both frames up front (the duplex buffer comfortably holds
        // both) and keep `server_ws` alive across the read: tungstenite
        // auto-queues a Pong in response to the Ping, and writing it out
        // would hit a broken pipe if the server side were already dropped.
        server_ws
            .send(Message::Ping(Vec::new().into()))
            .await
            .expect("send ping");
        server_ws
            .send(Message::Text(hello.to_string().into()))
            .await
            .expect("send hello");
        let result = read_hello_frame(&mut client_ws, &CancellationToken::new())
            .await
            .expect("a leading ping must not block the hello frame");
        assert_eq!(result.agent_instance_id, "instance-after-ping");
        drop(server_ws);
    }

    /// A rejected secret (HTTP 401) is a permanent condition — the
    /// reconnector must give up immediately rather than retrying under an
    /// unbounded policy.
    #[test]
    fn permanent_connect_error_is_detected_through_context_chain() {
        let err = anyhow::Error::new(PermanentConnectError).context("wrapped with more context");
        assert!(err.chain().any(|c| c.is::<PermanentConnectError>()));

        let transient = anyhow::anyhow!("plain transient error");
        assert!(!transient.chain().any(|c| c.is::<PermanentConnectError>()));
    }
}
