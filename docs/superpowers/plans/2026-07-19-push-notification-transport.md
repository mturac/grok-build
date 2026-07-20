# Push Notification Transport — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver agent notifications out-of-band (Web Push to the PWA) so autonomous background work is never invisible — the prerequisite for the CI Guardian loop (separate plan).

**Architecture:** Introduce a `Notifier` trait with a fan-out that dispatches every agent notification to N sinks. Sink 1 is the existing in-band ACP `session/update` path (refactored behind the trait, no behavior change). Sink 2 is a new `WebPushNotifier` that VAPID-signs and sends Web Push messages to PWA subscribers. Push subscriptions live in a **server-level** store (held in `ServerState`, persisted to a file under `$GROK_HOME`) because the axum route handlers that register subscriptions run outside any agent session and need shared access — this refines the spec's "registered resource" wording (§3.1/§4) to a server-global store, since subscriptions are server-wide, not per-session.

**Tech Stack:** Rust, axum/tower/hyper (already in workspace), `jsonwebtoken` + `p256` + `base64` (already in `Cargo.lock`) for VAPID ES256 JWT signing, `hkdf` + a payload-AEAD crate for RFC 8291 (`aes128gcm`) message encryption, vanilla JS PWA (`app.js`/`sw.js`).

## Global Constraints

Copied verbatim from the spec; every task's requirements implicitly include these.

- The agent must NEVER auto-push, auto-merge, or auto-deploy. (Not exercised here, but the notifier must never trigger such actions.)
- Remote clients run with `fs_write`/`terminal` capabilities forced off.
- New push routes reuse the EXACT existing auth: `validate_auth(&headers, &query, &state.secret)` — Bearer header or `?server-key=` query, `constant_time_eq` comparison (`server.rs:105-129`). Never unauthenticated.
- Never log the `gh` token, the VAPID private key, or push subscription secrets (`auth`/`p256dh`). Strip `?server-key=` from any URL before it can be persisted (Cache Storage / history) — follow the PWA's existing rule.
- The **root `Cargo.toml` is generated / read-only** — every new dependency goes in a crate-local `Cargo.toml` (here: `crates/codegen/xai-grok-shell/Cargo.toml`).
- Builds run with `CARGO_INCREMENTAL=0` (disk pressure in this environment); clean `target/debug/incremental` between heavy runs.
- Commit identity is `mturac <345446+mturac@users.noreply.github.com>`. NO AI/Claude attribution in any commit message.

## Dependency finding (verified against `Cargo.lock`)

- **No new crate needed for VAPID signing:** `jsonwebtoken`, `p256`, `ecdsa`, `elliptic-curve`, `base64`, `hkdf` are already resolved in the workspace lockfile. VAPID `Authorization: vapid t=<JWT>, k=<pubkey>` uses an ES256 JWT (`jsonwebtoken` with an EC key) — no new dep.
- **Payload encryption (RFC 8291 `aes128gcm`) MAY need one dep:** `aes-gcm` is **not** currently in `Cargo.lock`. Two options — pick during Task 4:
  - **(A) `web-push` crate** — bundles VAPID + ECE encryption; one well-scoped dep, less hand-rolled crypto. **Recommended** and requires user OK per the no-deps-without-justification rule.
  - **(B) Hand-roll `aes128gcm`** with `aes-gcm` + existing `hkdf`/`p256` — no high-level dep but more crypto surface to test/audit.
  - **MVP escape hatch:** Web Push permits a payload-less push (no encryption); the SW `push` handler then shows a static "grok: activity" message and the PWA fetches details over `/ws`. This ships zero new crypto deps for a first cut. Task 6 implements payload-less first; payload encryption is a follow-up task gated on the dep decision.

> **DECISION NEEDED FROM USER before Task 4:** approve option (A) `web-push` crate, or (B) hand-roll, or (C) ship payload-less MVP first. This plan is written so Tasks 1–3 + 5 + 7 are independent of that choice.

---

### Task 1: `Notifier` trait + fan-out (no behavior change)

**Files:**
- Create: `crates/codegen/xai-grok-shell/src/agent/notify/mod.rs`
- Create: `crates/codegen/xai-grok-shell/src/agent/notify/fanout.rs`
- Modify: `crates/codegen/xai-grok-shell/src/agent/mod.rs` (add `pub mod notify;`)
- Test: inline `#[cfg(test)] mod tests` in `fanout.rs`

**Interfaces:**
- Produces:
  ```rust
  /// A single agent-originated notification to deliver to clients.
  #[derive(Clone, Debug)]
  pub struct AgentNotification {
      pub kind: String,      // stable machine tag, e.g. "scheduled_task_fired"
      pub title: String,     // human summary line
      pub body: String,      // optional detail
      pub meta: serde_json::Value, // structured payload (pr, branch, etc.)
  }

  #[async_trait::async_trait]
  pub trait Notifier: Send + Sync {
      async fn notify(&self, event: &AgentNotification);
  }

  /// Dispatches each event to every registered sink; a failing sink never
  /// blocks the others.
  pub struct FanoutNotifier { sinks: Vec<std::sync::Arc<dyn Notifier>> }
  impl FanoutNotifier {
      pub fn new(sinks: Vec<std::sync::Arc<dyn Notifier>>) -> Self;
  }
  #[async_trait::async_trait]
  impl Notifier for FanoutNotifier { /* iterates sinks, awaits each, swallows per-sink error */ }
  ```
  (`async_trait` is already used across this crate — confirm it is in `crates/codegen/xai-grok-shell/Cargo.toml`; it is a workspace-wide dep, no new dep expected.)

- [ ] **Step 1: Write the failing test** — a recording sink receives every event; a panicking sink does not stop delivery to the others.

```rust
// fanout.rs #[cfg(test)]
#[derive(Default)]
struct RecordingSink { seen: tokio::sync::Mutex<Vec<String>> }
#[async_trait::async_trait]
impl Notifier for RecordingSink {
    async fn notify(&self, e: &AgentNotification) { self.seen.lock().await.push(e.kind.clone()); }
}
struct FailingSink;
#[async_trait::async_trait]
impl Notifier for FailingSink { async fn notify(&self, _e: &AgentNotification) { /* returns () but does nothing */ } }

#[tokio::test]
async fn fanout_delivers_to_all_sinks() {
    let rec = std::sync::Arc::new(RecordingSink::default());
    let fan = FanoutNotifier::new(vec![std::sync::Arc::new(FailingSink), rec.clone()]);
    fan.notify(&AgentNotification { kind: "k1".into(), title: "t".into(), body: "".into(), meta: serde_json::Value::Null }).await;
    assert_eq!(rec.seen.lock().await.as_slice(), &["k1".to_string()]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell fanout_delivers_to_all_sinks -- --nocapture`
Expected: FAIL — `FanoutNotifier`/`Notifier`/`AgentNotification` unresolved.

- [ ] **Step 3: Write minimal implementation** — the trait + struct exactly as in Interfaces; `notify` loops `for s in &self.sinks { s.notify(event).await; }`.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell fanout_delivers_to_all_sinks`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/notify/ crates/codegen/xai-grok-shell/src/agent/mod.rs
git commit -m "feat(agent): add Notifier trait and fan-out dispatcher"
```

---

### Task 2: In-band ACP sink (route existing notifications through the trait)

**Files:**
- Create: `crates/codegen/xai-grok-shell/src/agent/notify/inband.rs`
- Modify: `crates/codegen/xai-grok-shell/src/tools/notification_bridge.rs` — at the points that currently emit `session/update` for `ScheduledTaskFired`/`ScheduledTaskCreated` (grep confirms these live around `notification_bridge.rs:661-849`), ALSO construct an `AgentNotification` and hand it to the injected `Notifier`. **OPEN:** confirm the exact struct that owns the outbound `session/update` sender here and thread an `Arc<dyn Notifier>` into it; if that struct is built in `session/acp_session_impl/spawn.rs`, inject there.
- Test: inline tests in `inband.rs`

**Interfaces:**
- Consumes: `AgentNotification`, `Notifier` (Task 1).
- Produces:
  ```rust
  /// Wraps the existing session/update emission so in-band delivery is just
  /// another Notifier sink. Holds whatever channel the bridge already uses.
  pub struct InbandAcpSink { /* clone of existing session/update tx */ }
  #[async_trait::async_trait]
  impl Notifier for InbandAcpSink { async fn notify(&self, e: &AgentNotification) { /* map to existing SessionUpdate + send */ } }
  ```

- [ ] **Step 1: Write the failing test** — feeding an `AgentNotification` through `InbandAcpSink` produces exactly the same `session/update` JSON the bridge produced before (snapshot the JSON shape from the existing `scheduled_task_fired` test at `notification_bridge.rs` ~line 1627).

```rust
#[tokio::test]
async fn inband_sink_emits_session_update() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = InbandAcpSink::new(tx);
    sink.notify(&AgentNotification { kind: "scheduled_task_fired".into(), title: "fired".into(), body: "".into(), meta: serde_json::json!({"taskId":"abc"}) }).await;
    let msg = rx.recv().await.unwrap();
    assert!(msg.contains("session/update") || msg.contains("scheduled_task_fired"));
}
```
> **OPEN:** the exact channel type/message string must match the existing bridge — read `notification_bridge.rs:661-720` and mirror it; do not invent a new wire format.

- [ ] **Step 2: Run** — Expected FAIL (`InbandAcpSink` unresolved).
- [ ] **Step 3: Implement** — map `AgentNotification.kind` back to the existing `SessionUpdate` variant and send on the existing channel. Keep the pre-existing direct emission working; the sink is additive.
- [ ] **Step 4: Run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell inband_sink_emits_session_update` — Expected PASS. Also run the existing `cargo test -p xai-grok-shell notification_bridge` to prove no regression.
- [ ] **Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/notify/inband.rs crates/codegen/xai-grok-shell/src/tools/notification_bridge.rs
git commit -m "refactor(agent): route in-band notifications through Notifier sink"
```

---

### Task 3: Scheduler durability end-to-end test (close the known gap)

Closes the untested disk→fresh-process→reload→announce path (`scheduler-verify` finding; spec §8). No production code — a test only, but it locks in the persistence guarantee the CI-watch task depends on.

**Files:**
- Test: `crates/codegen/xai-grok-shell/tests/test_scheduler_persistence.rs` (new integration test)

**Interfaces:**
- Consumes: `ResourcesPersistence` (`crates/codegen/xai-grok-tools/src/persistence.rs`), `SchedulerState`/`ScheduledTask` (`.../scheduler/types.rs`), `SchedulerActor` (`.../scheduler/actor.rs`), the registry build path (`registry/types.rs:1068-1192`).

- [ ] **Step 1: Write the failing test** — save a durable task to a temp `resources_state.json`, build a fresh `Resources` via `ResourcesPersistence::load`, spawn `SchedulerActor`, assert a `ScheduledTaskCreated` is announced.

```rust
#[tokio::test]
async fn durable_task_reannounced_after_fresh_load() {
    let tmp = tempfile::tempdir().unwrap();
    let state_path = tmp.path().join("resources_state.json");
    // 1. Seed + save a durable SchedulerState to disk.
    // OPEN: use the SAME ResourcesPersistence::save API the registry uses
    //       (persistence.rs) so the on-disk shape is identical.
    // 2. Fresh Resources; ResourcesPersistence::load(&mut resources) from state_path.
    // 3. Build the notification channel, spawn SchedulerActor with the loaded resources.
    // 4. Assert the first ToolNotification is ScheduledTaskCreated for the seeded task id.
}
```
> **OPEN:** confirm the exact `ResourcesPersistence` constructor + `save`/`load` signatures and how `SchedulerActor` is constructed in tests (mirror `actor.rs:479 announces_existing_tasks_on_startup`, but route through disk instead of an in-memory `Resources`).

- [ ] **Step 2: Run** — Expected FAIL (test unimplemented / assertion fails).
- [ ] **Step 3: Implement** the test body per the OPEN notes.
- [ ] **Step 4: Run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell --test test_scheduler_persistence` — Expected PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/tests/test_scheduler_persistence.rs
git commit -m "test(scheduler): cover durable task reannounce after fresh on-disk load"
```

---

### Task 4: VAPID keypair + config (ES256 signing)

**Files:**
- Create: `crates/codegen/xai-grok-shell/src/agent/notify/vapid.rs`
- Modify: `crates/codegen/xai-grok-shell/Cargo.toml` (only if option A/B from the dependency finding is chosen; VAPID-only signing needs no new dep)
- Test: inline in `vapid.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct VapidKeys { /* p256 secret + public (uncompressed) */ }
  impl VapidKeys {
      /// Load from $GROK_HOME/push/vapid.json, generating + persisting on first use.
      pub fn load_or_generate(grok_home: &std::path::Path) -> anyhow::Result<Self>;
      /// URL-safe base64 (no pad) of the uncompressed public key — served to the PWA.
      pub fn public_key_b64(&self) -> String;
      /// Build the `Authorization: vapid t=<jwt>, k=<pub>` header value for an endpoint origin.
      pub fn sign_auth_header(&self, audience: &str, subject: &str) -> anyhow::Result<String>;
  }
  ```

- [ ] **Step 1: Write the failing test** — a fixed keypair signs a JWT whose header is `{"alg":"ES256","typ":"JWT"}` and whose `aud` claim matches the endpoint origin; `public_key_b64()` round-trips to 65 bytes (uncompressed P-256).

```rust
#[test]
fn vapid_signs_es256_jwt_for_audience() {
    let keys = VapidKeys::generate_for_test();
    let hdr = keys.sign_auth_header("https://fcm.googleapis.com", "mailto:dev@example.com").unwrap();
    assert!(hdr.starts_with("vapid t="));
    assert!(hdr.contains(", k="));
    assert_eq!(base64_url_nopad_decode(&keys.public_key_b64()).len(), 65);
}
```

- [ ] **Step 2: Run** — Expected FAIL.
- [ ] **Step 3: Implement** — generate a `p256::ecdsa::SigningKey`; encode the JWT with `jsonwebtoken` using `Algorithm::ES256` and an EС key built from the secret scalar; claims `{aud, exp: now+12h, sub}`. Persist to `$GROK_HOME/push/vapid.json` (mode 0600). **Never log the secret.**
- [ ] **Step 4: Run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell vapid_signs_es256_jwt_for_audience` — Expected PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/notify/vapid.rs
git commit -m "feat(push): VAPID ES256 keypair generation and JWT auth header"
```

---

### Task 5: Server-level push subscription store (persisted)

**Files:**
- Create: `crates/codegen/xai-grok-shell/src/agent/notify/store.rs`
- Test: inline in `store.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Debug)]
  pub struct PushSubscription { pub endpoint: String, pub p256dh: String, pub auth: String }

  #[derive(Clone)]
  pub struct PushStore { /* Arc<RwLock<Vec<PushSubscription>>> + path */ }
  impl PushStore {
      pub fn load(grok_home: &std::path::Path) -> Self;        // reads $GROK_HOME/push/subscriptions.json
      pub async fn add(&self, sub: PushSubscription);          // dedup by endpoint, persist
      pub async fn remove(&self, endpoint: &str);              // persist
      pub async fn prune(&self, endpoint: &str);               // remove on 410 Gone
      pub async fn all(&self) -> Vec<PushSubscription>;
  }
  ```

- [ ] **Step 1: Write the failing test** — add is idempotent by endpoint; remove/prune drop it; state round-trips through `load` from the same path.

```rust
#[tokio::test]
async fn push_store_dedups_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let s = PushStore::load(tmp.path());
    let sub = PushSubscription { endpoint: "https://e/1".into(), p256dh: "k".into(), auth: "a".into() };
    s.add(sub.clone()).await; s.add(sub.clone()).await;
    assert_eq!(s.all().await.len(), 1);
    let s2 = PushStore::load(tmp.path());          // reload from disk
    assert_eq!(s2.all().await, vec![sub.clone()]);
    s2.prune("https://e/1").await;
    assert_eq!(s2.all().await.len(), 0);
}
```

- [ ] **Step 2: Run** — Expected FAIL.
- [ ] **Step 3: Implement** — `Arc<RwLock<Vec<_>>>` + atomic write to `subscriptions.json` on every mutation.
- [ ] **Step 4: Run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell push_store_dedups_and_persists` — Expected PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/notify/store.rs
git commit -m "feat(push): server-level persisted push subscription store"
```

---

### Task 6: Authenticated push routes on the axum server

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/server.rs` — extend `ServerState` with `push_store: PushStore` and `vapid: Arc<VapidKeys>` (build them in `run_agent_server_on` from `$GROK_HOME`, `server.rs:520-538`); add three routes to the `Router` at `server.rs:527-538`.
- Test: `crates/codegen/xai-grok-shell/tests/test_push_routes.rs` (new; mirror `tests/test_webui_routes.rs`)

**Interfaces:**
- Consumes: `PushStore` (Task 5), `VapidKeys` (Task 4), `validate_auth` (`server.rs:113`).
- Produces routes:
  - `GET  /push/vapid-public-key` → `200 { "publicKey": "<b64>" }` (auth required)
  - `POST /push/subscribe` (body `PushSubscription`) → `204` (auth required)
  - `POST /push/unsubscribe` (body `{ "endpoint": "..." }`) → `204` (auth required)

Handler auth pattern (reuse EXACTLY, since these are plain HTTP not WS upgrades):
```rust
async fn push_subscribe(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<WsQueryParams>,
    Json(sub): Json<PushSubscription>,
) -> Response {
    if !validate_auth(&headers, &query, &state.secret) {
        return (StatusCode::UNAUTHORIZED, "Invalid or missing authorization token").into_response();
    }
    state.push_store.add(sub).await;
    StatusCode::NO_CONTENT.into_response()
}
```
Router additions (after `server.rs:537`):
```rust
        .route("/push/vapid-public-key", get(push_vapid_public_key))
        .route("/push/subscribe", post(push_subscribe))
        .route("/push/unsubscribe", post(push_unsubscribe))
```

- [ ] **Step 1: Write the failing test** — each route returns 401 without a secret and 2xx with the correct Bearer/`?server-key=`; a subscribe then shows up in the store.

```rust
// test_push_routes.rs — bind run_agent_server_on on 127.0.0.1:0, hit routes with reqwest/hyper.
#[tokio::test]
async fn push_subscribe_requires_auth_and_stores() {
    // 401 without secret:
    // POST /push/subscribe  -> 401
    // 204 with ?server-key=<secret>, then GET /push/vapid-public-key returns a base64 key.
}
```
> **OPEN:** copy the exact server-boot + HTTP-client helper from `tests/test_webui_routes.rs` (it already boots `run_agent_server_on` and makes requests); reuse it verbatim.

- [ ] **Step 2: Run** — Expected FAIL (routes 404 / handlers unresolved).
- [ ] **Step 3: Implement** the three handlers + `ServerState` fields + router lines + `use axum::routing::post;`.
- [ ] **Step 4: Run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell --test test_push_routes` — Expected PASS. Re-run `--test test_webui_routes` and `--test test_remote_agent_e2e` to prove `/ws` + hello-frame unchanged.
- [ ] **Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/server.rs crates/codegen/xai-grok-shell/tests/test_push_routes.rs
git commit -m "feat(push): authenticated subscribe/unsubscribe/vapid-public-key routes"
```

---

### Task 7: `WebPushNotifier` sink (payload-less MVP)

Ships the delivery sink WITHOUT the ECE payload-encryption crypto (see dependency finding option C). A payload-less push wakes the SW; the SW shows a static line and the PWA fetches detail over `/ws`. Encrypted payloads are a follow-up task gated on the user's dep decision.

**Files:**
- Create: `crates/codegen/xai-grok-shell/src/agent/notify/webpush.rs`
- Test: inline in `webpush.rs` (unit-test header construction against a mock endpoint)

**Interfaces:**
- Consumes: `Notifier`/`AgentNotification` (Task 1), `VapidKeys` (Task 4), `PushStore` (Task 5).
- Produces:
  ```rust
  pub struct WebPushNotifier { store: PushStore, vapid: std::sync::Arc<VapidKeys>, http: reqwest::Client, subject: String }
  #[async_trait::async_trait]
  impl Notifier for WebPushNotifier {
      async fn notify(&self, _e: &AgentNotification) {
          // for each sub: POST endpoint with VAPID Authorization header,
          // TTL header, no body (payload-less). On 404/410 -> store.prune(endpoint).
      }
  }
  ```
> **OPEN:** confirm an HTTP client is already a dep of `xai-grok-shell` (the remote client uses one for WS — check for `reqwest`/`hyper` client in `Cargo.toml`); reuse it rather than adding a new one.

- [ ] **Step 1: Write the failing test** — for a store with one subscription and a stub endpoint, `notify` sends a request whose `Authorization` header starts with `vapid t=` and, when the endpoint replies `410`, the subscription is pruned.
- [ ] **Step 2: Run** — Expected FAIL.
- [ ] **Step 3: Implement** — iterate `store.all()`, build the VAPID header via `vapid.sign_auth_header(endpoint_origin, &subject)`, POST with a short TTL, prune on 404/410.
- [ ] **Step 4: Run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell webpush` — Expected PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/notify/webpush.rs
git commit -m "feat(push): WebPushNotifier sink (payload-less delivery + 410 prune)"
```

---

### Task 8: Wire the fan-out into the agent + PWA subscribe/receive

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/server.rs` — when the persistent agent thread is spawned (`server.rs:201-217`), build `FanoutNotifier(vec![InbandAcpSink, WebPushNotifier])` from `state.push_store` + `state.vapid` and hand it to the agent/session construction so `notification_bridge` emits through it. **OPEN:** thread the `Arc<dyn Notifier>` from `ServerState` into the session build path (`session/acp_session_impl/spawn.rs`) — same injection point Task 2 identified.
- Modify: `crates/codegen/xai-grok-shell/src/agent/webui/app.js` — after connect, request `Notification.requestPermission()`, `GET /push/vapid-public-key` (with the secret), `PushManager.subscribe({ userVisibleOnly:true, applicationServerKey })`, then `POST /push/subscribe`. Strip `?server-key=` from the address bar first (existing rule).
- Modify: `crates/codegen/xai-grok-shell/src/agent/webui/sw.js` — add `self.addEventListener('push', ...)` (show a notification; use payload if present else a static line) and `notificationclick` (focus/open the PWA).
- Test: `crates/codegen/xai-grok-shell/tests/test_push_routes.rs` (extend) + a JS-shape assertion in `test_webui_routes.rs` that `sw.js` contains a `push` listener and `app.js` references `pushManager`.

**Interfaces:**
- Consumes: everything from Tasks 1–7.

- [ ] **Step 1: Write the failing test** — assert `service_worker()` asset body contains `addEventListener('push'` and `app_js()` body contains `pushManager` (compile-time `include_str!` assets, so a string assertion is sufficient and cheap).

```rust
#[test]
fn sw_and_app_have_push_handlers() {
    assert!(xai_grok_shell::agent::webui::SW_JS.contains("addEventListener('push'"));
    assert!(xai_grok_shell::agent::webui::APP_JS.contains("pushManager"));
}
```
> **OPEN:** confirm the exact `include_str!` const names exported by `webui.rs` (`SW_JS`/`APP_JS` or similar); mirror the existing `all_assets_carry_no_cache_header` test that already references them.

- [ ] **Step 2: Run** — Expected FAIL (assets have no push code yet).
- [ ] **Step 3: Implement** the `app.js`/`sw.js` changes and the notifier wiring in `server.rs`.
- [ ] **Step 4: Run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-shell agent::` plus `--test test_webui_routes --test test_push_routes --test test_remote_agent_e2e`. Expected: all PASS.
- [ ] **Step 5: Manual smoke (documented, not automated)** — `grok agent serve`, open the PWA on a phone, accept notifications, create a `/loop` task with a short interval, background the PWA, confirm an OS notification fires when the task fires. Record the result in the PR description.
- [ ] **Step 6: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/server.rs crates/codegen/xai-grok-shell/src/agent/webui/app.js crates/codegen/xai-grok-shell/src/agent/webui/sw.js crates/codegen/xai-grok-shell/tests/
git commit -m "feat(push): wire fan-out notifier into agent and PWA push subscribe/receive"
```

---

## Self-Review

**Spec coverage (§3.1, §4, §5, §7, §8, §9 steps 1–3):**
- Notifier trait + fan-out → Task 1. In-band sink refactor (no behavior change) → Task 2.
- Web Push VAPID → Task 4; subscription store → Task 5; three authenticated routes → Task 6; `WebPushNotifier` → Task 7; PWA subscribe + `sw.js` receive → Task 8. ✓ §3.1.
- `PushSubscriptions` persistence → Task 5 (refined to server-level store; rationale documented in Architecture). ✓ §4.
- New notification events → carried as `AgentNotification.kind` (Task 1) and mapped in Task 2; the specific CI events (`CiFixReady` etc.) belong to the CI Guardian plan, not this one. ✓ §5 (transport half).
- Security: auth reuse (Task 6), no-secret-logging + `?server-key=` strip (Global Constraints, Tasks 4/8), 410 prune (Tasks 5/7). ✓ §7.
- Tests: VAPID sign (Task 4), store add/remove/prune (Task 5), route auth (Task 6), sw/app handlers (Task 8), scheduler durability gap (Task 3). ✓ §8.
- Rollout steps 1–3 → Tasks 1–2 (step 1), Tasks 4–8 (step 2), Task 3 (step 3). ✓ §9.

**Placeholder scan:** No "TBD/handle-appropriately". Genuine unknowns are marked `OPEN:` (exact channel type in the bridge, exact `ResourcesPersistence` signatures, exact `include_str!` const names, exact HTTP client dep) — these are "confirm against real code before writing", not invented values, per the writing-plans no-fabrication rule.

**Type consistency:** `Notifier`/`AgentNotification`/`FanoutNotifier` (Task 1) reused verbatim in Tasks 2/7/8; `PushStore`/`PushSubscription` (Task 5) reused in Tasks 6/7/8; `VapidKeys` (Task 4) reused in Tasks 6/7. Route paths (`/push/subscribe`, `/push/unsubscribe`, `/push/vapid-public-key`) identical across Tasks 6 and 8.

**Known dependency gate:** Task 4/7 payload encryption decision (web-push crate vs hand-roll vs payload-less MVP) is surfaced for user approval before Task 4; Tasks 1–3, 5, 6, 8 do not depend on it.
