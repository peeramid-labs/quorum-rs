//! Client-side epic sync: clone the epic once (a full local replica for fast, offline
//! browsing), then `git pull` on each `project_advanced` notification (delta sync).
//!
//! Bulk content moves over git (clone/pull); the NATS `project_advanced` event is only
//! the "pull now" trigger. The client is read-only — it fast-forwards its replica,
//! never pushes. The replica ops here are testable against local repos; the subscribe
//! loop wraps them.

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `project_advanced` payload the worker publishes on `<prefix>.project.<id>.advanced`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectAdvanced {
    pub project_id: String,
    #[serde(default)]
    pub head: Option<String>,
}

/// A client's local replica of one epic, kept fresh by pulling on advanced events.
#[derive(Debug, Clone)]
pub struct EpicReplica {
    /// Path-independent project id (epic root-commit) this replica tracks.
    pub project_id: String,
    pub remote_url: String,
    /// Local replica path.
    pub local: PathBuf,
}

impl EpicReplica {
    /// True when `event` is for THIS project and reports a head we don't already hold —
    /// i.e. a pull is warranted. A different project, or the same head we already have,
    /// is skipped (no redundant pull).
    pub fn should_sync(&self, event: &ProjectAdvanced, local_head: Option<&str>) -> bool {
        if event.project_id != self.project_id {
            return false;
        }
        match (event.head.as_deref(), local_head) {
            (Some(remote), Some(local)) => remote != local, // advanced past our replica
            (Some(_), None) => true,                        // no replica head yet
            (None, _) => true,                              // no head info → pull to be safe
        }
    }

    /// The replica's current HEAD, or `None` if it isn't cloned yet.
    pub fn local_head(&self) -> Option<String> {
        git(&self.local, &["rev-parse", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// `true` once the replica has been cloned.
    pub fn is_cloned(&self) -> bool {
        self.local.join(".git").exists()
    }

    /// Clone the epic (full replica, submodules included) if not already present. The
    /// initial bulk transfer; subsequent updates are [`sync`](Self::sync) pulls.
    pub fn ensure_cloned(&self) -> Result<()> {
        if self.is_cloned() {
            return Ok(());
        }
        if let Some(parent) = self.local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run(Command::new("git")
            .args(["clone", "--recursive", &self.remote_url])
            .arg(&self.local))?;
        Ok(())
    }

    /// Fast-forward the replica to the remote tip and refresh submodules (delta sync).
    /// Read-only: `--ff-only` so a divergence surfaces as an error rather than a merge.
    /// Returns the new local head.
    pub fn sync(&self) -> Result<String> {
        git(&self.local, &["pull", "--ff-only"])?;
        // Submodules follow the superproject's gitlinks; best-effort (a missing one
        // shouldn't fail the whole sync).
        let _ = git(
            &self.local,
            &["submodule", "update", "--init", "--recursive"],
        );
        self.local_head()
            .ok_or_else(|| anyhow!("no HEAD after sync of {:?}", self.local))
    }

    /// Handle one advanced event: clone if needed, then pull when the event advances
    /// past our replica. Returns `Some(new_head)` if a sync ran, `None` if skipped.
    pub fn on_event(&self, event: &ProjectAdvanced) -> Result<Option<String>> {
        self.ensure_cloned()?;
        if self.should_sync(event, self.local_head().as_deref()) {
            Ok(Some(self.sync()?))
        } else {
            Ok(None)
        }
    }
}

/// The read side of [`project_registry::advanced_notification`](crate::project_registry::advanced_notification):
/// a client subscribes here to be told its epic advanced and pull. Kept in one place
/// so publisher and subscriber can't drift.
pub fn advanced_subject(subject_prefix: &str, project_id: &str) -> String {
    format!("{subject_prefix}.project.{project_id}.advanced")
}

/// Subscribe to the replica's `project_advanced` events and pull on each. Clones once
/// up front so browsing works before the first event. Runs until the subscription
/// ends; per-event errors are logged, not fatal (a transient git failure shouldn't
/// kill the loop).
pub async fn run_sync_loop(
    nats: &async_nats::Client,
    subject_prefix: &str,
    replica: &EpicReplica,
) -> Result<()> {
    use futures::StreamExt;
    replica.ensure_cloned()?;
    let subject = advanced_subject(subject_prefix, &replica.project_id);
    let mut sub = nats
        .subscribe(subject.clone())
        .await
        .map_err(|e| anyhow!("subscribe {subject}: {e}"))?;
    tracing::info!(subject = %subject, local = ?replica.local, "epic sync loop started");
    while let Some(msg) = sub.next().await {
        match serde_json::from_slice::<ProjectAdvanced>(&msg.payload) {
            Ok(event) => match replica.on_event(&event) {
                Ok(Some(head)) => {
                    tracing::info!(project = %event.project_id, head = %head, "epic synced")
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "epic sync failed; will retry on next event"),
            },
            Err(e) => tracing::warn!(error = %e, "unparseable project_advanced event"),
        }
    }
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    run(Command::new("git").arg("-C").arg(dir).args(args))
}

fn run(cmd: &mut Command) -> Result<String> {
    // Clear inherited git env so every op targets the replica by path, never an
    // ambient `GIT_DIR`. `GIT_DIR` overrides `-C`, so without this a caller running
    // under a git hook (GIT_DIR set) — or a test in a pre-commit run — would operate
    // on the wrong repo (e.g. `init --bare` reinitialising it as bare).
    cmd.env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv(project: &str, head: Option<&str>) -> ProjectAdvanced {
        ProjectAdvanced {
            project_id: project.to_string(),
            head: head.map(str::to_string),
        }
    }

    fn replica(project: &str, remote: &Path, local: &Path) -> EpicReplica {
        EpicReplica {
            project_id: project.to_string(),
            remote_url: remote.to_string_lossy().into_owned(),
            local: local.to_path_buf(),
        }
    }

    #[test]
    fn advanced_subject_is_project_scoped() {
        assert_eq!(
            advanced_subject("nsed", "root-sha"),
            "nsed.project.root-sha.advanced"
        );
    }

    #[test]
    fn should_sync_only_for_this_project_and_a_new_head() {
        let r = replica("mine", Path::new("/x"), Path::new("/y"));
        // Same project, advanced past local → pull.
        assert!(r.should_sync(&adv("mine", Some("h2")), Some("h1")));
        // Same project, same head → skip (no redundant pull).
        assert!(!r.should_sync(&adv("mine", Some("h1")), Some("h1")));
        // No local replica head yet → pull.
        assert!(r.should_sync(&adv("mine", Some("h1")), None));
        // No head info → pull to be safe.
        assert!(r.should_sync(&adv("mine", None), Some("h1")));
        // A different project → never.
        assert!(!r.should_sync(&adv("theirs", Some("h9")), Some("h1")));
    }

    fn g(dir: &Path, args: &[&str]) {
        // Same git-env isolation as `run` — a pre-commit test run inherits GIT_DIR,
        // which would override `-C` and hit the real repo.
        let o = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }

    fn rev(dir: &Path) -> String {
        let o = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    #[test]
    fn clone_then_pull_advances_the_replica() {
        let sbx = std::env::temp_dir().join(format!("qr-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sbx);
        // Bare origin + a working clone to push commits A then B.
        let origin = sbx.join("epic.git");
        std::fs::create_dir_all(&origin).unwrap();
        g(&origin, &["init", "-q", "-b", "main", "--bare"]);
        let work = sbx.join("work");
        g(
            &sbx,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        g(&work, &["config", "user.email", "a@b"]);
        g(&work, &["config", "user.name", "a"]);
        std::fs::write(work.join("kpi.md"), "A\n").unwrap();
        g(&work, &["add", "-A"]);
        g(&work, &["commit", "-qm", "A"]);
        g(&work, &["push", "-q", "origin", "main"]);
        let a = rev(&work);

        // Client replica: clone from origin, land at A.
        let r = replica("proj", &origin, &sbx.join("replica"));
        assert!(!r.is_cloned());
        r.ensure_cloned().unwrap();
        assert!(r.is_cloned());
        assert_eq!(r.local_head().as_deref(), Some(a.as_str()), "replica at A");

        // Consensus advances origin to B; the advanced event triggers a pull.
        std::fs::write(work.join("kpi.md"), "B\n").unwrap();
        g(&work, &["add", "-A"]);
        g(&work, &["commit", "-qm", "B"]);
        g(&work, &["push", "-q", "origin", "main"]);
        let b = rev(&work);

        let synced = r.on_event(&adv("proj", Some(&b))).unwrap();
        assert_eq!(synced.as_deref(), Some(b.as_str()), "on_event pulled to B");
        assert_eq!(
            r.local_head().as_deref(),
            Some(b.as_str()),
            "replica now at B"
        );

        // A repeat event at B → no-op (already there).
        assert_eq!(
            r.on_event(&adv("proj", Some(&b))).unwrap(),
            None,
            "same head → skip"
        );
        let _ = std::fs::remove_dir_all(&sbx);
    }
}
