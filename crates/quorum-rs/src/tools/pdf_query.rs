//! Scoped PageIndex semantic lookup — wraps `pdf_query.py` with a
//! basename-only sandbox on the tree argument plus the same timeout +
//! output-cap discipline `scoped_grep.rs` uses.
//!
//! The tool exists because non-claude agents (`provider_type: openai`)
//! have no Bash and cannot dispatch the `coverage_audit` /
//! `hardware_lookup` claude sub-agents that wrap pdf_query elsewhere
//! in the fleet. Without `pdf_query`, an aggregator like
//! GlmAggregatorBot can read a peer-cited HRM section verbatim via
//! `read_file` only if the peer's `node_id + pp.X-Y` citation is
//! correct; with this tool the aggregator can verify the citation by
//! re-running the same semantic lookup the peer did.
//!
//! Threat model:
//! - The `tree` argument MUST be a bare filename. Slashes, `..`, NUL,
//!   and absolute paths are rejected before any FS call. The resolved
//!   path is then canonicalized and required to remain under
//!   `trees_root` (defends against symlink-out within the trees
//!   directory).
//! - `query` is passed as a single argv element to the script — never
//!   interpolated into a shell.
//! - `top_k` is clamped to `[1, max_results]` so a malicious or
//!   confused agent can't request a billion-row response.
//! - `script_path`, `trees_root`, and `python_bin` are all validated
//!   at construction; bad config fails the agent-startup loop in
//!   `serve.rs` so the fleet never accepts traffic with a half-broken
//!   tool.
//! - Subprocess wall-clock cap and output byte cap mirror
//!   `ScopedGrepTool`. One audit log line per call.

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

/// Default page size in PDF result entries when the model omits
/// `limit`. Sized for openai-compat agents on tight context
/// windows: 5 hits per call ≈ 1-3 KB of JSONL depending on the
/// per-hit excerpt length.
pub const DEFAULT_PDFQUERY_PAGE_HITS: usize = 5;

#[derive(Deserialize, schemars::JsonSchema)]
struct PdfQueryArgs {
    /// Tree filename, e.g. `"adsp-sc595-sc596-sc598-hrm.json"`. Must
    /// be a bare basename — slashes, `..`, NUL and absolute paths are
    /// rejected. The tool resolves it to `<trees_root>/<tree>`.
    tree: String,
    /// Free-text semantic query, e.g. `"PLL lock timing"`. Passed as a
    /// single argv element; not shell-interpolated.
    query: String,
    /// Number of top-scoring nodes the underlying script should
    /// fetch. Defaults to the tool's configured `max_results`;
    /// saturates to that cap if higher. The pagination layer (see
    /// `offset` / `limit`) carves a page out of these results — so
    /// `top_k` is the upper bound on what's reachable across all
    /// pages of a single call sequence.
    #[serde(default)]
    top_k: Option<usize>,
    /// Hit-rank to start the response page from (0-based). Defaults
    /// to 0. Use `next_offset` from a prior response to walk the
    /// result list without re-querying the tree.
    #[serde(default)]
    offset: Option<usize>,
    /// Maximum hits to return THIS call. Defaults to 5. Hard-capped
    /// at `max_results` (the per-tool sandbox ceiling). Combine
    /// with `offset` to iterate.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct ScopedPdfQueryTool {
    trees_root: PathBuf,
    script_path: PathBuf,
    python_bin: String,
    max_bytes: usize,
    max_results: usize,
    timeout: Duration,
    agent_name: String,
}

impl ScopedPdfQueryTool {
    pub fn new(
        agent_name: String,
        trees_root: &str,
        script_path: &str,
        python_bin: String,
        max_bytes: usize,
        max_results: usize,
        timeout_secs: u64,
    ) -> Result<Self, String> {
        if max_results == 0 {
            return Err(
                "pdf_query tool requires `max_results >= 1` — refusing to instantiate \
                 with a zero result-count cap"
                    .to_string(),
            );
        }
        let trees_root = std::fs::canonicalize(trees_root).map_err(|e| {
            format!("pdf_query: failed to canonicalize trees_root {trees_root:?}: {e}")
        })?;
        if !trees_root.is_dir() {
            return Err(format!(
                "pdf_query: trees_root {} is not a directory",
                trees_root.display()
            ));
        }
        let script_path = std::fs::canonicalize(script_path).map_err(|e| {
            format!("pdf_query: failed to canonicalize script_path {script_path:?}: {e}")
        })?;
        if !script_path.is_file() {
            return Err(format!(
                "pdf_query: script_path {} is not a file",
                script_path.display()
            ));
        }
        let python_bin = python_bin.trim().to_string();
        if python_bin.is_empty() {
            return Err("pdf_query: python_bin must be non-empty".to_string());
        }
        Ok(Self {
            trees_root,
            script_path,
            python_bin,
            max_bytes,
            max_results,
            timeout: Duration::from_secs(timeout_secs.max(1)),
            agent_name,
        })
    }

    /// Resolve the agent-supplied `tree` argument to an absolute
    /// path under `trees_root`, rejecting traversal/absolute/NUL up
    /// front and confirming the canonical result still lies under
    /// the configured root (catches symlink-out within trees_root).
    fn resolve_tree(&self, tree: &str) -> Result<PathBuf, String> {
        if tree.is_empty() {
            return Err("`tree` must not be empty".to_string());
        }
        if tree.contains('\0') {
            return Err("`tree` contains NUL byte".to_string());
        }
        if tree.contains('/') || tree.contains('\\') {
            return Err(format!(
                "`tree` must be a bare filename, got {tree:?} (slashes are rejected)"
            ));
        }
        if tree == ".." || tree == "." {
            return Err(format!("`tree` must be a bare filename, got {tree:?}"));
        }
        if Path::new(tree).is_absolute() {
            return Err(format!(
                "`tree` must be a bare filename, got absolute path {tree:?}"
            ));
        }
        let candidate = self.trees_root.join(tree);
        let canonical = std::fs::canonicalize(&candidate)
            .map_err(|e| format!("tree {tree:?} not found under trees_root: {e}"))?;
        if !canonical.starts_with(&self.trees_root) {
            return Err(format!(
                "tree {tree:?} resolves outside trees_root (got {})",
                canonical.display()
            ));
        }
        Ok(canonical)
    }

    fn clamp_top_k(&self, requested: Option<usize>) -> usize {
        let k = requested.unwrap_or(self.max_results);
        k.clamp(1, self.max_results)
    }
}

#[async_trait]
impl Tool for ScopedPdfQueryTool {
    fn name(&self) -> String {
        "pdf_query".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        // List the actual tree filenames so the agent can pick one
        // without guessing — non-claude agents don't get Glob.
        let trees = match std::fs::read_dir(&self.trees_root) {
            Ok(rd) => {
                let mut v = rd
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name.ends_with(".json").then_some(name)
                    })
                    .collect::<Vec<_>>();
                v.sort();
                v
            }
            Err(_) => Vec::new(),
        };
        let trees_hint = if trees.is_empty() {
            String::new()
        } else {
            format!(" Available trees: [{}].", trees.join(", "))
        };
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(format!(
                    "Semantic lookup over PageIndex tree.json bundles via \
                     pdf_query.py. Pass a tree filename and a free-text query; \
                     returns JSON-Lines hits ({{node_id, title, pages, score, \
                     excerpt}}). Paginated: pass `offset` + `limit` to walk \
                     the result list. Default page is {} hits; hard cap is {} \
                     hits per call (also bounded by {} bytes / {}s). Response \
                     includes `total_hits`, `next_offset`, `has_more`.{}",
                    DEFAULT_PDFQUERY_PAGE_HITS,
                    self.max_results,
                    self.max_bytes,
                    self.timeout.as_secs(),
                    trees_hint
                )),
                parameters: Some(schema_for!(PdfQueryArgs).into()),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: PdfQueryArgs = serde_json::from_value(args)?;
        let tree_path = match self.resolve_tree(&args.tree) {
            Ok(p) => p,
            Err(detail) => {
                info!(
                    agent = %self.agent_name,
                    tool = "pdf_query",
                    tree = %args.tree,
                    result = "denied:out_of_sandbox",
                    "pdf_query: tree resolution failed"
                );
                return Ok(json!({
                    "error": "PDFQUERY_OUT_OF_SANDBOX",
                    "detail": detail,
                })
                .to_string());
            }
        };
        if args.query.trim().is_empty() {
            info!(
                agent = %self.agent_name,
                tool = "pdf_query",
                tree = %args.tree,
                result = "denied:empty_query",
                "pdf_query: rejected empty query"
            );
            return Ok(json!({
                "error": "PDFQUERY_EMPTY_QUERY",
                "detail": "`query` must not be empty",
            })
            .to_string());
        }
        let top_k = self.clamp_top_k(args.top_k);

        let mut cmd = Command::new(&self.python_bin);
        cmd.arg(&self.script_path)
            .arg("--tree")
            .arg(&tree_path)
            .arg("--query")
            .arg(&args.query)
            .arg("--top")
            .arg(top_k.to_string());
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("pdf_query: spawn failed: {e}"))?;

        let mut stdout_handle = child.stdout.take().expect("stdio piped at spawn");
        let mut stderr_handle = child.stderr.take().expect("stdio piped at spawn");

        let read_fut = async {
            // See scoped_grep.rs for the rationale: continue draining
            // stdout past `cap` (discarding overflow) and read stderr
            // concurrently so a child that emits more than max_bytes
            // can't block on a full pipe and stall `child.wait()`.
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
                    // Past cap: keep draining into the void.
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
                    tool = "pdf_query",
                    tree = %args.tree,
                    result = "error:io",
                    error = %e,
                    "pdf_query: stdout read failed"
                );
                return Ok(json!({
                    "error": "PDFQUERY_IO_ERROR",
                    "detail": e.to_string(),
                })
                .to_string());
            }
            Err(_) => {
                let _ = child.kill().await;
                info!(
                    agent = %self.agent_name,
                    tool = "pdf_query",
                    tree = %args.tree,
                    timeout_secs = self.timeout.as_secs(),
                    result = "denied:timeout",
                    "pdf_query: subprocess exceeded wall-clock cap"
                );
                return Ok(json!({
                    "error": "PDFQUERY_TIMEOUT",
                    "timeout_secs": self.timeout.as_secs(),
                    "detail": "subprocess wall-clock cap exceeded — tree may be \
                              very large or the script may have hung; consider a \
                              tighter query",
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

        let stdout_str = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();

        info!(
            agent = %self.agent_name,
            tool = "pdf_query",
            tree = %args.tree,
            top_k = top_k,
            output_bytes = stdout_bytes.len(),
            truncated = truncated,
            exit_code = ?exit_code,
            result = "ok",
            "pdf_query"
        );

        // pdf_query.py exits 1 on any error (missing tree, empty
        // query, LLM failure). Surface the script's stderr verbatim
        // so the agent can recover instead of seeing a silent empty.
        if matches!(exit_code, Some(c) if c != 0) {
            return Ok(json!({
                "error": "PDFQUERY_SCRIPT_ERROR",
                "exit_code": exit_code,
                "stderr": stderr_str,
                "stdout": stdout_str,
            })
            .to_string());
        }

        // Paginate the JSONL output by line (= hit). Each line is
        // one hit record. Slice by rank index so the model can
        // iterate via `next_offset` without re-running the script.
        //
        // Normalize stdout before counting: trim, drop empty lines,
        // drop the literal "[]" sentinel some pdf_query.py builds
        // emit when there are zero hits, and pop the trailing record
        // when truncation cut it mid-JSON (it would otherwise inflate
        // `total_hits` with an unparseable hit).
        let normalized = stdout_str.trim();
        let mut all_hits: Vec<&str> = normalized
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "[]")
            .collect();
        if truncated && !all_hits.is_empty() {
            let last = all_hits[all_hits.len() - 1];
            if serde_json::from_str::<serde_json::Value>(last).is_err() {
                all_hits.pop();
            }
        }
        let total_hits = all_hits.len();
        let req_offset = args.offset.unwrap_or(0);
        // Floor at 1 so a caller that passes `limit: 0` (or `limit` greater
        // than max_results == 0 — impossible per ctor but defensive) cannot
        // produce a zero page that would make `next_offset == offset` and
        // loop the caller forever.
        let req_limit = args
            .limit
            .unwrap_or(DEFAULT_PDFQUERY_PAGE_HITS)
            .min(self.max_results)
            .max(1);
        let start = req_offset.min(total_hits);
        let end = (start + req_limit).min(total_hits);
        let page_hits = &all_hits[start..end];
        let page_jsonl = page_hits.join("\n");
        let next_offset = if end < total_hits { Some(end) } else { None };
        let has_more = next_offset.is_some();

        Ok(json!({
            "tree": args.tree,
            "query": args.query,
            "top_k": top_k,
            "hits": page_hits.len(),
            "total_hits": total_hits,
            "offset": start,
            "has_more": has_more,
            "next_offset": next_offset,
            "results": page_jsonl,
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
    use std::io::Write;
    use tempfile::TempDir;

    /// Write a minimal mock pdf_query.py-compatible script that:
    /// - parses --tree / --query / --top,
    /// - emits N synthetic JSON-Lines hits whose `excerpt` echoes the
    ///   resolved tree path + query, so tests can assert what we
    ///   passed downstream.
    fn write_mock_script(dir: &Path) -> PathBuf {
        let p = dir.join("mock_pdf_query.py");
        let body = r#"#!/usr/bin/env python3
import argparse, json, sys
ap = argparse.ArgumentParser()
ap.add_argument("--tree", required=True)
ap.add_argument("--query", required=True)
ap.add_argument("--top", type=int, default=5)
args = ap.parse_args()
for i in range(args.top):
    print(json.dumps({
        "node_id": f"00{i}",
        "title": f"hit {i}",
        "pages": [i, i+1],
        "score": 100 - i,
        "excerpt": f"tree={args.tree} query={args.query}",
    }))
"#;
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        // Not strictly needed (we invoke `python3 <path>`) but keeps
        // future maintainers from being surprised.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&p).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&p, perms).unwrap();
        }
        p
    }

    fn write_tree(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, r#"{"title":"root","nodes":[]}"#).unwrap();
        p
    }

    fn make_tool(
        agent: &str,
        trees_root: &Path,
        script: &Path,
        max_results: usize,
        timeout_secs: u64,
    ) -> ScopedPdfQueryTool {
        ScopedPdfQueryTool::new(
            agent.to_string(),
            trees_root.to_str().unwrap(),
            script.to_str().unwrap(),
            "python3".to_string(),
            64 * 1024,
            max_results,
            timeout_secs,
        )
        .expect("ctor should succeed in test setup")
    }

    #[tokio::test]
    async fn returns_hits_with_passed_args() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        write_tree(trees.path(), "hrm.json");
        let script = write_mock_script(scripts.path());
        let tool = make_tool("AgentA", trees.path(), &script, 5, 30);

        let out = tool
            .call(json!({"tree": "hrm.json", "query": "PLL lock timing", "top_k": 3}))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["tree"], "hrm.json");
        assert_eq!(v["query"], "PLL lock timing");
        assert_eq!(v["top_k"], 3);
        assert_eq!(v["hits"], 3);
        let results = v["results"].as_str().unwrap();
        assert!(
            results.contains("\"node_id\": \"000\""),
            "first hit missing: {results}"
        );
        assert!(
            results.contains("query=PLL lock timing"),
            "query did not propagate: {results}"
        );
    }

    #[tokio::test]
    async fn rejects_path_traversal_in_tree_arg() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        write_tree(trees.path(), "hrm.json");
        let script = write_mock_script(scripts.path());
        let tool = make_tool("AgentB", trees.path(), &script, 5, 30);

        for bad in ["../etc/passwd", "subdir/foo.json", "/etc/passwd", "..", "."] {
            let out = tool.call(json!({"tree": bad, "query": "x"})).await.unwrap();
            assert!(
                out.contains("PDFQUERY_OUT_OF_SANDBOX"),
                "should reject {bad:?}: {out}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_symlink_out_of_trees_root() {
        #[cfg(unix)]
        {
            let trees = TempDir::new().unwrap();
            let scripts = TempDir::new().unwrap();
            let outside = TempDir::new().unwrap();
            let secret = outside.path().join("secret.json");
            std::fs::write(&secret, r#"{"title":"secret"}"#).unwrap();
            // Symlink lives inside trees_root but points outside.
            let link = trees.path().join("escape.json");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            let script = write_mock_script(scripts.path());
            let tool = make_tool("AgentC", trees.path(), &script, 5, 30);

            let out = tool
                .call(json!({"tree": "escape.json", "query": "anything"}))
                .await
                .unwrap();
            assert!(
                out.contains("PDFQUERY_OUT_OF_SANDBOX"),
                "should reject symlink-out: {out}"
            );
        }
    }

    #[tokio::test]
    async fn missing_tree_returns_structured_error() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        let script = write_mock_script(scripts.path());
        let tool = make_tool("AgentD", trees.path(), &script, 5, 30);

        let out = tool
            .call(json!({"tree": "nope.json", "query": "x"}))
            .await
            .unwrap();
        assert!(
            out.contains("PDFQUERY_OUT_OF_SANDBOX"),
            "missing tree should be rejected at sandbox layer: {out}"
        );
    }

    #[tokio::test]
    async fn empty_query_rejected() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        write_tree(trees.path(), "hrm.json");
        let script = write_mock_script(scripts.path());
        let tool = make_tool("AgentE", trees.path(), &script, 5, 30);

        let out = tool
            .call(json!({"tree": "hrm.json", "query": "   "}))
            .await
            .unwrap();
        assert!(
            out.contains("PDFQUERY_EMPTY_QUERY"),
            "blank query should be rejected: {out}"
        );
    }

    #[tokio::test]
    async fn top_k_clamped_to_max_results() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        write_tree(trees.path(), "hrm.json");
        let script = write_mock_script(scripts.path());
        // Cap at 4 — agent asks for 100, we should still see ≤4 hits.
        let tool = make_tool("AgentF", trees.path(), &script, 4, 30);

        let out = tool
            .call(json!({"tree": "hrm.json", "query": "x", "top_k": 100}))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["top_k"], 4, "agent-supplied top_k should clamp");
        assert!(v["hits"].as_u64().unwrap() <= 4);
    }

    #[tokio::test]
    async fn top_k_default_uses_max_results() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        write_tree(trees.path(), "hrm.json");
        let script = write_mock_script(scripts.path());
        let tool = make_tool("AgentG", trees.path(), &script, 6, 30);

        let out = tool
            .call(json!({"tree": "hrm.json", "query": "x"}))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["top_k"], 6,
            "absent top_k should default to configured max_results"
        );
    }

    #[tokio::test]
    async fn timeout_kills_runaway_script() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        write_tree(trees.path(), "hrm.json");
        // Script that ignores its args and sleeps forever.
        let script = scripts.path().join("hang.py");
        std::fs::write(&script, "import time, sys\nwhile True: time.sleep(1)\n").unwrap();
        let tool = make_tool("AgentH", trees.path(), &script, 5, 1);

        let out = tool
            .call(json!({"tree": "hrm.json", "query": "x"}))
            .await
            .unwrap();
        assert!(
            out.contains("PDFQUERY_TIMEOUT"),
            "long script should be killed by timeout: {out}"
        );
    }

    #[tokio::test]
    async fn script_nonzero_exit_relayed() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        write_tree(trees.path(), "hrm.json");
        let script = scripts.path().join("fail.py");
        std::fs::write(
            &script,
            "import sys\nprint('partial', flush=True)\n\
             sys.stderr.write('boom\\n')\nsys.exit(2)\n",
        )
        .unwrap();
        let tool = make_tool("AgentI", trees.path(), &script, 5, 10);

        let out = tool
            .call(json!({"tree": "hrm.json", "query": "x"}))
            .await
            .unwrap();
        assert!(
            out.contains("PDFQUERY_SCRIPT_ERROR"),
            "script error should be relayed: {out}"
        );
        assert!(out.contains("boom"), "stderr should be surfaced: {out}");
        assert!(
            out.contains("\"exit_code\":2"),
            "exit code should be reported: {out}"
        );
    }

    #[tokio::test]
    async fn output_truncated_at_max_bytes() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        write_tree(trees.path(), "hrm.json");
        let script = scripts.path().join("flood.py");
        // Spew well over the test cap (set to 1 KiB below).
        std::fs::write(
            &script,
            "import sys\nfor _ in range(20000):\n    sys.stdout.write('x')\n",
        )
        .unwrap();
        let tool = ScopedPdfQueryTool::new(
            "AgentJ".into(),
            trees.path().to_str().unwrap(),
            script.to_str().unwrap(),
            "python3".into(),
            1024,
            5,
            10,
        )
        .unwrap();

        let out = tool
            .call(json!({"tree": "hrm.json", "query": "x"}))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["truncated"], true, "should mark truncated: {out}");
        assert!(
            v["results"].as_str().unwrap().len() <= 1024,
            "should truncate at max_bytes"
        );
    }

    #[test]
    fn ctor_rejects_missing_trees_root() {
        let scripts = TempDir::new().unwrap();
        let script = write_mock_script(scripts.path());
        let err = ScopedPdfQueryTool::new(
            "X".into(),
            "/no/such/dir/xyz",
            script.to_str().unwrap(),
            "python3".into(),
            1024,
            5,
            10,
        )
        .unwrap_err();
        assert!(err.contains("trees_root"), "{err}");
    }

    #[test]
    fn ctor_rejects_missing_script() {
        let trees = TempDir::new().unwrap();
        let err = ScopedPdfQueryTool::new(
            "X".into(),
            trees.path().to_str().unwrap(),
            "/no/such/script.py",
            "python3".into(),
            1024,
            5,
            10,
        )
        .unwrap_err();
        assert!(err.contains("script_path"), "{err}");
    }

    #[test]
    fn ctor_rejects_empty_python_bin() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        let script = write_mock_script(scripts.path());
        let err = ScopedPdfQueryTool::new(
            "X".into(),
            trees.path().to_str().unwrap(),
            script.to_str().unwrap(),
            "   ".into(),
            1024,
            5,
            10,
        )
        .unwrap_err();
        assert!(err.contains("python_bin"), "{err}");
    }

    #[test]
    fn ctor_rejects_zero_max_results() {
        let trees = TempDir::new().unwrap();
        let scripts = TempDir::new().unwrap();
        let script = write_mock_script(scripts.path());
        let err = ScopedPdfQueryTool::new(
            "X".into(),
            trees.path().to_str().unwrap(),
            script.to_str().unwrap(),
            "python3".into(),
            1024,
            0,
            10,
        )
        .unwrap_err();
        assert!(err.contains("max_results"), "{err}");
    }
}
