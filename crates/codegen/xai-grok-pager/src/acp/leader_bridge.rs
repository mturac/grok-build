//! Bridge a leader IPC connection into an `AcpClientChannel`.
//!
//! Adapts the leader's raw JSON string channels into the typed ACP channel
//! interface, reusing `ClientSideConnection` from `agent_client_protocol`
//! for JSON-RPC ser/deser.

use std::sync::Arc;
use std::thread;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, simplex};
use tokio::sync::{Mutex as TokioMutex, mpsc};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use agent_client_protocol as acp;
use xai_acp_lib::{
    AcpClientChannel, AcpGatewayReceiver, AcpGatewaySender, LineBufferedRead, acp_channels,
};
pub use xai_grok_shell::leader::ConnectionStatus;
use xai_grok_shell::agent::RemoteWsReconnector;
use xai_grok_shell::leader::{LeaderConnection, LeaderReconnector, ReconnectPolicy};

use crate::acp::replay;

const MAX_BUF: usize = 8 * 1024 * 1024;

pub struct LeaderBridge {
    pub channel: AcpClientChannel,
    pub cancel: CancellationToken,
    pub thread_handle: thread::JoinHandle<Result<()>>,
}

/// Outcome of one successful reconnect attempt, normalized across transports
/// for `bridge_channels`'s reader task.
struct ReconnectedChannels {
    tx: mpsc::UnboundedSender<String>,
    rx: mpsc::UnboundedReceiver<String>,
    /// Whether the caller must replay `initialize` + `session/load` before
    /// resuming normal pumping.
    ///
    /// Always `false` for the leader transport: the TUI already replays a
    /// leader reconnect at the application layer, keyed off
    /// `ConnectionStatus` generations (see `event_loop.rs`), so
    /// `bridge_channels` doing it too would double-replay. `true` for a
    /// remote WS reconnect that landed on a different `agent_instance_id`
    /// (the server process — and its `MvpAgent` — was replaced).
    needs_replay: bool,
    /// The `agent_instance_id` observed on this reconnect, for the remote
    /// transport only (`None` for the leader transport, which has no
    /// instance-id concept). Passed to
    /// [`BridgeReconnector::confirm_instance`] once replay has actually
    /// succeeded — see that method's docs.
    instance_id: Option<String>,
}

/// Abstracts the two live reconnect strategies `bridge_channels` supports, so
/// one generic reconnect loop drives both the leader IPC transport and the
/// remote WebSocket transport.
pub(crate) enum BridgeReconnector {
    Leader(LeaderReconnector),
    RemoteWs(RemoteWsReconnector),
}

impl BridgeReconnector {
    async fn reconnect(
        &self,
        policy: ReconnectPolicy,
        cancel: &CancellationToken,
    ) -> anyhow::Result<ReconnectedChannels> {
        match self {
            Self::Leader(r) => {
                let (tx, rx, _disconnect_rx) = r.reconnect(policy, cancel).await?;
                Ok(ReconnectedChannels {
                    tx,
                    rx,
                    needs_replay: false,
                    instance_id: None,
                })
            }
            Self::RemoteWs(r) => {
                let outcome = r.reconnect(policy, cancel).await?;
                Ok(ReconnectedChannels {
                    tx: outcome.tx,
                    rx: outcome.rx,
                    needs_replay: outcome.needs_replay,
                    instance_id: Some(outcome.hello.agent_instance_id),
                })
            }
        }
    }

    /// Deliberately NOT called before the caller installs the fresh channels
    /// — see `LeaderReconnector::notify_connected`. No-op for the remote
    /// transport, which has no connection-status observers today.
    fn notify_connected(&self) {
        match self {
            Self::Leader(r) => r.notify_connected(),
            Self::RemoteWs(_) => {}
        }
    }

    /// Commit `instance_id` as the remote reconnector's new baseline for its
    /// NEXT `reconnect`'s `needs_replay` comparison. Call this ONLY after a
    /// `needs_replay` reconnect's replay has actually succeeded (or wasn't
    /// needed) — see `RemoteWsReconnector::reconnect`'s docs on why confirming
    /// too early strands the client sessionless on a failed mid-replay
    /// reconnect. No-op for the leader transport (`instance_id` is always
    /// `None` there).
    async fn confirm_instance(&self, instance_id: Option<&str>) {
        if let (Self::RemoteWs(r), Some(id)) = (self, instance_id) {
            r.confirm_instance(id).await;
        }
    }

    /// Whether `bridge_channels` must track outgoing `initialize` /
    /// `session/load` requests so they can be replayed on this
    /// reconnector's signal. Only the remote transport can report
    /// `needs_replay`; tracking it for the leader transport would be pure
    /// overhead since it's never acted on there.
    fn wants_replay_state(&self) -> bool {
        matches!(self, Self::RemoteWs(_))
    }
}

/// Cache one outgoing line for replay, when tracking is enabled.
///
/// Unconditional (no per-method prefilter) when `track_replay_state`:
/// `cache_outgoing_acp_state`'s own method match already no-ops on anything
/// irrelevant, and outgoing volume is tiny (user-initiated requests), so a
/// prefilter here buys nothing. A prior version gated this on
/// `pending.contains("\"session/close\"")` and friends, which could NEVER
/// match the real wire methods `x.ai/session/close` / `_x.ai/session/close`
/// (the byte before `session/close` on the wire is `/`, not `"`) — closed
/// sessions were silently never evicted from the replay cache and got
/// resurrected by the next replay.
fn cache_outgoing_if_tracking(
    track_replay_state: bool,
    pending: &str,
    state: &std::sync::Mutex<replay::ReplayState>,
) {
    if track_replay_state {
        replay::cache_outgoing_acp_state(pending, state);
    }
}

/// How [`forward_outbound_line`] resolved one outbound line.
#[derive(Debug, PartialEq, Eq)]
enum ForwardOutcome {
    Sent,
    /// The connection the line was composed for died and a new one replaced
    /// it; the line was dropped.
    DroppedStale,
    Cancelled,
}

/// Send one outbound line to the (swappable) leader tx.
///
/// A failed send means the connection is dead; the line is held — blocking
/// the lines queued behind it — until the reader task installs a fresh tx,
/// then dropped. Replaying it would be worse: a stale `session/load`
/// re-delivered on the new connection triggers a second full replay into the
/// same reload window (duplicated transcript); the reconnect re-init
/// re-establishes state explicitly instead.
///
/// Scoping is by FIRST OBSERVED send failure, a best-effort heuristic: a
/// pre-disconnect line whose first send happens after the swap never fails
/// and goes out on the new connection, and lines queued behind a held one
/// are forwarded post-swap regardless of when they were composed.
async fn forward_outbound_line(
    leader_tx: &TokioMutex<mpsc::UnboundedSender<String>>,
    cancel: &CancellationToken,
    mut pending: String,
) -> ForwardOutcome {
    let mut failed_on: Option<mpsc::UnboundedSender<String>> = None;
    loop {
        {
            let tx = leader_tx.lock().await;
            if let Some(ref dead) = failed_on
                && !tx.same_channel(dead)
            {
                return ForwardOutcome::DroppedStale;
            }
            pending = match tx.send(pending) {
                Ok(()) => return ForwardOutcome::Sent,
                Err(mpsc::error::SendError(returned)) => returned,
            };
            if failed_on.is_none() {
                failed_on = Some(tx.clone());
            }
        }
        tracing::debug!("Writer send failed; holding line until the reconnect swap");
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return ForwardOutcome::Cancelled,
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }
}

/// Bridge a `LeaderConnection` into an `AcpClientChannel`.
///
/// When `reconnector` is `Some`, the bridge automatically attempts to reconnect
/// on leader disconnect using the given `policy`. On reconnection failure (or if
/// `reconnector` is `None`), the cancel token fires so the caller can exit.
pub fn bridge_leader_connection(
    conn: LeaderConnection,
    cancel: CancellationToken,
    reconnector: Option<LeaderReconnector>,
    policy: ReconnectPolicy,
) -> Result<LeaderBridge> {
    let (leader_tx, leader_rx) = conn.into_channels();
    bridge_channels(
        leader_tx,
        leader_rx,
        cancel,
        reconnector.map(BridgeReconnector::Leader),
        policy,
    )
}

/// Bridge raw IPC channels into an `AcpClientChannel`.
///
/// Spawns a dedicated thread with a `LocalSet` because `ClientSideConnection`
/// uses `spawn_local` internally. On disconnect, reconnects via `reconnector`
/// (if provided) or fires the cancel token. When `reconnector` reports
/// `needs_replay` on a successful reconnect, replays cached `initialize` +
/// `session/load` requests into the incoming pipe before resuming normal
/// pumping (see `crate::acp::replay`).
pub(crate) fn bridge_channels(
    leader_tx: mpsc::UnboundedSender<String>,
    leader_rx: mpsc::UnboundedReceiver<String>,
    cancel: CancellationToken,
    reconnector: Option<BridgeReconnector>,
    policy: ReconnectPolicy,
) -> Result<LeaderBridge> {
    let (client_channel, agent_channel) = acp_channels();

    let (incoming_read, incoming_write) = simplex(MAX_BUF);
    let (outgoing_read, outgoing_write) = simplex(MAX_BUF);

    // Only the remote transport can ever signal `needs_replay`; tracking
    // outgoing/incoming ACP state for the leader transport would be dead
    // weight (see `BridgeReconnector::wants_replay_state`).
    let track_replay_state = reconnector
        .as_ref()
        .is_some_and(BridgeReconnector::wants_replay_state);
    let replay_state = Arc::new(std::sync::Mutex::new(replay::ReplayState::default()));

    let bridge_cancel = cancel.clone();
    let thread_handle = thread::Builder::new()
        .name("pager-leader-bridge".into())
        .spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let leader_tx_shared = Arc::new(TokioMutex::new(leader_tx));

                // Reader: leader IPC -> incoming simplex pipe -> ClientSideConnection
                let cancel_r = bridge_cancel.clone();
                let leader_tx_for_reader = leader_tx_shared.clone();
                let replay_state_for_reader = replay_state.clone();
                let reader_task = tokio::task::spawn_local(async move {
                    let mut incoming_write = incoming_write;
                    let mut leader_rx = leader_rx;
                    'reader: loop {
                        tokio::select! {
                            biased;
                            _ = cancel_r.cancelled() => break 'reader,
                            msg = leader_rx.recv() => {
                                match msg {
                                    Some(json_line) => {
                                        if track_replay_state
                                            && (json_line.contains("\"sessionId\"")
                                                || json_line.contains("\"session_id\""))
                                        {
                                            replay::cache_incoming_session_id(
                                                &json_line,
                                                &replay_state_for_reader,
                                            );
                                        }
                                        if incoming_write.write_all(json_line.as_bytes()).await.is_err()
                                            || incoming_write.write_all(b"\n").await.is_err()
                                        {
                                            break 'reader;
                                        }
                                    }
                                    None => {
                                        tracing::warn!("Leader connection closed");

                                        let Some(ref reconnector) = reconnector else {
                                            cancel_r.cancel();
                                            break 'reader;
                                        };

                                        // Retry reconnect+replay here, without
                                        // returning to the outer `leader_rx.recv()`
                                        // select in between: a failed replay must
                                        // re-dial immediately rather than pumping
                                        // whatever the half-replayed connection
                                        // happens to produce next.
                                        loop {
                                            tracing::info!("Attempting to reconnect to leader...");
                                            match reconnector.reconnect(policy, &cancel_r).await {
                                                Ok(ReconnectedChannels {
                                                    tx: new_tx,
                                                    rx: new_rx,
                                                    needs_replay,
                                                    instance_id,
                                                }) => {
                                                    tracing::info!("Reconnected to leader IPC");
                                                    leader_rx = new_rx;

                                                    // Hold the tx guard across the
                                                    // ENTIRE replay window, not just
                                                    // the swap: `forward_outbound_line`
                                                    // locks this same mutex before
                                                    // every outbound send, so
                                                    // releasing it between the swap
                                                    // and the replay would let a
                                                    // queued (or brand-new) client
                                                    // request reach the restarted
                                                    // peer mid-replay, ahead of (or
                                                    // interleaved with) the state
                                                    // replay is reconstructing.
                                                    let mut tx_guard = leader_tx_for_reader.lock().await;
                                                    *tx_guard = new_tx;

                                                    let mut replay_failed = false;
                                                    if needs_replay {
                                                        tracing::info!(
                                                            "peer restarted (new agent instance) — replaying ACP state"
                                                        );
                                                        let state_snapshot = replay_state_for_reader
                                                            .lock()
                                                            .unwrap_or_else(|e| e.into_inner())
                                                            .clone();
                                                        let had_state = state_snapshot.has_state_to_replay();
                                                        match replay::replay_acp_state_after_reconnect(
                                                            &tx_guard,
                                                            &mut leader_rx,
                                                            &mut incoming_write,
                                                            &state_snapshot,
                                                        )
                                                        .await
                                                        {
                                                            Some(sid) => {
                                                                tracing::info!(
                                                                    session_id = %sid,
                                                                    "replay succeeded; ACP state restored after reconnect"
                                                                );
                                                            }
                                                            None if !had_state => {
                                                                // Nothing was cached to replay
                                                                // (e.g. reconnect before any
                                                                // `initialize`) — benign, not a
                                                                // failure.
                                                            }
                                                            None => {
                                                                tracing::warn!(
                                                                    "replay failed after reconnect; re-dialing to retry"
                                                                );
                                                                replay_failed = true;
                                                            }
                                                        }
                                                    }
                                                    drop(tx_guard);

                                                    if replay_failed {
                                                        // Do NOT confirm the instance id: the
                                                        // next reconnect attempt must still
                                                        // observe the stale baseline so it
                                                        // reports `needs_replay = true` again
                                                        // and retries the replay, instead of
                                                        // silently stranding the client
                                                        // sessionless.
                                                        continue;
                                                    }

                                                    reconnector.confirm_instance(instance_id.as_deref()).await;
                                                    // Swap first, notify second — see
                                                    // `LeaderReconnector::notify_connected`.
                                                    reconnector.notify_connected();
                                                    continue 'reader;
                                                }
                                                Err(e) => {
                                                    tracing::error!(error = %e, "Failed to reconnect to leader");
                                                    cancel_r.cancel();
                                                    break 'reader;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                });

                // Writer: ClientSideConnection -> outgoing simplex pipe -> leader IPC
                let cancel_w = bridge_cancel.clone();
                let leader_tx_for_writer = leader_tx_shared;
                let replay_state_for_writer = replay_state;
                let writer_task = tokio::task::spawn_local(async move {
                    let mut reader = BufReader::new(outgoing_read);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        tokio::select! {
                            biased;
                            _ = cancel_w.cancelled() => break,
                            result = reader.read_line(&mut line) => {
                                match result {
                                    Ok(0) => break,
                                    Ok(_) => {
                                        let pending = line.trim_end();
                                        if pending.is_empty() {
                                            continue;
                                        }
                                        cache_outgoing_if_tracking(
                                            track_replay_state,
                                            pending,
                                            &replay_state_for_writer,
                                        );
                                        match forward_outbound_line(
                                            &leader_tx_for_writer,
                                            &cancel_w,
                                            pending.to_string(),
                                        )
                                        .await
                                        {
                                            ForwardOutcome::Sent => {}
                                            ForwardOutcome::DroppedStale => {
                                                // Unified-log marker: this drop is deliberate
                                                // (replaying a stale `session/load` would
                                                // double-replay the transcript), but it can eat
                                                // one-shot notifications like `session/cancel`, a
                                                // known stuck-cancel failure mode. Record WHAT was
                                                // dropped so the next
                                                // investigation sees it in the unified log.
                                                let method = serde_json::from_str::<serde_json::Value>(pending)
                                                    .ok()
                                                    .and_then(|j| {
                                                        j.get("method").and_then(|m| m.as_str()).map(str::to_owned)
                                                    });
                                                crate::unified_log::warn(
                                                    "leader.ipc.outbound_dropped_stale",
                                                    None,
                                                    Some(serde_json::json!({
                                                        "method": method,
                                                        "len": pending.len(),
                                                    })),
                                                );
                                                tracing::debug!(
                                                    "Dropped outbound line composed for a replaced leader connection"
                                                );
                                            }
                                            ForwardOutcome::Cancelled => break,
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                    }
                });

                // Wire ClientSideConnection for JSON-RPC ser/deser.
                let gw_tx = AcpGatewaySender::new(agent_channel.tx).with_tracing(true);
                let incoming = LineBufferedRead::spawn_local(incoming_read.compat());
                let (conn, handle_io) = acp::ClientSideConnection::new(
                    gw_tx,
                    outgoing_write.compat_write(),
                    incoming,
                    |fut| { tokio::task::spawn_local(fut); },
                );
                let gw_rx = AcpGatewayReceiver::new(agent_channel.rx, conn).with_tracing(true);
                tokio::task::spawn_local(handle_io);
                tokio::task::spawn_local(gw_rx.run());
                tokio::task::yield_now().await;

                bridge_cancel.cancelled().await;
                reader_task.abort();
                writer_task.abort();
                Ok(())
            })
        })?;

    Ok(LeaderBridge {
        channel: client_channel,
        cancel,
        thread_handle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_acp_lib::acp_send;

    /// Regression test for the writer's outgoing-cache gate: a realistic
    /// wire-format `x.ai/session/close` line (as the writer task actually
    /// reads it off the outgoing pipe) must evict the session from the
    /// replay cache. Exercises `cache_outgoing_if_tracking` — the exact
    /// function the writer task calls for every outbound line — not
    /// `replay::cache_outgoing_acp_state` directly: the bug this guards
    /// against was in the (now-deleted) `contains("\"session/close\"")`
    /// prefilter that used to gate that call, which could never match the
    /// real wire method (`x.ai/session/close` — the byte before
    /// `session/close` is `/`, not `"`), so prior direct-call unit tests of
    /// `cache_outgoing_acp_state` never caught it.
    #[test]
    fn cache_outgoing_if_tracking_evicts_closed_session_via_writer_gate() {
        let state = std::sync::Mutex::new(replay::ReplayState::default());
        replay::cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        replay::cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"A","cwd":"/tmp"}}"#,
            &state,
        );

        let close_line =
            r#"{"jsonrpc":"2.0","id":5,"method":"x.ai/session/close","params":{"sessionId":"A"}}"#;
        cache_outgoing_if_tracking(true, close_line, &state);

        // The session is gone, but `initialize` is still cached — confirms
        // eviction was scoped to the session, not a wholesale reset.
        assert!(
            !state.lock().unwrap().contains_session("A"),
            "session/close on the writer path must evict the closed session"
        );
        assert!(state.lock().unwrap().has_state_to_replay());
    }

    /// The gate must be a strict no-op when tracking is disabled (leader
    /// transport, which never signals `needs_replay`) — no wasted parsing,
    /// and definitely no caching.
    #[test]
    fn cache_outgoing_if_tracking_is_noop_when_disabled() {
        let state = std::sync::Mutex::new(replay::ReplayState::default());
        cache_outgoing_if_tracking(
            false,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        assert!(!state.lock().unwrap().has_state_to_replay());
    }

    #[tokio::test]
    async fn forward_outbound_line_delivers_on_live_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let shared = TokioMutex::new(tx);
        let cancel = CancellationToken::new();

        assert_eq!(
            forward_outbound_line(&shared, &cancel, "hello".into()).await,
            ForwardOutcome::Sent
        );
        assert_eq!(rx.recv().await.as_deref(), Some("hello"));
    }

    /// Connection-scoped redelivery semantics: a line whose send failed is
    /// HELD (blocking later lines) until the reader swaps in a fresh tx, then
    /// DROPPED — neither discarded at first failure (silent outbound loss)
    /// nor replayed onto the new connection (a stale `session/load` would
    /// double-replay the transcript). Lines queued behind it flow onto the
    /// new connection.
    #[tokio::test]
    async fn forward_outbound_line_drops_stale_line_after_swap_and_sends_next() {
        let (dead_tx, dead_rx) = mpsc::unbounded_channel::<String>();
        drop(dead_rx);
        let shared = Arc::new(TokioMutex::new(dead_tx));
        let cancel = CancellationToken::new();

        let (new_tx, mut new_rx) = mpsc::unbounded_channel::<String>();
        let swapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper_shared = shared.clone();
        let swapper_swapped = swapped.clone();
        let swapper = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            // Flag BEFORE the swap: a `DroppedStale` return implies the new
            // tx was observed, which happens-after this store.
            swapper_swapped.store(true, std::sync::atomic::Ordering::SeqCst);
            *swapper_shared.lock().await = new_tx;
        });

        assert_eq!(
            forward_outbound_line(&shared, &cancel, "stale request".into()).await,
            ForwardOutcome::DroppedStale
        );
        assert!(
            swapped.load(std::sync::atomic::Ordering::SeqCst),
            "the line must be HELD until the swap, not dropped on first failure"
        );
        swapper.await.unwrap();

        assert_eq!(
            forward_outbound_line(&shared, &cancel, "fresh request".into()).await,
            ForwardOutcome::Sent
        );
        assert_eq!(
            new_rx.recv().await.as_deref(),
            Some("fresh request"),
            "the first line on the new connection is the post-swap one"
        );
        assert!(
            new_rx.try_recv().is_err(),
            "the stale line must not be re-delivered onto the new connection"
        );
    }

    #[tokio::test]
    async fn forward_outbound_line_cancellation_exits_retry() {
        let (dead_tx, dead_rx) = mpsc::unbounded_channel::<String>();
        drop(dead_rx);
        let shared = TokioMutex::new(dead_tx);
        let cancel = CancellationToken::new();
        cancel.cancel();

        assert_eq!(
            forward_outbound_line(&shared, &cancel, "x".into()).await,
            ForwardOutcome::Cancelled
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_passes_initialize_round_trip() {
        let cancel = CancellationToken::new();

        let (fake_leader_tx, bridge_leader_rx) = mpsc::unbounded_channel::<String>();
        let (bridge_leader_tx, mut fake_leader_rx) = mpsc::unbounded_channel::<String>();

        let bridge = bridge_channels(
            bridge_leader_tx,
            bridge_leader_rx,
            cancel.clone(),
            None,
            ReconnectPolicy::bounded(),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let fake_leader = tokio::spawn(async move {
            let msg = fake_leader_rx.recv().await.expect("expected a message");
            let req: serde_json::Value =
                serde_json::from_str(&msg).expect("invalid JSON from bridge");
            let id = req.get("id").expect("missing id").clone();

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "1",
                    "serverCapabilities": {},
                    "authMethods": []
                }
            });
            fake_leader_tx
                .send(serde_json::to_string(&response).unwrap())
                .unwrap();
        });

        let _resp: acp::InitializeResponse = acp_send(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                acp::ClientCapabilities::new()
                    .fs(acp::FileSystemCapabilities::new())
                    .terminal(false),
            ),
            &bridge.channel.tx,
        )
        .await
        .expect("initialize should succeed through bridge");

        fake_leader.await.unwrap();
        cancel.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_cancels_on_leader_disconnect_without_reconnector() {
        let cancel = CancellationToken::new();

        let (leader_inbound_tx, bridge_leader_rx) = mpsc::unbounded_channel::<String>();
        let (bridge_leader_tx, _leader_outbound_rx) = mpsc::unbounded_channel::<String>();

        let bridge = bridge_channels(
            bridge_leader_tx,
            bridge_leader_rx,
            cancel.clone(),
            None,
            ReconnectPolicy::bounded(),
        )
        .unwrap();

        drop(leader_inbound_tx);

        tokio::time::timeout(std::time::Duration::from_secs(5), bridge.cancel.cancelled())
            .await
            .expect("bridge should cancel after leader disconnect");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_does_not_break_pipe_on_send_failure() {
        let cancel = CancellationToken::new();

        let (leader_inbound_tx, bridge_leader_rx) = mpsc::unbounded_channel::<String>();
        let (bridge_leader_tx, leader_outbound_rx) = mpsc::unbounded_channel::<String>();

        let bridge = bridge_channels(
            bridge_leader_tx,
            bridge_leader_rx,
            cancel.clone(),
            None,
            ReconnectPolicy::bounded(),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drop the outbound receiver so leader_tx.send() will fail in the writer.
        drop(leader_outbound_rx);

        // Spawn a background task that pushes an outbound ACP request.
        // acp_send blocks for a response that will never arrive (the outbound
        // receiver is dropped), but the writer task should survive the failed
        // send rather than breaking the simplex pipe.
        let tx = bridge.channel.tx.clone();
        tokio::spawn(async move {
            let _ = acp_send(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
                &tx,
            )
            .await;
        });

        // Give the writer time to hit the send failure and sleep/retry.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // The bridge should still be alive — the writer must not have broken
        // the pipe by exiting.
        assert!(
            !cancel.is_cancelled(),
            "writer should survive send failures (reader not disconnected yet)"
        );

        // Now disconnect the leader fully so the reader triggers cancel.
        drop(leader_inbound_tx);
        tokio::time::timeout(std::time::Duration::from_secs(5), bridge.cancel.cancelled())
            .await
            .expect("bridge should eventually cancel after full disconnect");
    }
}
