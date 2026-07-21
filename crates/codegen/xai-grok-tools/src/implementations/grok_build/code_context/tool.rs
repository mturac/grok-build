//! The `code_context` tool: token-cheap codebase lookups so the agent reads
//! less. Three tiers — summary (T1), search (T2), context (T3) — over the
//! working directory.

use std::path::PathBuf;

use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{resolve_cwd, shared_resources};

use super::outline::summarize;
use super::search::{build_context, search};
use super::types::{CodeContextInput, CodeContextMode, CodeContextOutput};

pub const CODE_CONTEXT_TOOL_NAME: &str = "code_context";

const DEFAULT_TOP_K: usize = 10;
const MAX_TOP_K: usize = 50;
const DEFAULT_MAX_TOKENS: usize = 2000;
const MAX_MAX_TOKENS: usize = 8000;

#[derive(Debug, Default)]
pub struct CodeContextTool;

/// Run a lookup against `root`. Pure of the runtime context so it is unit
/// testable with a plain directory.
pub fn run_lookup(root: &std::path::Path, input: &CodeContextInput) -> Result<String, String> {
    match input.mode {
        CodeContextMode::Summary => {
            let path = input
                .path
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or("`summary` requires `path`")?;
            let full = {
                let p = PathBuf::from(path);
                if p.is_absolute() { p } else { root.join(p) }
            };
            let content = std::fs::read_to_string(&full)
                .map_err(|e| format!("cannot read {path}: {e}"))?;
            // Label a file under the working dir by its relative path; for a
            // file outside it, echo the path as typed rather than the resolved
            // absolute path, so we don't leak the host filesystem layout.
            let label = match full.strip_prefix(root) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                Err(_) => path.replace('\\', "/"),
            };
            Ok(summarize(&label, &content))
        }
        CodeContextMode::Search => {
            let query = require_query(input)?;
            let top_k = input.top_k.unwrap_or(DEFAULT_TOP_K).clamp(1, MAX_TOP_K);
            Ok(search(root, query, top_k))
        }
        CodeContextMode::Context => {
            let query = require_query(input)?;
            let max_tokens = input
                .max_tokens
                .unwrap_or(DEFAULT_MAX_TOKENS)
                .clamp(200, MAX_MAX_TOKENS);
            Ok(build_context(root, query, max_tokens))
        }
    }
}

fn require_query(input: &CodeContextInput) -> Result<&str, String> {
    input
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| "this mode requires a non-empty `query`".to_string())
}

impl crate::types::tool_metadata::ToolMetadata for CodeContextTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }
    fn description_template(&self) -> &str {
        r#"Answer "what does this file do?" / "where is X?" cheaply over the working directory, so you read fewer whole files.

Modes:
- `summary` (needs `path`): a file's lead doc comment plus an outline of its top-level definitions — a ~20-line map instead of the whole file.
- `search` (needs `query`): ranked matching lines as `path:line: text`. Set `top_k` (default 10).
- `context` (needs `query`): the top matches with surrounding lines, concatenated up to `max_tokens` (default 2000) — a focused briefing.

Lexical (no embeddings), always current (no stale index), and respects .gitignore. Prefer this before reading a large file end-to-end; fall back to read_file only when you need the exact remaining lines."#
    }
    fn emitted_notifications(&self) -> &'static [&'static str] {
        &[]
    }
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for CodeContextTool {
    type Args = CodeContextInput;
    type Output = CodeContextOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(CODE_CONTEXT_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            CODE_CONTEXT_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.code_context", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: CodeContextInput,
    ) -> Result<CodeContextOutput, xai_tool_runtime::ToolError> {
        let resources = shared_resources(&ctx)?;
        let root = resolve_cwd(&ctx, &resources)
            .await
            .or_else(|_| std::env::current_dir().map_err(|e| {
                xai_tool_runtime::ToolError::custom("missing_cwd", e.to_string())
            }))?;
        let result =
            run_lookup(&root, &input).map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        Ok(CodeContextOutput { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_root() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/auth.rs"),
            "//! Authentication.\npub fn login(user: &str) {}\npub struct Session;\n",
        )
        .unwrap();
        tmp
    }

    fn input(mode: CodeContextMode, path: Option<&str>, query: Option<&str>) -> CodeContextInput {
        CodeContextInput {
            mode,
            path: path.map(str::to_string),
            query: query.map(str::to_string),
            top_k: None,
            max_tokens: None,
        }
    }

    #[test]
    fn summary_reads_relative_path() {
        let tmp = sample_root();
        let out = run_lookup(
            tmp.path(),
            &input(CodeContextMode::Summary, Some("src/auth.rs"), None),
        )
        .unwrap();
        assert!(out.contains("# src/auth.rs"));
        assert!(out.contains("Authentication."));
        assert!(out.contains("- fn login"));
        assert!(out.contains("- struct Session"));
    }

    #[test]
    fn summary_requires_path_and_reports_missing_file() {
        let tmp = sample_root();
        assert!(
            run_lookup(tmp.path(), &input(CodeContextMode::Summary, None, None))
                .unwrap_err()
                .contains("requires `path`")
        );
        assert!(
            run_lookup(
                tmp.path(),
                &input(CodeContextMode::Summary, Some("nope.rs"), None)
            )
            .unwrap_err()
            .contains("cannot read")
        );
    }

    #[test]
    fn search_and_context_need_query() {
        let tmp = sample_root();
        assert!(
            run_lookup(tmp.path(), &input(CodeContextMode::Search, None, None))
                .unwrap_err()
                .contains("query")
        );
        let out =
            run_lookup(tmp.path(), &input(CodeContextMode::Search, None, Some("login"))).unwrap();
        assert!(out.contains("src/auth.rs:2"), "got: {out}");
    }

    #[test]
    fn context_mode_builds_briefing() {
        let tmp = sample_root();
        let out = run_lookup(
            tmp.path(),
            &input(CodeContextMode::Context, None, Some("Session")),
        )
        .unwrap();
        assert!(out.contains("── src/auth.rs:"), "got: {out}");
    }

    #[test]
    fn summary_label_for_relative_escape_stays_relative() {
        // A `..`-escaping relative path must label by the as-typed path, never
        // a resolved absolute that leaks the host layout.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sub");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(tmp.path().join("outside.rs"), "pub fn out() {}\n").unwrap();

        let out = run_lookup(
            &root,
            &input(CodeContextMode::Summary, Some("../outside.rs"), None),
        )
        .unwrap();
        assert!(out.contains("../outside.rs"), "labeled by typed path: {out}");
        assert!(out.contains("- fn out"));
        // The absolute tempdir path must not appear in the label/output.
        assert!(
            !out.contains(&tmp.path().to_string_lossy().to_string()),
            "must not leak host absolute path: {out}"
        );
    }

    #[test]
    fn top_k_is_clamped() {
        // top_k above the cap must not error; it just clamps.
        let tmp = sample_root();
        let mut i = input(CodeContextMode::Search, None, Some("fn"));
        i.top_k = Some(9999);
        assert!(run_lookup(tmp.path(), &i).is_ok());
    }
}
