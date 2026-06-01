//! `quorum serve` — load a fleet config and run agents.
//!
//! Thin CLI wrapper around [`quorum_rs::serve::serve_fleet`].
//! Installs a default tracing subscriber, traps SIGTERM / SIGINT
//! and propagates a `CancellationToken` into every worker before
//! the runner future drops, and resolves the NATS URL from the
//! orchestrator at startup (or accepts an explicit `--nats-url`
//! override for offline / dev setups).
//!
//! The actual fleet dispatch (which agent type, what tools, how to
//! talk to the orchestrator's NATS bus) lives in the SDK so library
//! consumers can drive the same flow from their own binary —
//! useful for embedding agents in a larger service or wrapping the
//! runner with custom telemetry / dashboards.
//!
//! ## NATS URL resolution
//!
//! 1. `--nats-url <URL>` — explicit operator override.
//! 2. Workspace config (`nsed.yaml`) → resolve the room → look up the
//!    orchestrator entry → `mode: embedded` reads `nats_url`
//!    directly; `mode: remote` calls `GET /api/runtime/nats`.
//! 3. Hard error otherwise. **No localhost fallback. No silent
//!    fallback through `agent.yml.telemetry.endpoints[]` (that field
//!    is an observability sink, not a connection target).**
//!
//! The error path names the workspace path tried, the room resolved,
//! and the orchestrator address — so an operator can fix the missing
//! config without guessing.

use crate::cli::remote::{RemoteError, RemoteOrchestrator};
use crate::cli::workspace::{OrchestratorMode, WorkspaceConfig};
use crate::nats_utils::NatsAuth;
use crate::serve::{ServeOptions, serve_fleet};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Default fleet-config search order — keep aligned with the layout
/// `quorum init` writes and the layout `nsed serve` consumes in the
/// parent repo. If `--config` isn't passed, the CLI walks this list
/// and uses the first match.
const DEFAULT_FLEET_PATHS: &[&str] = &["agent.yml", "config/default.yml"];

/// Default operator creds-file location written by `quorum redeem`.
fn default_creds_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let mut p = PathBuf::from(home);
    p.push(".nsed");
    p.push("agent.creds");
    Some(p)
}

/// Resolve the fleet config path. Explicit `--config` wins;
/// otherwise the first existing path in [`DEFAULT_FLEET_PATHS`].
fn resolve_config_path(config: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = config {
        return Ok(p.to_path_buf());
    }
    for candidate in DEFAULT_FLEET_PATHS {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Ok(p);
        }
    }
    anyhow::bail!(
        "no fleet config found. Pass --config PATH or create one of: {}",
        DEFAULT_FLEET_PATHS.join(", ")
    )
}

/// Build the `NatsAuth` the runner uses for every agent.
///
/// Precedence (highest first):
/// 1. Explicit `--nats-creds PATH` flag.
/// 2. `~/.nsed/agent.creds` (written by `quorum redeem`).
/// 3. None — runner connects unauthenticated (dev orchestrators only).
fn resolve_nats_auth(creds_arg: Option<&Path>) -> Option<NatsAuth> {
    if let Some(p) = creds_arg {
        return Some(NatsAuth {
            creds_file: Some(p.display().to_string()),
            ..Default::default()
        });
    }
    if let Some(default) = default_creds_path()
        && default.exists()
    {
        return Some(NatsAuth {
            creds_file: Some(default.display().to_string()),
            ..Default::default()
        });
    }
    None
}

async fn resolve_nats_url(
    nats_url_flag: Option<&str>,
    workspace_path: &Path,
    room_flag: Option<&str>,
) -> Result<String> {
    if let Some(u) = nats_url_flag {
        return Ok(u.to_string());
    }

    let workspace = WorkspaceConfig::load(workspace_path).with_context(|| {
        format!(
            "no --nats-url passed and workspace config not loadable at {}",
            workspace_path.display()
        )
    })?;

    let (room_name, room) = workspace
        .resolve_room(room_flag)
        .with_context(|| "could not resolve a room for NATS-URL lookup")?;

    let orch_name = room.orchestrator.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "room `{room_name}` has no orchestrator wired — set `orchestrator: <name>` \
             or pass --nats-url"
        )
    })?;

    let orch = workspace.orchestrators.get(orch_name).ok_or_else(|| {
        anyhow::anyhow!(
            "room `{room_name}` references orchestrator `{orch_name}` which is not in \
             workspace.orchestrators"
        )
    })?;

    match orch.mode.as_ref() {
        Some(OrchestratorMode::Embedded) => orch.nats_url.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "orchestrator `{orch_name}` is embedded but has no `nats_url` set — fix the \
                 workspace config or pass --nats-url"
            )
        }),
        Some(OrchestratorMode::Remote) | None => {
            let address = orch.address.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "orchestrator `{orch_name}` is remote but has no `address` — fix the \
                     workspace config or pass --nats-url"
                )
            })?;
            let token_raw = orch.token.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "orchestrator `{orch_name}` is remote but has no `token` — fix the \
                     workspace config or pass --nats-url"
                )
            })?;
            let token = crate::config::resolve_env_token("token", token_raw);
            let client = RemoteOrchestrator::new(address, &token).with_context(|| {
                format!("building HTTP client for orchestrator `{orch_name}` at {address}")
            })?;
            client.runtime_nats().await.map_err(|e| match e {
                RemoteError::ApiError { status, body } => anyhow::anyhow!(
                    "orchestrator `{orch_name}` at {address} returned {status} on \
                     /api/runtime/nats: {body}. Pass --nats-url to bypass."
                ),
                other => anyhow::anyhow!(
                    "querying orchestrator `{orch_name}` at {address} for NATS URL: {other}"
                ),
            })
        }
    }
}

/// Entry point invoked by `Commands::Serve` in `main.rs`. Returns
/// when the runner exits (any worker fails, or SIGTERM / SIGINT
/// triggers the abort path).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: Option<&Path>,
    workspace_path: &Path,
    room: Option<&str>,
    nats_url: Option<&str>,
    nats_creds: Option<&Path>,
    agent_filter: Option<&[String]>,
    stream_name: Option<&str>,
    api_prefix: Option<&str>,
    dashboard_port: Option<u16>,
    dashboard_bind: Option<&str>,
) -> Result<()> {
    crate::serve::install_default_tracing();

    // `--dashboard-bind` flag wins over `QUORUM_DASHBOARD_BIND` env
    // var. Setting the env var here keeps the bind resolution
    // localized to `MultiAgentStatusServer::run_control_plane` —
    // library consumers driving the runner from their own binary
    // can set the env var directly without depending on this CLI
    // surface.
    if let Some(bind) = dashboard_bind {
        // SAFETY: env var mutation must happen before the runner
        // spawns the status server. Reached from a single-thread
        // entry point before tokio multi-threading kicks in for
        // worker tasks.
        unsafe {
            std::env::set_var("QUORUM_DASHBOARD_BIND", bind);
        }
    }

    let config_path = resolve_config_path(config)?;
    let fleet = crate::config::load_config(&config_path)
        .with_context(|| format!("failed to load fleet config at {}", config_path.display()))?;

    let resolved_nats_url = resolve_nats_url(nats_url, workspace_path, room).await?;
    tracing::info!(nats_url = %resolved_nats_url, "resolved NATS URL");

    let cancel = tokio_util::sync::CancellationToken::new();

    let opts = ServeOptions {
        nats_url: resolved_nats_url,
        nats_auth: resolve_nats_auth(nats_creds),
        agent_filter: agent_filter.map(|v| v.to_vec()),
        stream_name: stream_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| "sphera_jobs".to_string()),
        api_prefix: api_prefix
            .map(|s| s.to_string())
            .unwrap_or_else(|| "sphera".to_string()),
        cancel: Some(cancel.clone()),
        dashboard_port,
    };

    // Race the runner against a shutdown signal. On signal we
    // call `cancel.cancel()` — that signals the runner to abort
    // every worker BEFORE the select! finishes dropping the
    // runner future, so no worker task leaks.
    tokio::select! {
        result = serve_fleet(&fleet, opts) => result,
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal received; cancelling workers");
            cancel.cancel();
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            Ok(())
        }
    }
}

/// Cross-platform shutdown signal future. SIGTERM + SIGINT on Unix;
/// Ctrl-C on Windows. Resolves once when EITHER signal fires.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_config_path_prefers_explicit_arg() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("explicit.yml");
        std::fs::write(&p, "providers: {}\nagents: []\n").unwrap();
        let resolved = resolve_config_path(Some(&p)).unwrap();
        assert_eq!(resolved, p);
    }

    /// When neither --config nor any default-path file exists, the
    /// resolver bails with guidance about what paths it tried.
    #[test]
    fn resolve_config_path_bails_when_nothing_exists() {
        let tmp = TempDir::new().unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let err = resolve_config_path(None).unwrap_err();
        std::env::set_current_dir(cwd).unwrap();
        let msg = err.to_string();
        assert!(
            msg.contains("--config") && msg.contains("agent.yml"),
            "error must point at --config + default paths; got: {msg}"
        );
    }

    /// Explicit creds path wins over the default.
    #[test]
    fn resolve_nats_auth_uses_explicit_creds() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("custom.creds");
        std::fs::write(&p, "stub").unwrap();
        let auth = resolve_nats_auth(Some(&p)).unwrap();
        assert_eq!(
            auth.creds_file.as_deref(),
            Some(p.display().to_string().as_str())
        );
    }

    /// Without explicit creds AND without `~/.nsed/agent.creds`,
    /// returns None — runner connects unauthenticated, which is
    /// fine for a local dev NATS without an account JWT.
    #[test]
    #[serial_test::serial(home)]
    fn resolve_nats_auth_returns_none_when_no_creds_anywhere() {
        let tmp = TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: serialised via `#[serial(home)]`.
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let auth = resolve_nats_auth(None);
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        assert!(auth.is_none());
    }

    /// Offline / dev clusters can't reach the orchestrator at all —
    /// the workspace path may be wrong, missing, or unparseable. The
    /// `--nats-url` short-circuit must hold even then.
    #[tokio::test]
    async fn resolve_nats_url_explicit_flag_wins() {
        let tmp = TempDir::new().unwrap();
        let missing_ws = tmp.path().join("nsed.yaml");
        let resolved = resolve_nats_url(Some("nats://explicit:4222"), &missing_ws, None)
            .await
            .expect("explicit URL must short-circuit workspace lookup");
        assert_eq!(resolved, "nats://explicit:4222");
    }

    /// Workspace missing AND no `--nats-url` → structured error
    /// naming the workspace path. No localhost fallback.
    #[tokio::test]
    async fn resolve_nats_url_fails_loud_when_workspace_missing() {
        let tmp = TempDir::new().unwrap();
        let missing_ws = tmp.path().join("nsed.yaml");
        let err = resolve_nats_url(None, &missing_ws, None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nsed.yaml") && msg.contains("--nats-url"),
            "error must name workspace + suggest --nats-url; got: {msg}"
        );
        assert!(
            !msg.contains("localhost"),
            "error must NOT suggest localhost fallback; got: {msg}"
        );
    }

    /// Embedded orchestrator with a `nats_url` set → use it directly.
    /// Verifies the embedded short-circuit path: no HTTP call attempted,
    /// the value flows from yaml straight into the runner.
    #[tokio::test]
    async fn resolve_nats_url_embedded_uses_inline_field() {
        let tmp = TempDir::new().unwrap();
        let ws_path = tmp.path().join("nsed.yaml");
        std::fs::write(
            &ws_path,
            r#"
orchestrators:
  local:
    mode: embedded
    nats_url: "nats://embedded-host:4222"
policies:
  default:
    agents: [a, b]
rooms:
  main:
    policy: default
    orchestrator: local
default_room: main
"#,
        )
        .unwrap();
        let resolved = resolve_nats_url(None, &ws_path, None).await.unwrap();
        assert_eq!(resolved, "nats://embedded-host:4222");
    }
}
