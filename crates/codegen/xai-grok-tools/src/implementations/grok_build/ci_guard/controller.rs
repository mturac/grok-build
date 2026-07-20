//! CI Guardian controller: the per-tick decision logic that ties preflight →
//! probe → new-failure dedup → budget → investigate/fix → notify together, with
//! the anti-runaway guarantees (design spec §3.3).
//!
//! External effects (gh probe, investigation+fix, notification, state persist,
//! watch teardown) are behind [`CiEffects`] so the decision logic is unit-tested
//! deterministically without a network, `gh`, or git.

use async_trait::async_trait;

use super::gh::{CiState, CiStatus, Preflight};
use super::types::{CiGuardState, CiWatch};

/// Outcome of an investigation+fix attempt, ready to notify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvestigateOutcome {
    FixPrepared { branch: String, diff_summary: String },
    CannotAutofix { reason: String },
}

/// The side effects the controller drives. Real impls call gh/git/the notifier;
/// tests stub them.
#[async_trait]
pub trait CiEffects: Send + Sync {
    async fn preflight(&self, repo: &str) -> Preflight;
    async fn probe(&self, repo: &str, pr: u64) -> anyhow::Result<CiStatus>;
    /// Run the bounded investigation and (if a confident code fix is found)
    /// prepare it on a branch. Called only after the budget is spent + persisted.
    async fn investigate_and_fix(&self, watch: &CiWatch, status: &CiStatus)
    -> InvestigateOutcome;
    /// Emit a CI Guardian event (in-band + push).
    async fn notify(&self, pr: u64, event: CiEvent);
    /// Persist the whole guard state (crash-safety before fix side effects).
    async fn persist(&self, state: &CiGuardState);
    /// Tear down the watch's scheduler task (PR merged/closed).
    async fn stop_watch(&self, watch: &CiWatch);
}

/// The subset of `CiGuardEvent` the controller emits, decoupled from the shell's
/// notification type (the effect impl maps this to the wire event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiEvent {
    Blocked { reason: String },
    CannotAutofix { reason: String },
    FixReady { branch: String, diff_summary: String },
}

/// Run one poll tick for a single watched PR. Mutates `state` (budget, dedup
/// keys, watch_active) and drives `effects`. Idempotent per unchanged failure:
/// the same (head_sha, failing_check) is acted on at most once.
pub async fn run_tick<E: CiEffects>(effects: &E, state: &mut CiGuardState, repo: &str, pr: u64) {
    let Some(idx) = state
        .watches
        .iter()
        .position(|w| w.pr_number == pr && w.repo == repo && w.watch_active)
    else {
        return; // no active watch for this PR
    };

    // 1. Fail-closed preflight.
    if let Preflight::Blocked(reason) = effects.preflight(repo).await {
        effects.notify(pr, CiEvent::Blocked { reason }).await;
        return;
    }

    // 2. Probe CI.
    let status = match effects.probe(repo, pr).await {
        Ok(s) => s,
        Err(e) => {
            effects
                .notify(
                    pr,
                    CiEvent::CannotAutofix {
                        reason: format!("probe failed: {e}"),
                    },
                )
                .await;
            return;
        }
    };

    match status.state {
        CiState::MergedOrClosed => {
            let watch = {
                let w = &mut state.watches[idx];
                w.watch_active = false;
                w.clone()
            };
            effects.persist(state).await;
            effects.stop_watch(&watch).await;
            return;
        }
        CiState::Passed | CiState::Pending => return,
        CiState::Failed => {}
    }

    // 3. New failure? Dedup on (head_sha, failing_check).
    let is_new = {
        let w = &state.watches[idx];
        w.last_head_sha.as_deref() != Some(status.head_sha.as_str())
            || w.last_failing_check.as_deref() != status.failing_check.as_deref()
    };
    if !is_new {
        return; // already handled this exact failure
    }

    // 4. Budget gate. Record the failure identity either way so we don't
    //    re-notify the same failure every poll.
    if !state.watches[idx].budget_available() {
        {
            let w = &mut state.watches[idx];
            w.last_head_sha = Some(status.head_sha.clone());
            w.last_failing_check = status.failing_check.clone();
        }
        effects.persist(state).await;
        effects
            .notify(
                pr,
                CiEvent::CannotAutofix {
                    reason: "automation budget spent — run `/ci-guard rearm` to allow another fix"
                        .into(),
                },
            )
            .await;
        return;
    }

    // 5. Spend the budget and PERSIST before any fix side effect (a crash after
    //    the commit must not let a restart repeat the attempt).
    {
        let w = &mut state.watches[idx];
        w.consume_attempt();
        w.last_head_sha = Some(status.head_sha.clone());
        w.last_failing_check = status.failing_check.clone();
    }
    effects.persist(state).await;

    // 6. Investigate + fix, then notify the outcome.
    let watch = state.watches[idx].clone();
    let event = match effects.investigate_and_fix(&watch, &status).await {
        InvestigateOutcome::FixPrepared {
            branch,
            diff_summary,
        } => CiEvent::FixReady {
            branch,
            diff_summary,
        },
        InvestigateOutcome::CannotAutofix { reason } => CiEvent::CannotAutofix { reason },
    };
    effects.notify(pr, event).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        notifications: Mutex<Vec<(u64, CiEvent)>>,
        persists: Mutex<u32>,
        stopped: Mutex<Vec<u64>>,
        investigated: Mutex<u32>,
    }

    struct StubEffects {
        preflight: Preflight,
        status: CiStatus,
        fix: InvestigateOutcome,
        rec: std::sync::Arc<Recorder>,
    }

    #[async_trait]
    impl CiEffects for StubEffects {
        async fn preflight(&self, _repo: &str) -> Preflight {
            self.preflight.clone()
        }
        async fn probe(&self, _repo: &str, _pr: u64) -> anyhow::Result<CiStatus> {
            Ok(self.status.clone())
        }
        async fn investigate_and_fix(
            &self,
            _watch: &CiWatch,
            _status: &CiStatus,
        ) -> InvestigateOutcome {
            *self.rec.investigated.lock().unwrap() += 1;
            self.fix.clone()
        }
        async fn notify(&self, pr: u64, event: CiEvent) {
            self.rec.notifications.lock().unwrap().push((pr, event));
        }
        async fn persist(&self, _state: &CiGuardState) {
            *self.rec.persists.lock().unwrap() += 1;
        }
        async fn stop_watch(&self, watch: &CiWatch) {
            self.rec.stopped.lock().unwrap().push(watch.pr_number);
        }
    }

    fn failed_status(sha: &str) -> CiStatus {
        CiStatus {
            head_sha: sha.into(),
            state: CiState::Failed,
            failing_check: Some("unit-tests".into()),
        }
    }

    fn state_with_watch() -> CiGuardState {
        let mut s = CiGuardState::default();
        s.watches
            .push(CiWatch::new(7, "owner/repo".into(), "task1".into()));
        s
    }

    fn stub(preflight: Preflight, status: CiStatus, fix: InvestigateOutcome) -> StubEffects {
        StubEffects {
            preflight,
            status,
            fix,
            rec: std::sync::Arc::new(Recorder::default()),
        }
    }

    #[tokio::test]
    async fn blocked_preflight_notifies_and_pauses() {
        let e = stub(
            Preflight::Blocked("auth".into()),
            failed_status("sha1"),
            InvestigateOutcome::CannotAutofix { reason: "x".into() },
        );
        let mut st = state_with_watch();
        run_tick(&e, &mut st, "owner/repo", 7).await;
        let n = e.rec.notifications.lock().unwrap();
        assert_eq!(n.len(), 1);
        assert!(matches!(n[0].1, CiEvent::Blocked { .. }));
        assert_eq!(*e.rec.investigated.lock().unwrap(), 0, "must not investigate");
    }

    #[tokio::test]
    async fn merged_pr_stops_watch() {
        let e = stub(
            Preflight::Ok,
            CiStatus {
                head_sha: "sha1".into(),
                state: CiState::MergedOrClosed,
                failing_check: None,
            },
            InvestigateOutcome::CannotAutofix { reason: "x".into() },
        );
        let mut st = state_with_watch();
        run_tick(&e, &mut st, "owner/repo", 7).await;
        assert!(!st.watch("owner/repo", 7).unwrap().watch_active);
        assert_eq!(e.rec.stopped.lock().unwrap().as_slice(), &[7]);
    }

    #[tokio::test]
    async fn new_failure_with_budget_investigates_consumes_and_persists_first() {
        let e = stub(
            Preflight::Ok,
            failed_status("sha1"),
            InvestigateOutcome::FixPrepared {
                branch: "grok-ci-fix/7-sha1".into(),
                diff_summary: "f | 1 +".into(),
            },
        );
        let mut st = state_with_watch();
        run_tick(&e, &mut st, "owner/repo", 7).await;

        let w = st.watch("owner/repo", 7).unwrap();
        assert!(!w.budget_available(), "budget consumed");
        assert_eq!(w.attempts_used, 1);
        assert_eq!(w.last_head_sha.as_deref(), Some("sha1"));
        assert!(*e.rec.persists.lock().unwrap() >= 1, "persisted before fix");
        assert_eq!(*e.rec.investigated.lock().unwrap(), 1);
        let n = e.rec.notifications.lock().unwrap();
        assert!(matches!(n.last().unwrap().1, CiEvent::FixReady { .. }));
    }

    #[tokio::test]
    async fn same_failure_is_deduped() {
        let e = stub(
            Preflight::Ok,
            failed_status("sha1"),
            InvestigateOutcome::CannotAutofix { reason: "x".into() },
        );
        let mut st = state_with_watch();
        // Pre-seed last_* to the same failure, budget already spent.
        {
            let w = st.watch_mut("owner/repo", 7).unwrap();
            w.last_head_sha = Some("sha1".into());
            w.last_failing_check = Some("unit-tests".into());
            w.consume_attempt();
        }
        run_tick(&e, &mut st, "owner/repo", 7).await;
        assert_eq!(*e.rec.investigated.lock().unwrap(), 0);
        assert!(e.rec.notifications.lock().unwrap().is_empty(), "no re-notify");
    }

    #[tokio::test]
    async fn budget_exhausted_notifies_rearm_once() {
        let e = stub(
            Preflight::Ok,
            failed_status("sha2"), // a DIFFERENT (new) failure...
            InvestigateOutcome::CannotAutofix { reason: "x".into() },
        );
        let mut st = state_with_watch();
        {
            // ...but the budget is already spent on an earlier failure.
            let w = st.watch_mut("owner/repo", 7).unwrap();
            w.consume_attempt();
            w.last_head_sha = Some("sha1".into());
            w.last_failing_check = Some("unit-tests".into());
        }
        run_tick(&e, &mut st, "owner/repo", 7).await;
        assert_eq!(*e.rec.investigated.lock().unwrap(), 0, "no fix without budget");
        let n = e.rec.notifications.lock().unwrap();
        assert_eq!(n.len(), 1);
        match &n[0].1 {
            CiEvent::CannotAutofix { reason } => assert!(reason.contains("rearm")),
            other => panic!("expected CannotAutofix(rearm), got {other:?}"),
        }
        // The new failure identity was recorded so it won't re-notify next poll.
        assert_eq!(
            st.watch("owner/repo", 7).unwrap().last_head_sha.as_deref(),
            Some("sha2")
        );
    }

    #[tokio::test]
    async fn new_sha_after_consume_still_needs_rearm() {
        // Consume the budget on sha1, then a fresh failure on sha2 must NOT
        // trigger another fix (the core anti-runaway property).
        let e = stub(
            Preflight::Ok,
            failed_status("sha2"),
            InvestigateOutcome::FixPrepared {
                branch: "b".into(),
                diff_summary: "d".into(),
            },
        );
        let mut st = state_with_watch();
        {
            let w = st.watch_mut("owner/repo", 7).unwrap();
            w.consume_attempt(); // budget spent on a prior failure
            w.last_head_sha = Some("sha1".into());
            w.last_failing_check = Some("unit-tests".into());
        }
        run_tick(&e, &mut st, "owner/repo", 7).await;
        assert_eq!(
            *e.rec.investigated.lock().unwrap(),
            0,
            "a new SHA must not auto-refill the budget"
        );
    }
}
