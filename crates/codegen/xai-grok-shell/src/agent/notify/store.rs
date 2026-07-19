//! Server-level store of Web Push subscriptions.
//!
//! Subscriptions are registered by browser PWA clients over authenticated HTTP
//! routes on `grok agent serve` (see the push routes) and read back by the
//! `WebPushNotifier` when an event fires. The store is server-global rather than
//! a per-session tool resource because the axum route handlers that register
//! subscriptions run outside any agent session, and a push should reach a
//! device regardless of which session produced the event.
//!
//! Persisted to `$GROK_HOME/push/subscriptions.json` so subscriptions survive a
//! `grok agent serve` restart. Writes are atomic (temp file + rename) so a
//! concurrent reader never observes a torn file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// A single Web Push subscription as reported by the browser's `PushManager`.
/// `endpoint` is the push service URL; `p256dh` and `auth` are the client keys
/// used later for payload encryption (unused in the payload-less MVP, but stored
/// so encrypted payloads can be added without re-subscribing).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// Thread-safe, persisted collection of push subscriptions.
///
/// Cheap to clone (`Arc` inside): hand a clone to both the axum router state and
/// the notifier.
#[derive(Clone)]
pub struct PushStore {
    inner: Arc<RwLock<Vec<PushSubscription>>>,
    path: PathBuf,
}

impl PushStore {
    /// Load the store from `$GROK_HOME/push/subscriptions.json`, starting empty
    /// if the file is absent or unreadable (a corrupt/missing file must not stop
    /// the server from booting — subscriptions simply re-register).
    pub fn load(grok_home: &Path) -> Self {
        let path = grok_home.join("push").join("subscriptions.json");
        let subs = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<PushSubscription>>(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    // Present but unparseable: surface it so an operator knows
                    // why subscriptions vanished, then start clean.
                    tracing::warn!(
                        "push subscriptions file {:?} is corrupt, starting empty: {}",
                        path,
                        e
                    );
                    Vec::new()
                }
            },
            // Absent is the normal first-run case — silent.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(
                    "could not read push subscriptions {:?}, starting empty: {}",
                    path,
                    e
                );
                Vec::new()
            }
        };
        Self {
            inner: Arc::new(RwLock::new(subs)),
            path,
        }
    }

    /// Add a subscription, replacing any existing one with the same `endpoint`
    /// (a device re-subscribing must not create duplicates). Persists.
    pub async fn add(&self, sub: PushSubscription) {
        {
            let mut subs = self.inner.write().await;
            subs.retain(|s| s.endpoint != sub.endpoint);
            subs.push(sub);
        }
        self.persist().await;
    }

    /// Remove the subscription with this `endpoint` (client unsubscribed).
    /// Persists.
    pub async fn remove(&self, endpoint: &str) {
        {
            let mut subs = self.inner.write().await;
            subs.retain(|s| s.endpoint != endpoint);
        }
        self.persist().await;
    }

    /// Drop a subscription the push service reported as gone (HTTP 404/410).
    /// Semantically identical to `remove`; named for intent at the call site.
    pub async fn prune(&self, endpoint: &str) {
        self.remove(endpoint).await;
    }

    /// Snapshot of all current subscriptions.
    pub async fn all(&self) -> Vec<PushSubscription> {
        self.inner.read().await.clone()
    }

    /// Serialize and atomically write the current set to disk. Best-effort: a
    /// write failure is logged, not propagated — losing a persist is recoverable
    /// (the device re-subscribes) and must not break the caller's request.
    async fn persist(&self) {
        let snapshot = self.inner.read().await.clone();
        let path = self.path.clone();
        let result = tokio::task::spawn_blocking(move || Self::write_atomic(&path, &snapshot))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        if let Err(e) = result {
            tracing::warn!("failed to persist push subscriptions to {:?}: {}", self.path, e);
        }
    }

    fn write_atomic(path: &Path, subs: &[PushSubscription]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(subs).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        // Restrict to owner-only BEFORE the rename; the mode carries over to the
        // final path atomically. The file holds per-device push secrets
        // (p256dh/auth), so it must not be world-readable — matches vapid.json.
        Self::restrict_permissions(&tmp)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    #[cfg(unix)]
    fn restrict_permissions(path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)
    }

    #[cfg(not(unix))]
    fn restrict_permissions(_path: &Path) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(endpoint: &str) -> PushSubscription {
        PushSubscription {
            endpoint: endpoint.into(),
            p256dh: "key".into(),
            auth: "auth".into(),
        }
    }

    #[tokio::test]
    async fn push_store_dedups_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let s = PushStore::load(tmp.path());

        // Adding the same endpoint twice must not create a duplicate.
        s.add(sub("https://push/1")).await;
        s.add(sub("https://push/1")).await;
        assert_eq!(s.all().await.len(), 1);

        // A fresh store loaded from the same path sees the persisted sub.
        let s2 = PushStore::load(tmp.path());
        assert_eq!(s2.all().await, vec![sub("https://push/1")]);

        // prune (410 Gone) drops it, and the drop is persisted.
        s2.prune("https://push/1").await;
        assert_eq!(s2.all().await.len(), 0);
        let s3 = PushStore::load(tmp.path());
        assert!(s3.all().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persisted_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let s = PushStore::load(tmp.path());
        s.add(sub("https://push/secure")).await;
        let path = tmp.path().join("push").join("subscriptions.json");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "subscriptions.json must be owner-only (holds push secrets)");
    }

    #[tokio::test]
    async fn corrupt_file_starts_empty_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("push");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("subscriptions.json"), b"{ this is not valid json").unwrap();
        let s = PushStore::load(tmp.path());
        assert!(s.all().await.is_empty());
    }

    #[tokio::test]
    async fn add_and_remove_multiple_endpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let s = PushStore::load(tmp.path());
        s.add(sub("https://push/a")).await;
        s.add(sub("https://push/b")).await;
        assert_eq!(s.all().await.len(), 2);
        s.remove("https://push/a").await;
        let remaining = s.all().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].endpoint, "https://push/b");
    }
}
