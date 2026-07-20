//! Input/output types for the `orchestrate` deterministic fan-out tool.

use serde::{Deserialize, Serialize};

/// Maximum number of sub-tasks a single `orchestrate` call may fan out to.
pub const MAX_SUBTASKS: usize = 16;
/// Maximum nesting: an orchestrate sub-agent that itself calls orchestrate is
/// refused past this depth so a fan-out cannot grow exponentially.
pub const MAX_ORCHESTRATE_DEPTH: usize = 2;

/// One unit of parallel work.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OrchestrateSubtask {
    /// The prompt handed to a sub-agent.
    pub prompt: String,
    /// Optional label used to attribute this sub-task's output in the gather step.
    #[serde(default)]
    pub label: Option<String>,
}

/// How surviving sub-task outputs are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrateMode {
    /// Merge the outputs directly (via the synthesizer / labeled concat).
    Synthesize,
    /// Adversarially verify each output first; only CONFIRMED ones are gathered.
    Verify,
}

impl Default for OrchestrateMode {
    fn default() -> Self {
        Self::Synthesize
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OrchestrateInput {
    /// The sub-tasks to fan out (2..=MAX_SUBTASKS).
    pub subtasks: Vec<OrchestrateSubtask>,
    /// If set, a single synthesizer sub-agent merges the surviving outputs using
    /// this prompt and its output is returned. If unset, the labeled raw outputs
    /// are returned for the caller to synthesize itself.
    #[serde(default)]
    pub synthesis_prompt: Option<String>,
    /// Gather mode. Default: synthesize.
    #[serde(default)]
    pub mode: Option<OrchestrateMode>,
    /// Sub-agent type for the spawned workers. Default: the Task default type.
    #[serde(default)]
    pub subagent_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrateOutput {
    /// The synthesized text, or the concatenated labeled outputs.
    pub result: String,
    /// How many sub-tasks were requested.
    pub subtask_count: usize,
    /// How many produced a usable output.
    pub succeeded: usize,
    /// How many were skipped (failed/errored/cancelled, or refuted in verify mode).
    pub skipped: usize,
    /// In verify mode, how many outputs passed verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<usize>,
}

impl xai_tool_runtime::ToolOutput for OrchestrateOutput {}

/// Validate the argument shape independent of any backend: the sub-task count
/// must be in `2..=MAX_SUBTASKS` (use `Task` for a single sub-agent), and no
/// sub-task prompt may be empty.
pub fn validate(input: &OrchestrateInput) -> Result<(), String> {
    let n = input.subtasks.len();
    if n < 2 {
        return Err(format!(
            "orchestrate needs at least 2 subtasks (got {n}); use the `task` tool for one"
        ));
    }
    if n > MAX_SUBTASKS {
        return Err(format!(
            "orchestrate allows at most {MAX_SUBTASKS} subtasks (got {n})"
        ));
    }
    if input.subtasks.iter().any(|s| s.prompt.trim().is_empty()) {
        return Err("every subtask must have a non-empty prompt".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_with(n: usize) -> OrchestrateInput {
        OrchestrateInput {
            subtasks: (0..n)
                .map(|i| OrchestrateSubtask {
                    prompt: format!("task {i}"),
                    label: None,
                })
                .collect(),
            synthesis_prompt: None,
            mode: None,
            subagent_type: None,
        }
    }

    #[test]
    fn validate_rejects_out_of_range_subtask_counts() {
        assert!(validate(&input_with(1)).is_err(), "1 subtask rejected");
        assert!(validate(&input_with(2)).is_ok(), "2 subtasks ok");
        assert!(validate(&input_with(MAX_SUBTASKS)).is_ok(), "16 subtasks ok");
        assert!(
            validate(&input_with(MAX_SUBTASKS + 1)).is_err(),
            "17 subtasks rejected"
        );
    }

    #[test]
    fn validate_rejects_empty_prompt() {
        let mut input = input_with(2);
        input.subtasks[1].prompt = "   ".into();
        assert!(validate(&input).is_err());
    }

    #[test]
    fn mode_defaults_to_synthesize() {
        assert_eq!(OrchestrateMode::default(), OrchestrateMode::Synthesize);
    }
}
