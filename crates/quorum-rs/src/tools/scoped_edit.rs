//! Sandboxed `edit_file` / `write_file` tools — the write half of the file
//! interface a CLI-subprocess agent already has, offered unchanged to agents
//! that talk over the chat-completions wire.
//!
//! Those two kinds of agent were not equally equipped. A CLI agent reads a file,
//! gets its exact bytes, and edits them; a wire agent had `read_file` and
//! `grep` and no way to write at all, so anything that needed an edit had to
//! invent its own vocabulary on top. Two vocabularies for one job is a trap:
//! the same model then behaves differently depending on how it was launched,
//! and a caller editing their own repository meets whichever one their agent
//! happened to be wired with.
//!
//! So the parameters here are deliberately the ones the CLI tools use —
//! `file_path`, `old_string`, `new_string`, `replace_all`, `content` — rather
//! than a tidier local invention. A model that knows how to edit a file already
//! knows how to call these.
//!
//! Both tools confine themselves to the configured roots exactly as
//! [`super::scoped_read::ScopedReadFileTool`] does; a path resolving outside
//! every root is rejected rather than clamped.

use crate::tools::Tool;
use async_openai::types::{ChatCompletionTool, ChatCompletionToolType, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error;
use std::path::{Path, PathBuf};
use tracing::info;

/// Canonicalise `roots`, dropping any that cannot be resolved.
///
/// A dropped root is not fatal: with none left every call is denied, which is
/// the same fail-closed shape the read tool has.
fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|r| match r.canonicalize() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(root = %r.display(), error = %e, "dropping unresolvable sandbox root");
                None
            }
        })
        .collect()
}

/// Resolve `requested` inside `roots`.
///
/// `must_exist` distinguishes an edit (the file has to be there) from a write
/// (it may not be). For a new file the PARENT is canonicalised instead, so a
/// symlinked parent pointing out of the sandbox is still caught — canonicalising
/// the target itself would fail and tell us nothing.
fn resolve_in_roots(
    roots: &[PathBuf],
    requested: &str,
    must_exist: bool,
    code: &str,
) -> Result<PathBuf, String> {
    if roots.is_empty() {
        return Err(format!("{code}: no roots configured for this agent"));
    }
    let req = Path::new(requested);
    let candidate = if req.is_absolute() {
        req.to_path_buf()
    } else {
        roots
            .iter()
            .map(|r| r.join(req))
            .find(|p| !must_exist || p.exists())
            .ok_or_else(|| {
                format!("{code}: relative path {requested:?} not found under any configured root")
            })?
    };

    // Walk up to the deepest ancestor that exists and canonicalize THAT. A write
    // may name a file, or a whole directory chain, that is not there yet; only an
    // existing path can be resolved through symlinks, and resolving is what keeps
    // the sandbox honest.
    let mut probe = candidate.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !probe.exists() {
        let name = probe
            .file_name()
            .ok_or_else(|| format!("{code}: {requested:?} does not name a file"))?
            .to_owned();
        // `..` in the unresolved tail would climb back out after the check below.
        if name == ".." {
            return Err(format!(
                "{code}: {requested:?} traverses out of the sandbox"
            ));
        }
        tail.push(name);
        probe = probe
            .parent()
            .ok_or_else(|| format!("{code}: {requested:?} has no existing ancestor"))?
            .to_path_buf();
    }

    let canonical = probe
        .canonicalize()
        .map_err(|e| format!("{code}: cannot resolve {requested:?}: {e}"))?;
    if !roots.iter().any(|r| canonical.starts_with(r)) {
        return Err(format!("{code}: {requested:?} resolves outside every root"));
    }
    let mut out = canonical;
    for name in tail.into_iter().rev() {
        out.push(name);
    }
    Ok(out)
}

/// `edit_file` — replace an exact string, the CLI `Edit` contract.
#[derive(Clone, Debug)]
pub struct ScopedEditTool {
    agent_name: String,
    roots: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct EditArgs {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl ScopedEditTool {
    pub fn new(agent_name: impl Into<String>, roots: &[PathBuf]) -> Self {
        Self {
            agent_name: agent_name.into(),
            roots: canonical_roots(roots),
        }
    }
}

#[async_trait]
impl Tool for ScopedEditTool {
    fn name(&self) -> String {
        "edit_file".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(
                    "Replace an exact string in a file within the agent's sandboxed roots. \
                     Read the file first and copy `old_string` verbatim from what you read, \
                     including indentation — the match is byte-for-byte. `old_string` must \
                     appear exactly once unless `replace_all` is set, so include enough \
                     surrounding text to make it unique. Prefer this over rewriting a whole \
                     file: it leaves everything you did not name untouched."
                        .to_string(),
                ),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string", "description":
                            "Relative path under a sandbox root, or an absolute path within one."},
                        "old_string": {"type": "string", "description":
                            "The exact text to replace, copied verbatim from the file."},
                        "new_string": {"type": "string", "description":
                            "The replacement text. Empty deletes `old_string`."},
                        "replace_all": {"type": "boolean", "description":
                            "Replace every occurrence instead of requiring a unique match."}
                    },
                    "required": ["file_path", "old_string", "new_string", "replace_all"],
                    "additionalProperties": false
                })),
                strict: Some(true),
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let a: EditArgs = serde_json::from_value(args)?;
        if a.old_string == a.new_string {
            return Ok("EDIT_FILE_NO_CHANGE: old_string and new_string are identical".to_string());
        }
        let path =
            match resolve_in_roots(&self.roots, &a.file_path, true, "EDIT_FILE_OUT_OF_SANDBOX") {
                Ok(p) => p,
                Err(e) => return Ok(e),
            };
        let cur = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(format!("EDIT_FILE_UNREADABLE: {}: {e}", a.file_path)),
        };
        let hits = cur.matches(&a.old_string).count();
        if hits == 0 {
            return Ok(format!(
                "EDIT_FILE_NOT_FOUND: `old_string` does not appear in {}. Read the file and \
                 copy the text exactly as it is written there.",
                a.file_path
            ));
        }
        if hits > 1 && !a.replace_all {
            return Ok(format!(
                "EDIT_FILE_AMBIGUOUS: `old_string` appears {hits} times in {}. Include more \
                 surrounding text to make it unique, or set replace_all.",
                a.file_path
            ));
        }
        let updated = if a.replace_all {
            cur.replace(&a.old_string, &a.new_string)
        } else {
            cur.replacen(&a.old_string, &a.new_string, 1)
        };
        tokio::fs::write(&path, updated).await?;
        info!(agent = %self.agent_name, path = %a.file_path, replaced = hits, "edit_file");
        Ok(format!(
            "edited {} ({} replacement{})",
            a.file_path,
            if a.replace_all { hits } else { 1 },
            if a.replace_all && hits != 1 { "s" } else { "" }
        ))
    }
}

/// `write_file` — create or overwrite, the CLI `Write` contract.
#[derive(Clone, Debug)]
pub struct ScopedWriteTool {
    agent_name: String,
    roots: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct WriteArgs {
    file_path: String,
    content: String,
}

impl ScopedWriteTool {
    pub fn new(agent_name: impl Into<String>, roots: &[PathBuf]) -> Self {
        Self {
            agent_name: agent_name.into(),
            roots: canonical_roots(roots),
        }
    }
}

#[async_trait]
impl Tool for ScopedWriteTool {
    fn name(&self) -> String {
        "write_file".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(
                    "Create a file, or replace one entirely, within the agent's sandboxed \
                     roots. This discards whatever the file held — to change part of an \
                     existing file use `edit_file`, which leaves the rest alone."
                        .to_string(),
                ),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string", "description":
                            "Relative path under a sandbox root, or an absolute path within one."},
                        "content": {"type": "string", "description": "The file's full contents."}
                    },
                    "required": ["file_path", "content"],
                    "additionalProperties": false
                })),
                strict: Some(true),
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let a: WriteArgs = serde_json::from_value(args)?;
        let path = match resolve_in_roots(
            &self.roots,
            &a.file_path,
            false,
            "WRITE_FILE_OUT_OF_SANDBOX",
        ) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = a.content.len();
        tokio::fs::write(&path, a.content).await?;
        info!(agent = %self.agent_name, path = %a.file_path, bytes, "write_file");
        Ok(format!("wrote {} ({bytes} bytes)", a.file_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qr-edit-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    async fn call(t: &dyn Tool, v: Value) -> String {
        t.call(v).await.unwrap()
    }

    /// The contract a CLI agent already knows: exact match, unique unless told
    /// otherwise, and everything not named is left alone.
    #[tokio::test]
    async fn edit_replaces_exactly_what_was_named() {
        let d = sandbox("exact");
        std::fs::write(d.join("a.md"), "alpha\nbeta\ngamma\n").unwrap();
        let tool = ScopedEditTool::new("A", std::slice::from_ref(&d));

        let out = call(
            &tool,
            json!({"file_path": "a.md", "old_string": "beta",
                                     "new_string": "BETA", "replace_all": false}),
        )
        .await;
        assert!(out.starts_with("edited"), "{out}");
        assert_eq!(
            std::fs::read_to_string(d.join("a.md")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    /// An ambiguous match is refused rather than guessed at — the same reason a
    /// CLI edit asks for more context instead of picking the first hit.
    #[tokio::test]
    async fn edit_refuses_an_ambiguous_match_but_takes_replace_all() {
        let d = sandbox("ambig");
        std::fs::write(d.join("a.md"), "x\nx\n").unwrap();
        let tool = ScopedEditTool::new("A", std::slice::from_ref(&d));

        let out = call(
            &tool,
            json!({"file_path": "a.md", "old_string": "x",
                                     "new_string": "y", "replace_all": false}),
        )
        .await;
        assert!(out.contains("EDIT_FILE_AMBIGUOUS"), "{out}");
        assert_eq!(
            std::fs::read_to_string(d.join("a.md")).unwrap(),
            "x\nx\n",
            "untouched"
        );

        let out = call(
            &tool,
            json!({"file_path": "a.md", "old_string": "x",
                                     "new_string": "y", "replace_all": true}),
        )
        .await;
        assert!(out.starts_with("edited"), "{out}");
        assert_eq!(std::fs::read_to_string(d.join("a.md")).unwrap(), "y\ny\n");
    }

    /// A miss must say so plainly: this is the failure that, unexplained, makes
    /// an agent give up on editing and rewrite the file instead.
    #[tokio::test]
    async fn edit_reports_a_miss_instead_of_writing_something() {
        let d = sandbox("miss");
        std::fs::write(d.join("a.md"), "alpha\n").unwrap();
        let tool = ScopedEditTool::new("A", std::slice::from_ref(&d));

        let out = call(
            &tool,
            json!({"file_path": "a.md", "old_string": "nope",
                                     "new_string": "y", "replace_all": false}),
        )
        .await;
        assert!(out.contains("EDIT_FILE_NOT_FOUND"), "{out}");
        assert!(
            out.contains("Read the file"),
            "must say how to recover: {out}"
        );
        assert_eq!(std::fs::read_to_string(d.join("a.md")).unwrap(), "alpha\n");
    }

    #[tokio::test]
    async fn write_creates_and_replaces_within_the_sandbox() {
        let d = sandbox("write");
        let tool = ScopedWriteTool::new("A", std::slice::from_ref(&d));

        // A file that does not exist yet — the case an edit cannot serve.
        let out = call(
            &tool,
            json!({"file_path": "new/deep.md", "content": "hello\n"}),
        )
        .await;
        assert!(out.starts_with("wrote"), "{out}");
        assert_eq!(
            std::fs::read_to_string(d.join("new/deep.md")).unwrap(),
            "hello\n"
        );

        let out = call(
            &tool,
            json!({"file_path": "new/deep.md", "content": "again\n"}),
        )
        .await;
        assert!(out.starts_with("wrote"), "{out}");
        assert_eq!(
            std::fs::read_to_string(d.join("new/deep.md")).unwrap(),
            "again\n"
        );
    }

    /// The sandbox is the whole point: an agent editing its own worktree must
    /// not be able to reach the caller's tree through a path or a symlink.
    #[tokio::test]
    async fn neither_tool_escapes_its_roots() {
        let d = sandbox("escape");
        let outside = sandbox("outside");
        std::fs::write(outside.join("secret.md"), "private\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.md"), d.join("link.md")).unwrap();

        let edit = ScopedEditTool::new("A", std::slice::from_ref(&d));
        let write = ScopedWriteTool::new("A", std::slice::from_ref(&d));

        let abs = outside.join("secret.md").to_string_lossy().to_string();
        let out = call(
            &edit,
            json!({"file_path": abs, "old_string": "private",
                                     "new_string": "leaked", "replace_all": false}),
        )
        .await;
        assert!(out.contains("EDIT_FILE_OUT_OF_SANDBOX"), "{out}");

        let out = call(&write, json!({"file_path": abs, "content": "leaked"})).await;
        assert!(out.contains("WRITE_FILE_OUT_OF_SANDBOX"), "{out}");

        #[cfg(unix)]
        {
            let out = call(
                &edit,
                json!({"file_path": "link.md", "old_string": "private",
                                         "new_string": "leaked", "replace_all": false}),
            )
            .await;
            assert!(
                out.contains("EDIT_FILE_OUT_OF_SANDBOX"),
                "symlink escape: {out}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(outside.join("secret.md")).unwrap(),
            "private\n",
            "nothing outside the sandbox may be modified"
        );
    }

    /// A `..` in the part of the path that does not exist yet would climb back
    /// out AFTER the root check, so it is refused before the check happens.
    #[tokio::test]
    async fn write_refuses_traversal_through_a_path_that_does_not_exist_yet() {
        let d = sandbox("traverse");
        let write = ScopedWriteTool::new("A", std::slice::from_ref(&d));
        let out = call(
            &write,
            json!({"file_path": "nope/../../escaped.md",
                                      "content": "leaked"}),
        )
        .await;
        assert!(out.contains("WRITE_FILE_OUT_OF_SANDBOX"), "{out}");
        assert!(
            !d.parent().unwrap().join("escaped.md").exists(),
            "nothing may be written above the root"
        );
    }

    /// With no usable root every call is denied, rather than defaulting to the
    /// process working directory.
    #[tokio::test]
    async fn no_roots_denies_everything() {
        let missing = PathBuf::from("/definitely/not/here");
        let edit = ScopedEditTool::new("A", std::slice::from_ref(&missing));
        let out = call(
            &edit,
            json!({"file_path": "a.md", "old_string": "a",
                                     "new_string": "b", "replace_all": false}),
        )
        .await;
        assert!(out.contains("EDIT_FILE_OUT_OF_SANDBOX"), "{out}");
    }
}
