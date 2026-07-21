//! The `artifact` tool and its supporting store/service/render pieces.
//!
//! An artifact is a self-contained HTML or Markdown document the agent
//! publishes; `grok agent serve` hosts it at `/artifacts/<id>` so it can be
//! opened in a browser, the mobile PWA, or over a remote connection. The store
//! persists to disk (surviving restarts); the service (store + base URL) is a
//! server-lifetime global the serving routes and the tool both read.

pub mod render;
pub mod service;
pub mod store;
pub mod tool;
pub mod types;

pub use service::{ArtifactService, artifact_service, set_artifact_service};
pub use store::ArtifactStore;
pub use tool::{ARTIFACT_TOOL_NAME, ArtifactTool};
