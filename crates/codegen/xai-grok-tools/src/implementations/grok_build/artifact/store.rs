//! On-disk, in-memory artifact store.
//!
//! Each artifact is persisted as a single `<id>.json` file in the store
//! directory, so the set of files IS the index — no separate manifest to keep
//! in sync. The store keeps an in-memory map for fast lookup, hydrated from
//! disk on startup so published artifacts survive a server restart.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use super::types::ArtifactFormat;

/// A published artifact: its metadata plus the raw body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub title: String,
    pub format: ArtifactFormat,
    pub content: String,
    /// Creation time (Unix ms). Used only to order the index newest-first.
    pub created_ms: u64,
}

/// Lightweight index row (no body) for the `/artifacts` listing.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactMeta {
    pub id: String,
    pub title: String,
    pub format: ArtifactFormat,
    pub created_ms: u64,
}

/// Current Unix time in milliseconds (0 if the clock is before the epoch).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generate a stable id from the title, body, and creation time. Uses the full
/// 64-bit hash to keep birthday collisions negligible; the timestamp keeps two
/// publishes of identical content from colliding.
pub fn gen_id(title: &str, content: &str, created_ms: u64) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut h);
    content.hash(&mut h);
    created_ms.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// True when `id` is a safe single path segment (no traversal, no separators).
/// The tool generates ids itself, but `input.id` (update-in-place) is
/// caller-controlled and reaches the filesystem, so it is validated here.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Stores artifacts on disk and in memory.
pub struct ArtifactStore {
    dir: PathBuf,
    map: RwLock<HashMap<String, Artifact>>,
}

impl ArtifactStore {
    /// Open (creating if needed) the store at `dir` and hydrate every
    /// `<id>.json` already present. A malformed file is skipped, not fatal.
    pub fn load(dir: &Path) -> Self {
        let dir = dir.join("artifacts");
        let _ = std::fs::create_dir_all(&dir);
        let mut map = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Artifact>(&s).ok())
                {
                    // Defense in depth: an id that isn't a safe segment could
                    // only arrive from a hand-planted file (the tool never
                    // writes one), so drop it rather than index it.
                    Some(a) if is_valid_id(&a.id) => {
                        map.insert(a.id.clone(), a);
                    }
                    Some(a) => {
                        tracing::warn!("skipping artifact with invalid id {:?}", a.id)
                    }
                    None => tracing::warn!("skipping unreadable artifact {}", path.display()),
                }
            }
        }
        Self {
            dir,
            map: RwLock::new(map),
        }
    }

    /// The store directory (where `<id>.json` files live).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Persist and index an artifact (overwriting any existing one with the
    /// same id). Returns an error only if the file could not be written.
    pub fn put(&self, artifact: Artifact) -> std::io::Result<()> {
        let json = serde_json::to_string(&artifact)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let path = self.dir.join(format!("{}.json", artifact.id));
        // Write-then-rename so a crash mid-write can't leave a half-written
        // `.json` that `load` would skip (losing the artifact). The `.tmp`
        // suffix is not `.json`, so `load` ignores any stray temp file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        self.map
            .write()
            .expect("artifact map lock poisoned")
            .insert(artifact.id.clone(), artifact);
        Ok(())
    }

    /// Fetch an artifact by id.
    pub fn get(&self, id: &str) -> Option<Artifact> {
        self.map
            .read()
            .expect("artifact map lock poisoned")
            .get(id)
            .cloned()
    }

    /// All artifacts as index rows, newest first (ties broken by id for a
    /// stable order).
    pub fn list(&self) -> Vec<ArtifactMeta> {
        let mut rows: Vec<ArtifactMeta> = self
            .map
            .read()
            .expect("artifact map lock poisoned")
            .values()
            .map(|a| ArtifactMeta {
                id: a.id.clone(),
                title: a.title.clone(),
                format: a.format,
                created_ms: a.created_ms,
            })
            .collect();
        rows.sort_by(|a, b| {
            b.created_ms
                .cmp(&a.created_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(id: &str, title: &str, created_ms: u64) -> Artifact {
        Artifact {
            id: id.to_string(),
            title: title.to_string(),
            format: ArtifactFormat::Markdown,
            content: format!("# {title}"),
            created_ms,
        }
    }

    #[test]
    fn put_get_roundtrip_and_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::load(tmp.path());
        assert!(store.get("a1").is_none());

        store.put(artifact("a1", "First", 100)).unwrap();
        assert_eq!(store.get("a1").unwrap().title, "First");

        // Same id overwrites in place.
        store.put(artifact("a1", "Updated", 200)).unwrap();
        assert_eq!(store.get("a1").unwrap().title, "Updated");
    }

    #[test]
    fn hydrates_from_disk_on_reload() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let store = ArtifactStore::load(tmp.path());
            store.put(artifact("keep", "Persisted", 1)).unwrap();
        }
        // A fresh store over the same dir must see the persisted artifact.
        let reopened = ArtifactStore::load(tmp.path());
        assert_eq!(reopened.get("keep").unwrap().title, "Persisted");
    }

    #[test]
    fn list_is_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::load(tmp.path());
        store.put(artifact("old", "Old", 100)).unwrap();
        store.put(artifact("new", "New", 300)).unwrap();
        store.put(artifact("mid", "Mid", 200)).unwrap();
        let ids: Vec<String> = store.list().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["new", "mid", "old"]);
    }

    #[test]
    fn id_validation_rejects_traversal_and_separators() {
        assert!(is_valid_id("a1b2c3d4"));
        assert!(is_valid_id("my-report_2"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("../etc/passwd"));
        assert!(!is_valid_id("a/b"));
        assert!(!is_valid_id("a.b"));
        assert!(!is_valid_id(&"x".repeat(65)));
    }

    #[test]
    fn load_skips_hand_planted_invalid_id_and_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::load(tmp.path());
        store.put(artifact("good", "Good", 1)).unwrap();
        let dir = store.dir().to_path_buf();
        // A hand-planted file whose embedded id is a traversal segment.
        std::fs::write(
            dir.join("evil.json"),
            serde_json::to_string(&artifact("../ws", "Evil", 2)).unwrap(),
        )
        .unwrap();
        // A stray temp file from an interrupted write.
        std::fs::write(dir.join("good.json.tmp"), "{partial").unwrap();

        let reopened = ArtifactStore::load(tmp.path());
        assert!(reopened.get("good").is_some());
        assert!(reopened.get("../ws").is_none(), "invalid id must be dropped");
        let ids: Vec<String> = reopened.list().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["good"], "only the valid artifact is indexed");
    }

    #[test]
    fn gen_id_is_stable_and_hex() {
        let a = gen_id("t", "c", 42);
        let b = gen_id("t", "c", 42);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(is_valid_id(&a), "a generated id must pass id validation");
        // Different creation time → different id (no collision on identical body).
        assert_ne!(gen_id("t", "c", 42), gen_id("t", "c", 43));
    }
}
