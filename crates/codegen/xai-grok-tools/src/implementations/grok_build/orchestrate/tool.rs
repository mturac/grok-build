//! The `orchestrate` agent tool: deterministic fan-out + gather over the
//! existing `SubagentBackend`.

use crate::implementations::grok_build::task::backend::SubagentBackendResource;
use crate::implementations::grok_build::task::types::{
    CurrentPromptIdResource, SessionIdResource, SubagentDepthCounter, SubagentValidateTypeOutcome,
};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{resolve_cwd, shared_resources};

use super::fanout::{RequestBase, fan_out};
use super::gather::{labeled_concat, synthesize, verify_filter};
use super::types::{
    MAX_ORCHESTRATE_DEPTH, OrchestrateInput, OrchestrateMode, OrchestrateOutput, validate,
};

pub const ORCHESTRATE_TOOL_NAME: &str = "orchestrate";

/// Sub-agent type used when the caller doesn't specify one.
const DEFAULT_SUBAGENT_TYPE: &str = "general";

#[derive(Debug, Default)]
pub struct OrchestrateTool;

impl crate::types::tool_metadata::ToolMetadata for OrchestrateTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }
    fn description_template(&self) -> &str {
        r#"Deterministically fan a task out to several sub-agents at once, wait for all of them, and gather the results.

Give 2-16 `subtasks` (each a prompt). They run in parallel as sub-agents; a failed sub-task is skipped, not fatal.
- `synthesis_prompt` (optional): a final sub-agent merges the results under this prompt and its output is returned. Omit it to get the labeled raw outputs back.
- `mode: "verify"` (optional): each result is independently checked and only CONFIRMED ones are kept before synthesis.

Use this when you have independent pieces to do in parallel or want N attempts verified. For a single sub-agent, use `task` instead."#
    }
    fn emitted_notifications(&self) -> &'static [&'static str] {
        &[]
    }
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for OrchestrateTool {
    type Args = OrchestrateInput;
    type Output = OrchestrateOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(ORCHESTRATE_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            ORCHESTRATE_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.orchestrate", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: OrchestrateInput,
    ) -> Result<OrchestrateOutput, xai_tool_runtime::ToolError> {
        validate(&input).map_err(xai_tool_runtime::ToolError::invalid_arguments)?;

        let resources = shared_resources(&ctx)?;
        let (depth, backend, parent_session_id, parent_prompt_id) = {
            let res = resources.lock().await;
            let depth = res.get::<SubagentDepthCounter>().map(|d| d.0).unwrap_or(0);
            let backend = res
                .get::<SubagentBackendResource>()
                .ok_or_else(|| {
                    xai_tool_runtime::ToolError::custom(
                        "missing_resource",
                        "SubagentBackendResource (subagent support not initialized)",
                    )
                })?
                .clone();
            let parent_session_id = res
                .get::<SessionIdResource>()
                .map(|s| s.0.clone())
                .unwrap_or_default();
            let parent_prompt_id = res
                .get::<CurrentPromptIdResource>()
                .map(|p| p.0.clone())
                .filter(|id| !id.is_empty());
            (depth, backend, parent_session_id, parent_prompt_id)
        };

        // `depth` is the CURRENT session's subagent depth. We only read it here;
        // the increment for spawned children is done by the coordinator when it
        // builds each child session (`subagent::handle_request` sets
        // `child_depth = parent_depth + 1`, injected as `SubagentDepthCounter`),
        // exactly as the `task` tool relies on. So a sub-agent this call spawns
        // runs one level deeper, and if it calls `orchestrate` again it sees the
        // higher depth — making this cap effective without the tool touching the
        // counter (there is no depth field on `SubagentRequest` to set anyway).
        if depth as usize >= MAX_ORCHESTRATE_DEPTH {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "orchestrate nesting too deep (depth {depth}, max {MAX_ORCHESTRATE_DEPTH}); \
                 an orchestrated sub-agent cannot orchestrate again"
            )));
        }

        let subagent_type = input
            .subagent_type
            .clone()
            .unwrap_or_else(|| DEFAULT_SUBAGENT_TYPE.to_string());

        // Validate the sub-agent type up front so a typo fails fast with the
        // available options, rather than silently failing every sub-task.
        if let SubagentValidateTypeOutcome::Unknown { available } = backend
            .backend()
            .validate_type(&subagent_type, &parent_session_id)
            .await
        {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "unknown subagent_type {subagent_type:?}; available: {}",
                available.join(", ")
            )));
        }

        let base = RequestBase {
            subagent_type,
            parent_session_id,
            parent_prompt_id,
            cwd: resolve_cwd(&ctx, &resources)
                .await
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
        };

        let subtask_count = input.subtasks.len();
        let outcomes = fan_out(backend.backend(), &base, &input.subtasks).await;

        let mode = input.mode.unwrap_or_default();
        let (gather_outcomes, verified) = match mode {
            OrchestrateMode::Verify => {
                let (kept, n) = verify_filter(backend.backend(), &base, outcomes).await;
                (kept, Some(n))
            }
            OrchestrateMode::Synthesize => (
                outcomes.into_iter().filter(|o| o.output.is_some()).collect(),
                None,
            ),
        };

        let succeeded = gather_outcomes.len();
        let skipped = subtask_count - succeeded;

        let result = if gather_outcomes.is_empty() {
            format!(
                "All {subtask_count} subtasks failed or produced no usable output; nothing to gather."
            )
        } else if let Some(prompt) = &input.synthesis_prompt {
            synthesize(backend.backend(), &base, prompt, &gather_outcomes).await
        } else {
            labeled_concat(&gather_outcomes)
        };

        Ok(OrchestrateOutput {
            result,
            subtask_count,
            succeeded,
            skipped,
            verified,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::fanout::test_support::StubBackend;
    use super::super::types::OrchestrateSubtask;
    use super::*;
    use crate::types::resources::Resources;
    use std::sync::Arc;
    use xai_tool_runtime::Tool;

    fn ctx_with_backend(depth: Option<u32>) -> xai_tool_runtime::ToolCallContext {
        let mut resources = Resources::new();
        resources.insert(SubagentBackendResource(Arc::new(StubBackend::default())));
        resources.insert(SessionIdResource("parent-sid".into()));
        if let Some(d) = depth {
            resources.insert(SubagentDepthCounter(d));
        }
        crate::types::tool_metadata::test_ctx(resources.into_shared())
    }

    fn input(prompts: &[&str], synth: Option<&str>, mode: Option<OrchestrateMode>) -> OrchestrateInput {
        OrchestrateInput {
            subtasks: prompts
                .iter()
                .map(|p| OrchestrateSubtask {
                    prompt: (*p).into(),
                    label: Some((*p).into()),
                })
                .collect(),
            synthesis_prompt: synth.map(|s| s.into()),
            mode,
            subagent_type: None,
        }
    }

    #[tokio::test]
    async fn synthesize_over_three_subtasks() {
        let ctx = ctx_with_backend(None);
        let out = OrchestrateTool
            .run(ctx, input(&["a", "b", "c"], Some("merge"), None))
            .await
            .unwrap();
        assert_eq!(out.subtask_count, 3);
        assert_eq!(out.succeeded, 3);
        assert_eq!(out.skipped, 0);
        assert_eq!(out.verified, None);
        assert_eq!(out.result, "SYNTHESIZED");
    }

    #[tokio::test]
    async fn failed_subtask_is_skipped() {
        let ctx = ctx_with_backend(None);
        let out = OrchestrateTool
            .run(ctx, input(&["a", "FAIL-b", "c"], None, None))
            .await
            .unwrap();
        assert_eq!(out.subtask_count, 3);
        assert_eq!(out.succeeded, 2);
        assert_eq!(out.skipped, 1);
        // No synthesis prompt -> labeled raw outputs of the survivors.
        assert!(out.result.contains("## a"));
        assert!(out.result.contains("## c"));
        assert!(!out.result.contains("## FAIL-b"));
    }

    #[tokio::test]
    async fn verify_mode_filters_and_counts() {
        let ctx = ctx_with_backend(None);
        let out = OrchestrateTool
            .run(ctx, input(&["a", "REFUTE-ME-b", "c"], Some("merge"), Some(OrchestrateMode::Verify)))
            .await
            .unwrap();
        assert_eq!(out.subtask_count, 3);
        // b's output contains REFUTE-ME -> verifier REFUTES it.
        assert_eq!(out.verified, Some(2));
        assert_eq!(out.succeeded, 2);
        assert_eq!(out.skipped, 1);
    }

    #[tokio::test]
    async fn depth_at_cap_refuses_without_spawning() {
        let ctx = ctx_with_backend(Some(MAX_ORCHESTRATE_DEPTH as u32));
        let err = OrchestrateTool
            .run(ctx, input(&["a", "b"], None, None))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").to_lowercase().contains("deep"));
    }

    #[tokio::test]
    async fn missing_backend_errors() {
        let resources = Resources::new();
        let ctx = crate::types::tool_metadata::test_ctx(resources.into_shared());
        let err = OrchestrateTool
            .run(ctx, input(&["a", "b"], None, None))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("SubagentBackendResource"));
    }
}
