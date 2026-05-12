//! Scoped recursive grep — runs `grep -rEn` confined to a set of
//! allowed root directories, with per-call match-count, byte, and
//! wall-clock caps. Mirrors the security model of `scoped_read.rs`
//! (canonicalize-then-prefix).
//!
//! The tool exists because non-claude agents (provider_type openai)
//! do not get native Grep / Bash. Without it, an aggregator like
//! GlmAggregatorBot can fetch a single file via `read_file` but
//! can't *find* the line a peer cited as `pr3104.patch:NNN` unless
//! the line number is right. With grep_search, the aggregator can
//! verify peer citations like "BFIN_SERIAL_MAJOR is undefined" by
//! actually searching the patch for the symbol.
//!
//! Threat model is the same as `read_file`:
//! - All allowed roots canonicalized once at construction; bad roots
//!   fail loud at startup, not silently per-call.
//! - The optional `path` argument is canonicalized and must resolve
//!   under one of the allowed roots; symlink-out is rejected.
//! - The `pattern` argument is passed as a single argv element to
//!   grep — never interpolated into a shell. Catastrophic-backtracking
//!   patterns can stall grep, so a wall-clock timeout terminates the
//!   subprocess.
//! - Output is bounded by a stdout read cap and a Rust-side
//!   workspace-wide line cap (`max_results`). Over-cap output is
//!   truncated with an explicit `truncated: true` marker so the
//!   agent doesn't think it has the complete result. (Note: an
//!   earlier revision passed `grep -m <N>` to the subprocess; in
//!   recursive mode that flag is per-FILE, so it leaked up to
//!   `N × files` matches past the intended cap. The current
//!   implementation enforces the cap after collecting stdout.)
//! - One audit log line per call.

use crate::tools::Tool;
use async_openai::types::{ChatCompletionTool, ChatCompletionToolType, FunctionObject};
use async_trait::async_trait;
use schemars::schema_for;
use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::info;

/// Default page size in match LINES when the model omits `limit`.
/// Sized for openai-compat agents on tight context windows: 10
/// matches per call ≈ 200-500 tokens (typical match line is one
/// path:lineno:content).
pub const DEFAULT_GREP_PAGE_LINES: usize = 10;

#[derive(Deserialize, schemars::JsonSchema)]
struct GrepArgs {
    /// Extended regex (POSIX ERE). Used as `grep -E` argument.
    pattern: String,
    /// Optional sub-path under one of the allowed roots. When omitted,
    /// every allowed root is searched. Path must canonicalize under
    /// one of the allowed roots — `..`-traversal and out-of-sandbox
    /// symlinks are rejected.
    #[serde(default)]
    path: Option<String>,
    /// Optional shell-glob filter passed to `grep --include`.
    /// Examples: `"*.c"`, `"*.yaml"`, `"adi_*.h"`. Applies to
    /// filenames during the recursive walk.
    #[serde(default)]
    include: Option<String>,
    /// When true, pass `-i` for case-insensitive matching.
    #[serde(default)]
    case_insensitive: bool,
    /// Match-index to start from (0-based). Defaults to 0. Use
    /// `next_offset` from a prior response to iterate through
    /// match results without re-grepping the same prefix.
    #[serde(default)]
    offset: Option<usize>,
    /// Maximum match lines to return THIS call. Defaults to 10 so
    /// a high-frequency pattern doesn't blow the agent's context
    /// window in one shot. Hard-capped at `max_results` (the
    /// per-tool sandbox ceiling).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct ScopedGrepTool {
    roots: Vec<PathBuf>,
    max_bytes: usize,
    max_results: usize,
    timeout: Duration,
    agent_name: String,
}

impl ScopedGrepTool {
    pub fn new(
        agent_name: String,
        raw_roots: &[String],
        max_bytes: usize,
        max_results: usize,
        timeout_secs: u64,
    ) -> Result<Self, String> {
        if raw_roots.is_empty() {
            return Err(
                "grep_search tool requires at least one `roots:` entry — refusing to \
                 instantiate with an empty allow-list"
                    .to_string(),
            );
        }
        let mut roots = Vec::with_capacity(raw_roots.len());
        let mut errs: Vec<String> = Vec::new();
        for r in raw_roots {
            match std::fs::canonicalize(r) {
                Ok(p) => roots.push(p),
                Err(e) => errs.push(format!("{r}: {e}")),
            }
        }
        if !errs.is_empty() {
            return Err(format!(
                "grep_search failed to canonicalize root(s): {}",
                errs.join("; ")
            ));
        }
        Ok(Self {
            roots,
            max_bytes,
            max_results,
            timeout: Duration::from_secs(timeout_secs.max(1)),
            agent_name,
        })
    }

    /// Resolve the search-path argument. When `path` is supplied,
    /// canonicalize and ensure it sits under one of the allowed
    /// roots. When `path` is None, return every allowed root so the
    /// caller can grep them all in one invocation.
    fn resolve_search_paths(&self, path: Option<&str>) -> Result<Vec<PathBuf>, String> {
        match path {
            None => Ok(self.roots.clone()),
            Some(p) => {
                // Per the `path` arg doccomment, agents pass a
                // **sub-path under one of the allowed roots** — almost
                // always relative (e.g. `drivers/clk/`). Naive
                // `canonicalize(p)` would resolve relative paths
                // against the agent process's CWD, which is unrelated
                // to the allowed roots, so a path like `drivers/clk`
                // would either NOT-FOUND or land far outside the
                // sandbox by accident. Walk the roots first when the
                // input is relative; only an absolute path is resolved
                // as-is.
                let p_path = Path::new(p);
                let candidates: Vec<PathBuf> = if p_path.is_absolute() {
                    vec![p_path.to_path_buf()]
                } else {
                    self.roots.iter().map(|r| r.join(p_path)).collect()
                };
                let mut last_err: Option<String> = None;
                for cand in candidates {
                    match std::fs::canonicalize(&cand) {
                        Ok(canonical) => {
                            if self.roots.iter().any(|r| canonical.starts_with(r)) {
                                return Ok(vec![canonical]);
                            }
                            last_err = Some(format!(
                                "path {} not under any allowed root",
                                canonical.display()
                            ));
                        }
                        Err(e) => {
                            last_err = Some(format!("path {p:?} not found: {e}"));
                        }
                    }
                }
                Err(last_err.unwrap_or_else(|| format!("path {p:?}: no allowed roots configured")))
            }
        }
    }
}

#[async_trait]
impl Tool for ScopedGrepTool {
    fn name(&self) -> String {
        "grep_search".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        let allowed = self
            .roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(format!(
                    "Recursive regex search (grep -rEn) confined to allowed \
                     roots: [{allowed}]. Returns matching lines as \
                     'path:line:text'. Paginated: pass `offset` + `limit` to \
                     iterate through the match list. Default page is {} \
                     matches; hard cap is {} matches per call (also bounded \
                     by {} bytes of grep output). Response includes \
                     `total_matches`, `next_offset`, `has_more`.",
                    DEFAULT_GREP_PAGE_LINES, self.max_results, self.max_bytes
                )),
                parameters: Some(schema_for!(GrepArgs).into()),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: GrepArgs = serde_json::from_value(args)?;
        let search_paths = match self.resolve_search_paths(args.path.as_deref()) {
            Ok(p) => p,
            Err(detail) => {
                info!(
                    agent = %self.agent_name,
                    tool = "grep_search",
                    pattern = %args.pattern,
                    path = ?args.path,
                    result = "denied:out_of_sandbox",
                    "grep_search: path resolution failed"
                );
                return Ok(json!({
                    "error": "GREP_OUT_OF_SANDBOX",
                    "detail": detail,
                })
                .to_string());
            }
        };

        // Don't pass `-m{N}` here: in recursive mode (`-r`) grep
        // applies `-m` PER FILE, so a search across many files could
        // produce up to N×files matches. Enforce the global cap
        // Rust-side after collecting stdout (see truncate-to-cap
        // block below).
        let mut cmd = Command::new("grep");
        cmd.arg("-rEn"); // recursive, extended regex, line numbers
        if args.case_insensitive {
            cmd.arg("-i");
        }
        if let Some(g) = &args.include {
            cmd.arg("--include").arg(g);
        }
        cmd.arg("-e").arg(&args.pattern);
        for p in &search_paths {
            cmd.arg(p);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("grep_search: spawn failed: {e}"))?;

        let mut stdout_handle = child.stdout.take().expect("stdio piped at spawn");
        let mut stderr_handle = child.stderr.take().expect("stdio piped at spawn");

        let read_fut = async {
            // Cap stdout at max_bytes + 1 sentinel so we know if it
            // overflowed without buffering the entire (potentially
            // multi-GiB) output. We continue draining stdout after the
            // cap is reached (discarding the overflow) so the child
            // doesn't block on a full pipe — that previously made
            // `child.wait()` hang on grep outputs longer than max_bytes
            // (CR finding). Stderr is read concurrently for the same
            // reason: a noisy stderr (e.g. "permission denied" lines on
            // a large tree) could fill the stderr pipe while we were
            // still draining stdout, deadlocking the wait.
            let cap = self.max_bytes.saturating_add(1);
            let buf = Vec::with_capacity(cap.min(64 * 1024));
            let stdout_task = tokio::spawn(async move {
                let mut buf = buf;
                let mut chunk = [0u8; 8192];
                loop {
                    let n = stdout_handle.read(&mut chunk).await?;
                    if n == 0 {
                        break;
                    }
                    if buf.len() < cap {
                        let remaining = cap.saturating_sub(buf.len());
                        let take = n.min(remaining);
                        buf.extend_from_slice(&chunk[..take]);
                    }
                    // Past the cap we keep reading and discarding so
                    // the child's stdout pipe never fills.
                }
                Ok::<Vec<u8>, std::io::Error>(buf)
            });
            let stderr_task = tokio::spawn(async move {
                // Cap stderr memory at 64 KiB while still draining the pipe
                // so the child can't block writing to a full stderr buffer.
                // Mirrors the stdout drain pattern above: accumulate up to
                // the cap, then keep reading and discarding so EOF is reached.
                const MAX_STDERR_BYTES: usize = 64 * 1024;
                let mut errbuf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = stderr_handle.read(&mut chunk).await?;
                    if n == 0 {
                        break;
                    }
                    if errbuf.len() < MAX_STDERR_BYTES {
                        let remaining = MAX_STDERR_BYTES.saturating_sub(errbuf.len());
                        let take = n.min(remaining);
                        errbuf.extend_from_slice(&chunk[..take]);
                    }
                    // Past cap: keep draining into the void.
                }
                Ok::<Vec<u8>, std::io::Error>(errbuf)
            });
            let stdout_bytes = stdout_task
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))??;
            let stderr_bytes = stderr_task
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))??;
            Ok::<(Vec<u8>, Vec<u8>), std::io::Error>((stdout_bytes, stderr_bytes))
        };

        let result = match timeout(self.timeout, read_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                let _ = child.kill().await;
                info!(
                    agent = %self.agent_name,
                    tool = "grep_search",
                    pattern = %args.pattern,
                    result = "error:io",
                    error = %e,
                    "grep_search: stdout read failed"
                );
                return Ok(json!({
                    "error": "GREP_IO_ERROR",
                    "detail": e.to_string(),
                })
                .to_string());
            }
            Err(_) => {
                let _ = child.kill().await;
                info!(
                    agent = %self.agent_name,
                    tool = "grep_search",
                    pattern = %args.pattern,
                    timeout_secs = self.timeout.as_secs(),
                    result = "denied:timeout",
                    "grep_search: subprocess exceeded wall-clock cap"
                );
                return Ok(json!({
                    "error": "GREP_TIMEOUT",
                    "timeout_secs": self.timeout.as_secs(),
                    "detail": "subprocess wall-clock cap exceeded — pattern may be \
                              catastrophic-backtracking; try a tighter regex",
                })
                .to_string());
            }
        };
        // Bound child.wait under the same wall-clock as the read futures
        // so a process that survives after closing its streams can't hang
        // the tool. On timeout, kill the child and treat the exit as
        // unknown — the tool already has the captured stdout/stderr.
        let exit_code = match timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => status.code(),
            Ok(Err(_)) => None,
            Err(_) => {
                let _ = child.kill().await;
                None
            }
        };

        let (mut stdout_bytes, stderr_bytes) = result;
        let truncated = stdout_bytes.len() > self.max_bytes;
        if truncated {
            stdout_bytes.truncate(self.max_bytes);
        }

        // Parse stdout as UTF-8 lossy so weird bytes don't poison the
        // JSON response.
        let matches = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();

        // grep exit codes:
        //   0 = matches found
        //   1 = no matches (NOT an error — the agent should see an
        //       empty result, not an exception)
        //   2+ = real error (invalid regex, missing file, EPIPE, etc.)
        // Previously we ignored the status entirely, which made an
        // invalid-pattern user error indistinguishable from a clean
        // "no matches" result. Surface 2+ as a structured error so
        // the agent can fix its query rather than guess.
        if matches!(exit_code, Some(c) if c >= 2) {
            info!(
                agent = %self.agent_name,
                tool = "grep_search",
                pattern = %args.pattern,
                exit_code = ?exit_code,
                result = "error:grep_exit",
                "grep_search: subprocess exited with error code"
            );
            return Ok(json!({
                "error": "GREP_PROCESS_ERROR",
                "exit_code": exit_code,
                "stderr": stderr_str,
                "detail": "grep exited with a non-match-related error code \
                          (2+ usually means invalid pattern or unreadable \
                          path); see stderr for the upstream message",
            })
            .to_string());
        }

        // Paginate the match lines. Each line is a single
        // `path:lineno:content` record; slice by line index so the
        // model can iterate via `next_offset`.
        // Enforce the GLOBAL match cap here — `-m{N}` was per-file
        // in recursive mode; truncating the line list to the first
        // `max_results` entries gives a real workspace-wide cap. If
        // we drop entries here, mark truncated so the response
        // signals "there's more behind this".
        let raw_lines: Vec<&str> = matches.lines().collect();
        let over_cap = raw_lines.len() > self.max_results;
        let all_lines: Vec<&str> = if over_cap {
            raw_lines[..self.max_results].to_vec()
        } else {
            raw_lines
        };
        let truncated = truncated || over_cap;
        let total_matches = all_lines.len();
        let req_offset = args.offset.unwrap_or(0);
        // Floor at 1 so a caller that passes `limit: 0` cannot produce a
        // zero page that would make `next_offset == offset` and loop the
        // caller forever.
        let req_limit = args
            .limit
            .unwrap_or(DEFAULT_GREP_PAGE_LINES)
            .min(self.max_results)
            .max(1);
        let start = req_offset.min(total_matches);
        let end = (start + req_limit).min(total_matches);
        let page_lines = &all_lines[start..end];
        let page = page_lines.join("\n");
        let next_offset = if end < total_matches { Some(end) } else { None };
        let has_more = next_offset.is_some();

        info!(
            agent = %self.agent_name,
            tool = "grep_search",
            pattern = %args.pattern,
            include = ?args.include,
            roots_searched = search_paths.len(),
            output_bytes = stdout_bytes.len(),
            total_matches = total_matches,
            offset = start,
            returned = page_lines.len(),
            has_more = has_more,
            truncated = truncated,
            exit_code = ?exit_code,
            result = "ok",
            "grep_search"
        );

        Ok(json!({
            "pattern": args.pattern,
            "include": args.include,
            "case_insensitive": args.case_insensitive,
            "matches": page,
            "match_lines": page_lines.len(),
            "total_matches": total_matches,
            "offset": start,
            "has_more": has_more,
            "next_offset": next_offset,
            "truncated": truncated,
            "stderr": if stderr_str.is_empty() { Value::Null } else { Value::String(stderr_str) },
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn make(root: &TempDir, agent: &str, max_results: usize) -> ScopedGrepTool {
        ScopedGrepTool::new(
            agent.into(),
            &[root.path().display().to_string()],
            1024 * 1024,
            max_results,
            10,
        )
        .expect("ctor")
    }

    fn write(p: &std::path::Path, c: &str) {
        std::fs::write(p, c).expect("write");
    }

    #[tokio::test]
    async fn finds_pattern_under_root() {
        let root = TempDir::new().unwrap();
        write(
            &root.path().join("a.c"),
            "int hello = 1;\nNEEDLE here\nbye\n",
        );
        write(&root.path().join("b.c"), "no match\n");
        let tool = make(&root, "AgentA", 200);
        let out = tool.call(json!({"pattern": "NEEDLE"})).await.unwrap();
        assert!(out.contains("NEEDLE here"), "should find: {out}");
        assert!(out.contains("a.c:2:"), "should report path:line: {out}");
    }

    #[tokio::test]
    async fn denied_for_path_outside_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write(&outside.path().join("secret.txt"), "MATCH should never see");
        let tool = make(&root, "AgentB", 200);
        let out = tool
            .call(json!({
                "pattern": "MATCH",
                "path": outside.path().display().to_string()
            }))
            .await
            .unwrap();
        assert!(out.contains("GREP_OUT_OF_SANDBOX"), "should reject: {out}");
        assert!(!out.contains("should never see"), "must not leak: {out}");
    }

    #[tokio::test]
    async fn include_glob_filters_files() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("hit.c"), "FOO\n");
        write(&root.path().join("skip.txt"), "FOO\n");
        let tool = make(&root, "AgentC", 200);
        let out = tool
            .call(json!({"pattern": "FOO", "include": "*.c"}))
            .await
            .unwrap();
        assert!(out.contains("hit.c"), "should match .c: {out}");
        assert!(!out.contains("skip.txt"), "should skip .txt: {out}");
    }

    #[tokio::test]
    async fn max_results_bound_enforced() {
        let root = TempDir::new().unwrap();
        // 50 lines that all match the pattern in a single file; cap
        // at 5 → result is bounded.
        let body = (0..50)
            .map(|i| format!("MATCH line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(&root.path().join("many.txt"), &body);
        let tool = make(&root, "AgentD", 5);
        let out = tool.call(json!({"pattern": "MATCH"})).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let lines = v["match_lines"].as_u64().unwrap();
        assert!(lines <= 5, "should be ≤5: {out}");
    }

    #[tokio::test]
    async fn max_results_global_cap_across_files() {
        // Regression guard for the `-m{N}` removal: that flag was
        // per-file in recursive mode, so 10 files × 10 matches each
        // would have leaked 100 matches past a `max_results: 5` cap.
        // The Rust-side global truncation must catch it.
        let root = TempDir::new().unwrap();
        let body = (0..10)
            .map(|i| format!("MATCH line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for f in 0..10 {
            write(&root.path().join(format!("file{f}.txt")), &body);
        }
        let tool = make(&root, "AgentMulti", 5);
        let out = tool.call(json!({"pattern": "MATCH"})).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let lines = v["match_lines"].as_u64().unwrap();
        assert!(
            lines <= 5,
            "global cap should bound match_lines ≤5 across 10 files: {out}"
        );
        assert_eq!(
            v["truncated"].as_bool(),
            Some(true),
            "truncated flag must signal there's more behind the cap: {out}"
        );
    }

    #[tokio::test]
    async fn case_insensitive_flag() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("a.c"), "needle\nNEEDLE\nNeeDLe\n");
        let tool = make(&root, "AgentE", 200);
        let case_sens = tool.call(json!({"pattern": "NEEDLE"})).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&case_sens).unwrap();
        let n_sens = v["match_lines"].as_u64().unwrap();
        assert_eq!(n_sens, 1, "case-sensitive should match 1: {case_sens}");

        let case_insens = tool
            .call(json!({"pattern": "NEEDLE", "case_insensitive": true}))
            .await
            .unwrap();
        let v2: serde_json::Value = serde_json::from_str(&case_insens).unwrap();
        let n_insens = v2["match_lines"].as_u64().unwrap();
        assert_eq!(
            n_insens, 3,
            "case-insensitive should match 3: {case_insens}"
        );
    }

    #[tokio::test]
    async fn relative_path_resolves_under_root() {
        // Regression: previously `path: "sub/"` was passed through
        // std::fs::canonicalize as-is, which resolves relative to
        // the test process's CWD — usually the workspace root, far
        // outside the temp sandbox. The agent's intent is "search
        // <root>/sub/", not "search $CWD/sub/".
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        write(&root.path().join("sub").join("a.c"), "needle\n");
        write(&root.path().join("other.c"), "needle\n");
        let tool = make(&root, "AgentRel", 200);
        let scoped = tool
            .call(json!({"pattern": "needle", "path": "sub"}))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&scoped).unwrap();
        assert_eq!(
            v["match_lines"].as_u64().unwrap(),
            1,
            "relative path should scope under root, not CWD: {scoped}"
        );
    }

    #[tokio::test]
    async fn relative_path_outside_roots_rejected() {
        // A relative path that doesn't exist under any allowed root
        // must be rejected, not silently fall through to CWD.
        let root = TempDir::new().unwrap();
        write(&root.path().join("a.c"), "needle\n");
        let tool = make(&root, "AgentRel2", 200);
        let out = tool
            .call(json!({"pattern": "needle", "path": "no/such/dir"}))
            .await
            .unwrap();
        assert!(
            out.contains("not under any allowed root") || out.contains("not found"),
            "missing-under-root path should error: {out}"
        );
    }

    #[tokio::test]
    async fn no_match_returns_empty_no_error() {
        let root = TempDir::new().unwrap();
        write(&root.path().join("a.c"), "hello world\n");
        let tool = make(&root, "AgentF", 200);
        let out = tool.call(json!({"pattern": "MISSING"})).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["match_lines"].as_u64().unwrap(), 0);
        assert!(!out.contains("error"));
    }

    #[tokio::test]
    async fn grep_invalid_pattern_returns_structured_error() {
        // Invalid regex (unmatched bracket) — grep exits with code 2,
        // which we now surface as GREP_PROCESS_ERROR rather than
        // silently returning "no matches".
        let root = TempDir::new().unwrap();
        write(&root.path().join("a.c"), "anything\n");
        let tool = make(&root, "AgentG", 200);
        let out = tool.call(json!({"pattern": "[unclosed"})).await.unwrap();
        assert!(
            out.contains("GREP_PROCESS_ERROR"),
            "invalid regex should surface as error: {out}"
        );
    }

    #[tokio::test]
    async fn grep_handles_output_larger_than_max_bytes() {
        // 50K matching lines of "MATCH\n" (≈300 KB), max_bytes 1 KiB:
        // exercise the drain path. Without the fix, the child blocks
        // writing to a full pipe and child.wait() hangs.
        let root = TempDir::new().unwrap();
        let body = (0..50_000).map(|_| "MATCH").collect::<Vec<_>>().join("\n");
        write(&root.path().join("flood.txt"), &body);
        let tool = ScopedGrepTool::new(
            "AgentH".into(),
            &[root.path().display().to_string()],
            1024,
            100_000,
            10,
        )
        .expect("ctor");
        let out = tool.call(json!({"pattern": "MATCH"})).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["truncated"], true, "should mark truncated: {out}");
        assert!(
            v["matches"].as_str().unwrap().len() <= 1024,
            "should truncate at max_bytes"
        );
    }

    #[test]
    fn empty_roots_rejected() {
        let err = ScopedGrepTool::new("X".into(), &[], 1024, 200, 10).unwrap_err();
        assert!(err.contains("at least one"), "{err}");
    }

    #[tokio::test]
    async fn limit_zero_clamps_to_one_no_infinite_loop() {
        // Caller passing `limit: 0` must NOT produce a zero page that
        // would make `next_offset == offset`, which would loop the caller
        // forever. The implementation floors at 1 so the page advances.
        // CR finding on PR #411, cycle 2.
        let root = TempDir::new().unwrap();
        write(&root.path().join("a.c"), "MATCH\nMATCH\nMATCH\n");
        let tool = make(&root, "AgentZeroLimit", 200);
        let out = tool
            .call(json!({"pattern": "MATCH", "limit": 0}))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let match_lines = v["match_lines"].as_u64().unwrap();
        assert_eq!(match_lines, 1, "limit:0 must clamp to 1, got {match_lines}");
        let next_offset = v["next_offset"].as_u64();
        assert!(
            next_offset.is_some() && next_offset != Some(0),
            "next_offset must advance past 0 to avoid caller loop: {out}"
        );
    }

    #[test]
    fn missing_root_rejected() {
        let err = ScopedGrepTool::new("X".into(), &["/nope/does/not/exist".into()], 1024, 200, 10)
            .unwrap_err();
        assert!(err.contains("canonicalize"), "{err}");
    }
}
