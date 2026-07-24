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
/// `error` (set instead of the above) carries a refusal/failure so the client gets a
/// structured answer over NATS, never a silent timeout.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadReply {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ReadReply {
    fn err(message: impl std::fmt::Display) -> Self {
        ReadReply {
            error: Some(message.to_string()),
            ..Default::default()
        }
    }
}

/// The project id of a local epic clone — its canonical root-commit key, the SAME key
/// `patch_deliberation::project` derives and [`ProjectRegistry`](crate::project_registry)
/// groups on. Lets a node build its `held` map (`project_id` → path) from configured
/// epic paths without loading the dylib. Lexicographically-smallest root for a multi-root
/// repo, so every clone agrees.
pub fn project_id_of(epic: &Path) -> Result<String> {
    let roots = git(epic, &["rev-list", "--max-parents=0", "HEAD"])?;
    roots
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .min()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("epic {epic:?} has no root commit"))
}

/// The subject a fleet node subscribes (queue group) to serve reads for one project,
/// and the client addresses: `<prefix>.epic.<project_id>.read`. Project id is in the
/// subject so authz is enforced at the NATS layer (a node subscribes only for projects
/// it holds; the client's identity scopes which project subjects it may publish to).
pub fn read_subject(subject_prefix: &str, project_id: &str) -> String {
    format!("{subject_prefix}.epic.{project_id}.read")
}

/// Parse the `project_id` out of a `<prefix>.epic.<project_id>.read` subject.
pub fn project_id_from_subject(subject_prefix: &str, subject: &str) -> Option<String> {
    let rest = subject.strip_prefix(&format!("{subject_prefix}.epic."))?;
    rest.strip_suffix(".read").map(str::to_string)
}

/// Handle one raw read message: parse the project from the subject, deserialize the
/// request, serve it scoped to the node's held epics, and serialize the reply bytes.
/// Every failure path (bad subject, bad payload, unheld project, git error) becomes a
/// structured `ReadReply{error}` — the client always gets an answer, never a hang.
///
/// Pure: no NATS, testable directly. The oversized-reply cap is applied by the caller
/// ([`run_read_service`] via [`cap_bytes`]) since only it knows the NATS `max_payload`.
pub fn handle_read_message(
    held: &std::collections::HashMap<String, std::path::PathBuf>,
    subject_prefix: &str,
    subject: &str,
    payload: &[u8],
) -> Vec<u8> {
    let reply = match project_id_from_subject(subject_prefix, subject) {
        None => ReadReply::err(format!("malformed read subject {subject:?}")),
        Some(pid) => match serde_json::from_slice::<ReadRequest>(payload) {
            Err(e) => ReadReply::err(format!("bad read request: {e}")),
            Ok(req) => serve_scoped(held, &pid, &req).unwrap_or_else(ReadReply::err),
        },
    };
    serde_json::to_vec(&reply).unwrap_or_else(|_| b"{\"error\":\"serialize failed\"}".to_vec())
}

/// Cap a serialized reply at the NATS `max_payload`: an oversized reply can't be published
/// (async-nats rejects it), which would hang the client on a silent timeout. Instead
/// substitute a small structured error naming the size, which always fits.
fn cap_bytes(reply_bytes: Vec<u8>, max_reply_bytes: usize) -> Vec<u8> {
    if reply_bytes.len() <= max_reply_bytes {
        return reply_bytes;
    }
    let capped = ReadReply::err(format!(
        "reply {} bytes exceeds the {max_reply_bytes}-byte NATS payload limit — narrow the request (a subpath or single file)",
        reply_bytes.len()
    ));
    serde_json::to_vec(&capped).unwrap_or_else(|_| b"{\"error\":\"serialize failed\"}".to_vec())
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

/// Reject any ref/revspec (`at` / `base` / `target`) that git would parse as an OPTION.
/// These come from the client and are interpolated into git argv, so a value leading with
/// `-` is read as a flag, not a revision — e.g. `git diff --output=<file>..` writes a file
/// OUTSIDE the epic (an arbitrary-write primitive). A legitimate git ref can never start
/// with `-` (git-check-ref-format forbids it), so fail-closed on that. Empty is rejected
/// too — an empty rev is meaningless and would shift arg positions.
fn reject_unsafe_ref(kind: &str, r: &str) -> Result<()> {
    if r.is_empty() {
        bail!("{kind} ref is empty");
    }
    if r.starts_with('-') {
        bail!("{kind} ref {r:?} starts with `-` — refused (would be parsed as a git option)");
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
            reject_unsafe_ref("at", at)?;
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
            reject_unsafe_ref("at", at)?;
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
            reject_unsafe_ref("base", base)?;
            reject_unsafe_ref("target", target)?;
            let content = git(epic, &["diff", &format!("{base}..{target}")])?;
            Ok(ReadReply {
                content: Some(content),
                ..Default::default()
            })
        }
    }
}

/// The epic paths one agent's dylib middleware operates on: every `patch_deliberation`
/// dylib entry (across all pipeline stages) carries `config.patch_deliberation.upstream`,
/// the on-disk epic root. The same upstream repeats across stages (before_prompt /
/// on_completion / on_job_complete) — the caller dedups by project id.
fn dylib_upstreams(agent: &crate::AgentConfig) -> Vec<std::path::PathBuf> {
    use crate::middleware::MiddlewareEntry;
    let mw = &agent.middleware;
    [
        &mw.before_prompt,
        &mw.on_completion,
        &mw.on_job_complete,
        &mw.on_provider_response,
        &mw.before_release,
    ]
    .into_iter()
    .flatten()
    .filter_map(|e| match e {
        MiddlewareEntry::Dylib { config, .. } => config
            .get("patch_deliberation")
            .and_then(|pd| pd.get("upstream"))
            .and_then(|u| u.as_str())
            .map(std::path::PathBuf::from),
        _ => None,
    })
    .collect()
}

/// Discover the epics a fleet node holds: scan every agent's dylib middleware for a
/// `patch_deliberation.upstream` path and key each by its project id (root-commit).
/// Agents sharing one upstream collapse to a single entry. A path that isn't a readable
/// git epic is skipped (logged) — a misconfigured upstream disables the read service for that
/// epic rather than crashing serve. This is the `held` map [`run_read_service`] scopes to,
/// and the same on-disk clones [`project_sync`](crate::project_sync) keeps current, so a
/// served read reflects the latest deliberation commit with no extra fetch.
pub fn held_epics_from_fleet(
    fleet: &crate::config::AgentFleetConfig,
) -> std::collections::HashMap<String, std::path::PathBuf> {
    let mut held = std::collections::HashMap::new();
    for path in fleet.agents.iter().flat_map(dylib_upstreams) {
        match project_id_of(&path) {
            Ok(pid) => {
                held.entry(pid).or_insert(path);
            }
            Err(e) => {
                tracing::warn!(
                    upstream = ?path,
                    error = %e,
                    "epic-read: skipping upstream that isn't a readable git epic"
                );
            }
        }
    }
    held
}

/// Run the read service: a fleet node subscribes a QUEUE GROUP on
/// `<prefix>.epic.*.read` (so exactly one holder answers each request) and replies from
/// its local epic clones. `held` maps `project_id` → local epic path — the projects this
/// node holds; `serve_scoped` refuses anything else. Freshness: `held` should point at
/// the SAME clones the [`project_sync`](crate::project_sync) loop keeps current (it pulls
/// on `project_advanced`), so reads reflect the latest deliberation commit with no extra
/// fetch. Runs until the subscription ends; a bad message replies with a structured
/// error, never crashes the loop.
pub async fn run_read_service(
    nats: &async_nats::Client,
    subject_prefix: &str,
    queue_group: &str,
    held: std::collections::HashMap<String, std::path::PathBuf>,
) -> Result<()> {
    use futures::StreamExt;
    let subject = format!("{subject_prefix}.epic.*.read");
    let mut sub = nats
        .queue_subscribe(subject.clone(), queue_group.to_string())
        .await
        .map_err(|e| anyhow!("queue_subscribe {subject}: {e}"))?;
    let max_reply_bytes = nats.server_info().max_payload;
    tracing::info!(subject = %subject, group = %queue_group, projects = held.len(), max_reply_bytes, "epic read service started");
    while let Some(msg) = sub.next().await {
        let Some(reply_to) = msg.reply.clone() else {
            continue; // no reply subject → nothing to answer
        };
        let bytes = handle_read_message(&held, subject_prefix, msg.subject.as_str(), &msg.payload);
        let bytes = cap_bytes(bytes, max_reply_bytes);
        if let Err(e) = nats.publish(reply_to, bytes.into()).await {
            tracing::warn!(error = %e, "epic read: reply publish failed");
        }
    }
    Ok(())
}

/// Client side: send one read request to the fleet over NATS request/reply and decode
/// the [`ReadReply`]. The client needs no filesystem and no forgejo — just NATS reach to
/// a node holding the project (whichever queue-group member answers). A `reply.error`
/// surfaces a refusal/failure; transport failure is the `Err`.
pub async fn request_read(
    nats: &async_nats::Client,
    subject_prefix: &str,
    project_id: &str,
    req: &ReadRequest,
) -> Result<ReadReply> {
    let subject = read_subject(subject_prefix, project_id);
    let payload = serde_json::to_vec(req)?;
    let msg = nats
        .request(subject.clone(), payload.into())
        .await
        .map_err(|e| anyhow!("read request {subject}: {e}"))?;
    serde_json::from_slice(&msg.payload).map_err(|e| anyhow!("decode read reply: {e}"))
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

    /// A symlink committed into the epic must read back as its TARGET PATH (git stores it as
    /// a blob holding the link string), never the CONTENTS of whatever it points at — else a
    /// planted link would exfiltrate a file outside the epic. This pins that git behaviour so
    /// the confinement claim can't silently regress.
    #[cfg(unix)]
    #[test]
    fn symlinks_read_as_their_target_string_never_followed_off_the_epic() {
        let e = epic();
        let secret = e.parent().unwrap().join("SYM-SECRET.txt");
        std::fs::write(&secret, "TOP SECRET SYMLINK TARGET\n").unwrap();
        std::os::unix::fs::symlink(&secret, e.join("link_abs")).unwrap();
        std::os::unix::fs::symlink("../../../etc/hosts", e.join("link_rel")).unwrap();
        g(&e, &["add", "-A"]);
        g(&e, &["commit", "-qm", "add symlinks"]);

        // An absolute-target link → the target PATH string, not the secret's contents.
        let abs = serve_read(
            &e,
            &ReadRequest::FileRead {
                path: "link_abs".into(),
                at: None,
            },
        )
        .unwrap();
        let content = abs.content.unwrap();
        assert!(
            content.contains("SYM-SECRET.txt"),
            "reads the link target path: {content}"
        );
        assert!(
            !content.contains("TOP SECRET"),
            "never the pointed-to file contents"
        );

        // A relative escape link → its literal target string, not `/etc/hosts` contents.
        let rel = serve_read(
            &e,
            &ReadRequest::FileRead {
                path: "link_rel".into(),
                at: None,
            },
        )
        .unwrap();
        assert_eq!(rel.content.as_deref(), Some("../../../etc/hosts"));

        let _ = std::fs::remove_file(&secret);
        let _ = std::fs::remove_dir_all(&e);
    }

    /// A client-supplied ref (`at` / `base` / `target`) is interpolated into git argv, so a
    /// value leading with `-` is parsed as a FLAG, not a revision. `git diff --output=<file>`
    /// then writes OUTSIDE the epic — an arbitrary-write primitive. Every ref position must
    /// refuse such values, fail-closed, before git runs.
    #[test]
    fn flag_like_refs_are_refused_and_never_write_outside_the_epic() {
        let e = epic();
        // The write primitive: `base` as `--output=<stem>`. git would create `<stem>..HEAD`.
        let stem = std::env::temp_dir().join(format!("qr-epicread-pwned-{}", std::process::id()));
        let would_write = std::path::PathBuf::from(format!("{}..HEAD", stem.display()));
        let _ = std::fs::remove_file(&would_write);
        let diff = serve_read(
            &e,
            &ReadRequest::Diff {
                base: format!("--output={}", stem.display()),
                target: "HEAD".into(),
            },
        );
        assert!(diff.is_err(), "flag-like base must be refused");
        assert!(!would_write.exists(), "must NOT write outside the epic");
        let _ = std::fs::remove_file(&would_write);

        // `target` and `at` positions are guarded too.
        assert!(
            serve_read(
                &e,
                &ReadRequest::Diff {
                    base: "base/job1".into(),
                    target: "--x".into(),
                },
            )
            .is_err(),
            "flag-like target refused"
        );
        assert!(
            serve_read(
                &e,
                &ReadRequest::FileRead {
                    path: "docs/spec.md".into(),
                    at: Some("--x".into()),
                },
            )
            .is_err(),
            "flag-like at (file_read) refused"
        );
        assert!(
            serve_read(
                &e,
                &ReadRequest::FilesList {
                    path: String::new(),
                    at: Some("-x".into()),
                },
            )
            .is_err(),
            "flag-like at (files_list) refused"
        );
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
    fn project_id_of_is_the_root_commit_and_builds_the_held_map() {
        use std::collections::HashMap;
        let e = epic();
        let pid = project_id_of(&e).unwrap();
        assert_eq!(pid.len(), 40, "project id is the root-commit sha");
        // A node builds its held map from its epic paths, then serves scoped by it.
        let held: HashMap<String, std::path::PathBuf> = [(pid.clone(), e.clone())].into();
        let ok = serve_scoped(
            &held,
            &pid,
            &ReadRequest::FileRead {
                path: "README.md".into(),
                at: None,
            },
        );
        assert_eq!(ok.unwrap().content.as_deref(), Some("root"));
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn read_subject_round_trips_the_project_id() {
        let s = read_subject("nsed", "root-sha");
        assert_eq!(s, "nsed.epic.root-sha.read");
        assert_eq!(
            project_id_from_subject("nsed", &s).as_deref(),
            Some("root-sha")
        );
        // Wrong shape → None (not a read subject).
        assert!(project_id_from_subject("nsed", "nsed.project.x.advanced").is_none());
    }

    #[test]
    fn handle_read_message_dispatches_scoped_and_structures_every_error() {
        use std::collections::HashMap;
        let e = epic();
        let root = {
            let o = Command::new("git")
                .arg("-C")
                .arg(&e)
                .args(["rev-list", "--max-parents=0", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        let mut held: HashMap<String, std::path::PathBuf> = HashMap::new();
        held.insert(root.clone(), e.clone());
        let subject = read_subject("nsed", &root);

        // Happy path: a real request → the file content, over the wire.
        let req = serde_json::to_vec(&ReadRequest::FileRead {
            path: "README.md".into(),
            at: None,
        })
        .unwrap();
        let reply: ReadReply =
            serde_json::from_slice(&handle_read_message(&held, "nsed", &subject, &req)).unwrap();
        assert_eq!(reply.content.as_deref(), Some("root"));
        assert!(reply.error.is_none());

        // Unheld project → structured error (never a hang, never the wrong epic).
        let other = read_subject("nsed", "not-held");
        let r2: ReadReply =
            serde_json::from_slice(&handle_read_message(&held, "nsed", &other, &req)).unwrap();
        assert!(r2.error.as_deref().unwrap().contains("out of scope"));

        // Malformed payload → structured error.
        let r3: ReadReply =
            serde_json::from_slice(&handle_read_message(&held, "nsed", &subject, b"not json"))
                .unwrap();
        assert!(r3.error.as_deref().unwrap().contains("bad read request"));

        // Path escape over the wire → structured error (confinement holds at the edge).
        let escape = serde_json::to_vec(&ReadRequest::FileRead {
            path: "../../etc/passwd".into(),
            at: None,
        })
        .unwrap();
        let r4: ReadReply =
            serde_json::from_slice(&handle_read_message(&held, "nsed", &subject, &escape)).unwrap();
        assert!(
            r4.error
                .as_deref()
                .unwrap()
                .contains("escapes the epic tree")
        );

        // Malformed subject → structured error.
        let r5: ReadReply =
            serde_json::from_slice(&handle_read_message(&held, "nsed", "garbage", &req)).unwrap();
        assert!(
            r5.error
                .as_deref()
                .unwrap()
                .contains("malformed read subject")
        );
        let _ = std::fs::remove_dir_all(&e);
    }

    /// A reply larger than the NATS `max_payload` can't be published — async-nats rejects
    /// it and the client hangs on a silent timeout. `cap_bytes` substitutes a small size
    /// error instead, which always fits.
    #[test]
    fn cap_bytes_substitutes_a_size_error_when_the_reply_is_too_big() {
        // Under the cap → bytes pass through unchanged.
        let small = b"{\"content\":\"root\"}".to_vec();
        assert_eq!(cap_bytes(small.clone(), 10_000), small);

        // Over the cap → a structured size error naming the byte count, and it fits the cap.
        let capped = cap_bytes(small.clone(), 10);
        let reply: ReadReply = serde_json::from_slice(&capped).unwrap();
        let err = reply.error.as_deref().unwrap();
        assert!(err.contains("exceeds the 10-byte"), "size error: {err}");
        assert!(
            err.contains(&small.len().to_string()),
            "names the actual size: {err}"
        );
        assert!(
            reply.content.is_none(),
            "content dropped in favour of the error"
        );
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

    /// A fleet where two agents run the patch-deliberation dylib on the SAME epic and a
    /// third runs nothing. `upstream` is embedded via yaml exactly as production.yml does.
    fn fleet_holding(upstream: &Path) -> Result<crate::config::AgentFleetConfig> {
        let u = upstream.display();
        let y = format!(
            "providers: {{}}\n\
             agents:\n  \
             - name: A\n    \
               middleware:\n      \
                 on_completion:\n        \
                   - dylib: ./libpd.dylib\n          \
                     config: {{ patch_deliberation: {{ upstream: \"{u}\" }} }}\n  \
             - name: B\n    \
               middleware:\n      \
                 on_job_complete:\n        \
                   - dylib: ./libpd.dylib\n          \
                     config: {{ patch_deliberation: {{ upstream: \"{u}\" }} }}\n  \
             - name: C\n"
        );
        Ok(serde_yaml::from_str(&y)?)
    }

    #[test]
    fn held_map_dedups_shared_upstream() -> Result<()> {
        let e = epic();
        let held = held_epics_from_fleet(&fleet_holding(&e)?);
        assert_eq!(held.len(), 1, "two agents on one epic → one held entry");
        let pid = project_id_of(&e)?;
        assert_eq!(
            held.get(&pid),
            Some(&e),
            "keyed by project id, points at the epic"
        );
        let _ = std::fs::remove_dir_all(&e);
        Ok(())
    }

    #[test]
    fn held_map_skips_upstream_that_isnt_a_git_epic() -> Result<()> {
        // A misconfigured upstream must disable the read service for that epic, not crash serve.
        let bogus =
            std::env::temp_dir().join(format!("qr-epicread-nonexistent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bogus);
        assert!(
            held_epics_from_fleet(&fleet_holding(&bogus)?).is_empty(),
            "non-git upstream skipped"
        );
        Ok(())
    }
}
