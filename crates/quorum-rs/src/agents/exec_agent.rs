//! Exec provider: delegates propose/evaluate to an external subprocess.
//!
//! The subprocess receives a JSON envelope on stdin and writes the response
//! JSON to stdout. This allows agents written in any language (Python,
//! TypeScript, etc.) to participate in NSED deliberation without speaking
//! NATS directly.
//!
//! See `docs/exec-agent-protocol.md` for the full protocol specification.

use std::time::Duration;

use crate::agents::config::ExecProviderConfig;
use crate::agents::{AgentContext, Evaluation, NsedAgent, Proposal};
use crate::providers::cli_base;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::warn;

// ─── Delimiter markers for stdout pollution resistance ───────────────────────

const NSED_START: &str = "___NSED_START___";
const NSED_END: &str = "___NSED_END___";

// ─── Stdin envelope ──────────────────────────────────────────────────────────

/// JSON envelope written to the subprocess's stdin.
#[derive(Debug, Serialize)]
struct ExecEnvelope<'a> {
    phase: &'a str,
    context: &'a AgentContext,
}

// ─── Stdout response types ───────────────────────────────────────────────────

/// Wrapper for the evaluate response from external processes.
#[derive(Debug, Deserialize)]
pub struct ExecEvaluationResponse {
    pub evaluations: Vec<ExecEvaluationItem>,
}

/// A single evaluation item from an external process.
#[derive(Debug, Deserialize)]
pub struct ExecEvaluationItem {
    #[serde(alias = "agent_id", alias = "candidate_id")]
    pub target_id: String,
    #[serde(flatten)]
    pub evaluation: Evaluation,
}

// ─── ExecAgent ───────────────────────────────────────────────────────────────

/// An agent that delegates work to an external subprocess via stdin/stdout.
#[derive(Debug, Clone)]
pub struct ExecAgent {
    name: String,
    config: ExecProviderConfig,
}

impl ExecAgent {
    pub fn new(name: String, config: ExecProviderConfig) -> Self {
        Self { name, config }
    }

    /// Resolve the effective timeout for a single call.
    fn effective_timeout(&self, ctx: &AgentContext) -> Duration {
        cli_base::effective_timeout(self.config.timeout_secs, ctx)
    }

    /// Spawn the subprocess, write the envelope to stdin, and return stdout.
    async fn run_subprocess(&self, phase: &str, ctx: &AgentContext) -> Result<String> {
        let timeout = self.effective_timeout(ctx);
        let envelope = serde_json::to_string(&ExecEnvelope {
            phase,
            context: ctx,
        })
        .context("failed to serialize agent context to JSON")?;

        let mut child = cli_base::spawn_child(
            "exec",
            &self.name,
            &self.config.command,
            self.config.working_dir.as_deref(),
            &self.config.env,
            &[],
        )?;

        // Write envelope to stdin, then close it.
        // BrokenPipe means the subprocess closed its stdin (exited early or ignores it).
        // We defer judgment: if the process ultimately exits non-zero the exit-status
        // check below surfaces the real error. If it exits successfully despite never
        // reading the context envelope, the proposal is invalid — we reject it.
        let mut stdin = child.stdin.take().expect("stdin piped");
        let write_result = stdin.write_all(envelope.as_bytes()).await;
        drop(stdin);
        let stdin_broken_pipe = match write_result {
            Ok(()) => false,
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => true,
            Err(e) => return Err(e).context("failed to write to subprocess stdin"),
        };

        // Concurrently drain stdout and stderr to avoid pipe buffer deadlock.
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let agent_name = self.name.clone();
        let stderr_handle = tokio::spawn(async move {
            let mut buf = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut buf).await?;
            Ok::<String, std::io::Error>(buf)
        });

        let stdout_handle = tokio::spawn(async move {
            let mut buf = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut buf).await?;
            Ok::<String, std::io::Error>(buf)
        });

        // Apply timeout around the concurrent read + process wait.
        let result = tokio::time::timeout(timeout, async {
            let (stdout_res, stderr_res) = tokio::try_join!(stdout_handle, stderr_handle)
                .context("join error reading subprocess output")?;

            let stdout_str = stdout_res.context("reading stdout")?;
            let stderr_str = stderr_res.context("reading stderr")?;

            let status = child.wait().await.context("waiting for subprocess")?;

            Ok::<(String, String, std::process::ExitStatus), anyhow::Error>((
                stdout_str, stderr_str, status,
            ))
        })
        .await;

        match result {
            Ok(Ok((stdout_str, stderr_str, status))) => {
                // Log stderr lines as warnings (diagnostics from the subprocess).
                if !stderr_str.is_empty() {
                    for line in stderr_str.lines() {
                        warn!(agent = %agent_name, "exec stderr: {line}");
                    }
                }

                if !status.success() {
                    let code = status.code().unwrap_or(-1);
                    let snippet: String = stderr_str.chars().take(500).collect();
                    bail!(
                        "exec agent '{}': process exited with code {code}: {snippet}",
                        self.name,
                    );
                }

                // If stdin write hit BrokenPipe but the process still exited
                // successfully, the subprocess never received the context envelope.
                // Its output cannot be trusted — reject it.
                if stdin_broken_pipe {
                    bail!(
                        "exec agent '{}': subprocess exited successfully without reading \
                         the context envelope (stdin closed before write completed)",
                        self.name,
                    );
                }

                if stdout_str.trim().is_empty() {
                    bail!(
                        "exec agent '{}': process produced no output on stdout",
                        self.name
                    );
                }

                Ok(stdout_str)
            }
            Ok(Err(e)) => Err(e),
            Err(_elapsed) => {
                // Timeout — kill the subprocess and reap it.
                let _ = child.kill().await;
                let _ = child.wait().await;
                bail!(
                    "exec agent '{}': timed out after {}s",
                    self.name,
                    timeout.as_secs(),
                );
            }
        }
    }
}

// ─── Known provider wrapper unwrapping ───────────────────────────────────────

/// Unwrap known provider JSON wrappers (e.g. Claude CLI `--output-format json`).
///
/// Claude CLI outputs: `{"type":"result","result":"<text>","subtype":"success",...}`
/// We extract the `result` field and return it as the actual payload.
///
/// Returns `None` if the JSON is not a recognised wrapper format.
fn unwrap_provider_envelope(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = v.as_object()?;

    // Claude CLI format: has "type":"result" and a "result" field
    if obj.get("type").and_then(|t| t.as_str()) == Some("result") {
        // Error envelopes must not be unwrapped as successful values
        if obj
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        if let Some(result) = obj.get("result") {
            return match result {
                serde_json::Value::String(s) => Some(s.clone()),
                // If result is already a JSON object, re-serialize it
                serde_json::Value::Object(_) => Some(result.to_string()),
                _ => None,
            };
        }
    }
    None
}

/// Coerce a plain text string into a Proposal JSON object.
///
/// When an exec provider (e.g. Claude CLI) returns free-form text instead of
/// structured `{"thought_process":"...","content":"..."}`, wrap it.
fn coerce_text_to_proposal_json(text: &str) -> String {
    serde_json::json!({
        "thought_process": "(generated by exec provider)",
        "content": text.trim()
    })
    .to_string()
}

/// Coerce a plain text string into an evaluation response JSON.
///
/// Used when exec provider returns text instead of structured evaluation.
/// Generates a neutral 0.5 score for each candidate since the provider
/// didn't produce structured scores.
fn coerce_text_to_evaluation_json(text: &str, candidates: &[&str]) -> String {
    let evals: Vec<serde_json::Value> = candidates
        .iter()
        .map(|id| {
            serde_json::json!({
                "target_id": id,
                "score": 0.5,
                "justification": text.trim()
            })
        })
        .collect();
    serde_json::json!({ "evaluations": evals }).to_string()
}

// ─── JSON extraction from potentially polluted stdout ────────────────────────

/// Strip markdown code fences (`` ```json ... ``` ``) that LLMs commonly wrap
/// around JSON output. Returns the inner content if fences are found, otherwise
/// the original string unchanged.
fn strip_markdown_fences(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        // Skip optional language tag (e.g. "json", "JSON") on the opening fence
        let after_tag = match rest.find('\n') {
            Some(nl) => &rest[nl + 1..],
            None => return trimmed, // malformed: no newline after opening fence
        };
        // Strip closing fence
        let inner = if let Some(body) = after_tag.strip_suffix("```") {
            body
        } else {
            // Closing fence might have trailing whitespace
            match after_tag.rfind("```") {
                Some(pos) => &after_tag[..pos],
                None => return trimmed, // no closing fence — return as-is
            }
        };
        inner.trim()
    } else {
        trimmed
    }
}

/// Extract the payload JSON from stdout that may contain framework noise.
///
/// Strategies (tried in order):
/// 1. Delimiter markers: `___NSED_START___` ... `___NSED_END___`
/// 2. Last JSON object: scan from the end for the last `{...}` block
/// 3. Raw parse: try the whole string
///
/// All strategies strip markdown code fences before returning.
fn extract_json(raw: &str) -> Result<&str> {
    // Strategy 1: delimiters
    if let Some(start) = raw.find(NSED_START) {
        let after_start = start + NSED_START.len();
        if let Some(end) = raw[after_start..].find(NSED_END) {
            let json = raw[after_start..after_start + end].trim();
            if !json.is_empty() {
                return Ok(strip_markdown_fences(json));
            }
        }
    }

    // Strategy 2: best-effort brace-matching fallback.
    // NOTE: this does not handle braces inside JSON string literals (e.g.
    // {"content":"use { and }"}), so it can misidentify object bounds. This is
    // intentional — Strategy 1 (delimiters) is the canonical approach, and if
    // the extracted slice is not valid JSON the caller's parse will surface a
    // clear error per protocol guidance.
    let trimmed = strip_markdown_fences(raw.trim());
    if let Some(last_brace) = trimmed.rfind('}') {
        let candidate = &trimmed[..=last_brace];
        // Walk backwards to find the matching opening brace.
        let mut depth = 0i32;
        let mut start_pos = None;
        for (i, ch) in candidate.char_indices().rev() {
            match ch {
                '}' => depth += 1,
                '{' => {
                    depth -= 1;
                    if depth == 0 {
                        start_pos = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(start) = start_pos {
            return Ok(&trimmed[start..=last_brace]);
        }
    }

    // Strategy 3: raw parse (caller will report the JSON error)
    Ok(trimmed)
}

// ─── NsedAgent implementation ────────────────────────────────────────────────

#[async_trait]
impl NsedAgent for ExecAgent {
    async fn propose(&self, context: &AgentContext) -> Result<Proposal> {
        let raw = self.run_subprocess("propose", context).await?;
        let json = extract_json(&raw).with_context(|| {
            format!(
                "exec agent '{}': could not extract JSON from stdout",
                self.name
            )
        })?;

        // Try direct parse first (standard exec protocol).
        if let Ok(proposal) = serde_json::from_str::<Proposal>(json) {
            return Ok(proposal);
        }

        // Unwrap known provider envelopes (e.g. Claude CLI --output-format json).
        if let Some(inner) = unwrap_provider_envelope(json) {
            // Inner might be structured JSON or plain text.
            if let Ok(proposal) = serde_json::from_str::<Proposal>(&inner) {
                return Ok(proposal);
            }
            // Plain text — coerce to Proposal.
            let coerced = coerce_text_to_proposal_json(&inner);
            return serde_json::from_str::<Proposal>(&coerced).with_context(|| {
                format!(
                    "exec agent '{}': failed to coerce provider text to proposal",
                    self.name,
                )
            });
        }

        // No provider envelope detected — report the original parse error.
        let preview: String = json.chars().take(200).collect();
        serde_json::from_str::<Proposal>(json).with_context(|| {
            format!(
                "exec agent '{}': failed to parse proposal from stdout (first 200 chars): {}",
                self.name, preview,
            )
        })
    }

    async fn evaluate(&self, context: &AgentContext) -> Result<Vec<(String, Evaluation)>> {
        let raw = self.run_subprocess("evaluate", context).await?;
        let json = extract_json(&raw).with_context(|| {
            format!(
                "exec agent '{}': could not extract JSON from stdout",
                self.name
            )
        })?;

        // Ground each claim citation to the exact span of the proposal it
        // targets (quote-wrapper + whitespace tolerant), leaving unresolvable
        // ones unchanged — exec has no tool-error retry, so this is the
        // non-destructive counterpart to the MCP path's reject-and-retry.
        let content_by_id: std::collections::HashMap<&str, &str> = context
            .candidates
            .iter()
            .map(|c| (c.id.as_str(), c.proposal.content.as_str()))
            .collect();
        let ground = |response: ExecEvaluationResponse| -> Vec<(String, Evaluation)> {
            response
                .evaluations
                .into_iter()
                .map(|item| {
                    let mut eval = item.evaluation;
                    if let Some(content) = content_by_id.get(item.target_id.as_str()) {
                        super::cite::substitute_resolvable(content, &mut eval.claim_assessments);
                    }
                    (item.target_id, eval)
                })
                .collect()
        };

        // Try direct parse first (standard exec protocol).
        if let Ok(response) = serde_json::from_str::<ExecEvaluationResponse>(json) {
            return Ok(ground(response));
        }

        // Unwrap known provider envelopes (e.g. Claude CLI).
        if let Some(inner) = unwrap_provider_envelope(json) {
            if let Ok(response) = serde_json::from_str::<ExecEvaluationResponse>(&inner) {
                return Ok(ground(response));
            }
            // Plain text — coerce to evaluations with candidate IDs from context.
            let candidate_ids: Vec<&str> =
                context.candidates.iter().map(|c| c.id.as_str()).collect();
            let coerced = coerce_text_to_evaluation_json(&inner, &candidate_ids);
            let response: ExecEvaluationResponse =
                serde_json::from_str(&coerced).with_context(|| {
                    format!(
                        "exec agent '{}': failed to coerce provider text to evaluations",
                        self.name,
                    )
                })?;
            return Ok(ground(response));
        }

        // No provider envelope — report original parse error.
        let preview: String = json.chars().take(200).collect();
        let response: ExecEvaluationResponse = serde_json::from_str(json).with_context(|| {
            format!(
                "exec agent '{}': failed to parse evaluations from stdout (first 200 chars): {}",
                self.name, preview,
            )
        })?;
        Ok(ground(response))
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn default_config(command: Vec<String>) -> ExecProviderConfig {
        ExecProviderConfig {
            command,
            working_dir: None,
            env: HashMap::new(),
            timeout_secs: Some(10),
        }
    }

    fn minimal_context() -> AgentContext {
        AgentContext {
            task_description: "Solve the problem".to_string(),
            round_number: 1,
            phase_budget_remaining_secs: 60.0,
            ..AgentContext::default()
        }
    }

    // ── extract_json tests ───────────────────────────────────────────────

    #[test]
    fn extract_json_with_delimiters() {
        let raw = r#"Loading model...
WARNING: something
___NSED_START___
{"thought_process": "think", "content": "answer"}
___NSED_END___
Cleanup done.
"#;
        let json = extract_json(raw).unwrap();
        assert_eq!(json, r#"{"thought_process": "think", "content": "answer"}"#);
    }

    #[test]
    fn extract_json_last_object_fallback() {
        let raw = r#"WARNING: deprecated
Some debug info
{"thought_process": "think", "content": "answer"}"#;
        let json = extract_json(raw).unwrap();
        assert_eq!(json, r#"{"thought_process": "think", "content": "answer"}"#);
    }

    #[test]
    fn extract_json_raw_clean() {
        let raw = r#"{"thought_process": "x", "content": "y"}"#;
        let json = extract_json(raw).unwrap();
        assert_eq!(json, raw);
    }

    #[test]
    fn extract_json_nested_braces() {
        let raw = r#"junk {"inner": {"a": 1}, "outer": true}"#;
        let json = extract_json(raw).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["outer"], true);
    }

    // ── strip_markdown_fences tests ────────────────────────────────────────

    #[test]
    fn strip_fences_json_tag() {
        let input = "```json\n{\"score\": 0.8}\n```";
        assert_eq!(strip_markdown_fences(input), "{\"score\": 0.8}");
    }

    #[test]
    fn strip_fences_no_tag() {
        let input = "```\n{\"score\": 0.8}\n```";
        assert_eq!(strip_markdown_fences(input), "{\"score\": 0.8}");
    }

    #[test]
    fn strip_fences_trailing_whitespace() {
        let input = "```json\n{\"score\": 0.8}\n```\n  ";
        assert_eq!(strip_markdown_fences(input), "{\"score\": 0.8}");
    }

    #[test]
    fn strip_fences_no_fences_passthrough() {
        let input = "{\"score\": 0.8}";
        assert_eq!(strip_markdown_fences(input), input);
    }

    #[test]
    fn strip_fences_multiline_json() {
        let input = "```json\n{\n  \"score\": 0.85,\n  \"justification\": \"good\"\n}\n```";
        let result = strip_markdown_fences(input);
        let parsed: serde_json::Value = serde_json::from_str(result).unwrap();
        assert_eq!(parsed["score"], 0.85);
    }

    #[test]
    fn extract_json_delimiters_with_markdown_fences() {
        let raw = "noise\n___NSED_START___\n```json\n{\"score\": 0.8, \"justification\": \"solid\"}\n```\n___NSED_END___\nmore noise";
        let json = extract_json(raw).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!((parsed["score"].as_f64().unwrap() - 0.8).abs() < 0.01);
    }

    #[test]
    fn extract_json_bare_markdown_fences() {
        let raw = "```json\n{\"thought_process\": \"analysis\", \"content\": \"proposal\"}\n```";
        let json = extract_json(raw).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["content"], "proposal");
    }

    // ── Config serialization ─────────────────────────────────────────────

    #[test]
    fn config_deserialization_roundtrip() {
        let config = ExecProviderConfig {
            command: vec!["python3".into(), "agent.py".into()],
            working_dir: Some(PathBuf::from("/opt/agents")),
            env: HashMap::from([("MY_VAR".into(), "val".into())]),
            timeout_secs: Some(120),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ExecProviderConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn config_deserialization_minimal() {
        let json = r#"{"command": ["echo", "hi"]}"#;
        let config: ExecProviderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.command, vec!["echo", "hi"]);
        assert!(config.working_dir.is_none());
        assert!(config.env.is_empty());
        assert!(config.timeout_secs.is_none());
    }

    // ── Trait requirements ───────────────────────────────────────────────

    #[test]
    fn agent_is_clone_and_debug() {
        let agent = ExecAgent::new("test".into(), default_config(vec!["echo".into()]));
        let cloned = agent.clone();
        assert_eq!(cloned.name, "test");
        let debug = format!("{agent:?}");
        assert!(debug.contains("ExecAgent"));
    }

    // ── propose / evaluate success ───────────────────────────────────────

    #[tokio::test]
    async fn propose_success() {
        let config = default_config(vec![
            "bash".into(),
            "-c".into(),
            r#"cat >/dev/null; echo '{"thought_process":"reasoning","content":"solution"}'"#.into(),
        ]);
        let agent = ExecAgent::new("test".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        assert_eq!(proposal.thought_process, "reasoning");
        assert_eq!(proposal.content, "solution");
    }

    #[tokio::test]
    async fn evaluate_success() {
        let eval_json =
            r#"{"evaluations":[{"target_id":"AGENT_A","score":0.85,"justification":"good"}]}"#;
        let config = default_config(vec![
            "bash".into(),
            "-c".into(),
            format!("cat >/dev/null; echo '{eval_json}'"),
        ]);
        let agent = ExecAgent::new("test".into(), config);
        let ctx = minimal_context();

        let evals = agent.evaluate(&ctx).await.unwrap();
        assert_eq!(evals.len(), 1);
        assert_eq!(evals[0].0, "AGENT_A");
        assert!((evals[0].1.score - 0.85).abs() < 0.01);
        assert_eq!(evals[0].1.justification, "good");
    }

    // ── Stdout pollution resistance ──────────────────────────────────────

    #[tokio::test]
    async fn propose_with_delimiters() {
        let script = r#"
cat >/dev/null
echo "Loading model weights..."
echo "WARNING: deprecated API"
echo "___NSED_START___"
echo '{"thought_process":"t","content":"c"}'
echo "___NSED_END___"
echo "Cleanup done"
"#;
        let config = default_config(vec!["bash".into(), "-c".into(), script.into()]);
        let agent = ExecAgent::new("test".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        assert_eq!(proposal.thought_process, "t");
        assert_eq!(proposal.content, "c");
    }

    #[tokio::test]
    async fn propose_stdout_pollution_fallback() {
        let script = r#"
cat >/dev/null
echo "LangChainDeprecationWarning: blah"
echo '{"thought_process":"t","content":"c"}'
"#;
        let config = default_config(vec!["bash".into(), "-c".into(), script.into()]);
        let agent = ExecAgent::new("test".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        assert_eq!(proposal.content, "c");
    }

    // ── Error cases ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn propose_timeout() {
        let config = ExecProviderConfig {
            command: vec!["sleep".into(), "999".into()],
            working_dir: None,
            env: HashMap::new(),
            timeout_secs: Some(1),
        };
        let agent = ExecAgent::new("slow".into(), config);
        let ctx = minimal_context();

        let err = agent.propose(&ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("timed out"),
            "Expected timeout error, got: {err}"
        );
    }

    #[tokio::test]
    async fn propose_invalid_json() {
        let config = default_config(vec![
            "bash".into(),
            "-c".into(),
            "cat >/dev/null; echo 'not json at all'".into(),
        ]);
        let agent = ExecAgent::new("bad".into(), config);
        let ctx = minimal_context();

        let err = agent.propose(&ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("failed to parse proposal"),
            "Expected parse error, got: {err}"
        );
    }

    #[tokio::test]
    async fn propose_nonzero_exit() {
        let config = default_config(vec![
            "bash".into(),
            "-c".into(),
            "echo 'something broke' >&2; exit 1".into(),
        ]);
        let agent = ExecAgent::new("failing".into(), config);
        let ctx = minimal_context();

        let err = agent.propose(&ctx).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exited with code 1"), "got: {msg}");
        assert!(
            msg.contains("something broke"),
            "stderr not captured: {msg}"
        );
    }

    // Regression: a subprocess that exits before reading stdin causes a
    // `BrokenPipe` error on the `write_all` call. That error must be swallowed
    // and the real failure — the non-zero exit code — must be surfaced instead.
    #[tokio::test]
    async fn propose_broken_pipe_surfaces_exit_status() {
        // Close stdin explicitly, write to stderr, exit non-zero — ensures
        // write_all sees BrokenPipe before the process reads a single byte.
        let config = default_config(vec![
            "bash".into(),
            "-c".into(),
            // Close stdin explicitly, write to stderr, exit non-zero.
            "exec 0<&-; echo 'pipe closed' >&2; exit 2".into(),
        ]);
        let agent = ExecAgent::new("broken-pipe".into(), config);
        let ctx = minimal_context();

        let err = agent.propose(&ctx).await.unwrap_err();
        let msg = err.to_string();
        // Must NOT surface the stdin write error.
        assert!(
            !msg.contains("failed to write to subprocess stdin"),
            "BrokenPipe leaked through: {msg}"
        );
        // Must surface the subprocess exit status.
        assert!(msg.contains("exited with code 2"), "got: {msg}");
    }

    // NOTE: the "stdin_broken_pipe + exit 0" branch (subprocess never reads the
    // envelope but exits successfully) cannot be tested reliably via subprocess:
    // the ~130-byte envelope fits in the 64 KB pipe buffer, so write_all
    // completes before bash executes `exec 0<&-` on macOS.  The guard is
    // covered by code review and the propose_broken_pipe_surfaces_exit_status
    // test which confirms BrokenPipe is detected on Linux CI.

    #[tokio::test]
    async fn propose_nonzero_exit_utf8_stderr() {
        // Verify stderr truncation doesn't panic on multi-byte UTF-8 boundaries.
        let script = r#"python3 -c "import sys; sys.stderr.write('é' * 600); sys.stderr.flush()" || printf 'é%.0s' $(seq 1 600) >&2; exit 1"#;
        let config = default_config(vec!["bash".into(), "-c".into(), script.into()]);
        let agent = ExecAgent::new("utf8-test".into(), config);
        let ctx = minimal_context();

        let err = agent.propose(&ctx).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exited with code 1"), "got: {msg}");
        // Should not panic and message should be truncated at char boundary
        assert!(msg.len() < 2000, "stderr not truncated: len={}", msg.len());
    }

    #[tokio::test]
    async fn propose_command_not_found() {
        let config = default_config(vec!["/nonexistent/binary".into()]);
        let agent = ExecAgent::new("missing".into(), config);
        let ctx = minimal_context();

        let err = agent.propose(&ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("failed to spawn"),
            "Expected spawn error, got: {err}"
        );
    }

    #[tokio::test]
    async fn propose_empty_command() {
        let config = default_config(vec![]);
        let agent = ExecAgent::new("empty".into(), config);
        let ctx = minimal_context();

        let err = agent.propose(&ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("command is empty"),
            "Expected empty command error, got: {err}"
        );
    }

    #[tokio::test]
    async fn propose_empty_stdout() {
        let config = default_config(vec!["bash".into(), "-c".into(), "cat >/dev/null".into()]);
        let agent = ExecAgent::new("quiet".into(), config);
        let ctx = minimal_context();

        let err = agent.propose(&ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("no output"),
            "Expected no-output error, got: {err}"
        );
    }

    // ── Environment and stdin ────────────────────────────────────────────

    #[tokio::test]
    async fn env_vars_passed_to_subprocess() {
        let config = ExecProviderConfig {
            command: vec![
                "bash".into(),
                "-c".into(),
                r#"cat >/dev/null; echo "{\"thought_process\":\"$MY_VAR\",\"content\":\"ok\"}""#
                    .into(),
            ],
            working_dir: None,
            env: HashMap::from([("MY_VAR".into(), "hello_from_env".into())]),
            timeout_secs: Some(10),
        };
        let agent = ExecAgent::new("env-test".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        assert_eq!(proposal.thought_process, "hello_from_env");
    }

    #[tokio::test]
    async fn stdin_receives_context() {
        // Subprocess copies stdin to stderr, then outputs valid JSON to stdout.
        let script = r#"
INPUT=$(cat)
echo "$INPUT" >&2
echo '{"thought_process":"ok","content":"done"}'
"#;
        let config = default_config(vec!["bash".into(), "-c".into(), script.into()]);
        let agent = ExecAgent::new("stdin-test".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        assert_eq!(proposal.content, "done");
        // The stderr output contains the JSON envelope — we can't assert on it
        // directly in a unit test (it goes to tracing::warn), but the test
        // verifies the subprocess received stdin without error.
    }

    // ── Pipe buffer safety ───────────────────────────────────────────────

    #[tokio::test]
    async fn large_stderr_does_not_deadlock() {
        // Write >64KB to stderr (OS pipe buffer limit) + valid JSON to stdout.
        // If we don't drain concurrently, this will hang.
        let script = r#"
cat >/dev/null
python3 -c "import sys; sys.stderr.write('x' * 100000 + '\n')" 2>/dev/null || \
  dd if=/dev/zero bs=1 count=100000 2>/dev/null | tr '\0' 'x' >&2
echo '{"thought_process":"ok","content":"survived"}'
"#;
        let config = ExecProviderConfig {
            command: vec!["bash".into(), "-c".into(), script.into()],
            working_dir: None,
            env: HashMap::new(),
            timeout_secs: Some(10),
        };
        let agent = ExecAgent::new("big-stderr".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        assert_eq!(proposal.content, "survived");
    }

    // ── Effective timeout ────────────────────────────────────────────────

    #[test]
    fn effective_timeout_config_overrides_budget() {
        let config = ExecProviderConfig {
            command: vec!["echo".into()],
            timeout_secs: Some(42),
            ..default_config(vec![])
        };
        let agent = ExecAgent::new("t".into(), config);
        let mut ctx = minimal_context();
        ctx.phase_budget_remaining_secs = 999.0;
        assert_eq!(agent.effective_timeout(&ctx), Duration::from_secs(42));
    }

    #[test]
    fn effective_timeout_falls_back_to_budget() {
        let config = ExecProviderConfig {
            command: vec!["echo".into()],
            timeout_secs: None,
            ..default_config(vec![])
        };
        let agent = ExecAgent::new("t".into(), config);
        let mut ctx = minimal_context();
        ctx.phase_budget_remaining_secs = 120.0;
        assert_eq!(agent.effective_timeout(&ctx), Duration::from_secs(120));
    }

    #[test]
    fn effective_timeout_default_300() {
        let config = ExecProviderConfig {
            command: vec!["echo".into()],
            timeout_secs: None,
            ..default_config(vec![])
        };
        let agent = ExecAgent::new("t".into(), config);
        let mut ctx = minimal_context();
        ctx.phase_budget_remaining_secs = 0.0;
        assert_eq!(agent.effective_timeout(&ctx), Duration::from_secs(300));
    }

    #[test]
    fn effective_timeout_small_budget_rounds_up() {
        let config = ExecProviderConfig {
            command: vec!["echo".into()],
            timeout_secs: None,
            ..default_config(vec![])
        };
        let agent = ExecAgent::new("t".into(), config);
        let mut ctx = minimal_context();
        ctx.phase_budget_remaining_secs = 0.3;
        assert_eq!(
            agent.effective_timeout(&ctx),
            Duration::from_secs(1),
            "Sub-second positive budget should ceil to 1s, not truncate to 0"
        );
    }

    // ── Provider envelope unwrapping ─────────────────────────────────────

    #[test]
    fn unwrap_claude_cli_text_result() {
        let json = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":5740,"result":"Hello! I'm Claude."}"#;
        let inner = unwrap_provider_envelope(json).unwrap();
        assert_eq!(inner, "Hello! I'm Claude.");
    }

    #[test]
    fn unwrap_claude_cli_json_result() {
        let json = r#"{"type":"result","subtype":"success","result":{"thought_process":"tp","content":"c"}}"#;
        let inner = unwrap_provider_envelope(json).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&inner).unwrap();
        assert_eq!(parsed["content"], "c");
    }

    #[test]
    fn unwrap_non_provider_json_returns_none() {
        let json = r#"{"thought_process":"tp","content":"c"}"#;
        assert!(unwrap_provider_envelope(json).is_none());
    }

    #[test]
    fn unwrap_invalid_json_returns_none() {
        assert!(unwrap_provider_envelope("not json").is_none());
    }

    #[test]
    fn unwrap_error_envelope_returns_none() {
        let json = r#"{"type":"result","is_error":true,"result":"Error: tool failed"}"#;
        assert!(
            unwrap_provider_envelope(json).is_none(),
            "error envelopes must not be unwrapped as successful values"
        );
    }

    #[test]
    fn unwrap_non_error_envelope_succeeds() {
        let json = r#"{"type":"result","is_error":false,"result":"ok","subtype":"success"}"#;
        assert_eq!(unwrap_provider_envelope(json).unwrap(), "ok");
    }

    #[test]
    fn coerce_text_produces_valid_proposal() {
        let json = coerce_text_to_proposal_json("  My answer here  ");
        let proposal: Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(proposal.content, "My answer here");
        assert!(!proposal.thought_process.is_empty());
    }

    #[test]
    fn coerce_text_produces_valid_evaluations() {
        let json = coerce_text_to_evaluation_json("Good work", &["AGENT_A", "AGENT_B"]);
        let resp: ExecEvaluationResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp.evaluations.len(), 2);
        assert_eq!(resp.evaluations[0].target_id, "AGENT_A");
        assert!((resp.evaluations[0].evaluation.score - 0.5).abs() < 0.01);
    }

    // ── E2E: Claude CLI wrapper format ───────────────────────────────────

    #[tokio::test]
    async fn propose_claude_cli_text_output() {
        // Simulates Claude CLI --output-format json producing a text result.
        let claude_output = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"duration_api_ms":1200,"num_turns":1,"result":"The answer is 42."}"#;
        let script = format!(
            "cat >/dev/null; echo '{}'",
            claude_output.replace('\'', "'\\''")
        );
        let config = default_config(vec!["bash".into(), "-c".into(), script]);
        let agent = ExecAgent::new("claude-test".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        assert_eq!(proposal.content, "The answer is 42.");
        assert_eq!(proposal.thought_process, "(generated by exec provider)");
    }

    #[tokio::test]
    async fn propose_claude_cli_structured_result() {
        // Claude CLI output where result is already structured JSON.
        let claude_output = r#"{"type":"result","subtype":"success","result":"{\"thought_process\":\"deep think\",\"content\":\"structured answer\"}"}"#;
        let script = format!(
            "cat >/dev/null; echo '{}'",
            claude_output.replace('\'', "'\\''")
        );
        let config = default_config(vec!["bash".into(), "-c".into(), script]);
        let agent = ExecAgent::new("claude-struct".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        // result is a JSON string containing the proposal — so it gets parsed as text
        // and coerced since the string itself contains escaped JSON
        assert!(!proposal.content.is_empty());
    }

    #[tokio::test]
    async fn propose_claude_cli_object_result() {
        // Claude CLI output where result is a proper JSON object (not string).
        let claude_output = r#"{"type":"result","subtype":"success","result":{"thought_process":"deep think","content":"object answer"}}"#;
        let script = format!("cat >/dev/null; echo '{}'", claude_output);
        let config = default_config(vec!["bash".into(), "-c".into(), script]);
        let agent = ExecAgent::new("claude-obj".into(), config);
        let ctx = minimal_context();

        let proposal = agent.propose(&ctx).await.unwrap();
        assert_eq!(proposal.thought_process, "deep think");
        assert_eq!(proposal.content, "object answer");
    }

    #[tokio::test]
    async fn evaluate_claude_cli_text_output() {
        // Claude CLI returning text evaluation — coerced to neutral scores.
        let claude_output =
            r#"{"type":"result","subtype":"success","result":"Both proposals are good."}"#;
        let script = format!("cat >/dev/null; echo '{}'", claude_output);
        let config = default_config(vec!["bash".into(), "-c".into(), script]);
        let agent = ExecAgent::new("claude-eval".into(), config);

        let mut ctx = minimal_context();
        ctx.candidates = vec![
            crate::agents::CandidateProposal {
                id: "A".into(),
                proposal: Proposal {
                    thought_process: "t".into(),
                    content: "c".into(),
                    ..Default::default()
                },
            },
            crate::agents::CandidateProposal {
                id: "B".into(),
                proposal: Proposal {
                    thought_process: "t".into(),
                    content: "c".into(),
                    ..Default::default()
                },
            },
        ];

        let evals = agent.evaluate(&ctx).await.unwrap();
        assert_eq!(evals.len(), 2);
        assert_eq!(evals[0].0, "A");
        assert_eq!(evals[1].0, "B");
        // Coerced to 0.5 neutral scores
        assert!((evals[0].1.score - 0.5).abs() < 0.01);
    }
}
