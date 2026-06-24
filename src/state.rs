use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Dedup key for a feed entry: a stable `id` paired with its `updated` timestamp.
///
/// Statuspage/Instatus incidents keep a stable `<id>` whose `<updated>` bumps on
/// each progress update, so the `(id, updated)` pair makes each update notify
/// exactly once. `updated` serializes as RFC3339 (chrono's default serde).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SeenKey {
    pub id: String,
    pub updated: DateTime<Utc>,
}

/// On-disk representation: `{"seen":[{"id":"...","updated":"<RFC3339>"}]}`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SeenFile {
    seen: Vec<SeenKey>,
}

/// The in-memory seen-set of `(id, updated)` keys.
#[derive(Debug, Default)]
pub struct SeenStore {
    seen: HashSet<SeenKey>,
}

impl SeenStore {
    /// Load the seen-store from `path`.
    ///
    /// - Missing file: return an empty store (not an error — first run).
    /// - Corrupt/unparseable file: log a warning and return an empty store.
    ///   `seen.json` is machine-written and tolerated/reset, unlike the
    ///   user-authored config.
    pub fn load(path: &Path) -> SeenStore {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return SeenStore::default();
            }
            Err(err) => {
                log::warn!(
                    "could not read seen-store {}: {err}; starting empty",
                    path.display()
                );
                return SeenStore::default();
            }
        };

        match serde_json::from_str::<SeenFile>(&contents) {
            Ok(file) => SeenStore {
                seen: file.seen.into_iter().collect(),
            },
            Err(err) => {
                log::warn!(
                    "could not parse seen-store {}: {err}; starting empty",
                    path.display()
                );
                SeenStore::default()
            }
        }
    }

    /// Persist the seen-store to `path` via an atomic temp-file + rename.
    ///
    /// Writes to a temp file in the same directory as `path`, then renames it
    /// over the destination so a reader never observes a partially written file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create state dir: {}", parent.display()))?;
        }

        let file = SeenFile {
            seen: self.seen.iter().cloned().collect(),
        };
        let serialized = serde_json::to_string(&file).context("failed to serialize seen-store")?;

        // Unique temp name (pid) so two concurrent writers can't collide on a
        // fixed name; still in the same dir so the rename stays atomic.
        let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp_path, serialized)
            .with_context(|| format!("failed to write temp state file: {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to rename {} -> {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    /// Whether `key` is already in the seen-set.
    pub fn contains(&self, key: &SeenKey) -> bool {
        self.seen.contains(key)
    }

    /// Record `key` as seen.
    pub fn insert(&mut self, key: SeenKey) {
        self.seen.insert(key);
    }

    /// Drop keys whose `updated` is older than `now - max_age_minutes`.
    ///
    /// Keeps the seen-set bounded: the age window already prevents notifying on
    /// anything older than the window, so older keys can never be needed again.
    pub fn prune(&mut self, now: DateTime<Utc>, max_age_minutes: i64) {
        let cutoff = now - Duration::minutes(max_age_minutes);
        self.seen.retain(|key| key.updated >= cutoff);
    }
}

/// Test-only accessors, kept out of the production surface.
#[cfg(test)]
impl SeenStore {
    /// Number of keys currently held.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether the store holds no keys.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Build a unique temp path so file-IO tests never collide.
    fn unique_temp_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("status-notifications-seen-{pid}-{n}.json"))
    }

    fn key(id: &str, updated: DateTime<Utc>) -> SeenKey {
        SeenKey {
            id: id.to_string(),
            updated,
        }
    }

    #[test]
    fn save_load_round_trip_preserves_keys() {
        let path = unique_temp_path();
        let now = Utc::now();

        let mut store = SeenStore::default();
        store.insert(key("incident-1", now));
        store.insert(key("incident-2", now - Duration::minutes(5)));
        store.save(&path).expect("save");

        let loaded = SeenStore::load(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&key("incident-1", now)));
        assert!(loaded.contains(&key("incident-2", now - Duration::minutes(5))));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn prune_removes_only_keys_older_than_window() {
        let now = Utc::now();
        let mut store = SeenStore::default();

        let recent = key("recent", now - Duration::minutes(3));
        let edge = key("edge", now - Duration::minutes(10));
        let old = key("old", now - Duration::minutes(11));

        store.insert(recent.clone());
        store.insert(edge.clone());
        store.insert(old.clone());

        store.prune(now, 10);

        assert!(store.contains(&recent), "recent key kept");
        assert!(store.contains(&edge), "key at the window edge kept");
        assert!(!store.contains(&old), "key older than the window dropped");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let path = unique_temp_path();
        assert!(!path.exists());

        let store = SeenStore::load(&path);
        assert!(store.is_empty());
    }

    #[test]
    fn save_creates_missing_parent_directory() {
        // Save into a path under a non-existent subdir of a unique temp dir.
        let base = unique_temp_path(); // a unique *.json path; reuse as a dir stem
        let dir = base.with_extension("d").join("nested");
        let path = dir.join("seen.json");
        assert!(!dir.exists());

        let now = Utc::now();
        let mut store = SeenStore::default();
        store.insert(key("incident-1", now));
        store
            .save(&path)
            .expect("save should create missing parents");

        let loaded = SeenStore::load(&path);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(&key("incident-1", now)));

        // Clean up the whole created tree.
        std::fs::remove_dir_all(base.with_extension("d")).ok();
    }

    #[test]
    fn load_corrupt_file_returns_empty_store() {
        let path = unique_temp_path();
        std::fs::write(&path, b"\x00not valid json at all{{{").expect("write garbage");

        let store = SeenStore::load(&path);
        assert!(store.is_empty());

        std::fs::remove_file(&path).ok();
    }
}
