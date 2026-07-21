//! The `artifact` tool: publish a self-contained HTML/Markdown document that
//! `grok agent serve` hosts at a shareable URL.

use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

use super::service::{ArtifactService, artifact_service};
use super::store::{Artifact, gen_id, is_valid_id, now_ms};
use super::types::{ArtifactInput, ArtifactOutput};

pub const ARTIFACT_TOOL_NAME: &str = "artifact";

/// Upper bound on a single artifact body. Generous for a document, but a guard
/// against a runaway generation filling the disk.
const MAX_ARTIFACT_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct ArtifactTool;

/// Core publish logic, independent of the process-global service so it can be
/// unit-tested against a local store. `created_ms` is passed in for
/// determinism in tests.
pub fn publish(
    service: &ArtifactService,
    input: ArtifactInput,
    created_ms: u64,
) -> Result<ArtifactOutput, xai_tool_runtime::ToolError> {
    let title = input.title.trim();
    if title.is_empty() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "artifact title must not be empty",
        ));
    }
    if input.content.is_empty() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "artifact content must not be empty",
        ));
    }
    if input.content.len() > MAX_ARTIFACT_BYTES {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "artifact content is {} bytes; the limit is {MAX_ARTIFACT_BYTES}",
            input.content.len()
        )));
    }

    // A caller-supplied id (update-in-place) reaches the filesystem, so it is
    // validated; a generated id is always safe.
    let id = match input.id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            if !is_valid_id(id) {
                return Err(xai_tool_runtime::ToolError::invalid_arguments(
                    "artifact id must be 1-64 chars of [A-Za-z0-9_-] (no path separators)",
                ));
            }
            id.to_string()
        }
        _ => gen_id(title, &input.content, created_ms),
    };

    let artifact = Artifact {
        id: id.clone(),
        title: title.to_string(),
        format: input.format,
        content: input.content,
        created_ms,
    };
    service.store.put(artifact).map_err(|e| {
        xai_tool_runtime::ToolError::custom("artifact_write_failed", e.to_string())
    })?;

    let url = service.artifact_url(&id);
    Ok(ArtifactOutput {
        message: format!("Published artifact \"{title}\" — open it at {url}"),
        url,
        id,
    })
}

impl crate::types::tool_metadata::ToolMetadata for ArtifactTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }
    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }
    fn description_template(&self) -> &str {
        r#"Publish a self-contained document (a report, summary, table, chart, or small web page) that is hosted at a shareable URL you can open in a browser, the mobile PWA, or over a remote connection.

- `title`: shown in the browser tab and the artifact index.
- `content`: the body. With `format: "markdown"` (default) it is rendered to a styled page; with `format: "html"` you provide a complete, self-contained HTML document.
- `id` (optional): pass a previous artifact's id to update it in place and keep the same URL; omit to publish a new one.

The document must be fully self-contained. A strict, sandboxed Content-Security-Policy blocks all external and network requests, so inline everything: CSS in `<style>`, JavaScript in `<script>` (inline only — no external src, and no fetch/XHR/WebSocket at runtime), any data as JS literals, and images as data: URIs. Interactive inline JS is supported (charts, toggles, filtering over inlined data); the page is isolated from the app's storage and the network. Requires a running `grok agent serve`. Use this when the user would be better served by a viewable page than by terminal text."#
    }
    fn emitted_notifications(&self) -> &'static [&'static str] {
        &[]
    }
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ArtifactTool {
    type Args = ArtifactInput;
    type Output = ArtifactOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(ARTIFACT_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            ARTIFACT_TOOL_NAME,
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

    #[tracing::instrument(name = "tool.artifact", skip_all)]
    async fn run(
        &self,
        _ctx: xai_tool_runtime::ToolCallContext,
        input: ArtifactInput,
    ) -> Result<ArtifactOutput, xai_tool_runtime::ToolError> {
        let service = artifact_service().ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "artifacts_unavailable",
                "Artifacts require a running `grok agent serve` (the server hosts the published \
                 page). Start the server and try again.",
            )
        })?;
        publish(&service, input, now_ms())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::implementations::grok_build::artifact::store::ArtifactStore;
    use crate::implementations::grok_build::artifact::types::ArtifactFormat;

    // Returns the TempDir too so the caller keeps it alive for the test's life
    // (dropping it would delete the store directory).
    fn service() -> (tempfile::TempDir, ArtifactService) {
        let tmp = tempfile::tempdir().unwrap();
        let svc = ArtifactService::new(ArtifactStore::load(tmp.path()), "http://host:2419");
        (tmp, svc)
    }

    fn input(title: &str, content: &str, format: ArtifactFormat, id: Option<&str>) -> ArtifactInput {
        ArtifactInput {
            title: title.into(),
            content: content.into(),
            format,
            id: id.map(str::to_string),
        }
    }

    #[test]
    fn publish_returns_url_and_persists() {
        let (_tmp, svc) = service();
        let out = publish(
            &svc,
            input("My Report", "# Hi", ArtifactFormat::Markdown, None),
            123,
        )
        .unwrap();
        assert!(out.url.starts_with("http://host:2419/artifacts/"));
        assert!(out.url.ends_with(&out.id));
        assert!(out.message.contains("My Report"));
        // Persisted and fetchable.
        assert_eq!(svc.store.get(&out.id).unwrap().title, "My Report");
    }

    #[test]
    fn publish_with_id_updates_in_place_same_url() {
        let (_tmp, svc) = service();
        let first = publish(
            &svc,
            input("v1", "one", ArtifactFormat::Markdown, Some("mydoc")),
            1,
        )
        .unwrap();
        assert_eq!(first.id, "mydoc");
        let second = publish(
            &svc,
            input("v2", "two", ArtifactFormat::Markdown, Some("mydoc")),
            2,
        )
        .unwrap();
        // Same id → same URL; content updated in place.
        assert_eq!(second.url, first.url);
        assert_eq!(svc.store.get("mydoc").unwrap().title, "v2");
    }

    #[test]
    fn publish_rejects_empty_and_bad_id() {
        let (_tmp, svc) = service();
        assert!(publish(&svc, input("  ", "body", ArtifactFormat::Markdown, None), 1).is_err());
        assert!(publish(&svc, input("t", "", ArtifactFormat::Markdown, None), 1).is_err());
        let err = publish(
            &svc,
            input("t", "body", ArtifactFormat::Markdown, Some("../evil")),
            1,
        )
        .unwrap_err();
        assert!(format!("{err:?}").to_lowercase().contains("id"));
    }

    #[test]
    fn publish_rejects_oversized_content() {
        let (_tmp, svc) = service();
        let big = "x".repeat(MAX_ARTIFACT_BYTES + 1);
        let err = publish(&svc, input("t", &big, ArtifactFormat::Html, None), 1).unwrap_err();
        assert!(format!("{err:?}").contains("limit"));
    }
}
