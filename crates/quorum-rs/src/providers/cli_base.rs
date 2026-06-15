//! Shared subprocess plumbing for CLI-agent providers.
//!
//! [`ExecAgent`](crate::agents::exec_agent::ExecAgent) and
//! [`McpAgent`](crate::agents::mcp_agent::McpAgent) both spawn an
//! external process with piped stdio and a budget-derived timeout, but
//! diverge completely afterwards (exec = one-shot stdin/stdout; mcp =
//! line envelope + a live MCP session over the same pipes). Only the
//! spawn flags and the timeout-resolution rule are genuinely shared —
//! this module is that overlap, and nothing more.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};

use crate::agents::AgentContext;

/// Resolve the effective per-call timeout: explicit `timeout_secs` if
/// set, else the remaining phase budget rounded up (min 1s), else the
/// 300s default.
pub fn effective_timeout(timeout_secs: Option<u64>, ctx: &AgentContext) -> Duration {
    let secs = timeout_secs.unwrap_or_else(|| {
        let budget = ctx.phase_budget_remaining_secs;
        if budget > 0.0 {
            (budget.ceil() as u64).max(1)
        } else {
            300
        }
    });
    Duration::from_secs(secs)
}

/// Spawn a CLI-agent subprocess with stdin/stdout/stderr piped and
/// `kill_on_drop` set, applying `working_dir`, `env`, then `extra_env`
/// (session-identity vars and the like, layered last so they win).
///
/// `kind` is the provider label used in error messages (`"exec"` /
/// `"mcp"`). Returns an error if `command` is empty or the spawn fails.
pub fn spawn_child(
    kind: &str,
    agent_name: &str,
    command: &[String],
    working_dir: Option<&Path>,
    env: &HashMap<String, String>,
    extra_env: &[(&str, String)],
) -> Result<Child> {
    if command.is_empty() {
        bail!("{kind} agent '{agent_name}': command is empty");
    }

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    cmd.spawn()
        .with_context(|| format!("{kind} agent '{agent_name}': failed to spawn {command:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `AgentContext` for timeout tests, built via the same
    /// YAML/Default path the agents use. We only touch
    /// `phase_budget_remaining_secs`.
    fn ctx_with_budget(budget: f64) -> AgentContext {
        AgentContext {
            phase_budget_remaining_secs: budget,
            ..Default::default()
        }
    }

    #[test]
    fn explicit_timeout_wins_over_budget() {
        let ctx = ctx_with_budget(999.0);
        assert_eq!(effective_timeout(Some(42), &ctx), Duration::from_secs(42));
    }

    #[test]
    fn budget_used_when_no_explicit_timeout() {
        let ctx = ctx_with_budget(12.3);
        // ceil(12.3) = 13
        assert_eq!(effective_timeout(None, &ctx), Duration::from_secs(13));
    }

    #[test]
    fn zero_budget_falls_back_to_300s() {
        let ctx = ctx_with_budget(0.0);
        assert_eq!(effective_timeout(None, &ctx), Duration::from_secs(300));
    }

    #[test]
    fn tiny_positive_budget_floors_at_1s() {
        let ctx = ctx_with_budget(0.1);
        assert_eq!(effective_timeout(None, &ctx), Duration::from_secs(1));
    }

    #[test]
    fn empty_command_is_rejected() {
        let env = HashMap::new();
        let err = spawn_child("exec", "broken", &[], None, &env, &[]).unwrap_err();
        assert!(err.to_string().contains("command is empty"), "got: {err}");
    }

    #[tokio::test]
    async fn spawns_with_env_and_extra_env() {
        let mut env = HashMap::new();
        env.insert("BASE_VAR".to_string(), "base".to_string());
        let extra = [("EXTRA_VAR", "extra".to_string())];
        let command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf '%s-%s' \"$BASE_VAR\" \"$EXTRA_VAR\"".to_string(),
        ];
        let mut child = spawn_child("exec", "echoer", &command, None, &env, &extra)
            .expect("spawn must succeed");
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut out = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut out)
            .await
            .unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());
        assert_eq!(out, "base-extra");
    }
}
