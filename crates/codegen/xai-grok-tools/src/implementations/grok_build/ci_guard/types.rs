//! CI Guardian state: one durable `CiWatch` per watched GitHub PR.
//!
//! The anti-runaway core lives here (design spec §3.3): each watch carries a
//! PR-level automation budget of exactly one fix attempt, held as a
//! `remaining_attempts` token. Consuming the attempt drops the token to zero; a
//! *new* failing run (even on a new head SHA) does NOT refill it — only an
//! explicit human `rearm` resets the token to the budget. This is what stops the
//! "human pushes fix → new run fails → agent fixes again → …" loop from becoming
//! implicit unbounded auto-iteration.

use serde::{Deserialize, Serialize};

/// Automation budget per PR: one autonomous fix attempt until a human re-arms.
pub const DEFAULT_AUTOMATION_BUDGET: u32 = 1;

/// A single watched PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiWatch {
    /// PR number being watched.
    pub pr_number: u64,
    /// `owner/name` of the repository.
    pub repo: String,
    /// The durable scheduler task id that drives this watch's polling.
    pub scheduler_task_id: String,
    /// Head SHA of the most recent failure we acted on (dedup key component).
    pub last_head_sha: Option<String>,
    /// Identity of the most recent failing check we acted on (dedup key part).
    pub last_failing_check: Option<String>,
    /// The one-shot budget gate: autonomous fixes may run while this is > 0.
    /// `consume_attempt` decrements it; `rearm` resets it to the budget.
    pub remaining_attempts: u32,
    /// Monotonic audit counter of how many autonomous fixes have ever run for
    /// this PR. Never reset (survives `rearm`) — for observability only, never
    /// gates behaviour.
    pub attempts_used: u32,
    /// Whether the watch is still live (false once the PR merges/closes).
    pub watch_active: bool,
}

impl CiWatch {
    pub fn new(pr_number: u64, repo: String, scheduler_task_id: String) -> Self {
        Self {
            pr_number,
            repo,
            scheduler_task_id,
            last_head_sha: None,
            last_failing_check: None,
            remaining_attempts: DEFAULT_AUTOMATION_BUDGET,
            attempts_used: 0,
            watch_active: true,
        }
    }

    /// Whether an autonomous fix may run now: budget token remains.
    pub fn budget_available(&self) -> bool {
        self.remaining_attempts > 0
    }

    /// Record that an autonomous fix attempt was made: spend one budget token
    /// and bump the audit counter. No-op if the budget is already exhausted, so
    /// a spurious extra call cannot inflate the audit count.
    pub fn consume_attempt(&mut self) {
        if self.remaining_attempts == 0 {
            return;
        }
        self.remaining_attempts -= 1;
        self.attempts_used = self.attempts_used.saturating_add(1);
    }

    /// Grant a fresh single attempt (human-initiated). Resets the gate to the
    /// budget; does NOT stack (calling twice still yields one attempt) and does
    /// NOT touch the audit counter.
    pub fn rearm(&mut self) {
        self.remaining_attempts = DEFAULT_AUTOMATION_BUDGET;
    }
}

/// Durable, persisted CI Guardian state. Mirrors `SchedulerState`'s registration
/// so it rides the same `ResourcesPersistence` path (survives process restart).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CiGuardState {
    pub watches: Vec<CiWatch>,
}

crate::register_resource!("grok_build", "CiGuard", CiGuardState);

impl CiGuardState {
    /// Find a mutable watch by (repo, pr). Keyed by both because a single
    /// process may watch same-numbered PRs across different repositories.
    pub fn watch_mut(&mut self, repo: &str, pr_number: u64) -> Option<&mut CiWatch> {
        self.watches
            .iter_mut()
            .find(|w| w.pr_number == pr_number && w.repo == repo)
    }

    /// Find a watch by (repo, pr).
    pub fn watch(&self, repo: &str, pr_number: u64) -> Option<&CiWatch> {
        self.watches
            .iter()
            .find(|w| w.pr_number == pr_number && w.repo == repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_one_shot_until_rearm() {
        let mut w = CiWatch::new(7, "mturac/grok-build".into(), "task1".into());
        assert!(w.budget_available(), "a fresh watch has its one attempt");

        w.consume_attempt();
        assert!(
            !w.budget_available(),
            "one attempt per failure until a human re-arms"
        );
        assert_eq!(w.attempts_used, 1);

        // A new failing run on a new head SHA must NOT refill the budget.
        w.last_head_sha = Some("newsha".into());
        assert!(
            !w.budget_available(),
            "a new head SHA does not auto-refill the budget"
        );
        // A spurious consume while exhausted must not inflate the audit count.
        w.consume_attempt();
        assert_eq!(w.attempts_used, 1, "no-op consume while exhausted");

        w.rearm();
        assert!(w.budget_available(), "rearm grants exactly one more attempt");

        // rearm does not stack.
        w.rearm();
        w.consume_attempt();
        assert!(!w.budget_available(), "rearm never stacks beyond one");
        assert_eq!(w.attempts_used, 2, "audit counter is monotonic across rearm");
    }

    #[test]
    fn state_lookup_keyed_by_repo_and_pr() {
        let mut s = CiGuardState::default();
        s.watches.push(CiWatch::new(7, "owner/a".into(), "t1".into()));
        s.watches.push(CiWatch::new(7, "owner/b".into(), "t2".into()));
        // Same PR number, different repos must not collide.
        assert_eq!(s.watch("owner/a", 7).unwrap().scheduler_task_id, "t1");
        assert_eq!(s.watch("owner/b", 7).unwrap().scheduler_task_id, "t2");
        s.watch_mut("owner/a", 7).unwrap().watch_active = false;
        assert!(!s.watch("owner/a", 7).unwrap().watch_active);
        assert!(s.watch("owner/b", 7).unwrap().watch_active);
        assert!(s.watch("owner/c", 7).is_none());
    }
}
