//! Binary middleware — spawn an external process, pipe JSON in, get verdict out.
//!
//! Any language, any runtime — just implement the stdin/stdout protocol:
//!
//! | Direction | Format | Description |
//! |-----------|--------|-------------|
//! | stdin | JSON [`MiddlewareContext`] | Content, action, agent_id, job_id, round, stage, metadata, hook_state |
//! | stdout | JSON [`MiddlewareVerdict`] | Full verdict: `{ verdict, content?, reason?, category?, hook_state? }` |
//! | stdout | empty | Pass (no opinion) |
//! | stdout | non-JSON | **Block** (fail-closed — malformed output is treated as error) |
//! | exit 0 | — | Parse stdout as verdict; empty = pass; invalid JSON = block |
//! | exit non-zero | — | Try stdout JSON first; fall back to stderr as reason; then generic block |
//! | timeout | — | Block with "Middleware timed out", process killed via SIGKILL |
//!
//! The runtime accepts the full [`MiddlewareVerdict`] payload from stdout, including
//! optional `hook_state` (propagated to downstream middleware via [`MiddlewareContext`]).

use crate::middleware::{AgentMiddleware, MiddlewareContext, MiddlewareStage, MiddlewareVerdict};
use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Middleware that spawns an external binary for each invocation.
#[derive(Debug, Clone)]
pub struct BinaryMiddleware {
    /// Human-readable name (for logging).
    pub display_name: String,
    /// Path to the executable.
    pub path: PathBuf,
    /// Extra arguments passed to the binary.
    pub args: Vec<String>,
    /// Maximum time the binary may run before being killed.
    pub timeout: Duration,
    /// Which stages this middleware runs at.
    pub active_stages: Vec<MiddlewareStage>,
}

#[async_trait]
impl AgentMiddleware for BinaryMiddleware {
    async fn execute(&self, ctx: &MiddlewareContext) -> MiddlewareVerdict {
        let input = match serde_json::to_vec(ctx) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    middleware = self.display_name.as_str(),
                    error = %e,
                    "Failed to serialize middleware context"
                );
                return MiddlewareVerdict::block(
                    "binary_middleware",
                    format!("Serialization error: {e}"),
                );
            }
        };

        let mut child = match Command::new(&self.path)
            .args(&self.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    middleware = self.display_name.as_str(),
                    path = ?self.path,
                    error = %e,
                    "Failed to spawn binary middleware"
                );
                return MiddlewareVerdict::block(
                    "binary_middleware",
                    format!("Failed to spawn: {e}"),
                );
            }
        };

        // Max output size (stdout + stderr) to prevent unbounded buffering.
        const MAX_OUTPUT: usize = 1024 * 1024; // 1 MB

        // Take ownership of child I/O handles for concurrent processing.
        let mut stdin_handle = child.stdin.take();
        let mut stdout_handle = child.stdout.take();
        let mut stderr_handle = child.stderr.take();

        // Run all I/O (stdin write, stdout/stderr read, process wait) under
        // a single timeout so a slow stdin write also counts against the budget.
        let result = tokio::time::timeout(self.timeout, async {
            // Write context to stdin (concurrent with reads below via join)
            let write_fut = async {
                if let Some(ref mut stdin) = stdin_handle {
                    let r = stdin.write_all(&input).await;
                    drop(stdin_handle.take()); // Close stdin to signal EOF
                    r
                } else {
                    Ok(())
                }
            };

            // Read stdout with size cap
            let stdout_fut = async {
                let mut buf = Vec::with_capacity(4096);
                if let Some(ref mut out) = stdout_handle {
                    let mut limited = out.take(MAX_OUTPUT as u64);
                    tokio::io::AsyncReadExt::read_to_end(&mut limited, &mut buf).await?;
                }
                Ok::<Vec<u8>, std::io::Error>(buf)
            };

            // Read stderr with size cap
            let stderr_fut = async {
                let mut buf = Vec::with_capacity(1024);
                if let Some(ref mut err) = stderr_handle {
                    let mut limited = err.take(MAX_OUTPUT as u64);
                    tokio::io::AsyncReadExt::read_to_end(&mut limited, &mut buf).await?;
                }
                Ok::<Vec<u8>, std::io::Error>(buf)
            };

            // Run stdin write + stdout/stderr reads concurrently, then wait for exit
            let (write_res, stdout_res, stderr_res) =
                tokio::join!(write_fut, stdout_fut, stderr_fut);

            if let Err(e) = write_res {
                tracing::error!(middleware = self.display_name.as_str(), error = %e, "Failed to write stdin");
                // stdin failure is non-fatal — process may still produce output
            }

            let stdout = match stdout_res {
                Ok(buf) => buf,
                Err(e) => {
                    tracing::error!(middleware = self.display_name.as_str(), error = %e, "Failed to read stdout");
                    let _ = child.kill().await;
                    return (Vec::new(), Vec::new(), Err(e));
                }
            };
            let stderr = match stderr_res {
                Ok(buf) => buf,
                Err(e) => {
                    tracing::error!(middleware = self.display_name.as_str(), error = %e, "Failed to read stderr");
                    // stderr failure is non-fatal — we have stdout
                    Vec::new()
                }
            };
            let status = child.wait().await;

            (stdout, stderr, status)
        })
        .await;

        match result {
            Ok((stdout, _stderr, Ok(status))) if status.success() => {
                if stdout.is_empty() {
                    return MiddlewareVerdict::pass();
                }
                serde_json::from_slice(&stdout).unwrap_or_else(|e| {
                    tracing::warn!(
                        middleware = self.display_name.as_str(),
                        error = %e,
                        "Binary stdout was not valid JSON verdict — fail closed (block)"
                    );
                    MiddlewareVerdict::block(
                        "middleware_error",
                        format!("Middleware '{}' returned invalid JSON", self.display_name),
                    )
                })
            }
            Ok((stdout, stderr, Ok(status))) => {
                // Non-zero exit: try parsing stdout as JSON verdict first
                // (middleware may return a structured verdict even on failure).
                if !stdout.is_empty() {
                    if let Ok(verdict) = serde_json::from_slice::<MiddlewareVerdict>(&stdout) {
                        return verdict;
                    }
                }
                let stderr_str = String::from_utf8_lossy(&stderr);
                let reason = if stderr_str.trim().is_empty() {
                    format!("Binary exited with code {}", status.code().unwrap_or(-1))
                } else {
                    stderr_str.trim().to_string()
                };
                MiddlewareVerdict::block("binary_middleware", reason)
            }
            Ok((_, _, Err(e))) => {
                tracing::error!(middleware = self.display_name.as_str(), error = %e, "Binary process error");
                MiddlewareVerdict::block("binary_middleware", format!("Process error: {e}"))
            }
            Err(_) => {
                tracing::warn!(
                    middleware = self.display_name.as_str(),
                    timeout_secs = self.timeout.as_secs(),
                    "Binary middleware timed out, killing process"
                );
                // Timeout: child handle dropped here.
                // kill_on_drop(true) ensures the OS process receives SIGKILL.
                MiddlewareVerdict::block("binary_middleware", "Middleware timed out")
            }
        }
    }

    fn stages(&self) -> Vec<MiddlewareStage> {
        self.active_stages.clone()
    }

    fn name(&self) -> &str {
        &self.display_name
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{MiddlewareStage, Verdict};
    use std::collections::HashMap;

    fn make_ctx() -> MiddlewareContext {
        MiddlewareContext {
            content: serde_json::json!({"text": "test"}),
            action: "propose".to_string(),
            agent_id: "test".to_string(),
            job_id: "job-1".to_string(),
            round: 1,
            stage: MiddlewareStage::Release,
            metadata: serde_json::json!({}),
            hook_state: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn binary_pass_on_exit_zero_no_output() {
        // `true` command exits 0 with no output
        let mw = BinaryMiddleware {
            display_name: "true-cmd".to_string(),
            path: PathBuf::from("true"),
            args: vec![],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Pass);
    }

    #[tokio::test]
    async fn binary_block_on_exit_nonzero() {
        // `false` command exits 1
        let mw = BinaryMiddleware {
            display_name: "false-cmd".to_string(),
            path: PathBuf::from("false"),
            args: vec![],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Block);
    }

    #[tokio::test]
    async fn binary_timeout_blocks() {
        // `sleep 60` will be killed after 100ms timeout
        let mw = BinaryMiddleware {
            display_name: "sleeper".to_string(),
            path: PathBuf::from("sleep"),
            args: vec!["60".to_string()],
            timeout: Duration::from_millis(100),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Block);
        assert!(verdict.reason.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn binary_json_verdict_parsed() {
        // Use bash -c to read stdin (discarded) and output a JSON verdict
        let mw = BinaryMiddleware {
            display_name: "echo-json".to_string(),
            path: PathBuf::from("bash"),
            args: vec![
                "-c".to_string(),
                r#"cat > /dev/null; echo '{"verdict":"warn","category":"test","reason":"echo warning"}'"#.to_string(),
            ],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Warn);
        assert_eq!(verdict.category.as_deref(), Some("test"));
        assert_eq!(verdict.reason.as_deref(), Some("echo warning"));
    }

    #[tokio::test]
    async fn binary_nonexistent_path_blocks() {
        let mw = BinaryMiddleware {
            display_name: "missing".to_string(),
            path: PathBuf::from("/nonexistent/binary/path"),
            args: vec![],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Block);
        assert!(verdict.reason.unwrap().contains("spawn"));
    }

    // --- Review-discovered edge cases ---

    #[tokio::test]
    async fn binary_invalid_json_stdout_blocks() {
        // Binary outputs non-JSON on success → fail closed (block)
        let mw = BinaryMiddleware {
            display_name: "bad-json".to_string(),
            path: PathBuf::from("bash"),
            args: vec![
                "-c".to_string(),
                "cat > /dev/null; echo 'not json'".to_string(),
            ],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Block);
        assert!(verdict.reason.unwrap().contains("invalid JSON"));
    }

    #[tokio::test]
    async fn binary_nonzero_exit_with_json_verdict_on_stdout() {
        // Non-zero exit but stdout has valid JSON verdict → use stdout verdict
        let mw = BinaryMiddleware {
            display_name: "fail-with-verdict".to_string(),
            path: PathBuf::from("bash"),
            args: vec![
                "-c".to_string(),
                r#"cat > /dev/null; echo '{"verdict":"warn","reason":"controlled failure"}'; exit 1"#.to_string(),
            ],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Warn);
        assert_eq!(verdict.reason.as_deref(), Some("controlled failure"));
    }

    #[tokio::test]
    async fn binary_nonzero_exit_stderr_used_as_reason() {
        // Non-zero exit, no JSON on stdout, stderr has message → use stderr
        let mw = BinaryMiddleware {
            display_name: "fail-stderr".to_string(),
            path: PathBuf::from("bash"),
            args: vec![
                "-c".to_string(),
                "cat > /dev/null; echo 'custom error' >&2; exit 1".to_string(),
            ],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Block);
        assert_eq!(verdict.reason.as_deref(), Some("custom error"));
    }

    #[tokio::test]
    async fn binary_large_stdout_truncated_blocks() {
        // Binary outputs > MAX_OUTPUT (1MB) → truncated → invalid JSON → fail-closed block
        let mw = BinaryMiddleware {
            display_name: "large-output".to_string(),
            path: PathBuf::from("bash"),
            args: vec![
                "-c".to_string(),
                // Output 2MB of padding then valid JSON — truncation makes it invalid
                "cat > /dev/null; dd if=/dev/zero bs=1048576 count=2 2>/dev/null | tr '\\0' 'x'; echo '{\"verdict\":\"pass\"}'".to_string(),
            ],
            timeout: Duration::from_secs(10),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx();
        let verdict = mw.execute(&ctx).await;
        // Truncated output is not valid JSON → fail closed
        assert_eq!(verdict.verdict, Verdict::Block);
    }

    #[tokio::test]
    async fn binary_hook_state_propagated() {
        // Script reads stdin, checks for expected hook_state key, returns pass only if found
        let mw = BinaryMiddleware {
            display_name: "hook-state".to_string(),
            path: PathBuf::from("bash"),
            args: vec![
                "-c".to_string(),
                r#"INPUT=$(cat); echo "$INPUT" | grep -q '"prev_step"' && echo "$INPUT" | grep -q '"value1"' && echo '{"verdict":"pass"}' || echo '{"verdict":"block","reason":"hook_state not found"}'"#.to_string(),
            ],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let mut ctx = make_ctx();
        ctx.hook_state
            .insert("prev_step".to_string(), serde_json::json!("value1"));
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Pass);
    }

    #[tokio::test]
    async fn binary_hook_state_missing_fails() {
        // Same script but without the expected hook_state → should block
        let mw = BinaryMiddleware {
            display_name: "hook-state-missing".to_string(),
            path: PathBuf::from("bash"),
            args: vec![
                "-c".to_string(),
                r#"INPUT=$(cat); echo "$INPUT" | grep -q '"prev_step"' && echo "$INPUT" | grep -q '"value1"' && echo '{"verdict":"pass"}' || echo '{"verdict":"block","reason":"hook_state not found"}'"#.to_string(),
            ],
            timeout: Duration::from_secs(5),
            active_stages: vec![MiddlewareStage::Release],
        };

        let ctx = make_ctx(); // no hook_state set
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Block);
    }
}
