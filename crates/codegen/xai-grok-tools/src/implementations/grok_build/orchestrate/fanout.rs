//! Concurrent fan-out over the existing `SubagentBackend`, with per-sub-task
//! failure isolation. This is the deterministic barrier: every sub-task is
//! spawned, all are awaited together, and a sub-task that fails degrades to
//! `None` rather than aborting the batch.

use crate::implementations::grok_build::task::backend::SubagentBackend;
use crate::implementations::grok_build::task::types::{
    ModelOverrideProvenance, SubagentRequest, SubagentRuntimeOverrides,
};

use super::types::OrchestrateSubtask;

/// The request fields shared by every sub-task in one `orchestrate` call,
/// mirrored from the parent's session (see `task/mod.rs` request build).
pub struct RequestBase {
    pub subagent_type: String,
    pub parent_session_id: String,
    pub parent_prompt_id: Option<String>,
    pub cwd: Option<String>,
}

/// One resolved sub-task: its label and the sub-agent's output, or `None` if the
/// sub-agent failed/errored/cancelled.
#[derive(Debug, Clone)]
pub struct SubtaskOutcome {
    pub label: String,
    pub output: Option<String>,
}

fn label_for(st: &OrchestrateSubtask, index: usize) -> String {
    st.label
        .clone()
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| format!("subtask {}", index + 1))
}

/// Build the per-sub-task request. Foreground (`run_in_background:false`) so the
/// backend returns the result directly; `surface_completion:false` because these
/// are internal orchestration workers, not user-visible tasks.
fn build_request(base: &RequestBase, prompt: &str, description: &str) -> SubagentRequest {
    // Foreground spawn returns the result via the call; the oneshot is unused.
    let (result_tx, _rx) = tokio::sync::oneshot::channel();
    SubagentRequest {
        id: uuid::Uuid::new_v4().to_string(),
        prompt: prompt.to_string(),
        description: description.to_string(),
        subagent_type: base.subagent_type.clone(),
        parent_session_id: base.parent_session_id.clone(),
        parent_prompt_id: base.parent_prompt_id.clone(),
        resume_from: None,
        cwd: base.cwd.clone(),
        runtime_overrides: SubagentRuntimeOverrides {
            model: None,
            model_override_provenance: ModelOverrideProvenance::Tool,
            reasoning_effort: None,
            persona: None,
            capability_mode: None,
            isolation: None,
            harness_agent_type: None,
        },
        run_in_background: false,
        surface_completion: false,
        fork_context: false,
        result_tx,
    }
}

/// Spawn one sub-agent and reduce its result to `Some(output)` on success or
/// `None` on any failure/error/cancel. Shared with the gather step (verifier /
/// synthesizer sub-agents).
pub(crate) async fn spawn_one(
    backend: &dyn SubagentBackend,
    base: &RequestBase,
    prompt: &str,
    label: &str,
) -> Option<String> {
    let request = build_request(base, prompt, label);
    match backend.spawn(request).await {
        Ok(result) if result.success => Some(result.output.to_string()),
        _ => None,
    }
}

/// Spawn every sub-task concurrently, barrier on all of them, and isolate
/// failures. Returns one `SubtaskOutcome` per input sub-task, in input order.
///
/// Concurrency: this initiates all sub-task spawns at once, but the count is
/// hard-capped upstream at `MAX_SUBTASKS` (validated before we get here) and the
/// shared sub-agent coordinator behind `SubagentBackend` applies its own
/// running-subagent limit — so there is no unbounded fan-out or backend
/// starvation. `orchestrate` intentionally does not add a second cap.
pub async fn fan_out(
    backend: &dyn SubagentBackend,
    base: &RequestBase,
    subtasks: &[OrchestrateSubtask],
) -> Vec<SubtaskOutcome> {
    let futures = subtasks.iter().enumerate().map(|(i, st)| {
        let label = label_for(st, i);
        async move {
            let output = spawn_one(backend, base, &st.prompt, &label).await;
            SubtaskOutcome { label, output }
        }
    });
    futures::future::join_all(futures).await
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::implementations::grok_build::task::types::{
        SubagentCancelOutcome, SubagentDescribeOutcome, SubagentResult, SubagentSnapshot,
        SubagentValidateTypeOutcome,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A test backend that succeeds unless the prompt contains "FAIL", and for
    /// "VERIFY:<x>" echoes a CONFIRMED/REFUTED verdict for verify-mode tests.
    #[derive(Default)]
    pub struct StubBackend {
        pub calls: AtomicUsize,
    }

    pub fn mk_result(success: bool, output: &str) -> SubagentResult {
        SubagentResult {
            success,
            output: Arc::from(output),
            error: if success { None } else { Some("stub failure".into()) },
            cancelled: false,
            subagent_id: "stub-sid".into(),
            child_session_id: "stub-cid".into(),
            tool_calls: 0,
            turns: 1,
            duration_ms: 0,
            tokens_used: 0,
            worktree_path: None,
            backgrounded: false,
        }
    }

    #[async_trait::async_trait]
    impl SubagentBackend for StubBackend {
        async fn spawn(
            &self,
            request: SubagentRequest,
        ) -> Result<SubagentResult, xai_tool_runtime::ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let success = !request.prompt.contains("FAIL");
            // Verdict-aware so gather tests can drive verify/synthesize paths:
            // a verifier prompt echoes CONFIRMED unless the text says REFUTE-ME;
            // a synthesizer prompt echoes a marker.
            let output = if request.prompt.contains("Adversarially verify") {
                if request.prompt.contains("REFUTE-ME") {
                    "REFUTED".to_string()
                } else {
                    "CONFIRMED".to_string()
                }
            } else if request.prompt.contains("sub-task results") {
                "SYNTHESIZED".to_string()
            } else {
                format!("out:{}", request.prompt)
            };
            Ok(mk_result(success, &output))
        }
        async fn query(&self, _id: &str, _block: bool, _t: Option<u64>) -> Option<SubagentSnapshot> {
            None
        }
        async fn cancel(&self, _id: &str) -> SubagentCancelOutcome {
            SubagentCancelOutcome::Cancelled
        }
        async fn validate_type(&self, _t: &str, _p: &str) -> SubagentValidateTypeOutcome {
            SubagentValidateTypeOutcome::Ok
        }
        async fn describe_subagent_type(
            &self,
            _t: &str,
            _h: Option<&str>,
            _p: &str,
        ) -> SubagentDescribeOutcome {
            SubagentDescribeOutcome::Unknown { available: vec![] }
        }
    }

    pub fn base() -> RequestBase {
        RequestBase {
            subagent_type: "general".into(),
            parent_session_id: "parent-sid".into(),
            parent_prompt_id: Some("parent-pid".into()),
            cwd: None,
        }
    }

    pub fn sub(prompt: &str) -> OrchestrateSubtask {
        OrchestrateSubtask {
            prompt: prompt.into(),
            label: Some(prompt.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn fan_out_isolates_failures_and_barriers_all() {
        let backend = StubBackend::default();
        let subs = vec![sub("a"), sub("FAIL-b"), sub("c")];
        let out = fan_out(&backend, &base(), &subs).await;

        assert_eq!(out.len(), 3);
        assert!(out[0].output.is_some(), "a succeeds");
        assert!(out[1].output.is_none(), "FAIL-b isolated to None");
        assert!(out[2].output.is_some(), "c succeeds");
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "all subtasks spawned (barrier)"
        );
        // Outputs are attributed by label and preserve input order.
        assert_eq!(out[0].label, "a");
        assert_eq!(out[0].output.as_deref(), Some("out:a"));
    }

    #[tokio::test]
    async fn fan_out_all_success() {
        let backend = StubBackend::default();
        let subs = vec![sub("x"), sub("y")];
        let out = fan_out(&backend, &base(), &subs).await;
        assert_eq!(out.iter().filter(|o| o.output.is_some()).count(), 2);
    }
}
