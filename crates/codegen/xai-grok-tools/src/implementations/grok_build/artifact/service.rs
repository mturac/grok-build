//! Process-wide artifact service: the store plus the base URL used to build
//! shareable artifact links.
//!
//! Like the notification bridge, this is a server-lifetime singleton. The
//! `grok agent serve` startup builds it (store dir + the address it advertises)
//! and publishes it here; both the `artifact` tool (in this crate) and the
//! HTTP serving routes (in the shell crate) read it back. When the server is
//! not running the global is unset, and the tool reports that artifacts need
//! `grok agent serve`.

use std::sync::{Arc, OnceLock};

use super::store::ArtifactStore;

/// The store plus the base URL artifact links are built from.
pub struct ArtifactService {
    pub store: ArtifactStore,
    /// Origin the server advertises, e.g. `http://192.168.1.9:2419` (no
    /// trailing slash required — [`Self::artifact_url`] trims it).
    pub base_url: String,
}

impl ArtifactService {
    pub fn new(store: ArtifactStore, base_url: impl Into<String>) -> Self {
        Self {
            store,
            base_url: base_url.into(),
        }
    }

    /// The full URL for opening artifact `id`.
    pub fn artifact_url(&self, id: &str) -> String {
        format!("{}/artifacts/{id}", self.base_url.trim_end_matches('/'))
    }
}

static GLOBAL_ARTIFACTS: OnceLock<Arc<ArtifactService>> = OnceLock::new();

/// Publish the process-wide artifact service. First writer wins; later calls
/// are ignored (mirrors `set_global_notifier`).
pub fn set_artifact_service(service: Arc<ArtifactService>) {
    let _ = GLOBAL_ARTIFACTS.set(service);
}

/// The process-wide artifact service, or `None` when the server is not running.
pub fn artifact_service() -> Option<Arc<ArtifactService>> {
    GLOBAL_ARTIFACTS.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_url_joins_without_double_slash() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = ArtifactService::new(ArtifactStore::load(tmp.path()), "http://host:2419/");
        assert_eq!(svc.artifact_url("a1"), "http://host:2419/artifacts/a1");

        let svc2 = ArtifactService::new(ArtifactStore::load(tmp.path()), "http://host:2419");
        assert_eq!(svc2.artifact_url("a1"), "http://host:2419/artifacts/a1");
    }
}
