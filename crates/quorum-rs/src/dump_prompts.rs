//! Opt-in outgoing prompt / request dumps for token-efficiency analysis.
//!
//! Enabled by the `NSED_DUMP_PROMPTS_DIR` environment variable (unset → no-op, so
//! there is zero cost in production). Each provider writes the payload it is about
//! to send in its **native** format at the moment it sends it:
//!
//! - `claude` — the CLI argv (each flag + value, so `--append-system-prompt`
//!   payloads are visible) followed by the stdin prompt.
//! - OpenAI-wire (`openai`/`ollama`/`openrouter`/…) — the final Chat Completions
//!   request JSON (messages, tools, params) as sent to the endpoint.
//!
//! Files are named `{seq}-{agent}[-r{round}][-{phase}]-{provider}.{ext}` under the
//! dump dir, `{seq}` being a per-process call counter so ordering + uniqueness hold
//! without a clock. Writing is best-effort: a filesystem error is logged and
//! swallowed — a dump must never break a live agent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The dump directory from `NSED_DUMP_PROMPTS_DIR`, or `None` when unset/blank.
pub fn dir() -> Option<PathBuf> {
    match std::env::var("NSED_DUMP_PROMPTS_DIR") {
        Ok(d) if !d.trim().is_empty() => Some(PathBuf::from(d)),
        _ => None,
    }
}

/// Labels for the dump filename. `round`/`phase` are optional — the OpenAI-wire
/// path builds the request before the deliberation phase is threaded through, so
/// it dumps with the agent name alone.
pub struct Meta<'a> {
    pub agent: &'a str,
    pub round: Option<u32>,
    pub phase: Option<&'a str>,
}

/// Dump `body` when `NSED_DUMP_PROMPTS_DIR` is set; a no-op otherwise.
pub fn dump(meta: &Meta, provider: &str, ext: &str, body: &str) {
    let Some(dir) = dir() else {
        return;
    };
    if let Err(e) = dump_to(&dir, meta, provider, ext, body) {
        tracing::warn!(error = %e, dir = %dir.display(), "dump_prompts: write failed");
    }
}

/// Write the dump to `dir`, returning the file path. Pure of the environment so it
/// is unit-testable without touching the process-global `NSED_DUMP_PROMPTS_DIR`.
fn dump_to(
    dir: &Path,
    meta: &Meta,
    provider: &str,
    ext: &str,
    body: &str,
) -> std::io::Result<PathBuf> {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = format!("{seq:06}-{}", sanitize(meta.agent));
    if let Some(r) = meta.round {
        name.push_str(&format!("-r{r}"));
    }
    if let Some(p) = meta.phase {
        name.push_str(&format!("-{}", sanitize(p)));
    }
    name.push_str(&format!("-{}.{}", sanitize(provider), sanitize(ext)));
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Filename-safe token: keep alphanumerics / `-` / `_`, map everything else to `_`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render a claude CLI invocation for a dump: argv one-per-line (so long
/// `--append-system-prompt` values are readable) then the stdin prompt.
pub fn render_claude(command: &[String], prompt: &str) -> String {
    let mut out = String::new();
    for (i, a) in command.iter().enumerate() {
        out.push_str(&format!("[{i}] {a}\n"));
    }
    out.push_str("\n----- stdin prompt -----\n");
    out.push_str(prompt);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_to_writes_labeled_file_with_body() {
        let dir = std::env::temp_dir().join(format!("nsed-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let meta = Meta {
            agent: "PropductBot Fast",
            round: Some(3),
            phase: Some("propose"),
        };
        let path = dump_to(&dir, &meta, "claude", "txt", "hello body").unwrap();
        let fname = path.file_name().unwrap().to_string_lossy();
        assert!(
            fname.ends_with("-claude.txt"),
            "provider+ext in name: {fname}"
        );
        assert!(
            fname.contains("-r3-propose-"),
            "round+phase in name: {fname}"
        );
        // Space in the agent name is sanitized to keep the path safe.
        assert!(
            fname.contains("PropductBot_Fast"),
            "sanitized agent: {fname}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello body");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dump_to_omits_absent_round_and_phase() {
        let dir = std::env::temp_dir().join(format!("nsed-dump-min-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let meta = Meta {
            agent: "agentA",
            round: None,
            phase: None,
        };
        let path = dump_to(&dir, &meta, "openai", "json", "{}").unwrap();
        let fname = path.file_name().unwrap().to_string_lossy();
        assert!(
            fname.contains("-agentA-openai.json"),
            "minimal name: {fname}"
        );
        assert!(!fname.contains("-r"), "no round segment: {fname}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_none_when_env_unset_and_sanitize_maps_unsafe_chars() {
        // No test in this crate sets NSED_DUMP_PROMPTS_DIR, so the feature is off by
        // default — dump() is a no-op. (Env-mutation is avoided; it races parallel tests.)
        assert!(dir().is_none(), "unset env → feature off");
        assert_eq!(sanitize("a/b c:d"), "a_b_c_d");
    }

    #[test]
    fn render_claude_lists_argv_then_prompt() {
        let cmd = vec![
            "claude".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        let out = render_claude(&cmd, "the task");
        assert!(out.contains("[0] claude"));
        assert!(out.contains("[1] --model"));
        assert!(out.contains("----- stdin prompt -----\nthe task"));
    }
}
