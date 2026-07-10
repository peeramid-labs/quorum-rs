//! Client-owned thread store.
//!
//! A thread is the interactive client's durable record of a conversation:
//! a stable id, a subject, the policy currently acting as the "model", and the
//! ordered messages. The client owns this transcript (standard Chat Completions
//! semantics), so restoring a conversation does not depend on the orchestrator's
//! history retention. See
//! `docs/explanation/policy-as-model-and-threads.md`.
//!
//! This is deliberately separate from [`crate::agents::session_store`], which
//! maps Claude-CLI transcript UUIDs and holds no conversation content.
//!
//! Layout: one JSON file per thread under `~/.nsed/threads/{id}.json`
//! (`$NSED_THREAD_DIR` overrides the directory). one-file-per-thread keeps
//! listing cheap and avoids whole-store rewrite races between concurrent
//! clients.

use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// One message in a conversation. `policy_id` records which policy (the "model")
/// produced an assistant message, so a thread that swapped policy mid-thread stays
/// self-describing; `job_id` links the turn back to its deliberation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Stable node id. Tree edges use it (`parent_id`). Empty on pre-tree JSON;
    /// [`Thread::migrate_linear`] backfills it on load.
    #[serde(default)]
    pub id: String,
    /// Parent node in the thread tree; `None` for a root turn. A new turn roots
    /// under the message the user replied to (the cursor node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Branch identity = the per-branch `conversation_id`. A reply to a leaf
    /// inherits its parent's branch (linear continuation → session resume); a
    /// reply under a non-leaf node gets a fresh branch (a fork → new session).
    #[serde(default)]
    pub branch_id: String,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    pub ts: i64,
}

impl Message {
    /// A root turn stamped with the current wall-clock time and a fresh id +
    /// branch. Use [`Thread::reply`] to attach it under a parent (which sets
    /// `parent_id` and resolves `branch_id` from the fork rule).
    pub fn now(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            parent_id: None,
            branch_id: uuid::Uuid::new_v4().simple().to_string(),
            role: role.into(),
            content: content.into(),
            policy_id: None,
            job_id: None,
            ts: Utc::now().timestamp(),
        }
    }
}

/// A stored conversation. `server_thread` is the `x-nsed-session-id` used for
/// cheap same-policy continuation; it is an optimisation, not the source of
/// truth — the source of truth is `messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub subject: String,
    pub created: i64,
    pub updated: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_thread: Option<String>,
    /// Job id of a launched turn whose reply hasn't landed yet. Persisted so a
    /// deliberation that finishes while the TUI is closed can be reconciled on
    /// reopen (fetch the result by this id). Cleared when the reply is appended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_job: Option<String>,
    /// Unsent compose text (pastes already expanded), persisted so a long draft
    /// survives a restart / a failed send instead of being lost with the TUI
    /// process. Restored into the compose box on reopen; cleared once sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
}

impl Thread {
    /// A fresh thread with a generated id and the current timestamps.
    pub fn new(subject: impl Into<String>) -> Self {
        let now = Utc::now().timestamp();
        Self {
            id: format!("thread-{}", uuid::Uuid::new_v4().simple()),
            subject: subject.into(),
            created: now,
            updated: now,
            active_policy: None,
            orchestrator: None,
            server_thread: None,
            pending_job: None,
            draft: None,
            messages: Vec::new(),
        }
    }

    /// Append a message and bump `updated`. An unrooted turn (`parent_id`
    /// unset) continues the current tip — linear-append, the common case — so
    /// callers that don't care about branching keep working. Use [`Self::reply`]
    /// to root a turn under a specific node (or as a new root).
    pub fn push_message(&mut self, mut turn: Message) {
        if turn.parent_id.is_none()
            && let Some(prev) = self.messages.last()
        {
            turn.parent_id = Some(prev.id.clone());
            turn.branch_id = prev.branch_id.clone();
        }
        self.updated = turn.ts.max(self.updated);
        self.messages.push(turn);
    }

    // --- tree ops -----------------------------------------------------------

    /// Look up a message by id.
    pub fn get(&self, id: &str) -> Option<&Message> {
        self.messages.iter().find(|m| m.id == id)
    }

    /// Direct replies to `parent_id` (`None` = root turns).
    pub fn children(&self, parent_id: Option<&str>) -> Vec<&Message> {
        self.messages
            .iter()
            .filter(|m| m.parent_id.as_deref() == parent_id)
            .collect()
    }

    /// A message with no replies — the tip of its branch.
    pub fn is_leaf(&self, id: &str) -> bool {
        !self
            .messages
            .iter()
            .any(|m| m.parent_id.as_deref() == Some(id))
    }

    /// How many forks lie in a node's ancestry — its indent depth in the
    /// reader. A linear lineage shares one branch (depth 0); each fork along the
    /// path introduces a new branch (+1). = distinct `branch_id`s on the path − 1.
    pub fn fork_depth(&self, id: &str) -> usize {
        let mut seen = std::collections::HashSet::new();
        for m in self.path_to_root(id) {
            seen.insert(m.branch_id.clone());
        }
        seen.len().saturating_sub(1)
    }

    /// Root→node path following `parent_id`. Empty if `id` is unknown.
    pub fn path_to_root(&self, id: &str) -> Vec<&Message> {
        let mut path = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cur = self.get(id);
        while let Some(m) = cur {
            // Guard against a corrupt parent cycle — stop before looping forever.
            if !seen.insert(m.id.as_str()) {
                break;
            }
            path.push(m);
            cur = m.parent_id.as_deref().and_then(|p| self.get(p));
        }
        path.reverse();
        path
    }

    /// The newest message overall — the default reply target ("continue the
    /// conversation"). `None` on an empty thread. Ties on `ts` (same-second
    /// turns) resolve to the last appended: `max_by_key` returns the last of
    /// equal maxima, and messages are pushed in order.
    pub fn tip(&self) -> Option<&Message> {
        self.messages.iter().max_by_key(|m| m.ts)
    }

    /// Attach a new turn under `parent_id` and return its id. Branch rule: a
    /// reply to a current leaf inherits the parent's branch (linear
    /// continuation); a reply under a non-leaf node (or a root turn) starts a
    /// fresh branch (a fork). The pre-append leaf check is what makes the FIRST
    /// reply continue and a SECOND reply to the same node fork.
    pub fn reply(
        &mut self,
        parent_id: Option<&str>,
        role: impl Into<String>,
        content: impl Into<String>,
    ) -> String {
        let mut m = Message::now(role, content);
        m.parent_id = parent_id.map(|s| s.to_string());
        if let Some(pid) = parent_id
            && self.is_leaf(pid)
            && let Some(parent) = self.get(pid)
        {
            m.branch_id = parent.branch_id.clone();
        }
        let id = m.id.clone();
        // Raw push — `reply` sets `parent_id` explicitly (incl. `None` for a
        // root), so bypass `push_message`'s linear-append auto-link.
        self.updated = m.ts.max(self.updated);
        self.messages.push(m);
        id
    }

    /// Backfill tree fields on a pre-tree (linear) thread: assign ids, chain
    /// each message under the previous one, and share a single branch. A no-op
    /// once every message already carries an id.
    pub fn migrate_linear(&mut self) {
        // Already tree-shaped — nothing to backfill.
        if self
            .messages
            .iter()
            .all(|m| !m.id.is_empty() && !m.branch_id.is_empty())
        {
            return;
        }
        let branch = uuid::Uuid::new_v4().simple().to_string();
        // Only a FULLY pre-tree thread (every id empty) gets a fresh linear
        // parent chain. If some messages already carry ids, the thread is (or
        // was) tree-shaped — backfill missing fields but never rewrite existing
        // parent edges, or we'd flatten real branches.
        let fully_legacy = self.messages.iter().all(|m| m.id.is_empty());
        let mut prev: Option<String> = None;
        for m in &mut self.messages {
            if m.id.is_empty() {
                m.id = uuid::Uuid::new_v4().simple().to_string();
            }
            if m.branch_id.is_empty() {
                m.branch_id = branch.clone();
            }
            if fully_legacy {
                m.parent_id = prev.clone();
                prev = Some(m.id.clone());
            }
        }
    }

    /// Render the root→`parent_id` path plus a new user message into one task
    /// string for the native deliberation API. A fork only carries its own
    /// lineage (the path), which is what makes "reply under wsup? only" differ
    /// from "continue after Hi!". Subject leads as framing (see
    /// [`Self::to_deliberation_query`]).
    pub fn to_deliberation_query_from(
        &self,
        parent_id: Option<&str>,
        new_user_message: &str,
    ) -> String {
        let path = parent_id.map(|p| self.path_to_root(p)).unwrap_or_default();
        let pairs = path
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .chain(std::iter::once(("user", new_user_message)));
        let body = crate::conversation::flatten_conversation(pairs);
        let subject = self.subject.trim();
        if subject.is_empty() {
            body
        } else {
            format!("Subject: {subject}\n\n{body}")
        }
    }

    /// Render the stored conversation plus a new user message into a single
    /// task string for the native deliberation API (which takes one
    /// `user_query`, not a message array). Mirrors the server compat layer's
    /// `to_query_string` so a multi-turn thread over the native transport reads
    /// the same way: prior non-empty messages prefixed with `[role]`, the new
    /// question last. The thread's subject (when set) leads the request as a
    /// `Subject:` line so the agents see the conversation's framing.
    pub fn to_deliberation_query(&self, new_user_message: &str) -> String {
        // Default reply target is the tip; for a linear thread its root-path is
        // the whole conversation, so this matches the pre-tree behaviour.
        let tip = self.tip().map(|m| m.id.clone());
        self.to_deliberation_query_from(tip.as_deref(), new_user_message)
    }
}

/// Filesystem-backed store of threads.
#[derive(Debug, Clone)]
pub struct ThreadStore {
    dir: PathBuf,
}

impl Default for ThreadStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadStore {
    /// Resolve the store directory: `$NSED_THREAD_DIR` → `~/.nsed/threads` → a
    /// user-unique temp dir when no home is set (never a world-shared path,
    /// since transcripts may hold private content).
    pub fn new() -> Self {
        if let Ok(explicit) = std::env::var("NSED_THREAD_DIR")
            && !explicit.is_empty()
        {
            return Self {
                dir: PathBuf::from(explicit),
            };
        }
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return Self {
                dir: PathBuf::from(home).join(".nsed").join("threads"),
            };
        }
        let dir = std::env::temp_dir().join(format!("nsed-threads-{}", user_suffix()));
        Self { dir }
    }

    /// A store rooted at an explicit directory (tests only).
    #[cfg(test)]
    pub(crate) fn with_dir(dir: std::path::PathBuf) -> Self {
        Self { dir }
    }

    /// Path of a thread file, rejecting ids that are not a plain slug so a
    /// caller-supplied id can never escape the store directory.
    fn path_for(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return None;
        }
        Some(self.dir.join(format!("{id}.json")))
    }

    /// Persist a thread as-is, creating the store directory if needed.
    /// `updated` is owned by the thread ([`Thread::new`] /
    /// [`Thread::push_message`] maintain it), so saving is a plain write.
    pub fn save(&self, thread: &Thread) -> std::io::Result<()> {
        let path = self.path_for(&thread.id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid thread id")
        })?;
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec_pretty(thread)?;
        std::fs::write(path, json)
    }

    /// Delete a thread's stored file. `true` if it was removed (or already
    /// gone); `false` on an invalid id or a real filesystem error. The
    /// transcript is client-owned, so this is the only copy.
    pub fn delete(&self, id: &str) -> bool {
        let Some(path) = self.path_for(id) else {
            return false;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        }
    }

    /// Load a thread by id. `None` when it does not exist or the id is invalid.
    pub fn load(&self, id: &str) -> Option<Thread> {
        let path = self.path_for(id)?;
        let bytes = std::fs::read(path).ok()?;
        let mut thread: Thread = serde_json::from_slice(&bytes).ok()?;
        thread.migrate_linear();
        Some(thread)
    }

    /// All threads, newest-updated first. Unreadable/corrupt files are skipped.
    pub fn list(&self) -> Vec<Thread> {
        let mut out: Vec<Thread> = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .filter_map(|e| std::fs::read(e.path()).ok())
                .filter_map(|b| serde_json::from_slice::<Thread>(&b).ok())
                .map(|mut t| {
                    t.migrate_linear();
                    t
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        out.sort_by_key(|s| std::cmp::Reverse(s.updated));
        out
    }

    /// The most recently updated thread, for `--continue`.
    pub fn latest(&self) -> Option<Thread> {
        self.list().into_iter().next()
    }

    /// Load thread `id`, append an assistant reply (tagged with the deliberation
    /// `job_id` and the policy that produced it), and persist. Returns `true`
    /// on success; `false` if the thread is missing or the write fails. Used to
    /// record a deliberation's answer once it completes.
    pub fn append_reply(
        &self,
        id: &str,
        content: &str,
        job_id: &str,
        policy: Option<&str>,
    ) -> bool {
        let Some(mut thread) = self.load(id) else {
            return false;
        };
        // Idempotent: the live JobComplete path and the reconcile-on-reopen path
        // can both try to record the same job's reply — record it once.
        if thread
            .messages
            .iter()
            .any(|m| m.job_id.as_deref() == Some(job_id))
        {
            if thread.pending_job.as_deref() == Some(job_id) {
                thread.pending_job = None;
                let _ = self.save(&thread);
            }
            return true;
        }
        // Explicit policy wins; otherwise attribute the reply to the thread's
        // active policy (the one that produced it).
        let policy_id = policy
            .map(str::to_string)
            .or_else(|| thread.active_policy.clone());
        // Parent the reply under the tip (the user turn that launched this job),
        // inheriting its branch so the lineage stays linear.
        let tip = thread.tip().map(|m| m.id.clone());
        let reply_id = thread.reply(tip.as_deref(), "assistant", content);
        if let Some(m) = thread.messages.iter_mut().find(|m| m.id == reply_id) {
            m.job_id = Some(job_id.to_string());
            m.policy_id = policy_id;
        }
        // The awaited reply landed — clear the pending marker.
        if thread.pending_job.as_deref() == Some(job_id) {
            thread.pending_job = None;
        }
        self.save(&thread).is_ok()
    }

    /// Record which launched job a thread is awaiting a reply from, so a
    /// deliberation that finishes while the TUI is closed can be reconciled on
    /// reopen. Persists immediately; `false` if the thread is missing.
    pub fn set_pending_job(&self, id: &str, job_id: &str) -> bool {
        let Some(mut thread) = self.load(id) else {
            return false;
        };
        thread.pending_job = Some(job_id.to_string());
        self.save(&thread).is_ok()
    }

    /// Clear the pending-job marker (e.g. after a cancel — the turn stays
    /// reply-less so a follow-up can continue). `false` if the thread is missing.
    pub fn clear_pending_job(&self, id: &str) -> bool {
        let Some(mut thread) = self.load(id) else {
            return false;
        };
        thread.pending_job = None;
        self.save(&thread).is_ok()
    }
}

/// A stable per-user suffix for the no-home temp fallback, so two users on one
/// host do not share a transcript directory.
fn user_suffix() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn store_in(dir: &Path) -> ThreadStore {
        ThreadStore {
            dir: dir.to_path_buf(),
        }
    }

    /// A legacy pre-tree message (no id/parent/branch), as old JSON deserializes.
    fn legacy(role: &str, content: &str, ts: i64) -> Message {
        Message {
            id: String::new(),
            parent_id: None,
            branch_id: String::new(),
            role: role.into(),
            content: content.into(),
            policy_id: None,
            job_id: None,
            ts,
        }
    }

    #[test]
    fn reply_to_leaf_continues_branch_second_reply_forks() {
        let mut t = Thread::new("s");
        let root = t.reply(None, "user", "wsup?");
        let hi = t.reply(Some(&root), "assistant", "Hi!");
        // First reply to `hi` (a leaf) → same branch (linear continuation).
        let foo = t.reply(Some(&hi), "user", "foo");
        assert_eq!(
            t.get(&foo).unwrap().branch_id,
            t.get(&hi).unwrap().branch_id
        );
        // The whole linear lineage wsup?→Hi!→foo shares one branch (each was a
        // first reply to a leaf).
        assert_eq!(
            t.get(&root).unwrap().branch_id,
            t.get(&hi).unwrap().branch_id
        );
        // Second reply to `hi` (now a non-leaf) → fork, fresh branch distinct
        // from the linear lineage.
        let fork = t.reply(Some(&hi), "user", "other");
        assert_ne!(
            t.get(&fork).unwrap().branch_id,
            t.get(&hi).unwrap().branch_id
        );
    }

    #[test]
    fn path_to_root_stops_on_a_parent_cycle() {
        // Corrupt JSON could carry a cycle a↔b; the walk must terminate.
        let mut t = Thread::new("s");
        let mut a = Message::now("user", "a");
        let mut b = Message::now("user", "b");
        a.parent_id = Some(b.id.clone());
        b.parent_id = Some(a.id.clone());
        let (aid, bid) = (a.id.clone(), b.id.clone());
        t.messages = vec![a, b];
        // Must return (not hang) and never exceed the node count.
        assert!(t.path_to_root(&aid).len() <= 2);
        assert!(t.path_to_root(&bid).len() <= 2);
        assert!(t.fork_depth(&aid) <= 2);
    }

    #[test]
    fn migrate_preserves_existing_tree_edges_on_partial_legacy() {
        // A real tree with one stray legacy (empty-id) message must NOT be
        // flattened — existing parent edges are load-bearing branch structure.
        let mut t = Thread::new("s");
        let r = t.reply(None, "user", "root");
        let c = t.reply(Some(&r), "user", "child");
        t.messages.push(legacy("user", "orphan", 9));
        t.migrate_linear();
        assert_eq!(
            t.get(&c).unwrap().parent_id.as_deref(),
            Some(r.as_str()),
            "existing edge preserved"
        );
        assert!(
            t.messages
                .iter()
                .all(|m| !m.id.is_empty() && !m.branch_id.is_empty()),
            "legacy message backfilled"
        );
    }

    #[test]
    fn fork_depth_counts_forks_in_ancestry() {
        let mut t = Thread::new("s");
        let a = t.reply(None, "user", "root");
        let b = t.reply(Some(&a), "assistant", "hi"); // leaf reply → same branch
        let c = t.reply(Some(&a), "user", "fork1"); // a now non-leaf → fork
        let d = t.reply(Some(&c), "assistant", "d"); // continues the fork
        let e = t.reply(Some(&c), "user", "fork2"); // c non-leaf → fork-of-fork
        assert_eq!(t.fork_depth(&a), 0);
        assert_eq!(t.fork_depth(&b), 0);
        assert_eq!(t.fork_depth(&c), 1);
        assert_eq!(t.fork_depth(&d), 1);
        assert_eq!(t.fork_depth(&e), 2);
    }

    #[test]
    fn path_to_root_is_root_first() {
        let mut t = Thread::new("s");
        let a = t.reply(None, "user", "wsup?");
        let b = t.reply(Some(&a), "assistant", "Hi!");
        let c = t.reply(Some(&b), "user", "foo");
        let path: Vec<_> = t
            .path_to_root(&c)
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(path, vec!["wsup?", "Hi!", "foo"]);
    }

    #[test]
    fn fork_query_carries_only_its_lineage() {
        let mut t = Thread::new("chat");
        let a = t.reply(None, "user", "wsup?");
        let b = t.reply(Some(&a), "assistant", "Hi!");
        let _foo = t.reply(Some(&b), "user", "foo");
        // A fork rooted at `a` (wsup?) must NOT see the Hi!/foo lineage.
        let q = t.to_deliberation_query_from(Some(&a), "new rooted from wsup");
        assert!(q.contains("wsup?"));
        assert!(q.contains("new rooted from wsup"));
        assert!(
            !q.contains("Hi!"),
            "fork must not carry the sibling branch: {q}"
        );
        assert!(!q.contains("foo"));
        assert!(q.contains("Subject: chat"));
    }

    #[test]
    fn migrate_linear_backfills_ids_and_chain() {
        let mut t = Thread::new("s");
        t.messages = vec![
            legacy("user", "q1", 1),
            legacy("assistant", "a1", 2),
            legacy("user", "q2", 3),
        ];
        t.migrate_linear();
        assert!(t.messages.iter().all(|m| !m.id.is_empty()));
        // One shared branch, linear parent chain.
        let branch = &t.messages[0].branch_id;
        assert!(t.messages.iter().all(|m| &m.branch_id == branch));
        assert_eq!(t.messages[0].parent_id, None);
        assert_eq!(
            t.messages[1].parent_id.as_deref(),
            Some(t.messages[0].id.as_str())
        );
        assert_eq!(
            t.messages[2].parent_id.as_deref(),
            Some(t.messages[1].id.as_str())
        );
        // Idempotent.
        let before = t.messages.clone();
        t.migrate_linear();
        assert_eq!(t.messages, before);
    }

    #[test]
    fn append_reply_parents_to_tip_and_inherits_branch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut t = Thread::new("t");
        let u = t.reply(None, "user", "q");
        store.save(&t).unwrap();
        assert!(store.append_reply(&t.id, "answer", "job-1", None));
        let got = store.load(&t.id).unwrap();
        let reply = got.messages.iter().find(|m| m.role == "assistant").unwrap();
        assert_eq!(reply.parent_id.as_deref(), Some(u.as_str()));
        assert_eq!(reply.branch_id, got.get(&u).unwrap().branch_id);
    }

    #[test]
    fn append_reply_after_fork_lands_on_the_fork_branch() {
        // End-to-end branch check: fork under an old node, its deliberation's
        // reply must record on the fork's branch (its conversation_id lineage),
        // not the sibling branch.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut t = Thread::new("t");
        let a = t.reply(None, "user", "root");
        let _b = t.reply(Some(&a), "assistant", "hi"); // `a` now non-leaf
        let uf = t.reply(Some(&a), "user", "fork question"); // fork → fresh branch
        let fork_branch = t.get(&uf).unwrap().branch_id.clone();
        assert_ne!(
            fork_branch,
            t.get(&a).unwrap().branch_id,
            "the fork got its own branch"
        );
        store.save(&t).unwrap();

        assert!(store.append_reply(&t.id, "fork answer", "job-fork", None));
        let got = store.load(&t.id).unwrap();
        let reply = got
            .messages
            .iter()
            .find(|m| m.content == "fork answer")
            .unwrap();
        assert_eq!(
            reply.parent_id.as_deref(),
            Some(uf.as_str()),
            "under the fork turn"
        );
        assert_eq!(reply.branch_id, fork_branch, "on the fork branch");
    }

    #[test]
    fn delete_removes_the_thread_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let t = Thread::new("x");
        store.save(&t).unwrap();
        assert!(store.load(&t.id).is_some());
        assert!(store.delete(&t.id));
        assert!(store.load(&t.id).is_none());
        assert!(store.delete(&t.id), "idempotent on already-gone");
        assert!(!store.delete(""), "invalid id rejected");
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut s = Thread::new("first thread");
        s.active_policy = Some("nsed:review".into());
        s.push_message(Message::now("user", "what is rust?"));
        store.save(&s).unwrap();

        let got = store.load(&s.id).expect("loads");
        assert_eq!(got.id, s.id);
        assert_eq!(got.subject, "first thread");
        assert_eq!(got.active_policy.as_deref(), Some("nsed:review"));
        assert_eq!(got.messages.len(), 1);
        assert_eq!(got.messages[0].content, "what is rust?");
    }

    #[test]
    fn append_reply_adds_assistant_message() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut t = Thread::new("t");
        t.push_message(Message::now("user", "q"));
        store.save(&t).unwrap();

        assert!(store.append_reply(&t.id, "the answer", "job-42", Some("nsed:review")));
        let got = store.load(&t.id).unwrap();
        assert_eq!(got.messages.len(), 2);
        assert_eq!(got.messages[1].role, "assistant");
        assert_eq!(got.messages[1].content, "the answer");
        assert_eq!(got.messages[1].job_id.as_deref(), Some("job-42"));
        assert_eq!(got.messages[1].policy_id.as_deref(), Some("nsed:review"));
    }

    #[test]
    fn append_reply_clears_pending_and_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut t = Thread::new("t");
        t.push_message(Message::now("user", "q"));
        t.pending_job = Some("job-7".into());
        store.save(&t).unwrap();

        // First append records the reply + clears the pending marker.
        assert!(store.append_reply(&t.id, "answer", "job-7", None));
        let got = store.load(&t.id).unwrap();
        assert_eq!(got.messages.len(), 2);
        assert!(
            got.pending_job.is_none(),
            "pending cleared once the reply lands"
        );

        // The live JobComplete + reconcile paths can both fire — record once.
        assert!(store.append_reply(&t.id, "answer", "job-7", None));
        assert_eq!(
            store.load(&t.id).unwrap().messages.len(),
            2,
            "same job's reply is not duplicated"
        );
    }

    #[test]
    fn append_reply_keeps_pending_for_a_different_job() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut t = Thread::new("t");
        t.push_message(Message::now("user", "q"));
        t.pending_job = Some("job-A".into());
        store.save(&t).unwrap();
        // A different job's reply must not clear job-A's pending marker.
        assert!(store.append_reply(&t.id, "answer", "job-B", None));
        assert_eq!(
            store.load(&t.id).unwrap().pending_job.as_deref(),
            Some("job-A")
        );
    }

    #[test]
    fn set_pending_job_persists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let t = Thread::new("t");
        store.save(&t).unwrap();
        assert!(store.set_pending_job(&t.id, "job-9"));
        assert_eq!(
            store.load(&t.id).unwrap().pending_job.as_deref(),
            Some("job-9")
        );
        // Cancel clears it so a follow-up can send.
        assert!(store.clear_pending_job(&t.id));
        assert!(store.load(&t.id).unwrap().pending_job.is_none());
    }

    #[test]
    fn append_reply_missing_thread_is_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!store_in(tmp.path()).append_reply("thread-nope", "x", "job-1", None));
    }

    #[test]
    fn load_missing_is_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(store_in(tmp.path()).load("thread-nope").is_none());
    }

    #[test]
    fn list_is_newest_updated_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut a = Thread::new("a");
        a.updated = 100;
        let mut b = Thread::new("b");
        b.updated = 200;
        store.save(&a).unwrap();
        store.save(&b).unwrap();
        let ids: Vec<String> = store.list().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![b.id.clone(), a.id.clone()]);
    }

    #[test]
    fn latest_returns_most_recent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut older = Thread::new("older");
        older.updated = 100;
        store.save(&older).unwrap();
        let mut newer = Thread::new("newer");
        newer.updated = 200;
        store.save(&newer).unwrap();
        assert_eq!(store.latest().map(|s| s.id), Some(newer.id));
    }

    #[test]
    fn push_message_bumps_updated_and_appends() {
        let mut s = Thread::new("x");
        let base = s.updated;
        let mut t = Message::now("assistant", "hi");
        t.ts = base + 500;
        s.push_message(t);
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.updated, base + 500);
    }

    #[test]
    fn to_deliberation_query_first_turn_is_bare() {
        let s = Thread::new(""); // no subject → no prefix, tests flatten only
        assert_eq!(s.to_deliberation_query("hello?"), "hello?");
    }

    #[test]
    fn to_deliberation_query_leads_with_subject() {
        let s = Thread::new("Q3 audit");
        assert_eq!(
            s.to_deliberation_query("what's the risk?"),
            "Subject: Q3 audit\n\nwhat's the risk?"
        );
    }

    #[test]
    fn to_deliberation_query_multi_turn_prefixes_roles() {
        let mut s = Thread::new("");
        s.push_message(Message::now("user", "what is rust?"));
        s.push_message(Message::now("assistant", "a systems language"));
        let q = s.to_deliberation_query("how does it compare to go?");
        assert_eq!(
            q,
            "[user] what is rust?\n\n[assistant] a systems language\n\n[user] how does it compare to go?"
        );
    }

    #[test]
    fn to_deliberation_query_skips_empty_turns() {
        let mut s = Thread::new("");
        s.push_message(Message::now("assistant", "   "));
        s.push_message(Message::now("user", "real question"));
        let q = s.to_deliberation_query("follow up");
        assert_eq!(q, "[user] real question\n\n[user] follow up");
    }

    #[test]
    fn path_traversal_ids_are_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        assert!(store.path_for("../escape").is_none());
        assert!(store.path_for("a/b").is_none());
        assert!(store.path_for("").is_none());
        assert!(store.path_for("thread-abc_123").is_some());
    }

    #[test]
    fn list_skips_corrupt_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("broken.json"), b"{not json").unwrap();
        let good = Thread::new("good");
        store.save(&good).unwrap();
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, good.id);
    }

    // ── deep payload inspection ──────────────────────────────────────

    #[test]
    fn thread_on_disk_json_schema_is_stable() {
        // Lock the persisted shape: field names + which are omitted. A change
        // here silently breaks resume of older threads.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let mut t = Thread::new("Subject line");
        t.active_policy = Some("nsed:review".into());
        let mut m = Message::now("assistant", "hi");
        m.job_id = Some("job-1".into());
        m.policy_id = Some("nsed:review".into());
        t.push_message(m);
        store.save(&t).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join(format!("{}.json", t.id))).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["id"], t.id);
        assert_eq!(v["subject"], "Subject line");
        assert_eq!(v["active_policy"], "nsed:review");
        assert!(v["created"].is_number() && v["updated"].is_number());
        // orchestrator / server_thread are None → omitted, not null.
        assert!(v.get("orchestrator").is_none());
        assert!(v.get("server_thread").is_none());
        let msg = &v["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "hi");
        assert_eq!(msg["job_id"], "job-1");
        assert_eq!(msg["policy_id"], "nsed:review");
        assert!(msg["ts"].is_number());
    }

    #[test]
    fn message_omits_none_policy_and_job_id_in_json() {
        let m = Message::now("user", "q");
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert!(v.get("policy_id").is_none());
        assert!(v.get("job_id").is_none());
        assert_eq!(v["role"], "user");
    }

    #[test]
    fn to_deliberation_query_preserves_multiline_content() {
        let mut t = Thread::new("");
        t.push_message(Message::now("user", "line1\nline2"));
        let q = t.to_deliberation_query("next");
        assert_eq!(q, "[user] line1\nline2\n\n[user] next");
    }

    // ── e2e: the full client-side thread turn cycle ──────────────────

    #[test]
    fn full_thread_turn_cycle_persists_ordered_attributed_transcript() {
        // Mirrors the runtime flow without NATS: a thread is created, the user
        // message is recorded + persisted on submit, then the deliberation
        // reply is appended by the completion path (a separate load/save). The
        // reloaded transcript must be ordered and correctly attributed.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());

        // Turn 1: submit records the user message (as ThreadView::submit does).
        let mut t = Thread::new("audit");
        t.active_policy = Some("nsed:audit".into());
        let query_1 = t.to_deliberation_query("first question");
        assert_eq!(query_1, "Subject: audit\n\nfirst question");
        t.push_message(Message::now("user", "first question"));
        store.save(&t).unwrap();

        // Completion appends the reply (as the loop's JobComplete does).
        assert!(store.append_reply(&t.id, "first answer", "job-1", None));

        // Turn 2: reload (ThreadView::on_enter), submit again.
        let mut t = store.load(&t.id).unwrap();
        assert_eq!(t.messages.len(), 2);
        let query_2 = t.to_deliberation_query("second question");
        assert_eq!(
            query_2,
            "Subject: audit\n\n[user] first question\n\n[assistant] first answer\n\n[user] second question"
        );
        t.push_message(Message::now("user", "second question"));
        store.save(&t).unwrap();
        assert!(store.append_reply(&t.id, "second answer", "job-2", None));

        // Final transcript: 4 ordered, attributed messages.
        let final_thread = store.load(&t.id).unwrap();
        let roles: Vec<&str> = final_thread
            .messages
            .iter()
            .map(|m| m.role.as_str())
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
        assert_eq!(final_thread.messages[1].job_id.as_deref(), Some("job-1"));
        assert_eq!(final_thread.messages[3].job_id.as_deref(), Some("job-2"));
        // Replies inherit the thread's active policy when none is passed.
        assert_eq!(
            final_thread.messages[1].policy_id.as_deref(),
            Some("nsed:audit")
        );
        assert_eq!(
            final_thread.messages[3].policy_id.as_deref(),
            Some("nsed:audit")
        );
        assert!(final_thread.updated >= final_thread.created);
    }
}
