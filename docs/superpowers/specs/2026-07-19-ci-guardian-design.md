# CI Guardian — Closed-Loop CI Auto-Fix with Push Notifications

- **Status:** Approved design (pre-implementation)
- **Date:** 2026-07-19
- **Author:** mturac
- **Supersedes:** the separately-scoped "sub-project 2 (CI auto-fix)" and "sub-project 4 (push notifications)" — merged into one closed-loop spec per council ruling.

## 1. Background & Motivation

`grok-build` is an open-source Rust terminal AI coding agent (ratatui TUI + ACP
JSON-RPC runtime). Already shipped:

- `grok agent serve` — a WebSocket server exposing the agent over ACP (Bearer
  auth), one persistent `MvpAgent` process surviving reconnects. **No inbound
  HTTP other than the WS upgrade + the PWA static routes.**
- A `--remote` TUI client and a mobile PWA client that both attach to it.
- A complete scheduler (`SchedulerActor` + `/loop`): recurring/one-shot prompt
  injection on an interval, durable across sessions via `SchedulerState` +
  `ResourcesPersistence`.

**Hard invariant (never relaxed):** the agent must NEVER auto-push, auto-merge,
or auto-deploy. Every git push/merge is surfaced to the human for explicit
approval. Remote clients run with `fs_write`/`terminal` capabilities forced off.

### Why these two features are one spec

A council deliberation (Kimi = builder, GLM = reviewer, Codex CLI = adversarial
QA) ruled **unanimously** that push notifications are a *prerequisite* for a
CI auto-fix loop, not an independent follow-up: invisible autonomous background
work is unsafe — a fix that sits silently in a local branch is a surprise, not
an automation. Codex's verdict was explicit: build the auto-fix loop *only after*
notifications, cancellation/status UX, auth preflight, bounded background
execution, and re-arm semantics exist — otherwise defer. We therefore ship them
as one closed loop:

> CI fails → agent investigates (bounded) → fix prepared on a local branch →
> **user is notified** "fix ready for review in branch X" → user approves push →
> CI re-runs.

### Grounding facts (verified by desk-check, `scheduler-verify`)

- Durable scheduler state persists at
  `$GROK_HOME/sessions/<encoded_cwd>/<session_id>/resources_state.json`, keyed by
  cwd + session id, on disk, independent of process lifetime.
- `persistence.load()` runs before `SchedulerActor` is spawned on every session
  construction, so durable tasks survive a full `grok agent serve` process
  restart and are re-announced on reconnect. **Gap:** no automated test exercises
  the disk→fresh-process→reload→announce path today (see §8).
- The scheduler tool is reachable via remote/PWA clients — `force_remote_capabilities`
  only zeroes `terminal`/`fs_read`/`fs_write`; there is no tool allowlist gating,
  and the scheduler needs neither fs nor terminal.

## 2. Goals / Non-Goals

### Goals

1. Deliver agent notifications out-of-band (Web Push to the PWA) so autonomous
   background work is never invisible.
2. Watch a single configured GitHub PR's CI via the existing scheduler (polling,
   no new inbound HTTP).
3. On a *confident, localized* CI failure, prepare a fix on an isolated local
   branch and stop at the push gate — with a strong anti-runaway budget.
4. Be safe by construction: fail-closed preflight, bounded/cancellable
   investigation, one attempt per PR until a human re-arms.

### Non-Goals (explicitly deferred)

- GitHub webhooks / any new public inbound HTTP ingress.
- Multi-PR fan-out beyond a small handful of independent watchers.
- Auto-commenting on the GitHub PR.
- Retry chains / self-iterating fixes.
- Multi-agent orchestration.
- Auto-push / auto-merge (permanently out of scope — hard invariant).

## 3. Architecture

Two subsystems sharing one notification channel.

```
                       ┌─────────────────────────── grok agent serve (one process) ──────────────────────────┐
                       │                                                                                       │
  GitHub API  ◀── poll │  SchedulerActor ──fires──▶ CiGuard watch prompt ──▶ CiGuard controller               │
  (gh/gh api)          │      ▲  (durable)                                        │                            │
                       │      │                                                   ├─ preflight (fail-closed)   │
                       │  CiGuardState (durable resource)                         ├─ bounded background job    │
                       │      │                                                   │     ├─ fetch logs          │
                       │      │                                                   │     ├─ classify (diag-1st) │
                       │      │                                                   │     └─ fix on branch       │
                       │      ▼                                                   ▼                            │
                       │  Notification bridge ──▶ Notifier fan-out ──┬─ in-band ACP session/update ──▶ TUI/PWA │
                       │                                             └─ out-of-band PushNotifier ──▶ Web Push   │
                       └───────────────────────────────────────────────────────────────────────────────────┬─┘
                                                                                                             │
                                                                             VAPID-signed push ─────────────┘──▶ PWA service worker → OS notification
```

### 3.1 Subsystem A — Push Notification transport (the prerequisite)

- **`Notifier` trait** (new): `notify(event: &AgentNotification) -> Result<()>`.
  A fan-out `Notifier` dispatches every event to all registered sinks.
- **Sinks (MVP):**
  1. **In-band ACP sink** — reuses the existing `notification_bridge` →
     `session/update` path so a *connected* TUI/PWA sees events live. (Largely
     exists; we route the new event variants through it.)
  2. **`WebPushNotifier`** — out-of-band delivery to PWA subscribers via the Web
     Push protocol (VAPID). Chosen for MVP because the PWA already ships a
     service worker (`sw.js`); adding a `push` event handler is incremental.
- **Extensibility:** the trait lets a `CommandNotifier` (shell out to
  `ntfy`/`terminal-notifier`) be added later with no core change. Not in MVP.
- **Subscription storage:** a new durable registered resource
  `PushSubscriptions { subs: Vec<PushSubscription> }` (endpoint + p256dh + auth
  keys). Subscription registration is a new **authenticated** route on
  `grok agent serve` (Bearer/`?server-key=`), same auth as `/ws` — never
  unauthenticated.
- **New routes on the axum router** (alongside `/ws` and the PWA static routes):
  - `POST /push/subscribe` (auth required) — store a `PushSubscription`.
  - `POST /push/unsubscribe` (auth required) — remove one.
  - `GET  /push/vapid-public-key` (auth required) — hand the PWA the public key
    for `PushManager.subscribe`.
- **PWA changes:** `app.js` requests notification permission + subscribes via
  `PushManager`; `sw.js` gains a `push` listener that renders the notification
  and a `notificationclick` handler that focuses/opens the PWA at the relevant
  PR/branch view.

### 3.2 Subsystem B — CI Guardian loop (auto-fix on the scheduler)

- **Entry:** a slash flow / agent tool `/ci-guard`:
  - `/ci-guard <pr>` — start watching a PR (creates a durable scheduler task).
  - `/ci-guard rearm <pr>` — restore one automation attempt after a human review.
  - `/ci-guard stop <pr>` — stop watching (also auto-stops on PR merge/close).
  - `/ci-guard status` — list active watches + budgets.
- **Watch mechanism:** `/ci-guard <pr>` creates a **durable** `SchedulerActor`
  task (interval ~5m, min enforced 60s) whose fired prompt invokes the CiGuard
  controller. When the PR transitions to `merged`/`closed`, the controller
  deletes its own task (no zombie watchers).
- **Preflight (fail-closed, every fire):** verify `gh auth status`, repo access,
  and the required `checks:read` + `actions:read` scopes. On failure → emit a
  `Blocked{reason:"auth"}` notification and **pause** (do not silently look like
  "CI pending").
- **Failure detection key:** `(pr_number, head_sha, failing_check_id)`. A new
  failure is only "new" if this tuple changes.
- **Bounded background investigation:** on a new failure, spawn a background job
  with a `CancellationToken`, a wall-clock timeout, and **mutual exclusion**
  (only one CiGuard investigation runs at a time). It must NOT run inline on the
  persistent agent's request loop — a hung log download or test run must never
  freeze the TUI/ACP service. The job surfaces `queued`/`running`/`cancelled`
  state through notifications.
- **Diagnosis-first classification:** fetch workflow-run logs + annotations
  (`gh run view --log`, `gh api .../check-runs`). Classify:
  - `Flaky | Infra | Timeout | Permission | Ambiguous` → **stop**, notify
    `CannotAutoFix{reason}`. No writes.
  - `Confident + Localized` (cites specific failing test names + error lines) →
    proceed to fix.
- **Fix (local only):**
  1. Require a **clean worktree** (refuse if dirty — never commit unrelated user
     changes).
  2. Create isolated branch `grok-ci-fix/<pr>-<short_sha>`.
  3. Apply the fix, `git commit` locally. **Never push.**
  4. **Persist attempt state to `CiGuardState` BEFORE the commit side-effect** so
     a crash cannot silently duplicate a commit on restart.
- **Gate:** emit `FixReady{pr, branch, diff_summary}` (in-band + push). STOP.

### 3.3 Anti-runaway design (the core safety contribution)

Codex's key catch: "one attempt per check-run-id" does **not** stop runaway —
after a human pushes the prepared fix, the new commit produces a *new* run id;
another failure would start another cycle → an implicit infinite chain mediated
only by human pushes. Mitigations:

- **PR-level automation budget**, default **1 attempt**, consumed until a human
  runs `/ci-guard rearm <pr>`. A new `head_sha` does **not** auto-refill the
  budget.
- Bind each attempt to `pr + head_sha + failing_check`, not run id alone
  (reruns can mint new run ids with no code change).
- **Refuse stale writes:** if the PR head SHA changed during diagnosis, abort the
  write (the diagnosis is against stale code).
- Treat `Flaky | Infra | Timeout | Permission | Ambiguous` as non-fixable by
  default.

## 4. Data Model

New durable registered resource (mirrors `SchedulerState` registration pattern):

```rust
// register_resource!("grok_build", "CiGuard", CiGuardState);
struct CiGuardState { watches: Vec<CiWatch> }

struct CiWatch {
    pr_number: u64,
    repo: String,               // owner/name
    scheduler_task_id: String,  // the durable /loop task backing this watch
    last_head_sha: Option<String>,
    last_failing_check: Option<String>,
    attempts_used: u32,         // vs. budget (default budget = 1)
    armed: bool,                // false once budget consumed; true after rearm
    watch_active: bool,         // false after PR merged/closed
}
```

`PushSubscriptions { subs: Vec<PushSubscription> }` — separate durable resource.

Both persist through the same `ResourcesPersistence` path already used by
`SchedulerState`, so they inherit the verified process-restart durability.

## 5. New Agent Notification Events

Extend the notification enum (routed through `notification_bridge`):

- `CiWatchStarted { pr, interval }`
- `CiFixReady { pr, branch, diff_summary }`
- `CiCannotAutoFix { pr, reason }`
- `CiBlocked { pr, reason }`            // e.g. auth preflight failed
- `CiJobState { pr, state }`            // queued | running | cancelled | timed_out

Each event is delivered to every `Notifier` sink (in-band + Web Push).

## 6. Error Handling

| Condition | Behavior |
|---|---|
| `gh` auth / scope preflight fails | `CiBlocked{reason:"auth"}`, pause watch (no fix) |
| Log/annotation fetch fails (expired, fork, private) | `CiCannotAutoFix{reason:"logs-unavailable"}` |
| Background job exceeds timeout | cancel via token, `CiJobState{cancelled}` + `CiCannotAutoFix{reason:"timeout"}` |
| PR head moved during diagnosis | abort write, `CiCannotAutoFix{reason:"head-moved"}` |
| Worktree dirty | refuse, `CiBlocked{reason:"dirty-worktree"}` |
| Budget exhausted (`armed=false`) | skip fix, no notification spam (at most one "needs re-arm") |
| Any push/merge attempt | blocked by existing hard invariant |
| Web Push delivery fails (410 Gone) | prune that subscription from `PushSubscriptions` |

## 7. Security Considerations

- Push subscription routes require the same Bearer/`?server-key=` auth as `/ws`.
- VAPID private key stored in `$GROK_HOME` config, never in the repo, never sent
  to clients (only the public key is served).
- Never log the `gh` token, the VAPID private key, or push `auth`/`p256dh` secrets.
- `?server-key=` handling in push routes follows the PWA's existing rule: strip
  the secret from any URL before it can be persisted (Cache Storage / history).
- The fix branch is local only; the hard no-push invariant is the backstop.

## 8. Testing Strategy

**Push transport**
- VAPID signing unit test (deterministic keypair → known JWT header/claims).
- `PushSubscriptions` add/remove/prune (410) unit tests.
- `sw.js` `push` + `notificationclick` handler tests.
- Route tests: subscribe/unsubscribe/vapid-public-key all 401 without secret;
  succeed with secret; hello-frame / existing `/ws` behavior unchanged.

**CiGuard**
- Failure-classification table tests: flaky / infra / timeout / permission /
  ambiguous / confident-localized → correct action.
- Head-SHA dedup: same `(pr,sha,check)` fires once.
- Budget: consumed → no further fix until `rearm`; new SHA does NOT refill.
- Stale-write abort when head moves mid-diagnosis.
- Preflight fail-closed → `CiBlocked`, watch paused.
- Bounded job: timeout cancels, never blocks a concurrent ACP request
  (mutual-exclusion + cancellation asserted).
- Auto-cleanup: PR merged/closed → watch task deleted.

**Scheduler durability (fold in the gap `scheduler-verify` found)**
- End-to-end: save `SchedulerState`/`CiGuardState` to disk → construct a fresh
  `Resources` via `ResourcesPersistence::load` → spawn `SchedulerActor` →
  assert `ScheduledTaskCreated` re-announced. Closes the untested
  disk→fresh-process→reload→announce path we now depend on.

**E2E (mock `gh`)**
- Simulate a failed check → exactly one `CiFixReady` with a local branch.
- Simulate a second failing run on a new SHA → NO auto-fix without `rearm`.
- `rearm` → one more attempt allowed.

## 9. Rollout / Sequencing

1. Notifier trait + in-band sink refactor (no behavior change).
2. Web Push: VAPID + subscription resource + routes + PWA subscribe + `sw.js`.
3. Scheduler durability test (closes the known gap before building on it).
4. `CiGuardState` + `/ci-guard` watch lifecycle (start/stop/status, auto-cleanup).
5. Preflight + polling + failure detection (notify only, no fix yet).
6. Bounded background investigation + diagnosis-first classification.
7. Fix-on-branch + budget/rearm + stale-write guard + `CiFixReady` gate.

Each step ships green tests before the next. Steps 1–3 are independently
valuable (notifications work on their own) — if we stop after step 3 we still
have shipped the prerequisite.

## 10. Open Questions

- Interval default: fixed 5m for MVP, or user-configurable per watch? (Lean: 5m
  fixed for MVP, configurable later.)
- Where does the CiGuard controller logic live — a native tool (like the
  scheduler tools) driven by the fired prompt, or agent-prompt-driven with the
  agent calling `gh` via its own tools under a bounded sub-session? (Lean:
  native controller for the deterministic parts — preflight, keying, budget,
  branch/commit — and the agent only for the diagnosis+patch reasoning, run in a
  bounded background sub-session.)
