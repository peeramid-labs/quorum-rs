//! Git-over-NATS read bridge — the pure handler.
//!
//! The app client runs on another network with no filesystem and no forgejo access,
//! so it can neither read the epic worktrees nor clone the repo. A fleet node that
//! DOES hold the epic answers the client's read requests over NATS from its local
//! clone. This module is that answer, pure and testable against a local epic path:
//!
//! - **Browse plane** — [`ReadRequest::FilesList`] / [`ReadRequest::FileRead`]
//!   (`git ls-tree` / `git show`), optionally at a consensus commit.
//! - **Patch plane** — [`ReadRequest::RefsList`] (job branches + `base/<job>` /
//!   `head/<job>` audit tags) and [`ReadRequest::Diff`] (a job branch vs its base),
//!   so the client reconstructs the BranchGraph + derives hunks by content-addressing
//!   WITHOUT cloning.
//!
//! Every op is read-only and scoped to the epic path; git env is scrubbed so an
//! ambient `GIT_DIR` can't redirect the read (same isolation as `project_sync`).

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// A read request from the remote client. `at` (when present) pins reads to a
/// specific commit/ref for point-in-time "what did job X produce".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReadRequest {
    /// List tree entries under `path` (empty = repo root), at `at` or HEAD.
    FilesList {
        #[serde(default)]
        path: String,
        #[serde(default)]
        at: Option<String>,
    },
    /// Read one file's bytes (as UTF-8 text), at `at` or HEAD.
    FileRead {
        path: String,
        #[serde(default)]
        at: Option<String>,
    },
    /// List the patch-plane refs: `job/*` branches + `base/*` / `head/*` tags.
    RefsList,
    /// Diff a ref against a base (job branch vs `base/<job>`) — the client hunks it.
    Diff { base: String, target: String },
}

/// A reply. `paths` for a listing, `content` for a file/diff, `refs` for RefsList.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadReply {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

/// Serve a request SCOPED to a project id. The node answers only for epics it
/// actually holds (`held` maps `project_id` → local epic path — the same key the
/// [`ProjectRegistry`](crate::project_registry) groups agents on). A request for a
/// project this node doesn't hold is REFUSED, never served from the wrong epic.
///
/// This is the authz boundary: combined with the NATS layer (a node only subscribes
/// the read subject for projects it holds, and the client's identity scopes which
/// projects it may address), a client can read epic X iff it can reach an agent that
/// holds X — "shares the same fs access, by id, so can see the dir".
pub fn serve_scoped(
    held: &std::collections::HashMap<String, std::path::PathBuf>,
    project_id: &str,
    req: &ReadRequest,
) -> Result<ReadReply> {
    let epic = held.get(project_id).ok_or_else(|| {
        anyhow!("this node does not hold project {project_id:?} — read refused (out of scope)")
    })?;
    serve_read(epic, req)
}

/// Reject any path that could escape the epic tree. git `show <ref>:<path>` is already
/// repo-confined (it resolves against the tree root, never the filesystem), but we
/// fail-closed BEFORE touching git so an escape attempt is a clear error, never a
/// surprising git message — and so the confinement is an explicit, tested invariant.
/// Rejects: absolute paths, a leading `/`, any `..` component, and backslashes.
fn reject_unsafe_path(path: &str) -> Result<()> {
    let p = path.replace('\\', "/");
    if p.starts_with('/') || Path::new(&p).is_absolute() {
        bail!("path {path:?} is absolute — reads are confined to the epic tree");
    }
    if p.split('/').any(|c| c == "..") {
        bail!("path {path:?} escapes the epic tree (`..` component)");
    }
    Ok(())
}

/// Serve one read request from the fleet node's local epic clone. Read-only, and
/// strictly confined to the epic tree (paths validated; git resolves refs only within
/// this repo). Reads reflect the epic's CURRENT git state, so results update the moment
/// a deliberation commit lands.
pub fn serve_read(epic: &Path, req: &ReadRequest) -> Result<ReadReply> {
    match req {
        ReadRequest::FilesList { path, at } => {
            reject_unsafe_path(path)?;
            let at = at.as_deref().unwrap_or("HEAD");
            // `git ls-tree <at> -- <path>`; `--name-only` for the paths, `-r` NOT set
            // so a listing is one level (dirs shown as entries), matching a browser.
            let spec = if path.is_empty() {
                at.to_string()
            } else {
                format!("{at}:{}", path.trim_end_matches('/'))
            };
            let out = git(epic, &["ls-tree", "--name-only", &spec])?;
            let mut paths: Vec<String> = out.lines().map(str::to_string).collect();
            // ls-tree of a subtree yields bare names; prefix them with the dir so the
            // client gets full repo-relative paths.
            if !path.is_empty() {
                let dir = path.trim_end_matches('/');
                paths = paths.into_iter().map(|p| format!("{dir}/{p}")).collect();
            }
            Ok(ReadReply {
                paths,
                ..Default::default()
            })
        }
        ReadRequest::FileRead { path, at } => {
            reject_unsafe_path(path)?;
            let at = at.as_deref().unwrap_or("HEAD");
            let content = git(epic, &["show", &format!("{at}:{path}")])?;
            Ok(ReadReply {
                content: Some(content),
                ..Default::default()
            })
        }
        ReadRequest::RefsList => {
            // Patch-plane refs only: job branches + base/head audit tags.
            let branches = git(
                epic,
                &[
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads/job/",
                    "refs/remotes/origin/job/",
                ],
            )
            .unwrap_or_default();
            let tags = git(epic, &["tag", "-l", "base/*", "head/*"]).unwrap_or_default();
            let mut refs: Vec<String> = branches
                .lines()
                .chain(tags.lines())
                .map(str::to_string)
                .filter(|r| !r.is_empty())
                .collect();
            refs.sort();
            refs.dedup();
            Ok(ReadReply {
                refs,
                ..Default::default()
            })
        }
        ReadRequest::Diff { base, target } => {
            let content = git(epic, &["diff", &format!("{base}..{target}")])?;
            Ok(ReadReply {
                content: Some(content),
                ..Default::default()
            })
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        // Scrub inherited git env so an ambient GIT_DIR can't redirect the read off
        // the epic (same isolation as project_sync).
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow!("git spawn: {e}"))?;
    if !out.status.success() {
        bail!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(dir: &Path, args: &[&str]) {
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

    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Build a tiny epic: a base commit, a job branch with an added file (a "proposal"),
    /// and a `base/<job>` tag — enough to exercise all four ops. A per-call counter
    /// keeps parallel tests on distinct temp dirs (no `git init` race).
    fn epic() -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("qr-epicread-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        g(&dir, &["init", "-q", "-b", "main"]);
        g(&dir, &["config", "user.email", "a@b"]);
        g(&dir, &["config", "user.name", "a"]);
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("README.md"), "root\n").unwrap();
        std::fs::write(dir.join("docs/spec.md"), "spec v1\n").unwrap();
        g(&dir, &["add", "-A"]);
        g(&dir, &["commit", "-qm", "base"]);
        g(&dir, &["tag", "base/job1"]);
        // A proposal on a job branch: edit the spec.
        g(&dir, &["checkout", "-q", "-b", "job/job1/AgentA"]);
        std::fs::write(dir.join("docs/spec.md"), "spec v2 — AgentA\n").unwrap();
        g(&dir, &["add", "-A"]);
        g(&dir, &["commit", "-qm", "AgentA proposal"]);
        g(&dir, &["checkout", "-q", "main"]);
        dir
    }

    #[test]
    fn files_list_at_root_and_subdir() {
        let e = epic();
        let root = serve_read(
            &e,
            &ReadRequest::FilesList {
                path: String::new(),
                at: None,
            },
        )
        .unwrap();
        assert!(root.paths.contains(&"README.md".to_string()));
        assert!(root.paths.contains(&"docs".to_string()));
        let sub = serve_read(
            &e,
            &ReadRequest::FilesList {
                path: "docs".into(),
                at: None,
            },
        )
        .unwrap();
        assert_eq!(
            sub.paths,
            vec!["docs/spec.md".to_string()],
            "subdir paths are repo-relative"
        );
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn file_read_at_head_and_at_a_ref() {
        let e = epic();
        let head = serve_read(
            &e,
            &ReadRequest::FileRead {
                path: "docs/spec.md".into(),
                at: None,
            },
        )
        .unwrap();
        assert_eq!(head.content.as_deref(), Some("spec v1"));
        // Point-in-time: read the proposal's version off the job branch.
        let prop = serve_read(
            &e,
            &ReadRequest::FileRead {
                path: "docs/spec.md".into(),
                at: Some("job/job1/AgentA".into()),
            },
        )
        .unwrap();
        assert_eq!(prop.content.as_deref(), Some("spec v2 — AgentA"));
        // Missing file → error, not a silent empty.
        assert!(
            serve_read(
                &e,
                &ReadRequest::FileRead {
                    path: "nope.md".into(),
                    at: None
                }
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn refs_list_exposes_job_branches_and_audit_tags() {
        let e = epic();
        let refs = serve_read(&e, &ReadRequest::RefsList).unwrap().refs;
        assert!(
            refs.contains(&"job/job1/AgentA".to_string()),
            "job branch listed: {refs:?}"
        );
        assert!(
            refs.contains(&"base/job1".to_string()),
            "base tag listed: {refs:?}"
        );
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn diff_of_a_proposal_vs_base_is_hunkable_by_the_client() {
        let e = epic();
        let d = serve_read(
            &e,
            &ReadRequest::Diff {
                base: "base/job1".into(),
                target: "job/job1/AgentA".into(),
            },
        )
        .unwrap()
        .content
        .unwrap();
        // A real unified diff the client can parse + content-address into hunks.
        assert!(d.contains("docs/spec.md"), "diff names the changed file");
        assert!(
            d.contains("-spec v1") && d.contains("+spec v2 — AgentA"),
            "diff shows the hunk: {d}"
        );
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn reads_are_confined_to_the_epic_tree() {
        let e = epic();
        // Plant a secret OUTSIDE the epic (sibling dir) that an escape would reach.
        let secret = e.parent().unwrap().join("SECRET.txt");
        std::fs::write(&secret, "top secret\n").unwrap();
        for bad in [
            "../SECRET.txt",
            "../../etc/passwd",
            "/etc/passwd",
            "docs/../../SECRET.txt",
        ] {
            let read = serve_read(
                &e,
                &ReadRequest::FileRead {
                    path: bad.into(),
                    at: None,
                },
            );
            assert!(read.is_err(), "escape path {bad:?} must be rejected");
            let list = serve_read(
                &e,
                &ReadRequest::FilesList {
                    path: bad.into(),
                    at: None,
                },
            );
            assert!(list.is_err(), "escape list {bad:?} must be rejected");
        }
        // The secret is never returned by any in-tree read.
        let ok = serve_read(
            &e,
            &ReadRequest::FileRead {
                path: "README.md".into(),
                at: None,
            },
        )
        .unwrap();
        assert!(!ok.content.unwrap().contains("secret"));
        let _ = std::fs::remove_file(&secret);
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn reads_reflect_the_current_git_state_after_a_deliberation_commit() {
        let e = epic();
        let before = serve_read(
            &e,
            &ReadRequest::FileRead {
                path: "README.md".into(),
                at: None,
            },
        )
        .unwrap()
        .content
        .unwrap();
        assert_eq!(before, "root");
        // A deliberation commit lands on the epic (winner merged to main).
        std::fs::write(e.join("README.md"), "root — updated by consensus\n").unwrap();
        g(&e, &["add", "-A"]);
        g(&e, &["commit", "-qm", "consensus update"]);
        // The bridge serves the NEW state with no cache / restart.
        let after = serve_read(
            &e,
            &ReadRequest::FileRead {
                path: "README.md".into(),
                at: None,
            },
        )
        .unwrap()
        .content
        .unwrap();
        assert_eq!(
            after, "root — updated by consensus",
            "read reflects the new commit"
        );
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn scoped_serve_only_for_held_projects() {
        use std::collections::HashMap;
        let e = epic();
        // This node holds exactly one project, keyed by its real root-commit id.
        let pid = crate::project_registry::ProjectAdvertisement::from_verdict(
            &serde_json::json!({}),
            "n",
            None,
        );
        assert!(pid.is_none()); // sanity: no project_id in empty content
        let root = {
            // derive the real project id the same way patch_deliberation does
            let out = Command::new("git")
                .arg("-C")
                .arg(&e)
                .args(["rev-list", "--max-parents=0", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let mut held: HashMap<String, std::path::PathBuf> = HashMap::new();
        held.insert(root.clone(), e.clone());

        // In-scope project → served.
        let ok = serve_scoped(
            &held,
            &root,
            &ReadRequest::FileRead {
                path: "README.md".into(),
                at: None,
            },
        );
        assert_eq!(ok.unwrap().content.as_deref(), Some("root"));
        // A project this node does NOT hold → refused (out of scope), never served.
        let refused = serve_scoped(&held, "some-other-project", &ReadRequest::RefsList);
        assert!(refused.is_err(), "unheld project must be refused");
        assert!(format!("{}", refused.unwrap_err()).contains("out of scope"));
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn request_reply_json_round_trips() {
        let req = ReadRequest::FileRead {
            path: "docs/spec.md".into(),
            at: Some("HEAD".into()),
        };
        let back: ReadRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert!(matches!(back, ReadRequest::FileRead { .. }));
        // Wire tag is stable + snake_case.
        assert!(
            serde_json::to_string(&req)
                .unwrap()
                .contains("\"op\":\"file_read\"")
        );
    }
}
