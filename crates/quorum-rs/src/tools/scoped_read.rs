//! Sandboxed `read_file` tool. Confines reads to a configured set
//! of root directories; any path that resolves (after canonicalising
//! symlinks) outside every configured root is rejected with
//! `READ_FILE_OUT_OF_SANDBOX`.

use crate::tools::Tool;
use async_openai::types::{ChatCompletionTool, ChatCompletionToolType, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tracing::info;

/// Default per-call read cap. Sized so a single read fits comfortably
/// inside a typical agent's tool-output budget (≈16k tokens at 4 chars
/// per token). Oversized files return the first `max_bytes` followed by
/// a `[truncated…]` marker so the agent can still ground on a prefix
/// rather than failing the call outright.
pub const DEFAULT_READ_FILE_MAX_BYTES: u64 = 65_536;

#[derive(Clone, Debug)]
pub struct ScopedReadFileTool {
    agent_name: String,
    canonical_roots: Vec<PathBuf>,
    max_bytes: u64,
}

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
}

impl ScopedReadFileTool {
    /// Build the tool from raw root paths, canonicalising them up
    /// front so per-call validation is just a prefix check. Roots
    /// that fail to canonicalise (missing path, EACCES, …) are
    /// dropped with a warn.
    pub fn new(agent_name: impl Into<String>, roots: &[PathBuf]) -> Self {
        let agent_name = agent_name.into();
        let canonical_roots = roots
            .iter()
            .filter_map(|r| match r.canonicalize() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(
                        agent = %agent_name,
                        root = %r.display(),
                        error = %e,
                        "scoped read_file: dropping unresolvable root"
                    );
                    None
                }
            })
            .collect();
        Self {
            agent_name,
            canonical_roots,
            max_bytes: DEFAULT_READ_FILE_MAX_BYTES,
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Resolve the requested path against the canonical roots.
    /// Returns `(canonical, matched_root)` on success; an error string
    /// containing `READ_FILE_OUT_OF_SANDBOX` on rejection. The matched
    /// root lets callers log the path relative to it instead of the
    /// raw host path.
    fn resolve(&self, requested: &str) -> Result<(PathBuf, PathBuf), String> {
        if self.canonical_roots.is_empty() {
            return Err("READ_FILE_OUT_OF_SANDBOX: no roots configured for this agent".to_string());
        }

        let req_path = Path::new(requested);
        let candidate = if req_path.is_absolute() {
            req_path.to_path_buf()
        } else {
            // Try each root as a base; the first canonical-success
            // wins. Falling back across roots is intentional —
            // operators commonly mount multiple parallel trees
            // (corpus / linux / fixtures) and want a single relative
            // path to resolve against whichever owns it.
            let mut found: Option<PathBuf> = None;
            for root in &self.canonical_roots {
                let abs = root.join(req_path);
                if abs.exists() {
                    found = Some(abs);
                    break;
                }
            }
            found.ok_or_else(|| {
                format!(
                    "READ_FILE_OUT_OF_SANDBOX: relative path {requested:?} \
                     not found under any configured root"
                )
            })?
        };

        let canonical = candidate.canonicalize().map_err(|e| {
            format!("READ_FILE_OUT_OF_SANDBOX: failed to canonicalise {requested:?}: {e}")
        })?;

        let matched_root = self
            .canonical_roots
            .iter()
            .find(|root| canonical.starts_with(root))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "READ_FILE_OUT_OF_SANDBOX: {requested:?} resolves outside every configured root"
                )
            })?;
        Ok((canonical, matched_root))
    }
}

/// Path shown in audit logs. For successful resolutions we log the
/// canonical path relative to the matched root so host filesystem
/// layout doesn't leak when the agent submitted an absolute path.
fn audit_path<'a>(canonical: &'a Path, matched_root: &Path, fallback: &'a str) -> String {
    canonical
        .strip_prefix(matched_root)
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| fallback.to_string())
}

#[async_trait]
impl Tool for ScopedReadFileTool {
    fn name(&self) -> String {
        "read_file".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(
                    "Read a file from the agent's sandboxed roots. Pass a relative path \
                     (resolved against any configured root) or an absolute path within a \
                     root. Symlinks that resolve outside every root are rejected with \
                     READ_FILE_OUT_OF_SANDBOX. Files larger than the per-call cap \
                     (64 KiB by default) are truncated with a [truncated: …] marker \
                     so the agent still gets a usable prefix."
                        .to_string(),
                ),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path under a sandbox root, or an \
                                            absolute path within a root."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                })),
                strict: Some(true),
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let parsed: ReadFileArgs = serde_json::from_value(args)?;

        let (canonical, matched_root) = match self.resolve(&parsed.path) {
            Ok(pair) => pair,
            Err(e) => {
                info!(
                    agent = %self.agent_name,
                    tool = "read_file",
                    request = %parsed.path,
                    bytes = 0,
                    result = "denied",
                    "scoped read_file denied"
                );
                return Ok(json!({"error": e}).to_string());
            }
        };

        let log_path = audit_path(&canonical, &matched_root, &parsed.path);

        // Open ONCE and use the file handle for metadata + read so an
        // attacker can't swap the path between metadata() and open().
        // O_NOFOLLOW closes the residual race window where the final
        // component is replaced with a symlink between resolve() and
        // open(); the canonical target is by definition not a symlink,
        // so a successful resolution always opens with O_NOFOLLOW set.
        let mut open_opts = tokio::fs::OpenOptions::new();
        open_opts.read(true);
        #[cfg(unix)]
        open_opts.custom_flags(libc::O_NOFOLLOW);
        let file = open_opts.open(&canonical).await?;

        let metadata = file.metadata().await?;
        if !metadata.is_file() {
            info!(
                agent = %self.agent_name,
                tool = "read_file",
                path = %log_path,
                bytes = 0,
                result = "denied",
                "scoped read_file denied: not a regular file"
            );
            return Ok(json!({
                "error": "READ_FILE_OUT_OF_SANDBOX: target is not a regular file"
            })
            .to_string());
        }

        let total_len = metadata.len();
        let truncated = total_len > self.max_bytes;
        let read_limit = std::cmp::min(total_len, self.max_bytes);

        // Saturate at usize::MAX in u64 space before the cast so a
        // pathological max_bytes (or 32-bit usize) doesn't trigger a
        // multi-gigabyte pre-allocation here.
        let cap = read_limit.min(usize::MAX as u64) as usize;
        let mut buf = Vec::with_capacity(cap);
        file.take(read_limit).read_to_end(&mut buf).await?;
        let read_len = buf.len();
        let mut text = String::from_utf8_lossy(&buf).into_owned();
        if truncated {
            text.push_str(&format!(
                "\n\n[truncated: {total_len} bytes total, returned first {read_len}]"
            ));
        }

        info!(
            agent = %self.agent_name,
            tool = "read_file",
            path = %log_path,
            bytes = text.len(),
            truncated = truncated,
            result = "ok",
            "scoped read_file ok"
        );
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmp dir")
    }

    #[test]
    fn schema_advertises_read_file_with_path_argument() {
        let root = make_root();
        let tool = ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]);
        assert_eq!(tool.name(), "read_file");
        let schema = tool.schema();
        assert_eq!(schema.function.name, "read_file");
        let params = schema.function.parameters.expect("parameters present");
        let required = params["required"].as_array().expect("required is array");
        assert!(
            required.iter().any(|v| v == "path"),
            "schema must require `path`, got {required:?}"
        );
        let path_prop = &params["properties"]["path"];
        assert_eq!(path_prop["type"], "string", "path must be a string");
        assert!(
            path_prop["description"]
                .as_str()
                .is_some_and(|d| !d.is_empty()),
            "path property must carry a description for the LLM"
        );
        assert_eq!(
            params["additionalProperties"], false,
            "additionalProperties=false keeps the LLM from inventing extra args"
        );
    }

    #[tokio::test]
    async fn returns_content_for_path_under_root() {
        let root = make_root();
        std::fs::write(root.path().join("a.txt"), "hello").unwrap();
        let tool = ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]);
        let out = tool.call(json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn rejects_relative_path_escaping_root() {
        let root = make_root();
        let outside = root.path().parent().unwrap().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let tool = ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]);
        let out = tool.call(json!({"path": "../outside.txt"})).await.unwrap();
        assert!(out.contains("READ_FILE_OUT_OF_SANDBOX"), "got {out:?}");
    }

    #[tokio::test]
    async fn rejects_absolute_path_outside_root() {
        let root = make_root();
        let other = make_root();
        std::fs::write(other.path().join("evil.txt"), "secret").unwrap();
        let tool = ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]);
        let abs = other.path().join("evil.txt").display().to_string();
        let out = tool.call(json!({"path": abs})).await.unwrap();
        assert!(out.contains("READ_FILE_OUT_OF_SANDBOX"), "got {out:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_pointing_outside_root() {
        let root = make_root();
        let other = make_root();
        std::fs::write(other.path().join("evil.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(other.path().join("evil.txt"), root.path().join("link.txt"))
            .unwrap();
        let tool = ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]);
        let out = tool.call(json!({"path": "link.txt"})).await.unwrap();
        assert!(out.contains("READ_FILE_OUT_OF_SANDBOX"), "got {out:?}");
    }

    #[tokio::test]
    async fn truncates_oversize_file_with_marker() {
        let root = make_root();
        std::fs::write(root.path().join("big.txt"), vec![b'X'; 4096]).unwrap();
        let tool =
            ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]).with_max_bytes(1024);
        let out = tool.call(json!({"path": "big.txt"})).await.unwrap();
        assert!(out.contains("[truncated:"), "got {out:?}");
        assert!(out.contains("4096 bytes total"));
        assert!(out.contains("returned first 1024"));
        let xs = out.matches('X').count();
        assert_eq!(xs, 1024, "should return exactly max_bytes payload");
    }

    #[test]
    fn audit_path_strips_matched_root() {
        let root = make_root();
        let canonical = root.path().join("subdir/file.txt");
        let shown = audit_path(&canonical, root.path(), "ignored-fallback");
        assert_eq!(shown, "subdir/file.txt");
    }

    #[test]
    fn audit_path_falls_back_when_unrelated() {
        let r1 = make_root();
        let r2 = make_root();
        let outside = r2.path().join("x.txt");
        let shown = audit_path(&outside, r1.path(), "FALLBACK");
        assert_eq!(shown, "FALLBACK");
    }

    #[tokio::test]
    async fn returns_full_file_when_under_cap() {
        let root = make_root();
        std::fs::write(root.path().join("ok.txt"), "small").unwrap();
        let tool =
            ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]).with_max_bytes(1024);
        let out = tool.call(json!({"path": "ok.txt"})).await.unwrap();
        assert_eq!(out, "small");
        assert!(!out.contains("[truncated"));
    }

    #[tokio::test]
    async fn empty_roots_rejects_every_call() {
        let tool = ScopedReadFileTool::new("agent", &[]);
        let out = tool.call(json!({"path": "/etc/passwd"})).await.unwrap();
        assert!(out.contains("READ_FILE_OUT_OF_SANDBOX"));
    }

    #[tokio::test]
    async fn multi_root_resolves_against_first_match() {
        let r1 = make_root();
        let r2 = make_root();
        std::fs::write(r2.path().join("only-here.txt"), "from r2").unwrap();
        let tool =
            ScopedReadFileTool::new("agent", &[r1.path().to_path_buf(), r2.path().to_path_buf()]);
        let out = tool.call(json!({"path": "only-here.txt"})).await.unwrap();
        assert_eq!(out, "from r2");
    }

    #[tokio::test]
    async fn rejects_directory_target() {
        let root = make_root();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        let tool = ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]);
        let out = tool.call(json!({"path": "sub"})).await.unwrap();
        assert!(out.contains("not a regular file"), "got {out:?}");
    }

    #[tokio::test]
    async fn missing_required_path_field_errors() {
        let root = make_root();
        let tool = ScopedReadFileTool::new("agent", &[root.path().to_path_buf()]);
        let result = tool.call(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn drops_unresolvable_root_silently() {
        let real = make_root();
        std::fs::write(real.path().join("ok.txt"), "ok").unwrap();
        let tool = ScopedReadFileTool::new(
            "agent",
            &[
                PathBuf::from("/this/path/does/not/exist"),
                real.path().to_path_buf(),
            ],
        );
        let out = tool.call(json!({"path": "ok.txt"})).await.unwrap();
        assert_eq!(out, "ok");
    }

    /// Local copy of nsed-agent's `agents::nsed_agent::is_openai_family_provider`
    /// since that fn is `pub(crate)` in the BSL crate. Kept identical.
    fn is_openai_family_provider(config: &crate::agents::AgentConfig) -> bool {
        config.exec.is_none() && config.mcp.is_none() && config.claude.is_none()
    }

    #[test]
    fn openai_family_predicate_accepts_no_provider_section() {
        let cfg = crate::agents::AgentConfig::default();
        assert!(is_openai_family_provider(&cfg));
    }

    #[test]
    fn openai_family_predicate_rejects_claude_provider() {
        use crate::agents::config::ClaudeProviderConfig;
        let cfg = crate::agents::AgentConfig {
            claude: Some(ClaudeProviderConfig::default()),
            ..Default::default()
        };
        assert!(!is_openai_family_provider(&cfg));
    }
}
