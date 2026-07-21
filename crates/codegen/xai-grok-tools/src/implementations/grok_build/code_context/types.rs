//! Input/output types for the `code_context` tool.

use serde::{Deserialize, Serialize};

/// Which lookup tier to run. Mirrors the three-tier "read less, know more"
/// model: a cheap file summary, a ranked search, or a budgeted briefing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeContextMode {
    /// T1 — what a single file does: its lead doc comment plus a definition
    /// outline. Cheapest; needs `path`.
    Summary,
    /// T2 — where something is: ranked matching lines (path:line) for `query`.
    Search,
    /// T3 — a focused briefing: the top matches for `query` with surrounding
    /// context, concatenated up to `max_tokens`.
    Context,
}

/// Arguments for the `code_context` tool.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeContextInput {
    /// Which tier to run: `summary`, `search`, or `context`.
    pub mode: CodeContextMode,
    /// File to summarize (relative to the working dir or absolute). Required
    /// for `summary`; ignored otherwise.
    #[serde(default)]
    pub path: Option<String>,
    /// What to look for. Required for `search` and `context`; ignored for
    /// `summary`.
    #[serde(default)]
    pub query: Option<String>,
    /// `search`: max ranked lines to return (default 10, capped at 50).
    #[serde(default)]
    pub top_k: Option<usize>,
    /// `context`: approximate token budget for the briefing (default 2000,
    /// capped at 8000). Tokens are estimated as bytes/4.
    #[serde(default)]
    pub max_tokens: Option<usize>,
}

/// Result of a `code_context` lookup.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodeContextOutput {
    /// The formatted result, delivered to the model as the tool result.
    pub result: String,
}

impl xai_tool_runtime::ToolOutput for CodeContextOutput {}
