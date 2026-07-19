//! WebSocket client for connecting to a remote agent server.
//!
//! This is the client-side counterpart of [`super::server`] (`grok agent
//! serve`): it dials the server's `/ws` endpoint, authenticates with the
//! shared secret, and exposes the connection as a pair of line-oriented
//! string channels carrying raw ACP JSON-RPC messages — the same shape the
//! leader IPC transport produces, so callers can bridge it into a typed ACP
//! channel without caring about the transport.
//!
//! v1 semantics: a single dial, no automatic reconnect. When the socket
//! closes (server restart, network drop, liveness timeout) both channels end
//! and the caller observes a disconnect. Because the server keeps its
//! `MvpAgent` alive across connections, re-running the client and issuing
//! `session/load` resumes the conversation losslessly.

use super::proxy;
use super::relay::{SessionEndReason, is_handshake_unauthorized, run_websocket_session};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const CONNECT_TIMEOUT_SECS: u64 = 30;

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

/// Connect to a remote agent server and return raw ACP string channels.
///
/// Returns `(to_server_tx, from_server_rx)`: lines sent on the first are
/// forwarded to the server as WS text frames; inbound frames arrive on the
/// second. The underlying session task keeps the connection alive with
/// pings and tears it down on a read-liveness timeout (half-open TCP), at
/// which point both channels close.
pub async fn connect_remote_agent(
    config: RemoteAgentConfig,
    cancel: CancellationToken,
) -> anyhow::Result<(
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

    let ws = match connect_result {
        Ok((ws, _resp)) => ws,
        Err(e) => {
            if is_handshake_unauthorized(&e) {
                anyhow::bail!(
                    "remote agent rejected the secret (HTTP 401). \
                     Check --remote-secret / GROK_AGENT_SECRET against the \
                     server's `grok agent serve --secret`."
                );
            }
            return Err(e.context(format!("failed to connect to {}", config.ws_url)));
        }
    };
    info!(ws_url = %config.ws_url, "connected to remote agent server");

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

    Ok((from_client_tx, to_client_rx))
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
}
