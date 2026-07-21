//! Input/output types for the `artifact` tool.

use serde::{Deserialize, Serialize};

/// Content format of a published artifact.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    /// Markdown body, rendered to a styled HTML page server-side. The default.
    #[default]
    Markdown,
    /// A complete, self-contained HTML document, served as-is under a strict
    /// Content-Security-Policy (no external hosts, no network).
    Html,
}

impl ArtifactFormat {
    /// File extension used for on-disk persistence of the raw body.
    pub fn ext(self) -> &'static str {
        match self {
            ArtifactFormat::Markdown => "md",
            ArtifactFormat::Html => "html",
        }
    }
}

/// Arguments for the `artifact` tool.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ArtifactInput {
    /// Human-readable title, shown in the browser tab and the index listing.
    pub title: String,
    /// The document body: Markdown (rendered to HTML) or a full HTML document,
    /// per `format`. Must be self-contained — external requests are blocked.
    pub content: String,
    /// Content format. Defaults to `markdown`.
    #[serde(default)]
    pub format: ArtifactFormat,
    /// Existing artifact id to overwrite in place (keeps the same URL). Omit to
    /// publish a new artifact.
    #[serde(default)]
    pub id: Option<String>,
}

/// Result of publishing an artifact.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactOutput {
    /// The URL where the artifact can be opened (browser / PWA / remote).
    pub url: String,
    /// The stable artifact id — reuse it as `input.id` to update in place.
    pub id: String,
    /// Short human-facing confirmation; also the tool's prompt-format result.
    pub message: String,
}

impl xai_tool_runtime::ToolOutput for ArtifactOutput {}
