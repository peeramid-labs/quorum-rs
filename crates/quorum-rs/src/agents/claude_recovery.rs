//! Recovery for the claude-CLI provider when the model emits a
//! malformed or aborted terminal-tool call.
//!
//! ## The gap
//!
//! Claude-CLI runs each propose/evaluate phase as
//! `claude --print --output-format json --resume <uuid>`. The model
//! submits its result via the in-process MCP server's `nsed_propose`
//! / `nsed_evaluate` tools, whose argument structs are deserialized
//! by `rmcp`'s strict `serde` layer. If the model emits malformed
//! args (truncated string, bad escape, missing required field), MCP
//! returns a tool error to claude *inside the CLI session*; claude
//! internally re-tries within its own React loop, may eventually
//! exit without ever submitting a valid terminal call, and our
//! `result_rx.try_recv()` comes up empty.
//!
//! Meanwhile the OpenAI-compatible provider path runs the
//! [`unwrap_hallucinated_tool_calls`] / [`repair_invalid_escapes`]
//! / [`repair_truncated_json`] pipeline before deserialization and
//! recovers gracefully. That pipeline is **not wired into the
//! claude-CLI path** (issue #347). r12/r13 telemetry showed ~4×
//! retry inflation on opus largely because of this gap — every
//! malformed-args event burned a full `claude --print` invocation.
//!
//! ## What this module does
//!
//! Two recovery primitives, both used by `mcp_agent::run_phase`
//! when `result_rx.try_recv()` returns empty after an apparently-
//! successful claude exit:
//!
//!   1. **Session-jsonl scrape** ([`recover_from_session`]).
//!      Locates the per-(working-dir, session) jsonl that the
//!      claude CLI writes (`~/.claude/projects/<munged>/<uuid>.jsonl`),
//!      walks it for the LAST `tool_use` block targeting the
//!      terminal MCP tool (`mcp__nsed__nsed_propose` /
//!      `mcp__nsed__nsed_evaluate`), and returns its `input` field.
//!      Caller deserializes into the strict `Parameters<*>` struct;
//!      if it succeeds, we synthesize the McpResult locally and
//!      skip a fresh-session retry.
//!
//!   2. **Failure-feedback on retry** ([`retry_feedback_block`]).
//!      Builds a short `--append-system-prompt` block describing
//!      the last failure mode (`MissingTerminalCall`,
//!      `MalformedArgs { reason }`, `Timeout`) so the next
//!      `--resume` invocation has explicit context for what went
//!      wrong. Cheap to apply, small token cost, doesn't require
//!      reading the session jsonl.
//!
//! Both are best-effort. If the jsonl can't be located or doesn't
//! contain a recoverable tool_use, recovery returns `None` and the
//! caller falls through to the existing fresh-session retry / error
//! surfacing path. Recovery never changes correctness — only adds
//! a faster path through the failure case.

use std::path::{Component, Path, PathBuf};

/// Collapse `.` and `..` components in a path **without** touching
/// symlinks. Mirrors the way claude's Node runtime sees its cwd
/// (`process.cwd()` returns the kernel cwd, which has `.` / `..`
/// already resolved by `chdir`) so the on-disk munged project name
/// we compute matches the one claude itself writes under
/// `~/.claude/projects/`.
///
/// `std::fs::canonicalize` would also work but additionally resolves
/// symlinks, and the comment on `claude_project_dir_name` is explicit
/// that claude does NOT canonicalize symlinks — keeping behaviour
/// aligned matters here. CR PR #349 finding.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Map a working-directory path to the name claude CLI uses for its
/// per-project session directory.
///
/// claude's convention is to take the absolute working-dir path and
/// replace every path separator (`/`) AND every `.` with `-`. So
/// `/home/dev/project` becomes `-home-dev-project`, and a dotted
/// component like `/srv/.worktrees/…` becomes `-srv--worktrees-…` (the
/// `/` and the `.` each map to `-`, giving the double dash). The
/// leading `-` is preserved because the original starts with `/`.
///
/// The `.` is load-bearing: agent worktrees live under a dotted
/// directory, so omitting the `.` mapping produced a slug that never
/// matched claude's on-disk `<uuid>.jsonl` — the recoverability check
/// always missed the session and forced a fresh `--session-id` run with
/// the FULL prompt every round (the token-bloat regression).
///
/// We don't try to canonicalize symlinks here — claude doesn't
/// either, so the path string we get from `claude_config.working_dir`
/// (or `cwd` fallback) maps directly.
pub fn claude_project_dir_name(working_dir: &Path) -> String {
    let s = working_dir.to_string_lossy();
    // Normalize backslashes too in case a Windows-style path slips
    // through the absolute-path coercion upstream.
    s.replace(['/', '\\', '.'], "-")
}

/// Resolve the per-session jsonl path under the user's
/// `~/.claude/projects/<project>/<uuid>.jsonl`. Returns `None` if
/// `HOME` (or `USERPROFILE`) is unset — recovery is best-effort and
/// silent failure here is preferable to a hard error.
///
/// The `working_dir` is absolutized before being munged into the
/// project directory name so the path matches what claude itself
/// writes: at spawn time `cmd.current_dir(relative)` is resolved
/// against the process cwd, and claude then writes its session
/// jsonl under the absolute resolved path's munged form. If we
/// munged the raw relative path here, the two would diverge and
/// recovery would silently miss the file (CR PR #349 finding).
pub fn session_jsonl_path(working_dir: &Path, claude_session_uuid: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let absolutized: PathBuf = if working_dir.is_absolute() {
        working_dir.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(working_dir)
    };
    // Drop `.` / `..` components (without touching symlinks) so the
    // munged project name matches what claude writes — claude's
    // `process.cwd()` returns the kernel cwd which has those
    // components already resolved.
    let resolved = normalize_lexical(&absolutized);
    let project = claude_project_dir_name(&resolved);
    Some(
        PathBuf::from(home)
            .join(".claude")
            .join("projects")
            .join(project)
            .join(format!("{claude_session_uuid}.jsonl")),
    )
}

/// Path to claude-cli's `session-env/<uuid>/` lock dir for a given
/// session UUID. claude-cli creates this dir on session start and
/// removes it on clean exit. An empty dir left behind by a SIGKILL'd
/// claude-cli child blocks any subsequent `--session-id <uuid>` /
/// `--resume <uuid>` invocation with `Error: Session ID ... is
/// already in use`, even though the holding process is dead.
///
/// Returns `None` only when `$HOME` / `$USERPROFILE` are both unset.
pub fn session_env_lock_path(claude_session_uuid: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".claude")
            .join("session-env")
            .join(claude_session_uuid),
    )
}

/// Remove an orphaned `session-env/<uuid>/` lock dir if it exists
/// AND is empty. Empty = no live claude-cli writing files inside =
/// orphan from a parent process killed mid-LLM-call. Non-empty
/// (live) dirs are left alone, so this is safe to call before every
/// claude-cli spawn.
///
/// Returns `true` when a sweep happened. The corresponding
/// `~/.claude/projects/<cwd>/<uuid>.jsonl` transcript is independent
/// and is preserved — `--resume` still picks back up with full
/// conversational context.
pub fn sweep_orphan_session_env_lock(claude_session_uuid: &str) -> bool {
    let Some(dir) = session_env_lock_path(claude_session_uuid) else {
        return false;
    };
    if !dir.is_dir() {
        return false;
    }
    let is_empty = match std::fs::read_dir(&dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => {
            tracing::warn!(
                uuid = claude_session_uuid,
                path = %dir.display(),
                error = %e,
                "Could not read session-env lock dir; treating as non-empty (skip sweep)"
            );
            false
        }
    };
    if !is_empty {
        return false;
    }
    match std::fs::remove_dir(&dir) {
        Ok(()) => {
            tracing::info!(
                uuid = claude_session_uuid,
                "Swept orphaned claude session-env lock dir"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                uuid = claude_session_uuid,
                error = %e,
                "Failed to remove orphaned claude session-env lock"
            );
            false
        }
    }
}

/// Force-remove the `session-env/<uuid>/` lock dir even when it is
/// NON-empty. Unlike [`sweep_orphan_session_env_lock`] (empty-only,
/// safe to call speculatively), this is for the post-failure retry
/// path where the caller has already reaped its own claude-cli child
/// for this exact uuid — so the dir is definitively orphaned by a
/// dead process, and leaving it would block the fresh `--session-id`
/// retry with `Error: Session ID ... is already in use`. The
/// `<uuid>.jsonl` transcript lives elsewhere and is preserved.
///
/// Returns `true` when a dir was removed.
pub fn force_clear_session_env_lock(claude_session_uuid: &str) -> bool {
    let Some(dir) = session_env_lock_path(claude_session_uuid) else {
        return false;
    };
    if !dir.is_dir() {
        return false;
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {
            tracing::warn!(
                uuid = claude_session_uuid,
                "Force-cleared claude session-env lock left by a dead child (post-failure retry)"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                uuid = claude_session_uuid,
                error = %e,
                "Failed to force-clear claude session-env lock"
            );
            false
        }
    }
}

/// Walk a claude session jsonl content and return the `input`
/// payload of the **last** `tool_use` block whose `name` matches
/// any of `tool_names`. Returns `None` when no matching tool_use
/// exists in the transcript.
///
/// The "last" tie-breaker matters because claude often produces
/// several attempts within a single React loop before giving up;
/// the last one is usually the most-correct attempt the model
/// could muster after MCP rejected earlier malformed versions.
pub fn extract_last_tool_use_args(
    jsonl_content: &str,
    tool_names: &[&str],
) -> Option<serde_json::Value> {
    extract_last_tool_use_args_after(jsonl_content, tool_names, 0)
}

/// Variant of [`extract_last_tool_use_args`] that only considers
/// lines whose **byte start position** falls at or after
/// `after_offset`. Used by mcp_agent to scope recovery to the
/// current claude attempt — claude's `--resume` reuses the same
/// session uuid (and therefore the same jsonl file) across phases
/// of the same agent, so an unbounded scan can surface a stale
/// `tool_use` block from a prior round/phase as the "last" hit.
///
/// Caller is expected to capture the file size BEFORE spawning
/// claude (`session_jsonl_size`), then pass that value here after
/// failure. Lines wholly before the offset are skipped; the line
/// containing the offset (if any) is also skipped because its
/// pre-offset prefix means it can't have been written by the
/// current attempt.
pub fn extract_last_tool_use_args_after(
    jsonl_content: &str,
    tool_names: &[&str],
    after_offset: usize,
) -> Option<serde_json::Value> {
    let mut last_input: Option<serde_json::Value> = None;
    let mut byte_pos = 0usize;
    // Use `split_inclusive('\n')` so the byte counter stays exact
    // — `lines()` strips the trailing `\n` and drifts the offset.
    for line in jsonl_content.split_inclusive('\n') {
        let line_start = byte_pos;
        byte_pos += line.len();
        if line_start < after_offset {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(message) = rec.get("message") else {
            continue;
        };
        let Some(content) = message.get("content").and_then(|v| v.as_array()) else {
            continue;
        };
        for block in content {
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if block_type != "tool_use" {
                continue;
            }
            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if tool_names.contains(&name)
                && let Some(input) = block.get("input")
            {
                last_input = Some(input.clone());
            }
        }
    }
    last_input
}

/// Combine [`session_jsonl_path`] + [`extract_last_tool_use_args`]:
/// locate the session file for the given (working_dir, session_uuid)
/// pair, read it, and return the args of the last terminal-tool
/// `tool_use` block (or `None` when the file is missing, unreadable,
/// or contains no matching tool_use).
///
/// Equivalent to [`recover_from_session_after`] with offset 0;
/// retained as a thin wrapper for callers that don't need the
/// concurrency-scoping behaviour.
pub fn recover_from_session(
    working_dir: &Path,
    claude_session_uuid: &str,
    tool_names: &[&str],
) -> Option<serde_json::Value> {
    recover_from_session_after(working_dir, claude_session_uuid, tool_names, 0)
}

/// Variant of [`recover_from_session`] that only considers
/// content appended to the jsonl AFTER `after_offset` bytes.
/// Used by mcp_agent to scope post-failure recovery to the
/// current claude attempt — see
/// [`extract_last_tool_use_args_after`] for the rationale.
pub fn recover_from_session_after(
    working_dir: &Path,
    claude_session_uuid: &str,
    tool_names: &[&str],
    after_offset: u64,
) -> Option<serde_json::Value> {
    let path = session_jsonl_path(working_dir, claude_session_uuid)?;
    let content = std::fs::read_to_string(&path).ok()?;
    let offset = usize::try_from(after_offset).unwrap_or(usize::MAX);
    extract_last_tool_use_args_after(&content, tool_names, offset)
}

/// Return the current size, in bytes, of the per-session jsonl
/// for `(working_dir, claude_session_uuid)`. Returns 0 if the
/// file doesn't exist, can't be read, or `HOME` isn't set.
///
/// Captured before spawning claude so post-failure recovery
/// (`recover_from_session_after`) can scope its scan to the
/// content the current attempt produced — claude's `--resume`
/// reuses the same uuid across phases, so the file may contain
/// `tool_use` blocks from prior rounds that we must not pick up
/// as "the last attempt's args".
pub fn session_jsonl_size(working_dir: &Path, claude_session_uuid: &str) -> u64 {
    let Some(path) = session_jsonl_path(working_dir, claude_session_uuid) else {
        return 0;
    };
    std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
}

/// Best-effort unwrap of an `input` Value pulled from the claude
/// session jsonl into the inner-most arguments object before
/// permissive extraction runs. Mirrors the part of `clean_json_string`
/// (in the `llm_repair` crate) that still applies once the transcript
/// writer has parsed the wire payload into a Value — the textual
/// repair stages (invalid-escape patching, truncation patching) are
/// no-ops on already-parsed JSON, so they don't help here.
///
/// Two shapes the model occasionally emits that look unrecoverable
/// but contain a salvageable payload one or two steps in:
///
///   1. **Stringified args**: `"input": "{\"content\":\"hi\"}"` —
///      a JSON string whose contents are themselves a JSON object
///      or array. We try `from_str` once; on success, recurse so a
///      doubly-wrapped variant still resolves.
///   2. **Envelope wrap**: `"input": {"name": "nsed_propose",
///      "arguments": {"content":"hi"}}` — the OpenAI-style
///      tool-call envelope inside `input` instead of just the
///      args. Detect by an `arguments` object key; return that
///      and recurse.
///
/// Recursion is depth-capped to avoid pathological inputs.
pub fn unwrap_recovered_input(value: serde_json::Value) -> serde_json::Value {
    fn inner(v: serde_json::Value, depth: u8) -> serde_json::Value {
        if depth > 4 {
            return v;
        }
        match v {
            serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(&s) {
                Ok(parsed) if parsed.is_object() || parsed.is_array() => inner(parsed, depth + 1),
                _ => serde_json::Value::String(s),
            },
            serde_json::Value::Object(ref map) => {
                if let Some(args) = map.get("arguments")
                    && args.is_object()
                {
                    return inner(args.clone(), depth + 1);
                }
                v
            }
            other => other,
        }
    }
    inner(value, 0)
}

/// Outcome of attempting to recover an `McpResult` from the on-disk
/// claude session jsonl after a missing-terminal-tool failure.
///
/// Distinguishes three states the caller has to act on differently:
///
///   - `Recovered(_)` — a terminal `tool_use` was present and its
///     args were salvageable; `run_phase` returns this directly and
///     skips a fresh-session retry.
///   - `Malformed(reason)` — a terminal `tool_use` was present but
///     its args couldn't be turned into a valid `McpResult` (missing
///     required fields, bad types, etc). The next retry should
///     surface a `LastFailureKind::MalformedArgs { reason }` feedback
///     block so claude knows what specifically broke, instead of the
///     generic "you didn't call the tool" wording (CR PR #349
///     finding).
///   - `NotFound` — no matching `tool_use` was emitted at all
///     (typically claude exited mid-React-loop before reaching the
///     terminal step). Retry uses the `MissingTerminalCall` feedback
///     wording.
#[derive(Debug)]
pub enum RecoveryOutcome<T> {
    Recovered(T),
    Malformed(String),
    NotFound,
}

/// Reason the previous claude attempt failed. Drives the wording of
/// the retry-feedback system-prompt block — claude responds better
/// to specific guidance (e.g. "your last attempt's args couldn't be
/// parsed as JSON") than to a generic "try again".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastFailureKind {
    /// claude exited cleanly (status 0) but `result_rx` was empty —
    /// no terminal MCP tool was ever invoked. Most often means the
    /// model decided it was "done" without calling `nsed_propose` /
    /// `nsed_evaluate`.
    MissingTerminalCall,
    /// A terminal call was attempted but its args failed
    /// deserialization (rmcp returned a structured error to claude
    /// inside the session). `reason` carries the serde error text
    /// when available.
    MalformedArgs { reason: String },
    /// claude exited non-zero or the wrapper hit its phase timeout.
    /// Less actionable than the above but still useful in retry
    /// prompts so the model knows the previous attempt was cut off.
    Timeout,
}

/// Render the failure context as an `--append-system-prompt` block
/// to inject into the next claude `--resume` call. Wording is
/// tight — every retry adds tokens — but specific enough that the
/// model can correct course.
pub fn retry_feedback_block(failure: &LastFailureKind, terminal_tool: &str) -> String {
    let header = "<previous_attempt_failed>";
    let footer = "</previous_attempt_failed>";
    let body = match failure {
        LastFailureKind::MissingTerminalCall => format!(
            "STOP — your previous attempt ended WITHOUT calling `{terminal_tool}`, so \
             NOTHING was submitted and the turn failed. Calling `{terminal_tool}` is \
             mandatory and is the ONLY way to submit. Your very next action MUST be the \
             `{terminal_tool}` tool call with valid JSON arguments. Do NOT write any \
             prose, analysis, or explanation outside the tool call — call the tool now."
        ),
        LastFailureKind::MalformedArgs { reason } => format!(
            "Your previous `{terminal_tool}` call failed to deserialize: {reason}. \
             Re-emit the call with valid JSON args — pay particular attention to \
             string escaping (use `\\\"` for embedded quotes, `\\n` for newlines, \
             never raw control characters) and to required fields being non-null."
        ),
        LastFailureKind::Timeout => format!(
            "Your previous attempt was cut off before completing. \
             Be concise; produce your `{terminal_tool}` call promptly without \
             excessive intermediate reasoning."
        ),
    };
    format!("{header}\n{body}\n{footer}")
}

/// True when the process runs as root (euid 0), where the kernel bypasses
/// directory permission bits — a 0o000 dir is still readable to root.
/// Only consumed by the permission-sensitive sweep test.
#[cfg(all(unix, test))]
fn running_as_root() -> bool {
    // SAFETY: geteuid is a pure syscall with no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn project_dir_name_replaces_separators() {
        assert_eq!(
            claude_project_dir_name(&PathBuf::from("/home/dev/project")),
            "-home-dev-project"
        );
        assert_eq!(claude_project_dir_name(&PathBuf::from("/work")), "-work");
        // Dotted component (the agent worktrees live under `.pd-worktrees`): the `.`
        // must map to `-` too, matching claude's on-disk slug byte-for-byte, or the
        // session is never found and every round re-sends the full prompt.
        assert_eq!(
            claude_project_dir_name(&PathBuf::from(
                "/srv/deliberation/.worktrees/thread-abc/ReviewerBot"
            )),
            "-srv-deliberation--worktrees-thread-abc-ReviewerBot"
        );
        // Backslash form (Windows-y path passed through to_string_lossy).
        let mut p = PathBuf::from("");
        p.push("C:");
        p.push("Users");
        p.push("dev");
        // On unix this becomes `C:/Users/dev` — replacement still
        // strips both kinds of separator.
        let mapped = claude_project_dir_name(&p);
        assert!(!mapped.contains('/'));
        assert!(!mapped.contains('\\'));
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn session_jsonl_path_uses_home() {
        // Must not crash and must place the uuid as a leaf .jsonl.
        // Saved + restored HOME so we don't pollute other tests.
        // `#[serial]` ensures parallel cargo test runs don't race on
        // the global HOME env var (CR finding).
        let prev = std::env::var_os("HOME");
        // SAFETY: env mutation in tests is serialized via the
        // `home_env` serial-group key; keep the unsafe block tight
        // to the actual mutation.
        unsafe {
            std::env::set_var("HOME", "/tmp/nsed-test-home");
        }
        let p = session_jsonl_path(&PathBuf::from("/home/dev/project"), "abc-123-uuid")
            .expect("HOME set");
        assert!(
            p.ends_with(PathBuf::from(
                ".claude/projects/-home-dev-project/abc-123-uuid.jsonl"
            )),
            "got {}",
            p.display()
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn sweep_orphan_session_env_lock_removes_only_empty() {
        let prev = std::env::var_os("HOME");
        let tmp = tempfile::tempdir().expect("tempdir");
        // SAFETY: env mutation in tests is serialized via `home_env`.
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let session_env = tmp.path().join(".claude/session-env");
        std::fs::create_dir_all(&session_env).unwrap();

        // Empty orphan dir → swept, returns true.
        let orphan_uuid = "ffffffff-ffff-4fff-8fff-ffffffffffff";
        let orphan = session_env.join(orphan_uuid);
        std::fs::create_dir(&orphan).unwrap();
        assert!(super::sweep_orphan_session_env_lock(orphan_uuid));
        assert!(!orphan.exists());

        // Live dir (has a file inside) → NOT swept, returns false.
        let live_uuid = "11111111-1111-4111-8111-111111111111";
        let live = session_env.join(live_uuid);
        std::fs::create_dir(&live).unwrap();
        std::fs::write(live.join("env.json"), "{}").unwrap();
        assert!(!super::sweep_orphan_session_env_lock(live_uuid));
        assert!(live.exists(), "live session-env dir must be preserved");

        // No dir at all → idempotent no-op, returns false.
        let absent_uuid = "22222222-2222-4222-8222-222222222222";
        assert!(!super::sweep_orphan_session_env_lock(absent_uuid));

        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn force_clear_removes_a_non_empty_lock() {
        let prev = std::env::var_os("HOME");
        let tmp = tempfile::tempdir().expect("tempdir");
        // SAFETY: env mutation in tests is serialized via `home_env`.
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let session_env = tmp.path().join(".claude/session-env");
        std::fs::create_dir_all(&session_env).unwrap();

        // A non-empty lock (the quota-killed-child case) — empty-only sweep
        // skips it, but force-clear removes it so a fresh retry can proceed.
        let uuid = "33333333-3333-4333-8333-333333333333";
        let held = session_env.join(uuid);
        std::fs::create_dir(&held).unwrap();
        std::fs::write(held.join("env.json"), "{}").unwrap();
        assert!(
            !super::sweep_orphan_session_env_lock(uuid),
            "empty-only sweep skips it"
        );
        assert!(
            super::force_clear_session_env_lock(uuid),
            "force-clear removes it"
        );
        assert!(!held.exists());
        // Absent → idempotent no-op.
        assert!(!super::force_clear_session_env_lock(uuid));

        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn running_as_root_matches_geteuid() {
        // SAFETY: geteuid is always safe to call.
        let expected = unsafe { libc::geteuid() } == 0;
        assert_eq!(running_as_root(), expected);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(home_env)]
    fn sweep_orphan_session_env_lock_handles_unreadable_dir() {
        use std::os::unix::fs::PermissionsExt;
        // Root (e.g. CI containers running as uid 0) bypasses directory
        // permission bits, so a 0o000 dir stays readable and this test's
        // "unreadable → EACCES" premise cannot be reproduced: the sweep would
        // legitimately treat the dir as empty and remove it, then the perm
        // restore below hits NotFound. Skip rather than assert a false premise.
        if running_as_root() {
            return;
        }
        let prev = std::env::var_os("HOME");
        let tmp = tempfile::tempdir().expect("tempdir");
        // SAFETY: env mutation in tests is serialized via `home_env`.
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let session_env = tmp.path().join(".claude/session-env");
        std::fs::create_dir_all(&session_env).unwrap();
        let uuid = "33333333-3333-4333-8333-333333333333";
        let dir = session_env.join(uuid);
        std::fs::create_dir(&dir).unwrap();
        // Strip read perms so read_dir returns Err(EACCES). is_dir()
        // still succeeds because it stats from the parent.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Must NOT panic; must return false (treat-as-non-empty);
        // must leave the dir intact for later investigation.
        let result = super::sweep_orphan_session_env_lock(uuid);

        // Restore perms before assertions so failure cleanup works.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!result, "unreadable dir must not be swept");
        assert!(dir.exists(), "unreadable dir must be preserved");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn extract_last_tool_use_args_picks_last_match() {
        let content = r#"{"type":"queue-operation","operation":"enqueue"}
{"message":{"content":[{"type":"tool_use","name":"other","input":{"a":1}}]}}
{"message":{"content":[{"type":"tool_use","name":"mcp__nsed__nsed_propose","input":{"thought_process":"first attempt","content":"v1"}}]}}
{"message":{"content":[{"type":"tool_use","name":"mcp__nsed__nsed_propose","input":{"thought_process":"second attempt","content":"v2"}}]}}
{"type":"queue-operation","operation":"dequeue"}
"#;
        let got = extract_last_tool_use_args(content, &["mcp__nsed__nsed_propose"])
            .expect("should find at least one");
        assert_eq!(got["thought_process"], "second attempt");
        assert_eq!(got["content"], "v2");
    }

    #[test]
    fn extract_last_tool_use_args_returns_none_when_no_match() {
        let content = r#"{"message":{"content":[{"type":"tool_use","name":"some_other_tool","input":{}}]}}
{"message":{"content":[{"type":"text","text":"hello"}]}}
"#;
        assert!(extract_last_tool_use_args(content, &["mcp__nsed__nsed_propose"]).is_none());
    }

    #[test]
    fn extract_handles_malformed_lines_gracefully() {
        let content = "not json\n\
                       {\"valid\": \"but no message\"}\n\
                       {\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"mcp__nsed__nsed_evaluate\",\"input\":{\"evaluations\":[]}}]}}\n\
                       {malformed} json\n";
        let got = extract_last_tool_use_args(content, &["mcp__nsed__nsed_evaluate"]).unwrap();
        assert!(got["evaluations"].is_array());
    }

    #[test]
    fn retry_feedback_block_distinguishes_kinds() {
        let missing = retry_feedback_block(
            &LastFailureKind::MissingTerminalCall,
            "mcp__nsed__nsed_propose",
        );
        assert!(missing.to_lowercase().contains("without calling"));
        assert!(missing.contains("mandatory"));
        assert!(missing.contains("mcp__nsed__nsed_propose"));

        let malformed = retry_feedback_block(
            &LastFailureKind::MalformedArgs {
                reason: "missing field `content`".to_string(),
            },
            "mcp__nsed__nsed_propose",
        );
        assert!(malformed.contains("failed to deserialize"));
        assert!(malformed.contains("missing field `content`"));

        let timeout = retry_feedback_block(&LastFailureKind::Timeout, "mcp__nsed__nsed_evaluate");
        assert!(timeout.contains("cut off"));
    }

    #[test]
    fn unwrap_recovered_input_handles_envelope_and_stringified() {
        // Already-clean object → returned as-is.
        let clean = serde_json::json!({"content": "hi", "thought_process": "tp"});
        assert_eq!(unwrap_recovered_input(clean.clone()), clean);

        // Envelope wrap: {"name": ..., "arguments": {...}} → arguments.
        let envelope = serde_json::json!({
            "name": "nsed_propose",
            "arguments": {"content": "hi"}
        });
        let unwrapped = unwrap_recovered_input(envelope);
        assert_eq!(unwrapped, serde_json::json!({"content": "hi"}));

        // Stringified JSON object → parsed.
        let stringified = serde_json::Value::String(r#"{"content": "hi"}"#.to_string());
        let unwrapped = unwrap_recovered_input(stringified);
        assert_eq!(unwrapped, serde_json::json!({"content": "hi"}));

        // Doubly wrapped: stringified envelope → arguments.
        let doubly =
            serde_json::Value::String(r#"{"name":"x","arguments":{"content":"hi"}}"#.to_string());
        assert_eq!(
            unwrap_recovered_input(doubly),
            serde_json::json!({"content": "hi"})
        );

        // Non-recoverable string (not JSON) → returned unchanged.
        let plain = serde_json::Value::String("not json at all".to_string());
        assert_eq!(
            unwrap_recovered_input(plain.clone()),
            serde_json::Value::String("not json at all".to_string())
        );

        // Object without `arguments` key → returned as-is.
        let no_args = serde_json::json!({"foo": 1, "bar": "baz"});
        assert_eq!(unwrap_recovered_input(no_args.clone()), no_args);

        // `arguments` present but not an object → not unwrapped
        // (envelope shape requires args to be an object).
        let bad_args = serde_json::json!({"name": "x", "arguments": "not-an-object"});
        assert_eq!(unwrap_recovered_input(bad_args.clone()), bad_args);
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn session_jsonl_path_absolutizes_relative_working_dir() {
        // A relative working_dir like `.` must produce the SAME
        // path that an absolute working_dir of the current cwd
        // would. Otherwise claude (which resolves cmd.current_dir
        // against the process cwd at spawn time) writes its jsonl
        // under one munged path while recovery looks under another
        // — silent miss. CR PR #349 finding.
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/nsed-test-home-absolutize");
        }
        let cwd = std::env::current_dir().expect("cwd available");
        let abs = session_jsonl_path(&cwd, "abc-uuid").expect("HOME set");
        let rel = session_jsonl_path(&PathBuf::from("."), "abc-uuid").expect("HOME set");
        assert_eq!(
            abs, rel,
            "relative working_dir must produce the same session path as the resolved absolute cwd"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn extract_after_offset_skips_prior_content() {
        // Two `tool_use` blocks for the same terminal tool. The
        // first ("v1") is the "prior phase's content" we wrote
        // before spawning the current attempt; the second ("v2")
        // is what the current attempt appended. Capturing the
        // offset right after v1's record means recovery must
        // return v2 (or None when the offset covers everything).
        let prior = r#"{"message":{"content":[{"type":"tool_use","name":"mcp__nsed__nsed_propose","input":{"thought_process":"prior","content":"v1"}}]}}"#;
        let current = r#"{"message":{"content":[{"type":"tool_use","name":"mcp__nsed__nsed_propose","input":{"thought_process":"current","content":"v2"}}]}}"#;
        let mut full = String::new();
        full.push_str(prior);
        full.push('\n');
        let after_prior = full.len();
        full.push_str(current);
        full.push('\n');

        // Unbounded: returns the final entry (v2), as before.
        let unbounded = extract_last_tool_use_args(&full, &["mcp__nsed__nsed_propose"]).unwrap();
        assert_eq!(unbounded["content"], "v2");

        // Bounded at the start: identical to unbounded.
        let from_zero =
            extract_last_tool_use_args_after(&full, &["mcp__nsed__nsed_propose"], 0).unwrap();
        assert_eq!(from_zero["content"], "v2");

        // Bounded just after the prior record: still finds v2.
        let from_prior =
            extract_last_tool_use_args_after(&full, &["mcp__nsed__nsed_propose"], after_prior)
                .unwrap();
        assert_eq!(from_prior["content"], "v2");

        // Bounded past the prior record but BEFORE current is
        // appended (simulates what happens when claude exits
        // without writing anything new) → no recovery.
        let pre_current = extract_last_tool_use_args_after(
            prior, // only the prior portion, no current appended
            &["mcp__nsed__nsed_propose"],
            after_prior,
        );
        assert!(
            pre_current.is_none(),
            "offset past file end must return None — got {pre_current:?}"
        );

        // Bounded past everything → None.
        let past_end =
            extract_last_tool_use_args_after(&full, &["mcp__nsed__nsed_propose"], full.len() + 1);
        assert!(past_end.is_none());
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn session_jsonl_size_returns_zero_for_missing_file() {
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/nsed-test-home-jsonl-size-missing");
        }
        let size = session_jsonl_size(
            &PathBuf::from("/work"),
            "11111111-1111-1111-1111-111111111111",
        );
        assert_eq!(size, 0);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn session_jsonl_size_then_recover_after_round_trips() {
        // End-to-end: write a fake jsonl with a "prior" record,
        // capture size, append a "current" record, recover with
        // the captured offset → only the current record surfaces.
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let working_dir = PathBuf::from("/round-trip-test");
        let uuid = "22222222-2222-2222-2222-222222222222";
        let path = session_jsonl_path(&working_dir, uuid).expect("HOME set");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");

        let prior = "{\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"mcp__nsed__nsed_propose\",\"input\":{\"content\":\"prior-phase\"}}]}}\n";
        std::fs::write(&path, prior).expect("write prior");

        let captured_offset = session_jsonl_size(&working_dir, uuid);
        assert_eq!(captured_offset as usize, prior.len());

        // Append "current" record (mimics claude's append-only
        // write during this attempt).
        let current = "{\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"mcp__nsed__nsed_propose\",\"input\":{\"content\":\"current-attempt\"}}]}}\n";
        let mut combined = std::fs::read_to_string(&path).unwrap();
        combined.push_str(current);
        std::fs::write(&path, combined).expect("append current");

        // Unbounded picks the "last" — current.
        let unbounded =
            recover_from_session(&working_dir, uuid, &["mcp__nsed__nsed_propose"]).unwrap();
        assert_eq!(unbounded["content"], "current-attempt");

        // Bounded at captured_offset must ALSO pick current and
        // explicitly NOT prior.
        let bounded = recover_from_session_after(
            &working_dir,
            uuid,
            &["mcp__nsed__nsed_propose"],
            captured_offset,
        )
        .unwrap();
        assert_eq!(bounded["content"], "current-attempt");

        // Capture offset AFTER appending current (simulates a
        // retry that adds nothing new) → no recovery.
        let post_current_offset = session_jsonl_size(&working_dir, uuid);
        let none = recover_from_session_after(
            &working_dir,
            uuid,
            &["mcp__nsed__nsed_propose"],
            post_current_offset,
        );
        assert!(none.is_none());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn recover_from_session_returns_none_on_missing_file() {
        // Point at a non-existent session uuid; should silently
        // return None rather than panic.
        // Same `home_env` serial group as `session_jsonl_path_uses_home`
        // so HOME mutations don't race under parallel runs.
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/nsed-test-home-does-not-exist-xyz");
        }
        let got = recover_from_session(
            &PathBuf::from("/work"),
            "00000000-0000-0000-0000-000000000000",
            &["mcp__nsed__nsed_propose"],
        );
        assert!(got.is_none());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
