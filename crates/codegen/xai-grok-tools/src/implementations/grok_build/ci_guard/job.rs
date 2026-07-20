//! Bounded, cancellable investigation job with server-wide mutual exclusion.
//!
//! A CI failure kicks off an investigation (fetch logs → classify → maybe fix).
//! That work must NEVER run inline on the persistent agent's request loop and
//! must not pile up: only ONE investigation runs at a time server-wide, each is
//! wall-clock bounded, and each is cancellable. This module owns exactly that
//! orchestration; the actual investigation body is injected so the mechanism is
//! testable without a network or `gh`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Lifecycle state a single investigation resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Another investigation already holds the permit — this one did NOT run
    /// (the caller should skip, not queue, to avoid backlog).
    Queued,
    /// Cancelled before or during the work.
    Cancelled,
    /// Exceeded the wall-clock budget.
    TimedOut,
    /// The work completed within budget.
    Done,
}

/// Runs investigations under a shared single permit. Construct ONE per process
/// (the controller owns it) and clone it per job — all clones share the same
/// underlying permit, giving server-wide mutual exclusion.
#[derive(Clone)]
pub struct InvestigationJob {
    permit: Arc<Semaphore>,
}

impl InvestigationJob {
    /// A fresh job runner with its own single permit. In production there is one
    /// per process; tests make their own so they never contend with each other.
    pub fn new() -> Self {
        Self {
            permit: Arc::new(Semaphore::new(1)),
        }
    }

    /// Share the same permit (server-wide exclusion) across watches.
    pub fn with_permit(permit: Arc<Semaphore>) -> Self {
        Self { permit }
    }

    /// Run `work` under mutual exclusion + `timeout` + `cancel`.
    ///
    /// - If the single permit is already held, returns `Queued` immediately
    ///   WITHOUT running `work` (skip, don't pile up).
    /// - If `cancel` fires (before or during), returns `Cancelled`.
    /// - If `work` outlives `timeout`, returns `TimedOut`.
    /// - Otherwise `Done`.
    #[must_use]
    pub async fn run<F, Fut>(
        &self,
        cancel: CancellationToken,
        timeout: Duration,
        work: F,
    ) -> JobState
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        // Non-blocking acquire: a second concurrent investigation is skipped,
        // not queued behind the first.
        // Non-blocking acquire. The only error is `NoPermits` (the semaphore is
        // never closed), so a failure means "already investigating" → skip.
        let _permit = match self.permit.try_acquire() {
            Ok(p) => p,
            Err(_) => return JobState::Queued,
        };

        // No separate pre-check for cancellation: the biased branch below fires
        // immediately when the token is already cancelled, before `work` runs.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => JobState::Cancelled,
            result = tokio::time::timeout(timeout, work()) => match result {
                Ok(()) => JobState::Done,
                Err(_) => JobState::TimedOut,
            }
        }
    }
}

impl Default for InvestigationJob {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn work_completes_within_budget_is_done() {
        let job = InvestigationJob::new();
        let state = job
            .run(CancellationToken::new(), Duration::from_secs(5), || async {})
            .await;
        assert_eq!(state, JobState::Done);
    }

    #[tokio::test]
    async fn work_exceeding_budget_times_out() {
        let job = InvestigationJob::new();
        let state = job
            .run(CancellationToken::new(), Duration::from_millis(50), || async {
                tokio::time::sleep(Duration::from_secs(10)).await;
            })
            .await;
        assert_eq!(state, JobState::TimedOut);
    }

    #[tokio::test]
    async fn cancelled_token_yields_cancelled() {
        let job = InvestigationJob::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let state = job
            .run(cancel, Duration::from_secs(5), || async {
                tokio::time::sleep(Duration::from_secs(10)).await;
            })
            .await;
        assert_eq!(state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn second_concurrent_investigation_is_queued_not_inline() {
        let job = InvestigationJob::new();
        let running = Arc::new(tokio::sync::Notify::new());
        let running2 = running.clone();

        // First job acquires the permit and holds it while "working".
        let first = {
            let job = job.clone();
            tokio::spawn(async move {
                job.run(CancellationToken::new(), Duration::from_secs(5), move || async move {
                    running2.notify_one();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                })
                .await
            })
        };

        // Wait until the first job is actually inside `work` holding the permit.
        running.notified().await;

        // A second run must be turned away immediately, without running its work.
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran2 = ran.clone();
        let second = job
            .run(CancellationToken::new(), Duration::from_secs(5), move || async move {
                ran2.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await;
        assert_eq!(second, JobState::Queued);
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "queued job must NOT run its work inline"
        );

        assert_eq!(first.await.unwrap(), JobState::Done);
    }
}
