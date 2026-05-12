//! File-based session persistence for Claude CLI session mappings.
//!
//! Maps `(room_id, agent_name)` → UUID so the same OpenCode conversation
//! always resumes the same Claude CLI session, even across NSED restarts.
//!
//! Storage: `~/.nsed/sessions.json`
//!
//! Concurrency: an exclusive advisory file lock (via `fs4`) is held during
//! the entire read-modify-write cycle to prevent lost updates from parallel
//! agent processes.

use chrono::{DateTime, Utc};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;

/// Per-agent session entry in the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub uuid: String,
    pub last_used: DateTime<Utc>,
}

/// Top-level JSON structure: `{ room_id: { agent_name: SessionEntry } }`.
type SessionMap = HashMap<String, HashMap<String, SessionEntry>>;

/// Simple file-backed session store at `~/.nsed/sessions.json`.
///
/// All mutating operations acquire an exclusive advisory lock on the data file
/// for the duration of their read-modify-write cycle.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    /// Create a store at the default path (`~/.nsed/sessions.json`).
    ///
    /// Resolution order:
    /// 1. `NSED_SESSION_DIR` env var (if set and non-empty)
    /// 2. `$HOME/.nsed/` or `%USERPROFILE%\.nsed\`
    /// 3. A user-unique directory under the system temp dir (with warning)
    pub fn new() -> Self {
        if let Ok(explicit) = std::env::var("NSED_SESSION_DIR")
            && !explicit.is_empty()
        {
            return Self {
                path: PathBuf::from(explicit).join("sessions.json"),
            };
        }

        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return Self {
                path: PathBuf::from(home).join(".nsed").join("sessions.json"),
            };
        }

        // No home directory — fall back to a user-unique temp dir. Session
        // IDs can authenticate to the Claude CLI, so we avoid a shared
        // `/tmp/.nsed/` path that other local users could read or write.
        let uid_suffix = Self::user_suffix();
        let dir = std::env::temp_dir().join(format!("nsed-sessions-{uid_suffix}"));
        eprintln!(
            "WARN session_store: HOME/USERPROFILE not set, falling back to {}",
            dir.display()
        );
        Self {
            path: dir.join("sessions.json"),
        }
    }

    fn user_suffix() -> String {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".to_string())
    }

    /// Create a directory (including parents) with owner-only permissions on
    /// Unix. On non-Unix platforms, falls back to the default std API.
    ///
    /// Mode is also re-applied on already-existing directories so that
    /// installations that created `~/.nsed` before this hardening shipped
    /// get their permissions tightened on the next invocation.
    fn create_dir_secure(path: &std::path::Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
            // Tighten permissions on pre-existing parent directories too.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(path)
        }
    }

    /// Create a store at a custom path (for testing).
    #[cfg(test)]
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// Record a session mapping. Called after session UUID is computed.
    /// Best-effort: failures are logged but don't block the agent.
    ///
    /// Holds an exclusive file lock for the entire read-modify-write cycle.
    pub fn record(&self, room_id: &str, agent_name: &str, uuid: &str) -> anyhow::Result<()> {
        self.with_locked_map(|map| {
            let room = map.entry(room_id.to_string()).or_default();
            room.insert(
                agent_name.to_string(),
                SessionEntry {
                    uuid: uuid.to_string(),
                    last_used: Utc::now(),
                },
            );
        })
    }

    /// Look up an existing session UUID.
    ///
    /// Acquires a shared (read) lock to avoid reading a partially-written file
    /// during concurrent `record()` calls.
    #[allow(dead_code)] // Used by tests and the future cleanup worker.
    pub fn get(&self, room_id: &str, agent_name: &str) -> Option<String> {
        let map = self.load_with_shared_lock().ok()?;
        map.get(room_id)?.get(agent_name).map(|e| e.uuid.clone())
    }

    /// Remove entries older than `max_age`. Returns count of removed entries.
    ///
    /// Holds an exclusive file lock for the entire read-modify-write cycle.
    #[allow(dead_code)] // Used by tests and the future cleanup worker.
    pub fn cleanup_stale(&self, max_age: chrono::Duration) -> anyhow::Result<usize> {
        let cutoff = Utc::now() - max_age;
        let mut removed = 0;

        self.with_locked_map(|map| {
            map.retain(|_room, agents| {
                agents.retain(|_name, entry| {
                    let keep = entry.last_used >= cutoff;
                    if !keep {
                        removed += 1;
                    }
                    keep
                });
                !agents.is_empty()
            });
        })?;

        Ok(removed)
    }

    /// Acquire an exclusive lock, load the map, apply `f`, then save and release.
    ///
    /// The lock is held on the data file itself. We truncate + rewrite in place
    /// (no rename) so that competing processes waiting on the same fd see the
    /// updated content once they acquire the lock.
    fn with_locked_map(&self, f: impl FnOnce(&mut SessionMap)) -> anyhow::Result<()> {
        use std::io::Seek;

        if let Some(parent) = self.path.parent() {
            Self::create_dir_secure(parent)?;
        }

        // Open (or create) the data file and lock it exclusively.
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&self.path)?;
        // Enforce 0o600 on every open: mode() in OpenOptionsExt only applies
        // when the file is freshly created, so a pre-existing loose file
        // would otherwise stay world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
            {
                eprintln!(
                    "WARN session_store: could not tighten permissions on sessions.json: {e}"
                );
            }
        }
        file.lock_exclusive()?;

        // Load existing data while holding the lock.
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let mut map: SessionMap = if contents.trim().is_empty() {
            HashMap::new()
        } else {
            match serde_json::from_str(&contents) {
                Ok(m) => m,
                Err(e) => {
                    // Corrupt file (e.g. trailing characters from pre-fix writes).
                    // Start fresh rather than losing the ability to persist.
                    eprintln!("WARN session_store: corrupt sessions.json, resetting: {e}");
                    HashMap::new()
                }
            }
        };

        f(&mut map);

        // Rewrite in-place: seek to start, truncate, write.
        // This preserves the lock (same fd, same inode).
        let serialized = serde_json::to_string_pretty(&map)?;
        file.seek(std::io::SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(serialized.as_bytes())?;
        file.flush()?;

        // Lock is released when `file` is dropped.
        Ok(())
    }

    /// Read with a shared (read) lock to prevent dirty reads during writes.
    #[allow(clippy::incompatible_msrv)] // lock_shared() is from fs4::FileExt, not std
    #[allow(dead_code)] // Used by tests and `get()` (which is test-only for now).
    fn load_with_shared_lock(&self) -> anyhow::Result<SessionMap> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let mut file = OpenOptions::new().read(true).open(&self.path)?;
        file.lock_shared()?;

        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        // Lock released when `file` is dropped.
        if contents.trim().is_empty() {
            return Ok(HashMap::new());
        }
        Ok(serde_json::from_str(&contents)?)
    }

    /// Save helper for tests that need to prepopulate data.
    #[cfg(test)]
    fn save_map(&self, map: &SessionMap) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            Self::create_dir_secure(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(map)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store() -> (SessionStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = SessionStore::with_path(dir.path().join("sessions.json"));
        (store, dir)
    }

    #[test]
    fn record_and_get_roundtrip() {
        let (store, _dir) = test_store();
        store.record("room-1", "agent-a", "uuid-aaa").unwrap();
        assert_eq!(store.get("room-1", "agent-a"), Some("uuid-aaa".to_string()));
    }

    #[test]
    fn get_missing_returns_none() {
        let (store, _dir) = test_store();
        assert_eq!(store.get("no-room", "no-agent"), None);
    }

    #[test]
    fn multiple_agents_per_room() {
        let (store, _dir) = test_store();
        store.record("room-1", "agent-a", "uuid-a").unwrap();
        store.record("room-1", "agent-b", "uuid-b").unwrap();
        assert_eq!(store.get("room-1", "agent-a"), Some("uuid-a".to_string()));
        assert_eq!(store.get("room-1", "agent-b"), Some("uuid-b".to_string()));
    }

    #[test]
    fn multiple_rooms_isolated() {
        let (store, _dir) = test_store();
        store.record("room-1", "agent-a", "uuid-1a").unwrap();
        store.record("room-2", "agent-a", "uuid-2a").unwrap();
        assert_eq!(store.get("room-1", "agent-a"), Some("uuid-1a".to_string()));
        assert_eq!(store.get("room-2", "agent-a"), Some("uuid-2a".to_string()));
    }

    #[test]
    fn record_updates_existing() {
        let (store, _dir) = test_store();
        store.record("room-1", "agent-a", "uuid-old").unwrap();
        store.record("room-1", "agent-a", "uuid-new").unwrap();
        assert_eq!(store.get("room-1", "agent-a"), Some("uuid-new".to_string()));
    }

    #[test]
    fn cleanup_stale_removes_old_entries() {
        let (store, _dir) = test_store();
        let mut map: SessionMap = HashMap::new();
        map.entry("old-room".to_string()).or_default().insert(
            "agent-a".to_string(),
            SessionEntry {
                uuid: "old-uuid".to_string(),
                last_used: Utc::now() - chrono::Duration::days(30),
            },
        );
        map.entry("new-room".to_string()).or_default().insert(
            "agent-b".to_string(),
            SessionEntry {
                uuid: "new-uuid".to_string(),
                last_used: Utc::now(),
            },
        );
        store.save_map(&map).unwrap();

        let removed = store.cleanup_stale(chrono::Duration::days(7)).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.get("old-room", "agent-a"), None);
        assert_eq!(
            store.get("new-room", "agent-b"),
            Some("new-uuid".to_string())
        );
    }

    #[test]
    fn cleanup_removes_empty_rooms() {
        let (store, _dir) = test_store();
        // Room with only stale entries should be removed entirely.
        let mut map: SessionMap = HashMap::new();
        map.entry("stale-room".to_string()).or_default().insert(
            "agent-a".to_string(),
            SessionEntry {
                uuid: "gone".to_string(),
                last_used: Utc::now() - chrono::Duration::days(100),
            },
        );
        store.save_map(&map).unwrap();

        let removed = store.cleanup_stale(chrono::Duration::days(7)).unwrap();
        assert_eq!(removed, 1);

        // Verify file has no "stale-room" key
        let reloaded = store.load_with_shared_lock().unwrap();
        assert!(reloaded.is_empty());
    }

    #[test]
    fn empty_file_loads_ok() {
        let (store, _dir) = test_store();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, "").unwrap();
        assert_eq!(store.get("any", "any"), None);
    }

    #[test]
    fn persistence_across_instances() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sessions.json");

        let store1 = SessionStore::with_path(path.clone());
        store1.record("room-1", "agent-a", "uuid-abc").unwrap();
        drop(store1);

        let store2 = SessionStore::with_path(path);
        assert_eq!(
            store2.get("room-1", "agent-a"),
            Some("uuid-abc".to_string())
        );
    }

    #[test]
    fn corrupt_json_self_heals() {
        let (store, _dir) = test_store();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, "{ not valid json !!!").unwrap();
        // record should self-heal: reset to empty map and write the new entry
        store.record("room", "agent", "uuid").unwrap();
        assert_eq!(store.get("room", "agent"), Some("uuid".to_string()));
    }

    #[test]
    fn record_creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let store =
            SessionStore::with_path(dir.path().join("deep").join("nested").join("sessions.json"));
        store.record("room-1", "agent-a", "uuid-deep").unwrap();
        assert_eq!(
            store.get("room-1", "agent-a"),
            Some("uuid-deep".to_string())
        );
    }

    #[test]
    fn concurrent_records_no_data_loss() {
        // Simulate concurrent writes from multiple threads using the same store path.
        // Each thread records a unique agent; all should be present at the end.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sessions.json");
        let num_threads = 8;

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let store = SessionStore::with_path(p);
                    store
                        .record("room-shared", &format!("agent-{i}"), &format!("uuid-{i}"))
                        .unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let store = SessionStore::with_path(path);
        for i in 0..num_threads {
            assert_eq!(
                store.get("room-shared", &format!("agent-{i}")),
                Some(format!("uuid-{i}")),
                "agent-{i} should be present after concurrent writes"
            );
        }
    }

    #[test]
    fn cleanup_on_nonexistent_file_is_noop() {
        let (store, _dir) = test_store();
        // No file exists yet — cleanup should succeed with 0 removed
        let removed = store.cleanup_stale(chrono::Duration::days(7)).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn record_with_empty_strings() {
        let (store, _dir) = test_store();
        // Edge case: empty room_id and agent_name should still work
        store.record("", "", "uuid-empty").unwrap();
        assert_eq!(store.get("", ""), Some("uuid-empty".to_string()));
    }

    #[test]
    fn record_with_special_characters() {
        let (store, _dir) = test_store();
        // Room IDs from OpenCode may contain various characters
        store
            .record("oc-abc123-def456", "Sir_Wunderwaffel/v2", "uuid-special")
            .unwrap();
        assert_eq!(
            store.get("oc-abc123-def456", "Sir_Wunderwaffel/v2"),
            Some("uuid-special".to_string())
        );
    }

    #[test]
    fn last_used_updated_on_re_record() {
        let (store, _dir) = test_store();
        store.record("room", "agent", "uuid-1").unwrap();
        let first_map = store.load_with_shared_lock().unwrap();
        let first_ts = first_map["room"]["agent"].last_used;

        // Small sleep to ensure timestamp difference
        std::thread::sleep(std::time::Duration::from_millis(10));

        store.record("room", "agent", "uuid-2").unwrap();
        let second_map = store.load_with_shared_lock().unwrap();
        let second_ts = second_map["room"]["agent"].last_used;

        assert!(second_ts >= first_ts, "last_used should be updated");
        assert_eq!(second_map["room"]["agent"].uuid, "uuid-2");
    }
}
