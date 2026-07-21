//! WebSocket server for remote agent connections.
//!
//! This module provides a WebSocket server that allows remote TUI clients to
//! connect to a grok agent running on a different machine.
//!
//! The agent persists across WebSocket reconnections: a single MvpAgent instance
//! is created on first connection and reused for all subsequent connections. This
//! ensures that session actors (and any in-flight prompts) survive client
//! disconnects — when a client reconnects and loads an existing session, ongoing
//! work continues to stream to the new connection.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        ConnectInfo, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, simplex};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{info, warn};

use agent_client_protocol as acp;
use xai_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    AcpClientMessage, LineBufferedRead,
};

use crate::agent::config::{Config as AgentConfig, ModelEntry};
use crate::agent::models::{ModelFetchAuth, prefetch_models_blocking};
use crate::agent::mvp_agent::MvpAgent;
use crate::agent::notify::PushStore;
use crate::agent::notify::vapid::VapidKeys;
use crate::agent::remote_client::REMOTE_PROTOCOL_VERSION;
use crate::agent::webui;

use indexmap::IndexMap;

/// Swappable destination for the relay task.
///
/// Points at the current ACP connection's gateway sender. When no client is
/// connected, the value is `None` and outbound messages are silently dropped
/// (matching the old behaviour where the gateway channel's receiver was simply
/// gone).
type RelayDest = Rc<RefCell<Option<mpsc::UnboundedSender<AcpClientMessage>>>>;

const MAX_BUFFER_SIZE: usize = 8 * 1024 * 1024;
const KEEPALIVE_INTERVAL_SECS: u64 = 15;

/// Configuration for the agent WebSocket server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Address to bind the server to
    pub bind_addr: SocketAddr,
    /// Secret token for client authentication (required)
    pub secret: String,
}

/// Shared state for the WebSocket server.
struct ServerState {
    agent_config: AgentConfig,
    secret: String,
    /// Generated once per server process at startup (see [`run_agent_server_on`]).
    /// Sent to every connecting client in the hello frame so it can tell
    /// whether a reconnect landed on the same agent process (same id, no ACP
    /// replay needed) or a restarted one (different id, `MvpAgent` and every
    /// session gone — full replay required).
    instance_id: String,
    /// Channel to send new WebSocket connections to the persistent agent thread.
    /// Lazily initialised on first connection; protected by a tokio Mutex so the
    /// axum handler (which is `Send`) can acquire it.
    agent_conn_tx: tokio::sync::Mutex<Option<mpsc::UnboundedSender<NewConnectionChannels>>>,
    /// Server-global Web Push subscription store (persisted under `$GROK_HOME`).
    /// Written by the authenticated `/push/subscribe` routes, read by the push
    /// notifier when an event fires.
    push_store: PushStore,
    /// VAPID keypair identifying this server to push services. The public key is
    /// served to the PWA; the private key signs each push's auth header.
    vapid: Arc<VapidKeys>,
}

/// Channels bridging a single WebSocket connection to the agent thread.
struct NewConnectionChannels {
    from_ws_rx: mpsc::UnboundedReceiver<String>,
    to_ws_tx: mpsc::UnboundedSender<String>,
}

/// Query parameters for WebSocket connection.
#[derive(Debug, serde::Deserialize, Default)]
pub struct WsQueryParams {
    #[serde(rename = "server-key")]
    pub server_key: Option<String>,
}

/// Constant-time equality for secret comparison.
///
/// Folds the XOR of every byte pair so the comparison time does not depend on
/// the position of the first mismatch (a plain `==` short-circuits, which
/// leaks prefix-match length to a network attacker). Length is still compared
/// up front — leaking the secret's length is acceptable.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Validate the bearer token from request headers or query parameters.
fn validate_auth(headers: &HeaderMap, query: &WsQueryParams, expected_secret: &str) -> bool {
    // Try Authorization header
    if let Some(token) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return constant_time_eq(token.as_bytes(), expected_secret.as_bytes());
    }

    // Fall back to query parameter for browser connections
    if let Some(ref key) = query.server_key {
        return constant_time_eq(key.as_bytes(), expected_secret.as_bytes());
    }

    false
}

/// `GET /push/vapid-public-key` — hand the PWA the server's VAPID public key so
/// it can call `PushManager.subscribe({ applicationServerKey })`. Secret-gated.
async fn push_vapid_public_key(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<WsQueryParams>,
) -> Response {
    if !validate_auth(&headers, &query, &state.secret) {
        return (
            StatusCode::UNAUTHORIZED,
            "Invalid or missing authorization token",
        )
            .into_response();
    }
    Json(serde_json::json!({ "publicKey": state.vapid.public_key_b64() })).into_response()
}

/// `POST /push/subscribe` — register a browser push subscription. Secret-gated.
/// The body is taken raw and parsed only AFTER auth, so an unauthenticated
/// caller can never reach the JSON parser.
async fn push_subscribe(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<WsQueryParams>,
    body: Bytes,
) -> Response {
    if !validate_auth(&headers, &query, &state.secret) {
        return (
            StatusCode::UNAUTHORIZED,
            "Invalid or missing authorization token",
        )
            .into_response();
    }
    let sub: crate::agent::notify::PushSubscription = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid subscription json").into_response(),
    };
    state.push_store.add(sub).await;
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /push/unsubscribe` — drop a subscription by endpoint. Secret-gated.
async fn push_unsubscribe(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<WsQueryParams>,
    body: Bytes,
) -> Response {
    if !validate_auth(&headers, &query, &state.secret) {
        return (
            StatusCode::UNAUTHORIZED,
            "Invalid or missing authorization token",
        )
            .into_response();
    }
    #[derive(serde::Deserialize)]
    struct UnsubscribeBody {
        endpoint: String,
    }
    let parsed: UnsubscribeBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid unsubscribe json").into_response(),
    };
    state.push_store.remove(&parsed.endpoint).await;
    StatusCode::NO_CONTENT.into_response()
}

/// WebSocket upgrade handler with authentication.
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<WsQueryParams>,
) -> Response {
    // Validate secret token from header or query param
    if !validate_auth(&headers, &query, &state.secret) {
        warn!("Unauthorized connection attempt from {}", addr);
        return (
            StatusCode::UNAUTHORIZED,
            "Invalid or missing authorization token",
        )
            .into_response();
    }

    info!("Authenticated WebSocket connection from {}", addr);
    ws.on_upgrade(move |socket| handle_connection(socket, state, addr))
}

/// Handle an authenticated WebSocket connection.
///
/// On first connection, spawns a persistent agent thread that owns the MvpAgent.
/// On subsequent connections (reconnects), sends new WS channels to the existing
/// agent thread so that session actors can continue streaming to the new client.
async fn handle_connection(ws: WebSocket, state: Arc<ServerState>, peer_addr: SocketAddr) {
    info!("New WebSocket connection from {}", peer_addr);

    let (mut ws_write, mut ws_read) = ws.split();

    // Hello frame FIRST, before any ACP traffic: lets the client learn the
    // server's protocol version (fail fast on skew) and instance id (decide
    // whether a reconnect needs to replay ACP state). The remote client
    // (`connect_remote_agent`) consumes exactly this frame before handing its
    // channels to the caller.
    let hello = serde_json::json!({
        "type": "hello",
        "agent_instance_id": state.instance_id,
        "protocol_version": REMOTE_PROTOCOL_VERSION,
        "binary_version": xai_grok_version::VERSION,
    });
    if ws_write
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        warn!("Failed to send hello frame to {}", peer_addr);
        return;
    }

    // Channels for bridging WS <-> Agent thread
    let (to_agent_tx, to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (from_agent_tx, mut from_agent_rx) = mpsc::unbounded_channel::<String>();

    // Ensure the persistent agent thread is running (lazy init on first connection).
    // If the previous agent thread died (panic, etc.), clear the stale sender so we
    // respawn a fresh one.
    {
        let mut agent_tx_guard = state.agent_conn_tx.lock().await;

        // Check if existing sender is still alive (receiver not dropped)
        if let Some(ref tx) = *agent_tx_guard
            && tx.is_closed()
        {
            warn!("Persistent agent thread died — will respawn");
            *agent_tx_guard = None;
        }

        if agent_tx_guard.is_none() {
            let (conn_tx, conn_rx) = mpsc::unbounded_channel();

            let agent_config = state.agent_config.clone();
            let _agent_thread = thread::Builder::new()
                .name("agent-persistent".to_string())
                .spawn(move || {
                    // Prefetch models before creating the runtime (blocking is OK here)
                    let auth = agent_config.create_auth_manager().current();
                    let fetch_auth =
                        ModelFetchAuth::resolve(&agent_config.endpoints, auth.is_some());
                    let prefetched_models = if auth.is_some()
                        || agent_config.endpoints.has_custom_endpoint()
                        || fetch_auth != ModelFetchAuth::Session
                    {
                        prefetch_models_blocking(&agent_config.endpoints, auth.as_ref(), fetch_auth)
                    } else {
                        None
                    };

                    info!("Prefetched models: {:?}", prefetched_models);

                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create runtime for agent");

                    let local_set = tokio::task::LocalSet::new();
                    local_set.block_on(&rt, async move {
                        run_persistent_agent(agent_config, conn_rx, prefetched_models).await
                    });

                    warn!("Persistent agent thread exiting");
                });

            *agent_tx_guard = Some(conn_tx);
            info!("Persistent agent thread spawned");
        }

        // Send new WS channels to the agent thread
        if let Some(ref tx) = *agent_tx_guard
            && tx
                .send(NewConnectionChannels {
                    from_ws_rx: to_agent_rx,
                    to_ws_tx: from_agent_tx,
                })
                .is_err()
        {
            warn!("Failed to send connection channels to agent thread");
        }
    }

    // Task: Read from WS, send to agent thread
    let read_task = tokio::spawn(async move {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let text_str: &str = text.as_ref();
                    let trimmed = text_str.trim_end_matches(['\r', '\n']);
                    // Skip browser keepalive pings (non-JSON text)
                    if trimmed == "ping" || trimmed.is_empty() {
                        continue;
                    }
                    if to_agent_tx.send(trimmed.to_string()).is_err() {
                        break;
                    }
                }
                Ok(Message::Binary(bin)) => {
                    if let Ok(s) = std::str::from_utf8(&bin) {
                        let trimmed = s.trim_end_matches(['\r', '\n']);
                        if trimmed == "ping" || trimmed.is_empty() {
                            continue;
                        }
                        if to_agent_tx.send(trimmed.to_string()).is_err() {
                            break;
                        }
                    }
                }
                Ok(Message::Close(frame)) => {
                    if let Some(f) = frame {
                        info!(
                            "WebSocket close from {}: {} {}",
                            peer_addr, f.code, f.reason
                        );
                    }
                    break;
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                Err(e) => {
                    warn!("WebSocket read error from {}: {:?}", peer_addr, e);
                    break;
                }
            }
        }
    });

    // Task: Read from agent thread, send to WS (with keepalive)
    let write_task = tokio::spawn(async move {
        let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));

        loop {
            tokio::select! {
                Some(msg) = from_agent_rx.recv() => {
                    if ws_write.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                _ = keepalive.tick() => {
                    if ws_write.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
                else => break,
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }

    info!("WebSocket connection ended for {}", peer_addr);
}

/// Run the persistent agent on a dedicated thread with LocalSet.
///
/// The MvpAgent is created **once** and reused across WebSocket reconnections.
/// A persistent gateway channel ensures that session actors (which hold cloned
/// `GatewaySender` handles) can always send notifications. A relay task forwards
/// messages from the persistent channel to the *current* ACP connection's channel,
/// so notifications reach whichever client is currently connected.
async fn run_persistent_agent(
    agent_config: AgentConfig,
    mut connection_rx: mpsc::UnboundedReceiver<NewConnectionChannels>,
    prefetched_models: Option<IndexMap<String, ModelEntry>>,
) {
    // Persistent gateway channel — the MvpAgent and all session actors hold
    // clones of `gw_tx`. This channel survives across reconnections.
    let (gw_tx, mut gw_rx) = tokio::sync::mpsc::unbounded_channel::<AcpClientMessage>();
    let gateway = GatewaySender::new(gw_tx);

    // Create MvpAgent ONCE -- it persists for the lifetime of the server.
    let auth_manager = Arc::new(agent_config.create_auth_manager());
    // Proactive token refresh; runs until process exit.
    auth_manager.start_proactive_refresh(tokio_util::sync::CancellationToken::new());
    // Restore managed policy right before bootstrap reads it — the agent is created lazily here,
    // so an earlier restore could go stale before the gate.
    crate::managed_config::ensure_managed_policy_present(&auth_manager).await;
    let agent = Rc::new(
        MvpAgent::new(gateway, &agent_config, auth_manager, prefetched_models)
            .unwrap_or_else(crate::agent::init::exit_on_config_error),
    );

    let relay_dest: RelayDest = Rc::new(RefCell::new(None));

    // Relay task: reads from the persistent gateway channel and forwards to
    // whichever ACP connection is currently active.
    let relay_dest_for_task = relay_dest.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = gw_rx.recv().await {
            let maybe_tx = relay_dest_for_task.borrow().clone();
            if let Some(tx) = maybe_tx
                && tx.send(msg).is_err()
            {
                // Connection's gateway receiver was dropped — clear it.
                *relay_dest_for_task.borrow_mut() = None;
            }
            // If no connection, the message (and its response_tx) is dropped.
            // The caller (session actor) gets a send error which is already
            // handled with `let _ = ...`.
        }
    });

    // Accept new connections in a loop
    while let Some(channels) = connection_rx.recv().await {
        info!("Agent thread: setting up new ACP connection (reconnect)");
        setup_acp_connection(agent.clone(), channels, relay_dest.clone());
    }

    info!("Agent thread: connection channel closed, exiting");
}

/// Set up a new ACP connection for a WebSocket connection, reusing the existing
/// MvpAgent. The relay destination is updated so that session actor notifications
/// flow to the new client.
fn setup_acp_connection(
    agent: Rc<MvpAgent>,
    channels: NewConnectionChannels,
    relay_dest: RelayDest,
) {
    let NewConnectionChannels {
        mut from_ws_rx,
        to_ws_tx,
    } = channels;

    // Create new simplex IO streams for this ACP connection
    let (agent_read_rx, mut agent_read_tx) = simplex(MAX_BUFFER_SIZE);
    let (agent_write_rx, agent_write_tx) = simplex(MAX_BUFFER_SIZE);

    let incoming = agent_read_rx.compat();
    let outgoing = agent_write_tx.compat_write();

    // Create a per-connection gateway channel for the GatewayReceiver.
    // The relay task will forward persistent-channel messages here.
    let (conn_gw_tx, conn_gw_rx) = tokio::sync::mpsc::unbounded_channel::<AcpClientMessage>();

    // Point the relay at this new connection's channel
    *relay_dest.borrow_mut() = Some(conn_gw_tx);

    // Create new ACP connection reusing the same MvpAgent (via Rc clone).
    // `Agent` is implemented for `Rc<T: Agent>` so this works.
    let incoming = LineBufferedRead::spawn_local(incoming);
    let (conn, handle_io) = acp::AgentSideConnection::new(agent, outgoing, incoming, |fut| {
        tokio::task::spawn_local(fut);
    });
    tokio::task::spawn_local(
        GatewayReceiver::new(conn_gw_rx, conn)
            .with_on_meta(xai_file_utils::trace_context::span_from_meta_traceparent)
            .run(),
    );

    // Task: Forward WS messages → agent (incoming ACP bytes)
    tokio::task::spawn_local(async move {
        while let Some(msg) = from_ws_rx.recv().await {
            // Log messages that lack both `id` and `method` — the ACP layer
            // only prints "received message with neither id nor method" without
            // the payload, making debugging impossible.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg)
                && v.get("id").is_none()
                && v.get("method").is_none()
            {
                warn!(
                    len = msg.len(),
                    "incoming WS message has neither id nor method"
                );
            }
            if agent_read_tx.write_all(msg.as_bytes()).await.is_err() {
                break;
            }
            if agent_read_tx.write_all(b"\n").await.is_err() {
                break;
            }
        }
        // WS disconnected — the simplex writer is dropped, causing `handle_io`
        // to complete. The GatewayReceiver for this connection will also stop.
        // But the MvpAgent and session actors stay alive, ready for the next
        // connection.
    });

    // Task: Forward agent messages → WS (outgoing ACP bytes)
    tokio::task::spawn_local(async move {
        let mut reader = BufReader::new(agent_write_rx);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let msg = line.trim_end_matches(['\r', '\n']);
                    if !msg.is_empty() && to_ws_tx.send(msg.to_string()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Run the ACP IO handler — fire-and-forget since we don't block the
    // connection loop. It completes when the WS disconnects.
    tokio::task::spawn_local(async move {
        let _ = handle_io.await;
        info!("ACP connection IO handler completed");
    });
}

/// Run the agent WebSocket server.
///
/// This starts a WebSocket server that accepts authenticated connections from
/// remote TUI clients. A single agent instance is shared across all connections
/// (persisted across reconnections) so that in-flight session work survives
/// client disconnects.
///
/// # Arguments
/// * `config` - Server configuration (bind address and secret)
/// * `agent_config` - Agent configuration to use for each connection
///
/// # Example
/// ```ignore
/// let server_config = ServerConfig {
///     bind_addr: "0.0.0.0:9000".parse().unwrap(),
///     secret: "my-secret-token".to_string(),
/// };
/// run_agent_server(server_config, agent_config).await?;
/// ```
pub async fn run_agent_server(
    config: ServerConfig,
    agent_config: AgentConfig,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.bind_addr).await?;
    run_agent_server_on(listener, config.secret, agent_config).await
}

/// Run the agent WebSocket server on an already-bound listener.
///
/// Split out from [`run_agent_server`] so tests can bind `127.0.0.1:0`, read
/// the ephemeral port with `TcpListener::local_addr`, and then hand the
/// listener over.
pub async fn run_agent_server_on(
    listener: TcpListener,
    secret: String,
    agent_config: AgentConfig,
) -> anyhow::Result<()> {
    let bind_addr = listener.local_addr()?;
    let instance_id = uuid::Uuid::new_v4().to_string();
    info!(agent_instance_id = %instance_id, "generated agent instance id for this server process");

    // Web Push transport: load the persisted subscription store and the VAPID
    // keypair (generated + persisted on first run) from the user-global grok
    // home. Both are server-global and shared with the push notifier.
    let grok_home = xai_grok_config::grok_home();
    let push_store = PushStore::load(&grok_home);
    let vapid = Arc::new(VapidKeys::load_or_generate(&grok_home)?);

    // Publish the process-wide out-of-band notifier so each session's
    // notification bridge can also deliver to Web Push subscribers. The `sub`
    // claim is a self-hosting contact URI; push services don't validate it
    // strictly. FanoutNotifier keeps room for more sinks (e.g. a shell-command
    // notifier) without touching the bridge.
    let web_push = std::sync::Arc::new(crate::agent::notify::WebPushNotifier::new(
        push_store.clone(),
        vapid.clone(),
        "mailto:grok-agent@localhost",
    ));
    crate::agent::notify::set_global_notifier(std::sync::Arc::new(
        crate::agent::notify::FanoutNotifier::new(vec![web_push]),
    ));

    // Publish the process-wide artifact service so the `artifact` tool can
    // store documents under grok-home and hand back links on this server's
    // origin, served by the `/artifacts` routes below.
    {
        use xai_grok_tools::implementations::grok_build::{
            ArtifactService, ArtifactStore, set_artifact_service,
        };
        let store = ArtifactStore::load(&grok_home);
        let base_url = format!("http://{}:{}", bind_addr.ip(), bind_addr.port());
        set_artifact_service(std::sync::Arc::new(ArtifactService::new(store, base_url)));
    }

    let state = Arc::new(ServerState {
        agent_config,
        secret,
        instance_id,
        agent_conn_tx: tokio::sync::Mutex::new(None),
        push_store,
        vapid,
    });

    let app = Router::new()
        .route("/ws", get(ws_handler))
        // Mobile/browser PWA chat client shell. No auth on these routes —
        // the shell carries no secrets; `/ws` above still enforces the
        // server's secret via `validate_auth`. See `webui` module docs.
        .route("/", get(webui::index))
        .route("/app.js", get(webui::app_js))
        .route("/style.css", get(webui::style_css))
        .route("/manifest.webmanifest", get(webui::manifest))
        .route("/sw.js", get(webui::service_worker))
        .route("/icon.svg", get(webui::icon_svg))
        // Artifact viewing: an index of published artifacts and each artifact's
        // page. Read-only, no secret (the pages carry no secrets), and every
        // artifact response ships a strict CSP so a page cannot call back to
        // `/ws` or exfiltrate the server secret. See the `artifact` module.
        .route("/artifacts", get(artifacts_index))
        .route("/artifacts/{id}", get(artifact_view))
        // Web Push subscription management. Unlike the static PWA shell routes
        // above, these carry data and are secret-gated exactly like `/ws`.
        .route("/push/vapid-public-key", get(push_vapid_public_key))
        .route("/push/subscribe", post(push_subscribe))
        .route("/push/unsubscribe", post(push_unsubscribe))
        .with_state(state);

    info!("Agent server listening on ws://{}/ws", bind_addr);
    info!(
        "Clients should connect with: --remote ws://{}:{}/ws --remote-secret <token>",
        bind_addr.ip(),
        bind_addr.port()
    );
    info!(
        "Or open the mobile web client: http://{}:{}/",
        bind_addr.ip(),
        bind_addr.port()
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// GET `/artifacts` — index of published artifacts.
async fn artifacts_index() -> Response {
    use xai_grok_tools::implementations::grok_build::{artifact::render, artifact_service};
    let Some(svc) = artifact_service() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "artifacts unavailable").into_response();
    };
    html_response(render::render_index(&svc.store.list()))
}

/// GET `/artifacts/{id}` — render one published artifact under a strict CSP.
async fn artifact_view(axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    use xai_grok_tools::implementations::grok_build::{artifact::render, artifact_service};
    let Some(svc) = artifact_service() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "artifacts unavailable").into_response();
    };
    match svc.store.get(&id) {
        Some(artifact) => html_response(render::render(&artifact)),
        None => (StatusCode::NOT_FOUND, "artifact not found").into_response(),
    }
}

/// Build a `text/html` response carrying the strict artifact CSP so a served
/// page cannot reach the network (no callback to `/ws`, no secret exfiltration).
fn html_response(body: String) -> Response {
    use axum::http::header::{
        CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
    };
    use xai_grok_tools::implementations::grok_build::artifact::render::ARTIFACT_CSP;
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .header(CONTENT_SECURITY_POLICY, ARTIFACT_CSP)
        // Defense in depth alongside the CSP sandbox: don't let the browser
        // sniff a different type, and don't leak the artifact URL as a referrer.
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(REFERRER_POLICY, "no-referrer")
        .body(axum::body::Body::from(body))
        .expect("valid artifact response")
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    #[test]
    fn constant_time_eq_matches_equal_and_rejects_unequal() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn validate_auth_accepts_header_and_query() {
        let query_none = WsQueryParams::default();
        assert!(validate_auth(
            &headers_with_bearer("tok"),
            &query_none,
            "tok"
        ));
        assert!(!validate_auth(
            &headers_with_bearer("wrong"),
            &query_none,
            "tok"
        ));

        let query = WsQueryParams {
            server_key: Some("tok".into()),
        };
        assert!(validate_auth(&HeaderMap::new(), &query, "tok"));
        assert!(!validate_auth(&HeaderMap::new(), &query_none, "tok"));
    }

    #[test]
    fn validate_auth_header_takes_precedence_over_query() {
        // A wrong header must not fall through to a correct query param.
        let query = WsQueryParams {
            server_key: Some("tok".into()),
        };
        assert!(!validate_auth(&headers_with_bearer("wrong"), &query, "tok"));
    }
}
