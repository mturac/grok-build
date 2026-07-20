# CI Guardian Loop — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a watched GitHub PR's CI fails, the agent autonomously diagnoses it and, only when confident, prepares a fix on an isolated local branch — then STOPS at the push gate and notifies the human — with a hard PR-level automation budget that prevents runaway.

**Architecture:** A durable scheduler task polls one PR's CI via `gh`. On a *new* failure (keyed by PR + head SHA + failing check), a **bounded, cancellable background job** fetches logs, classifies the failure diagnosis-first, and — only for a confident, localized code failure — writes a fix to a `grok-ci-fix/<pr>-<sha>` branch and commits locally. It never pushes. State lives in a durable `CiGuardState` resource; each fix consumes a per-PR budget that only a human `rearm` refills. All outcomes are delivered through the **already-built** `Notifier` fan-out (in-band ACP + Web Push).

**Tech Stack:** Rust, the existing `SchedulerActor`/`/loop` durable task system, the `Notifier`/`global_notifier()` transport (shipped in the push-notification-transport plan), `gh` CLI via `tokio::process::Command`, `git2` (already a workspace dep) or `git` CLI for branch/commit, `tokio_util::sync::CancellationToken`.

## Global Constraints

- **NEVER auto-push, auto-merge, or auto-deploy.** The fix stops at a local commit on an isolated branch; the human pushes. This is the one invariant that never relaxes.
- Remote clients run with `fs_write`/`terminal` forced off — the CI Guardian controller runs server-side (native tool), not delegated to a remote client.
- Commit identity: `mturac <345446+mturac@users.noreply.github.com>`. NO AI/Claude attribution in any commit message (neither this plan's commits nor the fix commits the feature itself creates).
- Root `Cargo.toml` is generated/read-only — crate-local deps only. No new crate should be needed (`gh` is a subprocess; `git2`, `tokio`, `serde`, `chrono`, `uuid` are present).
- Builds: `CARGO_INCREMENTAL=0`. Note: `xai-grok-shell` compiles in ~13 min; put the controller/state in `xai-grok-tools` where it fits (faster) and keep `xai-grok-shell` touches minimal.
- Never log the `gh` token or repo secrets.
- Anti-runaway is a correctness requirement, not a nicety (see the design spec §3.3).

## Prerequisite (DONE)

The push-notification-transport plan is merged/available: `Notifier`, `AgentNotification`, `FanoutNotifier`, `global_notifier()`, and the `NotificationBridgeConfig.notifier` hook all exist. This plan delivers CI-specific `AgentNotification`s through that same transport — no new delivery mechanism.

## Grounding the implementer must do first (OPEN items to confirm against real code)

- `SchedulerActor` create/delete/list command surface and how a durable task's fired prompt is dispatched (`crates/codegen/xai-grok-tools/src/implementations/grok_build/scheduler/{actor,create,types}.rs`). The CI watch is a durable scheduler task; confirm whether the controller is best driven by a fired *prompt* (agent-invoked) or a native periodic callback.
- The `register_resource!` macro + `ResourcesPersistence` pattern (`.../types/resources.rs`, `persistence.rs`) — `CiGuardState` mirrors `SchedulerState` exactly. The durability test pattern is already established (`scheduler/actor.rs::durable_task_reannounced_after_fresh_on_disk_load`).
- How agent tools are registered and reach `SharedResources` (`.../registry/types.rs`, `bridge.rs`) — the `/ci-guard` tool(s) follow the scheduler tool registration.
- The notifier: emit via the same path the scheduler uses (`notification_bridge.rs` `scheduled_task_fired_notification` is the model) or directly through `global_notifier()`.

---

### Task 1: `CiGuardState` durable resource + `CiWatch` model

**Files:**
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/ci_guard/types.rs`
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/ci_guard/mod.rs`
- Modify: the grok_build implementations `mod.rs` to add `pub mod ci_guard;`
- Test: inline in `types.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct CiWatch {
      pub pr_number: u64,
      pub repo: String,                       // "owner/name"
      pub scheduler_task_id: String,          // the durable /loop task backing this watch
      pub last_head_sha: Option<String>,
      pub last_failing_check: Option<String>,
      pub attempts_used: u32,                 // vs. budget (default 1)
      pub armed: bool,                        // false once budget consumed; true after rearm
      pub watch_active: bool,                 // false after PR merged/closed
  }
  #[derive(Clone, Debug, Default, Serialize, Deserialize)]
  pub struct CiGuardState { pub watches: Vec<CiWatch> }
  // register_resource!("grok_build", "CiGuard", CiGuardState);

  pub const DEFAULT_AUTOMATION_BUDGET: u32 = 1;
  impl CiWatch {
      pub fn new(pr_number: u64, repo: String, scheduler_task_id: String) -> Self; // armed:true, attempts_used:0, watch_active:true
      pub fn budget_available(&self) -> bool;   // armed && attempts_used < DEFAULT_AUTOMATION_BUDGET
      pub fn consume_attempt(&mut self);        // attempts_used += 1; armed = false
      pub fn rearm(&mut self);                  // armed = true (does NOT reset attempts_used history; grants one more)
  }
  ```

- [ ] **Step 1: Write the failing test** — a fresh `CiWatch` has budget; `consume_attempt` removes it (a new head SHA does NOT restore it); `rearm` restores exactly one.

```rust
#[test]
fn budget_is_one_shot_until_rearm() {
    let mut w = CiWatch::new(7, "mturac/grok-build".into(), "task1".into());
    assert!(w.budget_available());
    w.consume_attempt();
    assert!(!w.budget_available(), "one attempt per failure until a human re-arms");
    w.last_head_sha = Some("newsha".into()); // a new failing run must NOT refill
    assert!(!w.budget_available());
    w.rearm();
    assert!(w.budget_available());
}
```

- [ ] **Step 2: Run** — `CARGO_INCREMENTAL=0 cargo test -p xai-grok-tools budget_is_one_shot_until_rearm` — Expected FAIL.
- [ ] **Step 3: Implement** the structs, `register_resource!`, and the methods.
- [ ] **Step 4: Run** — Expected PASS.
- [ ] **Step 5: Commit** — `feat(ci-guard): CiGuardState durable resource with one-shot automation budget`.

---

### Task 2: `gh` preflight (fail-closed) + PR status probe

**Files:**
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/ci_guard/gh.rs`
- Test: inline (unit-test the parsers on canned `gh` JSON; the subprocess itself is integration-only).

**Interfaces:**
- Produces:
  ```rust
  pub enum Preflight { Ok, Blocked(String) }  // reason: "auth" | "repo-access" | "missing-scope"
  pub async fn preflight(repo: &str) -> Preflight;              // runs `gh auth status` + a scoped probe
  pub struct CiStatus { pub head_sha: String, pub state: CiState, pub failing_check: Option<String> }
  pub enum CiState { Pending, Passed, Failed, MergedOrClosed }
  pub async fn probe_pr(repo: &str, pr: u64) -> anyhow::Result<CiStatus>; // `gh pr view` + `gh pr checks --json`
  /// Pure parser over the JSON `gh pr checks --json ...` emits — the unit-tested seam.
  pub(crate) fn parse_check_rollup(json: &str) -> anyhow::Result<CiState>;
  ```

- [ ] **Step 1: Write the failing test** — `parse_check_rollup` maps a rollup with a failing check → `Failed`, all-success → `Passed`, any-pending → `Pending`.

```rust
#[test]
fn parse_rollup_classifies_states() {
    assert!(matches!(parse_check_rollup(r#"[{"state":"SUCCESS"},{"state":"FAILURE"}]"#).unwrap(), CiState::Failed));
    assert!(matches!(parse_check_rollup(r#"[{"state":"SUCCESS"}]"#).unwrap(), CiState::Passed));
    assert!(matches!(parse_check_rollup(r#"[{"state":"IN_PROGRESS"},{"state":"SUCCESS"}]"#).unwrap(), CiState::Pending));
}
```
> **OPEN:** confirm the exact `gh pr checks --json` field names against the installed `gh` version; adjust the serde shape. Keep the parser pure so it stays unit-testable without a network.

- [ ] **Step 2–4:** FAIL → implement (`tokio::process::Command` for the subprocess side; pure parser for the tested side) → PASS.
- [ ] **Step 5: Commit** — `feat(ci-guard): gh preflight (fail-closed) and PR CI status probe`.

---

### Task 3: failure classification (diagnosis-first)

**Files:**
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/ci_guard/classify.rs`
- Test: inline (table tests).

**Interfaces:**
- Produces:
  ```rust
  pub enum FailureClass { Flaky, Infra, Timeout, Permission, Ambiguous, ConfidentCode { failing_tests: Vec<String> } }
  /// Classify from fetched log text + annotations. Only `ConfidentCode` (with
  /// specific failing test/error citations) unlocks the write path; everything
  /// else stops at "cannot auto-fix".
  pub fn classify(log_tail: &str, annotations: &[String]) -> FailureClass;
  ```

- [ ] **Step 1: Write the failing test** — table: a log with a clear `test foo ... FAILED` + assertion → `ConfidentCode`; a network/timeout/"runner lost communication" → `Infra`/`Timeout`; a flaky-retry marker → `Flaky`; a permission/secret error → `Permission`; anything unrecognized → `Ambiguous`.

```rust
#[test]
fn classify_table() {
    assert!(matches!(classify("test math::add ... FAILED\nassertion `left == right`", &[]),
        FailureClass::ConfidentCode { .. }));
    assert!(matches!(classify("Error: The runner has received a shutdown signal", &[]), FailureClass::Infra));
    assert!(matches!(classify("error: failed to run custom build command (network timeout)", &[]), FailureClass::Timeout));
    assert!(matches!(classify("remote: Permission to repo denied", &[]), FailureClass::Permission));
    assert!(matches!(classify("something totally unrecognized", &[]), FailureClass::Ambiguous));
}
```

- [ ] **Step 2–4:** FAIL → implement (conservative: default to `Ambiguous`; only cite `ConfidentCode` when a specific failing test line is present) → PASS.
- [ ] **Step 5: Commit** — `feat(ci-guard): diagnosis-first failure classification`.

---

### Task 4: bounded, cancellable investigation job (mutual exclusion)

**Files:**
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/ci_guard/job.rs`
- Test: inline.

**Interfaces:**
- Consumes: `gh` (Task 2), `classify` (Task 3).
- Produces:
  ```rust
  pub enum JobState { Queued, Running, Cancelled, TimedOut, Done }
  pub struct InvestigationJob { /* CancellationToken, timeout, a single-permit lock */ }
  impl InvestigationJob {
      /// Runs at most ONE investigation at a time (server-wide) so a hung log
      /// download can never block the persistent agent. Emits JobState via the notifier.
      pub async fn run(repo: &str, pr: u64, cancel: CancellationToken, timeout: Duration) -> JobState;
  }
  ```
  Uses a `tokio::sync::Semaphore(1)` (or `try_lock`) for mutual exclusion; wraps the whole run in `tokio::time::timeout`; checks `cancel` between steps.

- [ ] **Step 1: Write the failing test** — a job whose body sleeps past the timeout resolves to `TimedOut`; a second concurrent `run` while one holds the permit does not run inline (returns `Queued`/waits, asserted via ordering); a cancelled token yields `Cancelled`.
- [ ] **Step 2–4:** FAIL → implement with a static `Semaphore` + `timeout` + token checks → PASS.
- [ ] **Step 5: Commit** — `feat(ci-guard): bounded cancellable investigation job with mutual exclusion`.

---

### Task 5: fix application on an isolated branch (clean-worktree, stale-write guards)

**Files:**
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/ci_guard/fix.rs`
- Test: inline (drive against a throwaway temp git repo).

**Interfaces:**
- Produces:
  ```rust
  pub enum FixOutcome { Prepared { branch: String, diff_summary: String }, RefusedDirtyWorktree, StaleHeadAborted, NoFix }
  /// Requires a clean worktree; creates `grok-ci-fix/<pr>-<short_sha>`; applies the
  /// patch; commits locally. NEVER pushes. Aborts if the PR head moved since
  /// diagnosis (stale). Persists the attempt to CiGuardState BEFORE committing.
  pub async fn apply_fix(repo_path: &Path, pr: u64, diagnosed_head: &str, patch: &Patch) -> anyhow::Result<FixOutcome>;
  ```

- [ ] **Step 1: Write the failing test** — with a dirty worktree → `RefusedDirtyWorktree` (nothing committed); with a clean worktree + a matching head → a new `grok-ci-fix/...` branch exists with exactly one commit and the working tree of the base branch is untouched; if `diagnosed_head` != current head → `StaleHeadAborted`.
> **OPEN:** decide `git2` vs `git` CLI for branch/commit; match whatever the repo already uses elsewhere (grep for `git2::Repository`). Keep "never push" structural — this function has no push path at all.
- [ ] **Step 2–4:** FAIL → implement (clean-check → branch → apply → commit; persist-before-commit ordering) → PASS.
- [ ] **Step 5: Commit** — `feat(ci-guard): prepare fix on isolated branch with clean-worktree and stale-head guards`.

---

### Task 6: CI-specific notifications

**Files:**
- Modify: `crates/codegen/xai-grok-tools/src/notification/types.rs` (add variants) and the bridge mapping in `crates/codegen/xai-grok-shell/src/tools/notification_bridge.rs` (map to `AgentNotification`, mirroring `scheduled_task_fired_notification`).
- Test: inline pure-mapping tests (like Task 2 of the push plan).

**Interfaces:**
- Produces notification variants / `AgentNotification.kind`s: `ci_watch_started`, `ci_fix_ready { pr, branch, diff_summary }`, `ci_cannot_autofix { pr, reason }`, `ci_blocked { pr, reason }`, `ci_job_state { pr, state }`. Each maps to an `AgentNotification` and rides the existing fan-out (in-band + Web Push).

- [ ] **Steps:** failing pure-mapping test → implement mapping helpers → PASS → commit `feat(ci-guard): CI guardian notification events over the existing fan-out`.

---

### Task 7: `/ci-guard` tool surface + controller wiring (start/stop/status/rearm, budget, auto-cleanup)

**Files:**
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/ci_guard/tool.rs` (the agent tool(s), registered like the scheduler tools in `registry/types.rs`).
- Create: `crates/codegen/xai-grok-tools/src/implementations/grok_build/ci_guard/controller.rs` (ties preflight → probe → key-by (pr,sha,check) → budget check → job → classify → fix → notify; deletes the watch on `MergedOrClosed`).
- Modify: `registry/types.rs` to register the tool(s).
- Test: inline controller tests with the `gh` calls stubbed behind a trait so the decision logic (new-failure detection, budget consumption, auto-cleanup, stale-head abort) is unit-tested without a network.

**Interfaces:**
- `/ci-guard <pr>` (start: creates the durable scheduler watch task + a `CiWatch`), `/ci-guard stop <pr>`, `/ci-guard status`, `/ci-guard rearm <pr>`.
- The controller is invoked on each watch tick. It:
  1. `preflight` → on `Blocked` emit `ci_blocked` and pause.
  2. `probe_pr`; on `MergedOrClosed` delete the watch task + mark `watch_active=false`.
  3. On `Failed` with a NEW `(pr, head_sha, failing_check)` AND `budget_available()`: run the bounded job → classify → (ConfidentCode) `apply_fix` → `consume_attempt` → emit `ci_fix_ready`; (else) emit `ci_cannot_autofix`.

- [ ] **Steps (larger task — split if a reviewer would reject one half):** failing tests for (a) new-failure keyed dedup fires once, (b) budget consumed → no second fix until `rearm`, (c) a new head SHA does NOT auto-refill, (d) `MergedOrClosed` auto-deletes the watch, (e) `Blocked` preflight pauses without a fix. Implement controller + tool registration. PASS. Commit `feat(ci-guard): /ci-guard tool and controller with budget, keyed dedup, and auto-cleanup`.

---

### Task 8: docs + end-to-end smoke (mock `gh`)

**Files:**
- Create: `crates/codegen/xai-grok-tools/tests/test_ci_guard_e2e.rs` (drive the controller with a stub `gh` that flips a PR failed→a new run; assert exactly one `ci_fix_ready`, and that a second failing run on a new SHA yields NO fix without `rearm`, then `rearm` allows one).
- Modify: `crates/codegen/xai-grok-pager/docs/user-guide/15-agent-mode.md` — document `/ci-guard`, the safety model (never pushes; one attempt per PR until re-arm), and the notification outcomes.

- [ ] **Steps:** e2e test (mock gh) → PASS → docs → commit `feat(ci-guard): end-to-end guard test and user-guide docs`.

---

## Self-Review

**Spec coverage (design spec §3.2 CI Guardian loop, §3.3 anti-runaway, §5 events, §6 error handling, §8 CiGuard tests):**
- Watch lifecycle + scheduler-backed polling → Tasks 1, 7. Preflight fail-closed → Task 2, 7. Bounded/cancellable investigation → Task 4. Diagnosis-first classification → Task 3. Fix-on-branch + clean-worktree + stale-head + persist-before-commit → Task 5. PR-level budget + rearm + head-SHA binding (anti-runaway §3.3) → Tasks 1, 7. Notifications → Task 6. Auto-cleanup on merge/close → Task 7. Tests incl. budget/dedup/stale/preflight → Tasks 1–8.
- Never-auto-push invariant: structural — `fix.rs` has no push path; the whole design stops at a local commit. ✓

**Placeholder scan:** genuine unknowns are marked `OPEN:` (exact `gh --json` fields, git2-vs-CLI, scheduler drive mechanism) — confirm against real code before writing, do not fabricate.

**Type consistency:** `CiWatch`/`CiGuardState` (Task 1) reused in Task 7; `CiStatus`/`CiState` (Task 2) consumed by Task 7; `FailureClass` (Task 3) gates Task 5's write; notification kinds (Task 6) emitted by Task 7.

**Scope:** deferred per spec — webhooks, multi-PR fan-out, GitHub auto-comment, retry chains, multi-agent. Not in this plan.

## Execution note

This feature performs local git operations and shells out to `gh`. Recommend implementing under supervision (or at least a first review of Task 5's git path) rather than fully unattended, given it writes commits — even though it never pushes.
