//! Real-NATS integration: the epic-read bridge end to end.
//!
//! A fleet node runs `run_read_service` over an actual NATS server, serving one held
//! epic. A client (no filesystem, no forgejo — just NATS) issues `request_read` and gets
//! the file/refs/diff back, scoped to the project it holds. Skipped when NATS is absent.

use super::common::{subject_prefix, try_connect_nats, unique_id};
use quorum_rs::epic_read::{ReadRequest, project_id_of, request_read, run_read_service};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
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

/// A tiny epic: base commit + a `base/<job>` tag + a `job/<job>/<agent>` proposal branch.
fn make_epic(uid: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qr-epicread-it-{uid}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "a@b"]);
    git(&dir, &["config", "user.name", "a"]);
    std::fs::write(dir.join("kpi.md"), "consensus v1\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["tag", "base/job1"]);
    git(&dir, &["checkout", "-q", "-b", "job/job1/AgentA"]);
    std::fs::write(dir.join("kpi.md"), "consensus v2 — AgentA\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "AgentA proposal"]);
    git(&dir, &["checkout", "-q", "main"]);
    dir
}

/// A one-commit epic whose base file carries `body` — distinct `body` ⇒ distinct root
/// commit ⇒ distinct project id, so a node can hold several and they can't be confused.
fn make_epic_named(uid: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qr-epicread-it-{uid}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "a@b"]);
    git(&dir, &["config", "user.name", "a"]);
    std::fs::write(dir.join("kpi.md"), body).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "base"]);
    dir
}

#[tokio::test]
async fn epic_read_bridge_round_trip_over_real_nats() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let prefix = subject_prefix(&uid);
    let epic = make_epic(&uid);
    let pid = project_id_of(&epic).unwrap();

    // Fleet node: hold this one project, run the read service in the background.
    let held: HashMap<String, PathBuf> = [(pid.clone(), epic.clone())].into();
    let svc = {
        let client = client.clone();
        let prefix = prefix.clone();
        let group = format!("epic-read-{uid}");
        tokio::spawn(async move {
            let _ = run_read_service(&client, &prefix, &group, held).await;
        })
    };
    // Give the subscription a moment to register.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let head = request_read(
        &client,
        &prefix,
        &pid,
        &ReadRequest::FileRead {
            path: "kpi.md".into(),
            at: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        head.content.as_deref(),
        Some("consensus v1"),
        "served the current epic file"
    );
    assert!(head.error.is_none());

    // Patch plane: refs list exposes the proposal branch + base tag.
    let refs = request_read(&client, &prefix, &pid, &ReadRequest::RefsList)
        .await
        .unwrap()
        .refs;
    assert!(
        refs.iter().any(|r| r == "job/job1/AgentA"),
        "proposal branch: {refs:?}"
    );
    assert!(refs.iter().any(|r| r == "base/job1"), "base tag: {refs:?}");

    // Diff proposal vs base → the hunk the client content-addresses.
    let diff = request_read(
        &client,
        &prefix,
        &pid,
        &ReadRequest::Diff {
            base: "base/job1".into(),
            target: "job/job1/AgentA".into(),
        },
    )
    .await
    .unwrap()
    .content
    .unwrap();
    assert!(
        diff.contains("+consensus v2 — AgentA"),
        "diff carries the proposal hunk: {diff}"
    );

    // Freshness: a deliberation commit lands → the next read reflects it (no restart).
    std::fs::write(epic.join("kpi.md"), "consensus v3 — merged\n").unwrap();
    git(&epic, &["add", "-A"]);
    git(&epic, &["commit", "-qm", "consensus v3"]);
    let fresh = request_read(
        &client,
        &prefix,
        &pid,
        &ReadRequest::FileRead {
            path: "kpi.md".into(),
            at: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        fresh.content.as_deref(),
        Some("consensus v3 — merged"),
        "read reflects the new commit"
    );

    // Scoped: a project this node does NOT hold → structured refusal, not the wrong epic.
    let refused = request_read(
        &client,
        &prefix,
        "not-a-held-project",
        &ReadRequest::RefsList,
    )
    .await
    .unwrap();
    assert!(refused.error.as_deref().unwrap().contains("out of scope"));

    // Confinement holds over the wire: a path escape → structured error.
    let escape = request_read(
        &client,
        &prefix,
        &pid,
        &ReadRequest::FileRead {
            path: "../../etc/passwd".into(),
            at: None,
        },
    )
    .await
    .unwrap();
    assert!(
        escape
            .error
            .as_deref()
            .unwrap()
            .contains("escapes the epic tree")
    );

    // A flag-like ref (arbitrary-write primitive) is refused over the wire, no file written.
    let stem = std::env::temp_dir().join(format!("qr-epicread-it-pwned-{uid}"));
    let would_write = PathBuf::from(format!("{}..HEAD", stem.display()));
    let _ = std::fs::remove_file(&would_write);
    let flag = request_read(
        &client,
        &prefix,
        &pid,
        &ReadRequest::Diff {
            base: format!("--output={}", stem.display()),
            target: "HEAD".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        flag.error
            .as_deref()
            .unwrap()
            .contains("parsed as a git option"),
        "flag-like base refused over the wire: {flag:?}"
    );
    assert!(!would_write.exists(), "no out-of-epic write over the wire");
    let _ = std::fs::remove_file(&would_write);

    svc.abort();
    let _ = std::fs::remove_dir_all(&epic);
}

/// One node holds TWO epics: the `<prefix>.epic.*.read` wildcard must demux each request
/// to the right epic by its project id, and still refuse a third it doesn't hold. This is
/// the "scoped by id" guarantee under a node that holds more than one project.
#[tokio::test]
async fn epic_read_service_demuxes_multiple_held_projects_by_id() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let prefix = subject_prefix(&uid);
    let epic_a = make_epic_named(&format!("{uid}-a"), "alpha\n");
    let epic_b = make_epic_named(&format!("{uid}-b"), "bravo\n");
    let pid_a = project_id_of(&epic_a).unwrap();
    let pid_b = project_id_of(&epic_b).unwrap();
    assert_ne!(pid_a, pid_b, "distinct epics → distinct project ids");

    let held: HashMap<String, PathBuf> = [
        (pid_a.clone(), epic_a.clone()),
        (pid_b.clone(), epic_b.clone()),
    ]
    .into();
    let svc = {
        let client = client.clone();
        let prefix = prefix.clone();
        let group = format!("epic-read-{uid}");
        tokio::spawn(async move {
            let _ = run_read_service(&client, &prefix, &group, held).await;
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let read = |pid: String| {
        let client = client.clone();
        let prefix = prefix.clone();
        async move {
            request_read(
                &client,
                &prefix,
                &pid,
                &ReadRequest::FileRead {
                    path: "kpi.md".into(),
                    at: None,
                },
            )
            .await
            .unwrap()
        }
    };

    // Each project id routes to ITS epic — no cross-talk.
    assert_eq!(read(pid_a.clone()).await.content.as_deref(), Some("alpha"));
    assert_eq!(read(pid_b.clone()).await.content.as_deref(), Some("bravo"));

    // A third project this node does NOT hold → refused, even holding two others.
    let refused = request_read(&client, &prefix, "unheld-project", &ReadRequest::RefsList)
        .await
        .unwrap();
    assert!(refused.error.as_deref().unwrap().contains("out of scope"));

    svc.abort();
    let _ = std::fs::remove_dir_all(&epic_a);
    let _ = std::fs::remove_dir_all(&epic_b);
}
