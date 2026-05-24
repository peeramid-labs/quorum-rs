//! `quorum serve` — load a fleet config and run agents.
//!
//! Thin CLI wrapper around [`quorum_rs::serve::serve_fleet`].
//! Resolves the NATS URL + creds from a combination of flags and
//! sensible defaults (`~/.nsed/agent.creds` from `quorum redeem`),
//! installs a default tracing subscriber, traps SIGTERM / SIGINT so
//! the runner shuts down gracefully, and surfaces the SDK error
//! verbatim on failure.
//!
//! The actual fleet dispatch (which agent type, what tools, how to
//! talk to the orchestrator's NATS bus) lives in the SDK so library
//! consumers can drive the same flow from their own binary —
//! useful for embedding agents in a larger service or wrapping the
//! runner with custom telemetry / dashboards.

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

/// Entry point invoked by `Commands::Serve` in `main.rs`. Returns
/// when the runner exits (any worker fails, or SIGTERM / SIGINT
/// triggers the abort path).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: Option<&Path>,
    nats_url: Option<&str>,
    nats_creds: Option<&Path>,
    agent_filter: Option<&[String]>,
    stream_name: Option<&str>,
    api_prefix: Option<&str>,
) -> Result<()> {
    crate::serve::install_default_tracing();

    let config_path = resolve_config_path(config)?;
    let fleet = crate::config::load_config(&config_path)
        .with_context(|| format!("failed to load fleet config at {}", config_path.display()))?;

    // Resolution order: --nats-url > $NATS_URL > first
    // `telemetry.endpoints[].nats_url` in the loaded fleet config >
    // built-in `nats://localhost:4222`. The telemetry endpoint is
    // the only place an `agent.yml` carries a NATS URL today (the
    // `orchestrators[].url` field is HTTP, used for credential
    // discovery — different transport).
    let resolved_nats_url = nats_url.map(|s| s.to_string()).unwrap_or_else(|| {
        if let Ok(v) = std::env::var("NATS_URL")
            && !v.is_empty()
        {
            return v;
        }
        if let Some(url) = fleet
            .telemetry
            .endpoints
            .iter()
            .find_map(|e| e.nats_url.clone())
            .filter(|u| !u.is_empty())
        {
            return url;
        }
        default_nats_url()
    });

    // The cancellation token threads through serve_fleet →
    // MultiAgentRunner → each worker task. Without it, the
    // `tokio::select!` shutdown path below would just drop the
    // runner future — and tokio tasks spawned INSIDE that future
    // (one per agent) would detach and keep running. With the
    // token, the shutdown branch calls `.cancel()` and the
    // runner aborts every worker before returning. CR flagged
    // this as the major finding on PR #13.
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
            // Give workers a brief window to drop their NATS
            // connections cleanly. The select! will then drop the
            // runner future; with the cancel token already fired,
            // the runner's own cancel-aware loop has by then
            // either completed or is mid-abort — both are safe.
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            Ok(())
        }
    }
}

/// Default NATS URL when neither `--nats-url` nor `$NATS_URL` are
/// set. Picked to match the local dev orchestrator's bind address;
/// production deployments must pass `--nats-url` explicitly
/// because the orchestrator never advertises NATS over an unsealed
/// channel.
fn default_nats_url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string())
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
        // Run from a tempdir so default paths don't accidentally
        // resolve to a file the test runner has lying around.
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
    ///
    /// `#[serial(home)]` because the test mutates process-wide
    /// `$HOME`; any other test touching HOME (or that ends up
    /// reading it via XDG/dirs) must opt into the same group.
    #[test]
    #[serial_test::serial(home)]
    fn resolve_nats_auth_returns_none_when_no_creds_anywhere() {
        // Steer the default path at a tempdir that has no creds.
        let tmp = TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        // SAFETY: serialised via `#[serial(home)]`; the prev/restore
        // dance below guarantees the env var is back to its prior
        // value once this test completes.
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
}
