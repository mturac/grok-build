//! Replay cached ACP state (`initialize` + `session/load`) after a transport
//! reconnect.
//!
//! Shared by two call sites that both bridge a raw JSON-line ACP stream over
//! a transport that can drop and come back:
//!
//! - The stdio↔leader bridge in `xai-grok-pager-bin` (an external ACP client
//!   talks stdio to this process, which forwards to a leader over IPC).
//! - The remote WebSocket bridge (`crate::acp::leader_bridge::bridge_channels`)
//!   when a `grok agent serve` process restarts and its `MvpAgent` — and thus
//!   every session — is gone.
//!
//! Both cases need the same thing: remember the client's `initialize` and
//! every `session/load` (or `session/new`) it issued, then, once a fresh
//! connection exists, replay them in order and block until each is actually
//! restored before resuming normal traffic.

/// How to rebuild one session's `session/load` after a reconnect.
#[derive(Default, Clone)]
struct CachedSession {
    /// Verbatim `session/load` request JSON (preferred replay form: preserves
    /// the client's exact cwd / mcpServers / meta). `None` when the session
    /// was only ever created via `session/new` — the load is synthesized.
    load_request_json: Option<String>,
    /// `cwd` captured from `session/new` / `session/load` params.
    cwd: Option<String>,
    /// `mcpServers` captured from `session/new` / `session/load` params.
    mcp_servers_json: Option<String>,
}

/// ACP state cached from an outbound JSON stream for replay after a
/// reconnect.
///
/// Tracks EVERY session the client has open (IDE clients drive multiple
/// sessions over one bridge), not just the most recent one — a transport
/// restart must restore all of them or the others die with "unknown session
/// id" on their next prompt.
#[derive(Default, Clone)]
pub struct ReplayState {
    initialize_json: Option<String>,
    /// Sessions to restore on reconnect, keyed by session id, in first-seen
    /// order (Vec keeps replay order deterministic).
    sessions: Vec<(String, CachedSession)>,
    /// cwd/mcp from the most recent `session/new` REQUEST whose response has
    /// not been observed yet. Folded into `sessions` when the response
    /// carrying the assigned session id arrives. Never replayed while
    /// unconfirmed (the id is unknown; the client's own request died with the
    /// old transport and is its to retry).
    pending_new: Option<CachedSession>,
    /// Most recently created/loaded session id — reported as the primary
    /// restored session.
    last_session_id: Option<String>,
}

impl ReplayState {
    fn upsert_session(&mut self, sid: &str, cached: CachedSession) {
        if let Some((_, existing)) = self.sessions.iter_mut().find(|(id, _)| id == sid) {
            *existing = cached;
        } else {
            self.sessions.push((sid.to_string(), cached));
        }
    }
    fn remove_session(&mut self, sid: &str) {
        self.sessions.retain(|(id, _)| id != sid);
        if self.last_session_id.as_deref() == Some(sid) {
            self.last_session_id = None;
        }
    }

    /// Whether this snapshot has anything worth replaying: a cached
    /// `initialize` or at least one cached session. An unconfirmed
    /// `session/new` (`pending_new`) does NOT count — its id is unknown, so
    /// there is nothing to replay for it yet.
    ///
    /// Used to tell "replay found nothing cached to do" (benign — e.g. a
    /// reconnect that lands before the client ever sent `initialize`) apart
    /// from "replay attempted something and it failed", both of which
    /// `replay_acp_state_after_reconnect` reports as `None`.
    pub fn has_state_to_replay(&self) -> bool {
        self.initialize_json.is_some() || !self.sessions.is_empty()
    }

    /// Whether `sid` is currently cached for replay. Crate-visible for
    /// regression tests that need to check eviction (e.g. `session/close`)
    /// without reaching into private fields.
    #[cfg(test)]
    pub(crate) fn contains_session(&self, sid: &str) -> bool {
        self.sessions.iter().any(|(id, _)| id == sid)
    }
}

/// Cache an outbound ACP request (`initialize`, `session/load`, `session/new`,
/// `session/close`) for later replay.
pub fn cache_outgoing_acp_state(msg: &str, state: &std::sync::Mutex<ReplayState>) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(msg) else {
        return;
    };
    let method = json
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    match method {
        "initialize" => {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.initialize_json = Some(msg.to_string());
        }
        "session/load" => {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(params) = json.get("params") {
                let sid = params
                    .get("sessionId")
                    .or_else(|| params.get("session_id"))
                    .and_then(|v| v.as_str());
                if let Some(sid) = sid {
                    let cached = CachedSession {
                        load_request_json: Some(msg.to_string()),
                        cwd: params
                            .get("cwd")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        mcp_servers_json: params
                            .get("mcpServers")
                            .and_then(|m| serde_json::to_string(m).ok()),
                    };
                    s.upsert_session(sid, cached);
                    s.last_session_id = Some(sid.to_string());
                }
            }
        }
        "session/new" => {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            let params = json.get("params");
            s.pending_new = Some(CachedSession {
                load_request_json: None,
                cwd: params
                    .and_then(|p| p.get("cwd"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                mcp_servers_json: params
                    .and_then(|p| p.get("mcpServers"))
                    .and_then(|m| serde_json::to_string(m).ok()),
            });
        }
        "x.ai/session/close" | "_x.ai/session/close" => {
            if let Some(sid) = json
                .get("params")
                .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                .and_then(|v| v.as_str())
            {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.remove_session(sid);
            }
        }
        _ => {}
    }
}

/// Observe an inbound ACP response for a `sessionId` (the result of a
/// `session/new` or `session/load`), folding a pending `session/new` into the
/// tracked session list once its id is known.
pub fn cache_incoming_session_id(msg: &str, state: &std::sync::Mutex<ReplayState>) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(msg) else {
        return;
    };
    if let Some(sid) = json
        .get("result")
        .and_then(|r| r.get("sessionId").or_else(|| r.get("session_id")))
        .and_then(|v| v.as_str())
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = s.pending_new.take() {
            s.upsert_session(sid, pending);
        }
        s.last_session_id = Some(sid.to_string());
    }
}

/// Synthetic JSON-RPC id for the `session/load` the replay constructs itself
/// (when the client only ever sent `session/new`). A string id can never
/// collide with a numeric id the client may have in flight.
const REPLAY_LOAD_REQUEST_ID: &str = "x.ai/leader-replay/session-load";

/// Max silence between two messages from the (re-)connected peer during a
/// replayed request. A `session/load` streams replay notifications
/// continuously once it starts, but the pre-replay phase (MCP resolution,
/// session file reads) can be quiet for a while on large sessions.
const REPLAY_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Overall deadline for one replayed request's response.
const REPLAY_RESPONSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);

/// Outcome of one replayed request (see [`replay_request_until_response`]).
enum ReplayOutcome {
    /// The response for the replayed request arrived with a `result`.
    ResponseOk,
    /// The response arrived but carried an `error` (e.g. session not found).
    ResponseErr,
    /// The connection closed / timed out / stdout broke before the response.
    Failed,
}

/// True when `msg` is the JSON-RPC *response* to the request with `expected_id`
/// (no `method` key + matching `id`).
fn parse_replay_response(msg: &str, expected_id: &serde_json::Value) -> Option<ReplayOutcome> {
    let json = serde_json::from_str::<serde_json::Value>(msg).ok()?;
    if json.get("method").is_some() {
        return None;
    }
    if json.get("id") != Some(expected_id) {
        return None;
    }
    if json.get("error").is_some() {
        Some(ReplayOutcome::ResponseErr)
    } else {
        Some(ReplayOutcome::ResponseOk)
    }
}

/// Send one replayed request to the (new) peer and pump messages until its
/// response arrives.
///
/// `session/load` emits the full replay stream (session/update notifications)
/// BEFORE its response, so "wait for the next message" is not "wait for the
/// response". Everything that is not the response itself is forwarded verbatim
/// to `stdout` — exactly what the pre-reconnect stream would have carried.
/// Only the response to the replayed request is swallowed (the client already
/// received a response for its original send and must not see a duplicate or
/// unknown-id response).
///
/// Returning before the `session/load` response is the root cause of the
/// "unknown session id" failures after a leader crash: the bridge declared
/// the reconnect complete while the new peer was still loading the session,
/// and the client's next `session/prompt` raced (and lost against) the load.
async fn replay_request_until_response(
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    request_json: &str,
    what: &str,
) -> ReplayOutcome {
    use tokio::io::AsyncWriteExt as _;
    let expected_id = serde_json::from_str::<serde_json::Value>(request_json)
        .ok()
        .and_then(|v| v.get("id").cloned());
    if tx.send(request_json.to_string()).is_err() {
        tracing::warn!(what, "replay: failed to send request");
        return ReplayOutcome::Failed;
    }
    let Some(expected_id) = expected_id else {
        return ReplayOutcome::ResponseOk;
    };
    let deadline = tokio::time::Instant::now() + REPLAY_RESPONSE_DEADLINE;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            tracing::warn!(what, "replay: response deadline exceeded");
            return ReplayOutcome::Failed;
        }
        let per_recv = REPLAY_RECV_TIMEOUT.min(deadline - now);
        match tokio::time::timeout(per_recv, rx.recv()).await {
            Ok(Some(msg)) => {
                if let Some(outcome) = parse_replay_response(&msg, &expected_id) {
                    tracing::debug!(
                        what,
                        ok = matches!(outcome, ReplayOutcome::ResponseOk),
                        "replay: response received"
                    );
                    return outcome;
                }
                if stdout.write_all(msg.as_bytes()).await.is_err()
                    || stdout.write_all(b"\n").await.is_err()
                    || stdout.flush().await.is_err()
                {
                    tracing::warn!(what, "replay: stdout closed while forwarding");
                    return ReplayOutcome::Failed;
                }
            }
            Ok(None) => {
                tracing::warn!(what, "replay: peer closed before response");
                return ReplayOutcome::Failed;
            }
            Err(_) => {
                tracing::warn!(what, "replay: timed out waiting for response");
                return ReplayOutcome::Failed;
            }
        }
    }
}

/// Build the `session/load` JSON to replay for one cached session: the
/// verbatim client request when available, else a synthesized load from the
/// captured `session/new` parameters.
fn replay_load_json(sid: &str, cached: &CachedSession) -> Option<String> {
    if let Some(ref verbatim) = cached.load_request_json {
        return Some(verbatim.clone());
    }
    let cwd = cached.cwd.as_deref()?;
    let mut params = serde_json::json!({ "sessionId": sid, "cwd": cwd });
    if let Some(ref mcp_raw) = cached.mcp_servers_json
        && let Ok(mcp_val) = serde_json::from_str::<serde_json::Value>(mcp_raw)
    {
        params["mcpServers"] = mcp_val;
    }
    Some(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": REPLAY_LOAD_REQUEST_ID,
            "method": "session/load",
            "params": params,
        })
        .to_string(),
    )
}

/// Replay cached `initialize` + every cached `session/load` to a freshly
/// (re-)established peer, blocking until the peer has actually finished
/// loading EACH session (loads are sent strictly sequentially, each awaiting
/// its response — the synthesized-id reuse relies on this ordering).
///
/// Returns the primary restored session id (the most recently active one,
/// falling back to any successfully restored session). `None` when there was
/// nothing to replay or every restore failed — callers should signal the
/// client to re-establish state itself in that case.
pub async fn replay_acp_state_after_reconnect(
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    state: &ReplayState,
) -> Option<String> {
    if let Some(ref init_json) = state.initialize_json {
        match replay_request_until_response(tx, rx, stdout, init_json, "initialize").await {
            ReplayOutcome::ResponseOk => {}
            ReplayOutcome::ResponseErr => {
                tracing::warn!("replay: initialize was rejected by new peer");
                return None;
            }
            ReplayOutcome::Failed => return None,
        }
    } else {
        tracing::debug!("replay: no cached initialize; skipping ACP replay");
        return None;
    }
    if state.sessions.is_empty() {
        tracing::debug!("replay: no sessions to replay");
        return None;
    }
    let mut restored: Vec<String> = Vec::new();
    for (sid, cached) in &state.sessions {
        let Some(load_json) = replay_load_json(sid, cached) else {
            tracing::warn!(
                session_id = %sid,
                "replay: no way to rebuild session/load; skipping"
            );
            continue;
        };
        match replay_request_until_response(tx, rx, stdout, &load_json, "session/load").await {
            ReplayOutcome::ResponseOk => {
                tracing::info!(session_id = %sid, "replay: session restored");
                restored.push(sid.clone());
            }
            ReplayOutcome::ResponseErr => {
                tracing::warn!(
                    session_id = %sid,
                    "replay: session/load was rejected by new peer"
                );
            }
            ReplayOutcome::Failed => {
                tracing::warn!(
                    session_id = %sid,
                    "replay: transport failure during session/load; aborting remaining replays"
                );
                break;
            }
        }
    }
    state
        .last_session_id
        .as_ref()
        .filter(|sid| restored.iter().any(|r| r == *sid))
        .cloned()
        .or_else(|| restored.last().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> std::sync::Mutex<ReplayState> {
        std::sync::Mutex::new(ReplayState::default())
    }

    #[test]
    fn cache_initialize_request() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        cache_outgoing_acp_state(msg, &state);
        let s = state.lock().unwrap();
        assert_eq!(s.initialize_json.as_deref(), Some(msg));
    }

    #[test]
    fn cache_session_load_preserves_full_request() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp","mcpServers":[]}}"#;
        cache_outgoing_acp_state(msg, &state);
        let s = state.lock().unwrap();
        let (sid, cached) = &s.sessions[0];
        assert_eq!(sid, "s1");
        assert_eq!(cached.load_request_json.as_deref(), Some(msg));
        assert_eq!(cached.cwd.as_deref(), Some("/tmp"));
        assert!(cached.mcp_servers_json.is_some());
        assert_eq!(s.last_session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn cache_session_new_is_pending_until_response_assigns_id() {
        let state = make_state();
        let load = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#;
        cache_outgoing_acp_state(load, &state);
        let new = r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/home"}}"#;
        cache_outgoing_acp_state(new, &state);
        {
            let s = state.lock().unwrap();
            assert_eq!(s.sessions.len(), 1);
            assert_eq!(s.sessions[0].0, "s1");
            assert!(s.pending_new.is_some());
            assert_eq!(
                s.pending_new.as_ref().unwrap().cwd.as_deref(),
                Some("/home")
            );
        }
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s2"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert!(s.pending_new.is_none());
        assert_eq!(s.sessions.len(), 2);
        assert_eq!(s.sessions[1].0, "s2");
        assert_eq!(s.sessions[1].1.cwd.as_deref(), Some("/home"));
        assert_eq!(s.last_session_id.as_deref(), Some("s2"));
    }

    #[test]
    fn cache_session_close_stops_replaying_it() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"_x.ai/session/close","params":{"sessionId":"s1"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert!(s.sessions.is_empty(), "closed session must not be replayed");
        assert!(s.last_session_id.is_none());
    }

    /// An UNCONFIRMED `session/new` (peer died before its response) must not
    /// be replayed — its id was never assigned — but previously loaded
    /// sessions still restore.
    #[tokio::test]
    async fn replay_after_unconfirmed_session_new_restores_prior_sessions() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        let load_a = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"session-A","cwd":"/old"}}"#;
        cache_outgoing_acp_state(load_a, &state);
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/new"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load = leader_rx.recv().await.unwrap();
            assert!(load.contains("session-A"), "unexpected replay: {load}");
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#.to_string())
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(
            result.as_deref(),
            Some("session-A"),
            "prior session must be restored even though the new one was unconfirmed"
        );
        responder.await.unwrap();
    }

    #[test]
    fn fallback_replay_json_escapes_special_chars() {
        let cached = CachedSession {
            load_request_json: None,
            cwd: Some(r#"C:\Users\test path"#.into()),
            mcp_servers_json: None,
        };
        let json = replay_load_json(r#"session"with"quotes"#, &cached)
            .expect("cwd present → load synthesized");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("fallback replay JSON must be valid");
        assert_eq!(
            parsed["params"]["sessionId"].as_str().unwrap(),
            r#"session"with"quotes"#
        );
        assert_eq!(
            parsed["params"]["cwd"].as_str().unwrap(),
            r#"C:\Users\test path"#
        );
        assert_eq!(parsed["id"].as_str(), Some(REPLAY_LOAD_REQUEST_ID));
    }

    #[test]
    fn cache_incoming_session_id_from_response() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp"}}"#,
            &state,
        );
        let msg = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"abc123"}}"#;
        cache_incoming_session_id(msg, &state);
        let s = state.lock().unwrap();
        assert_eq!(s.last_session_id.as_deref(), Some("abc123"));
        assert_eq!(s.sessions.len(), 1);
        assert_eq!(s.sessions[0].0, "abc123");
    }

    /// A multi-session client (IDE driving several sessions over one bridge)
    /// gets EVERY session replayed after a reconnect, in first-seen order.
    #[tokio::test]
    async fn replay_restores_all_cached_sessions() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-1","cwd":"/a"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/b"}}"#,
            &state,
        );
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"sess-2"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load1 = leader_rx.recv().await.unwrap();
            assert!(load1.contains("sess-1"), "expected sess-1 first: {load1}");
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#.to_string())
                .unwrap();
            let load2 = leader_rx.recv().await.unwrap();
            assert!(load2.contains("sess-2"), "expected sess-2 second: {load2}");
            let load2_json: serde_json::Value = serde_json::from_str(&load2).unwrap();
            assert_eq!(load2_json["id"].as_str(), Some(REPLAY_LOAD_REQUEST_ID));
            response_tx
                .send(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": REPLAY_LOAD_REQUEST_ID,
                        "result": {}
                    })
                    .to_string(),
                )
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(result.as_deref(), Some("sess-2"));
        responder.await.unwrap();
    }

    /// One broken session must not doom the rest: a rejected load is skipped
    /// and the remaining sessions still restore.
    #[tokio::test]
    async fn replay_skips_rejected_session_and_restores_the_rest() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-bad","cwd":"/a"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/load","params":{"sessionId":"sess-good","cwd":"/b"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _bad = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params","data":"unknown session id"}}"#
                        .to_string(),
                )
                .unwrap();
            let good = leader_rx.recv().await.unwrap();
            assert!(good.contains("sess-good"));
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#.to_string())
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(result.as_deref(), Some("sess-good"));
        responder.await.unwrap();
    }

    #[test]
    fn cache_incoming_ignores_non_session_response() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":1,"result":{"models":[]}}"#;
        cache_incoming_session_id(msg, &state);
        let s = state.lock().unwrap();
        assert!(s.last_session_id.is_none());
        assert!(s.sessions.is_empty());
    }

    #[tokio::test]
    async fn replay_with_no_cached_state_returns_none() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let state = ReplayState::default();
        let mut sink = Vec::new();
        let result = replay_acp_state_after_reconnect(&tx, &mut rx, &mut sink, &state).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn replay_sends_initialize_and_session_load() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#.to_string())
                .unwrap();
        });
        let mut rx = response_rx;
        let mut sink = Vec::new();
        let result = replay_acp_state_after_reconnect(&leader_tx, &mut rx, &mut sink, &state).await;
        assert_eq!(result.as_deref(), Some("s1"));
        assert!(
            sink.is_empty(),
            "responses to replayed requests must be swallowed, not forwarded"
        );
        responder.await.unwrap();
    }

    /// Regression test for the post-leader-crash "unknown session id" bug.
    ///
    /// `session/load` streams replay notifications BEFORE its response. The
    /// old drain logic consumed exactly one message per replayed request and
    /// returned — declaring the reconnect complete while the new peer was
    /// still loading the session. The replay must instead:
    ///   1. wait for the actual `session/load` RESPONSE (matched by id),
    ///   2. forward interleaved notifications to the client verbatim,
    ///   3. swallow only the responses to the replayed requests.
    #[tokio::test]
    async fn replay_waits_for_load_response_through_notifications() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":8,"method":"session/load","params":{"sessionId":"s9","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","method":"x.ai/leader/version_mismatch","params":{}}"#
                        .to_string(),
                )
                .unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            for i in 0..3 {
                response_tx
                    .send(format!(
                        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s9","n":{i}}}}}"#
                    ))
                    .unwrap();
            }
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":8,"result":{}}"#.to_string())
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert_eq!(
            result.as_deref(),
            Some("s9"),
            "replay must succeed once the load response arrives"
        );
        let forwarded = String::from_utf8(sink).unwrap();
        let lines: Vec<&str> = forwarded.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "expected exactly the 4 notifications, got: {lines:?}"
        );
        assert!(lines[0].contains("version_mismatch"));
        assert!(lines[1].contains(r#""n":0"#));
        assert!(lines[2].contains(r#""n":1"#));
        assert!(lines[3].contains(r#""n":2"#));
        assert!(!forwarded.contains(r#""id":7"#), "init response leaked");
        assert!(!forwarded.contains(r#""id":8"#), "load response leaked");
        responder.await.unwrap();
    }

    /// A `session/load` rejected by the new peer (error response) must
    /// surface as a failed replay (`None`) so the caller can signal the
    /// client to re-establish state itself.
    #[tokio::test]
    async fn replay_returns_none_when_load_is_rejected() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"gone","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params","data":"unknown session id"}}"#
                        .to_string(),
                )
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert!(result.is_none(), "rejected load must not claim success");
        responder.await.unwrap();
    }

    /// The synthetic fallback `session/load` (client only ever sent
    /// `session/new`) uses a string request id that cannot collide with the
    /// client's numeric ids — and the response matcher honors it.
    #[tokio::test]
    async fn replay_fallback_load_uses_reserved_string_id() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp"}}"#,
            &state_mutex,
        );
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-new"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load = leader_rx.recv().await.unwrap();
            let load_json: serde_json::Value = serde_json::from_str(&load).unwrap();
            assert_eq!(
                load_json["id"].as_str(),
                Some(REPLAY_LOAD_REQUEST_ID),
                "fallback load must use the reserved string id"
            );
            response_tx
                .send(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": REPLAY_LOAD_REQUEST_ID,
                        "result": {}
                    })
                    .to_string(),
                )
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert_eq!(result.as_deref(), Some("s-new"));
        responder.await.unwrap();
    }
}
