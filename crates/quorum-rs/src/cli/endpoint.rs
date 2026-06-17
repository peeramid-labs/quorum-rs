//! Config-free client endpoint resolution.
//!
//! `quorum redeem` writes the operator bearer to `~/.nsed/operator.token`
//! and the orchestrator's HTTP address to `~/.nsed/orchestrator`. The
//! discovery/submit commands (`run`, `status`, `rooms`, `trace`) read both
//! back here when no workspace `nsed.yaml` is present, so onboarding is
//! `redeem → run` with zero config files.
//!
//! Endpoint precedence: `$QUORUM_ORCHESTRATOR` env wins, then the
//! persisted `~/.nsed/orchestrator` file. The token always comes from
//! `~/.nsed/operator.token`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::workspace::{OrchestratorConfig, OrchestratorMode, WorkspaceConfig};

/// File name redeem persists the orchestrator HTTP address to, beside the
/// operator token (`~/.nsed/orchestrator`).
const ENDPOINT_FILE: &str = "orchestrator";

/// File name redeem persists the operator bearer to
/// (`~/.nsed/operator.token`).
const TOKEN_FILE: &str = "operator.token";

/// Env var that overrides the persisted orchestrator endpoint.
const ENDPOINT_ENV: &str = "QUORUM_ORCHESTRATOR";

/// `~/.nsed` — where redeem persists creds/token/endpoint.
pub fn nsed_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let mut p = PathBuf::from(home);
    p.push(".nsed");
    Some(p)
}

/// Persist the orchestrator HTTP address beside the operator token
/// (`<token_dir>/orchestrator`) so the config-free client can reach the
/// orchestrator later. The address is not a secret, so it is written at
/// the default umask (unlike the 0600 token/creds files). Returns the
/// path written.
pub fn persist_endpoint(token_path: &Path, url: &str) -> std::io::Result<PathBuf> {
    let dir = token_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let path = dir.join(ENDPOINT_FILE);
    std::fs::write(&path, format!("{}\n", url.trim()))?;
    Ok(path)
}

/// Read a file, returning its trimmed contents only if non-empty.
fn read_nonempty(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Build a synthetic single-orchestrator workspace from the redeemed
/// endpoint + token, for the config-free client.
///
/// Pure form: reads `<nsed_dir>/orchestrator` and
/// `<nsed_dir>/operator.token`, with `env_endpoint` (the
/// `$QUORUM_ORCHESTRATOR` value) overriding the persisted address.
/// Returns an actionable error when the endpoint or token can't be
/// resolved (the operator hasn't redeemed yet).
fn remote_workspace_from(
    nsed_dir: &Path,
    env_endpoint: Option<&str>,
) -> Result<WorkspaceConfig, String> {
    let address = env_endpoint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| read_nonempty(&nsed_dir.join(ENDPOINT_FILE)))
        .ok_or_else(|| {
            "no orchestrator endpoint: run `quorum redeem <code>` first, set \
             $QUORUM_ORCHESTRATOR, or point --config at a nsed.yaml"
                .to_string()
        })?;
    let token_path = nsed_dir.join(TOKEN_FILE);
    let token = read_nonempty(&token_path).ok_or_else(|| {
        format!(
            "no operator token at {}: run `quorum redeem <code>` first",
            token_path.display()
        )
    })?;

    let mut orchestrators = HashMap::new();
    orchestrators.insert(
        "default".to_string(),
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Remote),
            address: Some(address),
            token: Some(token),
            nats_url: None,
            config_file: None,
        },
    );

    Ok(WorkspaceConfig {
        policies: HashMap::new(),
        orchestrators,
        rooms: HashMap::new(),
        shared: None,
        default_room: None,
        agents: None,
    })
}

/// Build the config-free workspace from the real `~/.nsed/` files and the
/// `$QUORUM_ORCHESTRATOR` override. See [`remote_workspace_from`].
pub fn remote_workspace() -> Result<WorkspaceConfig, String> {
    let dir = nsed_dir().ok_or_else(|| {
        "cannot determine home directory for ~/.nsed; set $QUORUM_ORCHESTRATOR and \
         pass --token-out, or use a nsed.yaml"
            .to_string()
    })?;
    let env_endpoint = std::env::var(ENDPOINT_ENV).ok();
    remote_workspace_from(dir.as_path(), env_endpoint.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_endpoint_writes_beside_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let token = tmp.path().join("operator.token");
        let written = persist_endpoint(&token, "http://orch.example:8080").unwrap();
        assert_eq!(written, tmp.path().join("orchestrator"));
        assert_eq!(
            std::fs::read_to_string(&written).unwrap().trim(),
            "http://orch.example:8080"
        );
    }

    #[test]
    fn persist_endpoint_creates_missing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let token = tmp.path().join("nested").join("operator.token");
        let written = persist_endpoint(&token, "http://x").unwrap();
        assert!(written.exists());
    }

    #[test]
    fn remote_workspace_from_files_builds_single_remote() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("orchestrator"), "http://orch:8080\n").unwrap();
        std::fs::write(tmp.path().join("operator.token"), "bearer-xyz\n").unwrap();

        let ws = remote_workspace_from(tmp.path(), None).unwrap();
        assert_eq!(ws.orchestrators.len(), 1);
        let orch = ws.orchestrators.get("default").unwrap();
        assert_eq!(orch.mode, Some(OrchestratorMode::Remote));
        assert_eq!(orch.address.as_deref(), Some("http://orch:8080"));
        assert_eq!(orch.token.as_deref(), Some("bearer-xyz"));
        assert!(ws.policies.is_empty() && ws.rooms.is_empty());
    }

    #[test]
    fn env_endpoint_overrides_persisted_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("orchestrator"), "http://from-file\n").unwrap();
        std::fs::write(tmp.path().join("operator.token"), "tok\n").unwrap();

        let ws = remote_workspace_from(tmp.path(), Some("http://from-env")).unwrap();
        assert_eq!(
            ws.orchestrators.get("default").unwrap().address.as_deref(),
            Some("http://from-env")
        );
    }

    #[test]
    fn blank_env_endpoint_falls_back_to_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("orchestrator"), "http://from-file\n").unwrap();
        std::fs::write(tmp.path().join("operator.token"), "tok\n").unwrap();

        let ws = remote_workspace_from(tmp.path(), Some("   ")).unwrap();
        assert_eq!(
            ws.orchestrators.get("default").unwrap().address.as_deref(),
            Some("http://from-file")
        );
    }

    #[test]
    fn missing_endpoint_errors_actionably() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("operator.token"), "tok\n").unwrap();
        let err = remote_workspace_from(tmp.path(), None).unwrap_err();
        assert!(err.contains("quorum redeem"), "got: {err}");
    }

    #[test]
    fn missing_token_errors_actionably() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("orchestrator"), "http://orch\n").unwrap();
        let err = remote_workspace_from(tmp.path(), None).unwrap_err();
        assert!(err.contains("operator token"), "got: {err}");
    }

    #[test]
    fn env_endpoint_works_without_persisted_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("operator.token"), "tok\n").unwrap();
        let ws = remote_workspace_from(tmp.path(), Some("http://env-only")).unwrap();
        assert_eq!(
            ws.orchestrators.get("default").unwrap().address.as_deref(),
            Some("http://env-only")
        );
    }

    // ── HOME-injected wrapper tests ──────────────────────────────────
    //
    // `remote_workspace()` and the `load_or_remote_default` synth branch
    // read the real `~/.nsed`. Point `$HOME` at a temp dir (serialised on
    // the `home_env` group so concurrent tests don't race the process env)
    // to exercise them deterministically.

    /// Run `f` with `$HOME` = `dir` and `$QUORUM_ORCHESTRATOR` = `env`,
    /// restoring both afterwards.
    fn with_home<R>(dir: &Path, env: Option<&str>, f: impl FnOnce() -> R) -> R {
        let prev_home = std::env::var_os("HOME");
        let prev_env = std::env::var_os(ENDPOINT_ENV);
        // SAFETY: serialised via `serial_test::serial(home_env)` on every
        // caller; no other code mutates these vars during the call window.
        unsafe {
            std::env::set_var("HOME", dir);
            match env {
                Some(v) => std::env::set_var(ENDPOINT_ENV, v),
                None => std::env::remove_var(ENDPOINT_ENV),
            }
        }
        let out = f();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_env {
                Some(v) => std::env::set_var(ENDPOINT_ENV, v),
                None => std::env::remove_var(ENDPOINT_ENV),
            }
        }
        out
    }

    /// Seed `<home>/.nsed/{orchestrator,operator.token}`.
    fn seed_nsed(home: &Path, endpoint: &str, token: &str) {
        let dir = home.join(".nsed");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(ENDPOINT_FILE), format!("{endpoint}\n")).unwrap();
        std::fs::write(dir.join(TOKEN_FILE), format!("{token}\n")).unwrap();
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn remote_workspace_reads_home_nsed() {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_nsed(tmp.path(), "http://home-orch:8080", "home-bearer");
        let ws = with_home(tmp.path(), None, remote_workspace).unwrap();
        let orch = ws.orchestrators.get("default").unwrap();
        assert_eq!(orch.address.as_deref(), Some("http://home-orch:8080"));
        assert_eq!(orch.token.as_deref(), Some("home-bearer"));
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn remote_workspace_env_overrides_home_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_nsed(tmp.path(), "http://home-orch", "tok");
        let ws = with_home(tmp.path(), Some("http://env-orch"), remote_workspace).unwrap();
        assert_eq!(
            ws.orchestrators.get("default").unwrap().address.as_deref(),
            Some("http://env-orch")
        );
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn remote_workspace_errors_when_nothing_redeemed() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Empty HOME, no env override → no endpoint resolvable.
        let err = with_home(tmp.path(), None, remote_workspace).unwrap_err();
        assert!(err.contains("quorum redeem"), "got: {err}");
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn load_or_remote_default_missing_file_synthesizes() {
        use crate::cli::workspace::WorkspaceConfig;
        let tmp = tempfile::TempDir::new().unwrap();
        seed_nsed(tmp.path(), "http://home-orch", "tok");
        let missing = tmp.path().join("does-not-exist.yaml");
        let ws = with_home(tmp.path(), None, || {
            WorkspaceConfig::load_or_remote_default(&missing)
        })
        .unwrap();
        assert_eq!(ws.orchestrators.len(), 1);
        assert!(ws.orchestrators.contains_key("default"));
        assert!(ws.policies.is_empty() && ws.rooms.is_empty());
    }
}
