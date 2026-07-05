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
//! 2. The fleet config's `telemetry.endpoints[].nats_url` — `quorum init`
//!    writes the orchestrator's NATS URL there at redeem time precisely so
//!    `quorum serve` connects with no flags and no workspace (a policy-free,
//!    room-free, or absent `nsed.yaml`).
//! 3. Workspace config (`nsed.yaml`) → resolve the room → look up the
//!    orchestrator entry → `mode: embedded` reads `nats_url`
//!    directly; `mode: remote` calls `GET /api/runtime/nats`.
//! 4. Hard error otherwise. **No localhost fallback.**
//!
//! The error path names the workspace path tried, the room resolved,
//! and the orchestrator address — so an operator can fix the missing
//! config without guessing.

use crate::cli::remote::{AgentInfo, RemoteError, RemoteOrchestrator};
use crate::cli::workspace::{OrchestratorMode, PolicyConfig, QuorumConfig, WorkspaceConfig};
use crate::nats_utils::NatsAuth;
use crate::serve::{ServeOptions, serve_fleet};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Delay before the post-boot registration self-check fires. Long enough
/// for the first heartbeat (~15s interval) to reach the orchestrator and
/// be processed, so a healthy agent isn't flagged as merely not-yet-seen.
const SELFCHECK_DELAY_SECS: u64 = 20;

/// Verdict of comparing a locally-configured agent against what the
/// orchestrator actually reports at `GET /agents`. Surfaces the silent
/// attribution failures an agent can't otherwise observe: the heartbeat
/// is fire-and-forget, so a server-side drop or a blank operator never
/// reaches `quorum serve` without this explicit read-back.
#[derive(Debug, PartialEq, Eq)]
enum RegVerdict {
    /// Visible, attributed to a real operator with at least one tag.
    Ok,
    /// Not in `GET /agents` — heartbeat dropped (no operator link, the
    /// orchestrator invariant) or not registered yet. Receives no jobs.
    Dropped,
    /// Registered but `operator` is empty/null — fails grant-based eligibility.
    Unattributed,
    /// `operator == "local"` — the agent code was minted without an
    /// `operator_name`, so redeem used the `local` fallback. No grants/tags.
    LocalFallback,
    /// Operator set but has no tags — fails grant-based room eligibility.
    NoOperatorTags,
}

/// Pure comparison of a configured agent id against the orchestrator's
/// reported agent list. Network-free so it can be unit-tested.
fn evaluate_registration(agent_id: &str, agents: &[AgentInfo]) -> RegVerdict {
    let Some(a) = agents.iter().find(|a| a.agent_id == agent_id) else {
        return RegVerdict::Dropped;
    };
    match a.operator.as_deref() {
        None | Some("") => RegVerdict::Unattributed,
        Some("local") => RegVerdict::LocalFallback,
        Some(_) if a.operator_tags.is_empty() => RegVerdict::NoOperatorTags,
        Some(_) => RegVerdict::Ok,
    }
}

/// Query the orchestrator and log a verdict per configured agent. Errors /
/// warnings here are the agent-side surfacing of attribution failures the
/// fire-and-forget heartbeat hides. Best-effort: a failed query is logged
/// and skipped, never fatal.
async fn run_registration_selfcheck(orch: &RemoteOrchestrator, agent_ids: &[String]) {
    let agents = match orch.agents().await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "registration self-check skipped: GET /agents failed");
            return;
        }
    };
    for id in agent_ids {
        match evaluate_registration(id, &agents) {
            RegVerdict::Ok => {
                tracing::info!(agent_id = %id, "registered and attributed at orchestrator")
            }
            RegVerdict::Dropped => tracing::error!(
                agent_id = %id,
                "NOT visible at orchestrator after {SELFCHECK_DELAY_SECS}s — heartbeat dropped \
                 (no operator link) or not registered. This agent will receive no jobs. \
                 Re-redeem the agent code with operator_name set."
            ),
            RegVerdict::Unattributed => tracing::error!(
                agent_id = %id,
                "registered but has NO operator at orchestrator — will fail grant-based \
                 eligibility and receive no jobs"
            ),
            RegVerdict::LocalFallback => tracing::warn!(
                agent_id = %id,
                "operator is `local` at orchestrator — the agent code was minted without \
                 operator_name, so it has no grants/tags and will fail eligibility. \
                 Re-mint + redeem the agent code with operator_name set."
            ),
            RegVerdict::NoOperatorTags => tracing::warn!(
                agent_id = %id,
                "operator set but has no tags at orchestrator — will fail grant-based \
                 room eligibility"
            ),
        }
    }
}

/// Best-effort build of an HTTP orchestrator client from the workspace +
/// room, for the post-boot self-check. Shares [`resolve_remote_orchestrator`],
/// so it inherits the same `~/.nsed` address/token fallback. Returns `None`
/// when the orchestrator isn't a reachable remote — embedded mode, a
/// `--nats-url` run with no workspace, or no address/token resolvable from
/// config or `~/.nsed`. The self-check is a diagnostic, never a hard dependency.
fn try_build_orchestrator_client(
    workspace_path: &Path,
    room_flag: Option<&str>,
) -> Option<RemoteOrchestrator> {
    let (address, token) = resolve_remote_orchestrator(workspace_path, room_flag)?;
    RemoteOrchestrator::new(&address, &token).ok()
}

/// Resolve `(orchestrator HTTP url, operator bearer token)` from the workspace
/// for a remote orchestrator. The token is the workspace orchestrator entry's
/// `token` (the operator token from `quorum redeem`/`init --invite`), which
/// carries `manage_agents` and is what attributes agents to the operator.
/// `None` for embedded / no-workspace / missing-field runs.
pub(crate) fn resolve_remote_orchestrator(
    workspace_path: &Path,
    room_flag: Option<&str>,
) -> Option<(String, String)> {
    let workspace = QuorumConfig::load_workspace(workspace_path).ok()?;
    let (_room_name, room) = workspace.resolve_room(room_flag).ok()?;
    let orch_name = room.orchestrator.as_deref()?;
    let orch = workspace.orchestrators.get(orch_name)?;
    match orch.mode.as_ref() {
        Some(OrchestratorMode::Remote) | None => {
            let address =
                nonblank(orch.address.as_deref()).or_else(crate::cli::endpoint::nsed_endpoint)?;
            let token = orch
                .token
                .as_deref()
                .map(|raw| crate::config::resolve_env_token("token", raw))
                .and_then(|resolved| nonblank(Some(&resolved)))
                .or_else(crate::cli::endpoint::nsed_operator_token)?;
            Some((address, token))
        }
        Some(OrchestratorMode::Embedded) => None,
    }
}

/// Trim `s`, returning an owned copy only when non-empty. Blank/missing config
/// fields (`address: ""`, absent `token:`) collapse to `None` so remote
/// resolution can fall back to the redeemed `~/.nsed` endpoint/token.
fn nonblank(s: Option<&str>) -> Option<String> {
    let trimmed = s?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Role-based policies from the workspace — the set `serve` registers at
/// boot. Static agent-list policies dispatch by name and need no content-hash
/// push (matching `quorum run`, which only pushes role-based policies).
fn policies_to_register(workspace: &WorkspaceConfig) -> Vec<(&String, &PolicyConfig)> {
    workspace
        .policies
        .iter()
        .filter(|(_, p)| p.roles.is_some())
        .collect()
}

/// Push every role-based workspace policy to the orchestrator so its content
/// hash is known before any request (including OpenAI-compat `nsed:<tag>`
/// model names) references it — otherwise the orchestrator 404s until someone
/// runs `quorum run` for that policy. Idempotent (push is keyed by hash) and
/// best-effort: a failed push is logged, never fatal to serving.
async fn register_workspace_policies(orch: &RemoteOrchestrator, workspace_path: &Path) {
    let workspace = match QuorumConfig::load_workspace(workspace_path) {
        Ok(w) => w,
        Err(_) => return,
    };
    for (name, policy) in policies_to_register(&workspace) {
        match orch.push_policy(name, policy).await {
            Ok(r) => tracing::info!(
                policy = %name,
                policy_id = %r.policy_id,
                created = r.created,
                "registered workspace policy"
            ),
            Err(e) => tracing::warn!(
                policy = %name,
                error = %e,
                "failed to register workspace policy (will 404 until pushed)"
            ),
        }
    }
}

/// Register every fleet agent with the orchestrator under the operator bearer.
///
/// This is what makes operator≠agent work with no manual step: the operator
/// token (`manage_agents`) registers each differently-named agent via
/// `/credentials/register`, which mints per-agent scoped NATS creds AND records
/// `set_agent_operator(agent_id, operator)` server-side. Runs every boot
/// (idempotent). Returns a map of agent name → its registered connection;
/// agents missing from the map fall back to the shared connection.
async fn register_fleet_agents(
    orch_url: &str,
    bearer: &str,
    fleet: &crate::config::AgentFleetConfig,
    agent_filter: Option<&[String]>,
) -> std::collections::HashMap<String, crate::serve::AgentConn> {
    let mut map = std::collections::HashMap::new();
    let names: Vec<String> = fleet
        .agents
        .iter()
        .filter(|a| agent_filter.is_none_or(|f| f.iter().any(|n| n == &a.name)))
        .map(|a| a.name.clone())
        .collect();
    if bearer.trim().is_empty() {
        tracing::error!(
            "no operator token resolved for agent registration — agents will be unattributed \
             and DROPPED by the orchestrator. Set the orchestrator `token` in your workspace \
             (the operator token from `quorum redeem` / `init --invite`)."
        );
        return map;
    }
    for name in names {
        match crate::nats_utils::register_with_orchestrator_with_retry(orch_url, &name, bearer, 5)
            .await
        {
            Ok(reg) => {
                tracing::info!(agent = %name, "registered + attributed under operator");
                map.insert(
                    name,
                    crate::serve::AgentConn {
                        nats_url: reg.nats_url,
                        nats_auth: Some(NatsAuth {
                            inline_creds: Some(reg.creds),
                            ..Default::default()
                        }),
                    },
                );
            }
            Err(e) => {
                tracing::error!(
                    agent = %name,
                    error = %e,
                    "agent registration FAILED — this agent will be unattributed and DROPPED. \
                     Check the operator token carries the `manage_agents` role."
                );
            }
        }
    }
    map
}

/// Default fleet-config search order — keep aligned with the layout
/// `quorum init` writes and the layout `nsed serve` consumes in the
/// parent repo. If `--config` isn't passed, the CLI walks this list
/// and uses the first match.
const DEFAULT_FLEET_PATHS: &[&str] = &[
    "quorum.yml",
    "quorum.yaml",
    "agent.yml",
    "config/agent.yml",
    "config/default.yml",
];

/// Load the agent fleet from `path`, accepting EITHER a unified `quorum.yml`
/// (workspace + fleet in one file) or a legacy `agent.yml`. The unified parse
/// is tried first; a legacy `agent.yml` (whose `orchestrators:` is a list, not
/// a map) fails it and falls through to [`crate::config::load_config`].
pub(crate) fn load_fleet_unified(path: &Path) -> Result<crate::config::AgentFleetConfig> {
    match QuorumConfig::load(path) {
        Ok(q) => Ok(q.to_fleet()),
        Err(_) => crate::config::load_config(path),
    }
}

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
    fleet_nats_url: Option<&str>,
    workspace_path: &Path,
    room_flag: Option<&str>,
) -> Result<String> {
    if let Some(u) = nats_url_flag {
        return Ok(u.to_string());
    }

    // The fleet config's telemetry endpoint carries the NATS URL `quorum
    // init` captured at redeem time — prefer it so `quorum serve` needs no
    // --nats-url and no loadable workspace (e.g. a policy-free, room-free
    // workspace, or none at all).
    if let Some(u) = fleet_nats_url {
        return Ok(u.to_string());
    }

    let workspace = QuorumConfig::load_workspace(workspace_path).with_context(|| {
        format!(
            "no --nats-url passed, no telemetry NATS URL in the fleet config, and workspace \
             config not loadable at {}",
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
    // Unified single-file (`quorum.yml`) carries BOTH the fleet and the
    // workspace. When `config_path` is unified, it doubles as the workspace so
    // operators never pass two files; otherwise the legacy split applies
    // (agent.yml fleet + nsed.yaml workspace).
    let is_unified = QuorumConfig::load(&config_path).is_ok();
    let fleet = load_fleet_unified(&config_path)
        .with_context(|| format!("failed to load fleet config at {}", config_path.display()))?;
    let effective_workspace: &Path = if is_unified {
        config_path.as_path()
    } else {
        workspace_path
    };

    let fleet_nats_url = fleet
        .telemetry
        .endpoints
        .iter()
        .find_map(|e| e.nats_url.as_deref());
    let resolved_nats_url =
        resolve_nats_url(nats_url, fleet_nats_url, effective_workspace, room).await?;
    tracing::info!(nats_url = %resolved_nats_url, "resolved NATS URL");

    // Auto-register each agent under the operator token so operator≠agent
    // works with no manual step: this mints per-agent scoped creds AND records
    // the agent→operator link server-side, every boot. Only runs against a
    // remote orchestrator with a resolvable operator token; otherwise serve
    // falls back to the shared `--nats-creds` connection (existing behaviour).
    let agent_auth = match resolve_remote_orchestrator(effective_workspace, room) {
        Some((orch_url, bearer)) => {
            register_fleet_agents(&orch_url, &bearer, &fleet, agent_filter).await
        }
        None => std::collections::HashMap::new(),
    };

    // Register the workspace's role-based policies at boot so OpenAI-compat
    // model names (`nsed:<tag>`) and `--policy` runs resolve without a separate
    // `quorum run`. Best-effort against a reachable remote orchestrator.
    if let Some(orch) = try_build_orchestrator_client(effective_workspace, room) {
        register_workspace_policies(&orch, effective_workspace).await;
    }

    let cancel = tokio_util::sync::CancellationToken::new();

    let opts = ServeOptions {
        nats_url: resolved_nats_url,
        agent_auth,
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
        registry: None,
    };

    // Post-boot registration self-check: a heartbeat is fire-and-forget, so a
    // server-side drop (no operator link → orchestrator invariant) or a blank
    // operator never reaches this process. Read it back from `GET /agents`
    // once heartbeats have had time to land, and log a per-agent verdict.
    // Best-effort: only runs when the orchestrator is a reachable remote.
    if let Some(orch) = try_build_orchestrator_client(effective_workspace, room) {
        let selected: Vec<String> = fleet
            .agents
            .iter()
            .filter(|a| agent_filter.is_none_or(|f| f.iter().any(|n| n == &a.name)))
            .map(|a| a.name.clone())
            .collect();
        let cancel_sc = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(SELFCHECK_DELAY_SECS)) => {
                    run_registration_selfcheck(&orch, &selected).await;
                }
                _ = cancel_sc.cancelled() => {}
            }
        });
    }

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
    #[serial_test::serial(home_env)]
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
        let resolved = resolve_nats_url(Some("nats://explicit:4222"), None, &missing_ws, None)
            .await
            .expect("explicit URL must short-circuit workspace lookup");
        assert_eq!(resolved, "nats://explicit:4222");
    }

    /// Fleet telemetry NATS URL is used when no flag is passed — without
    /// touching the workspace (so a missing/room-free workspace is fine).
    #[tokio::test]
    async fn resolve_nats_url_fleet_telemetry_used_before_workspace() {
        let tmp = TempDir::new().unwrap();
        let missing_ws = tmp.path().join("nsed.yaml");
        let resolved =
            resolve_nats_url(None, Some("nats://from-telemetry:4222"), &missing_ws, None)
                .await
                .expect("fleet telemetry URL must be used without a loadable workspace");
        assert_eq!(resolved, "nats://from-telemetry:4222");
    }

    /// Explicit flag still wins over fleet telemetry.
    #[tokio::test]
    async fn resolve_nats_url_flag_beats_fleet_telemetry() {
        let tmp = TempDir::new().unwrap();
        let missing_ws = tmp.path().join("nsed.yaml");
        let resolved = resolve_nats_url(
            Some("nats://explicit:4222"),
            Some("nats://from-telemetry:4222"),
            &missing_ws,
            None,
        )
        .await
        .unwrap();
        assert_eq!(resolved, "nats://explicit:4222");
    }

    /// Workspace missing AND no `--nats-url` → structured error
    /// naming the workspace path. No localhost fallback.
    #[tokio::test]
    async fn resolve_nats_url_fails_loud_when_workspace_missing() {
        let tmp = TempDir::new().unwrap();
        let missing_ws = tmp.path().join("nsed.yaml");
        let err = resolve_nats_url(None, None, &missing_ws, None)
            .await
            .unwrap_err();
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
        let resolved = resolve_nats_url(None, None, &ws_path, None).await.unwrap();
        assert_eq!(resolved, "nats://embedded-host:4222");
    }

    fn agent_info(id: &str, operator: Option<&str>, tags: &[&str]) -> AgentInfo {
        AgentInfo {
            agent_id: id.to_string(),
            operator: operator.map(String::from),
            operator_tags: tags.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_remote_orchestrator_returns_address_and_token() {
        let tmpdir = TempDir::new().unwrap();
        let ws_path = tmpdir.path().join("nsed.yaml");
        std::fs::write(
            &ws_path,
            r#"
orchestrators:
  prod:
    mode: remote
    address: "https://api.example.com"
    token: "op-bearer-xyz"
policies:
  default:
    agents: [justindgx, justindgy]
rooms:
  main:
    policy: default
    orchestrator: prod
default_room: main
"#,
        )
        .unwrap();
        let (url, token) = resolve_remote_orchestrator(&ws_path, None).expect("remote resolves");
        assert_eq!(url, "https://api.example.com");
        assert_eq!(token, "op-bearer-xyz");
    }

    #[test]
    fn resolve_remote_orchestrator_none_for_embedded() {
        let tmpdir = TempDir::new().unwrap();
        let ws_path = tmpdir.path().join("nsed.yaml");
        std::fs::write(
            &ws_path,
            r#"
orchestrators:
  local:
    mode: embedded
    nats_url: "nats://x:4222"
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
        // Loads cleanly; None specifically because the orchestrator is embedded.
        assert!(resolve_remote_orchestrator(&ws_path, None).is_none());
    }

    /// Blank `address` + absent `token` → both inherited from the redeemed
    /// `~/.nsed/{orchestrator,operator.token}`, so a config file need only
    /// name the orchestrator to reach it.
    #[test]
    #[serial_test::serial(home_env)]
    fn resolve_remote_orchestrator_blank_fields_fall_back_to_nsed() {
        let home = TempDir::new().unwrap();
        let nsed = home.path().join(".nsed");
        std::fs::create_dir_all(&nsed).unwrap();
        std::fs::write(nsed.join("orchestrator"), "https://home-orch\n").unwrap();
        std::fs::write(nsed.join("operator.token"), "home-bearer\n").unwrap();

        let ws = TempDir::new().unwrap();
        let ws_path = ws.path().join("nsed.yaml");
        std::fs::write(
            &ws_path,
            r#"
orchestrators:
  prod:
    mode: remote
    address: ""
policies:
  default:
    agents: [a, b]
rooms:
  main:
    policy: default
    orchestrator: prod
default_room: main
"#,
        )
        .unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_env = std::env::var_os("QUORUM_ORCHESTRATOR");
        // SAFETY: serialised via `#[serial(home)]`.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::remove_var("QUORUM_ORCHESTRATOR");
        }
        let resolved = resolve_remote_orchestrator(&ws_path, None);
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_env {
                Some(v) => std::env::set_var("QUORUM_ORCHESTRATOR", v),
                None => std::env::remove_var("QUORUM_ORCHESTRATOR"),
            }
        }
        assert_eq!(
            resolved,
            Some(("https://home-orch".into(), "home-bearer".into()))
        );
    }

    /// An `${UNSET}` token reference resolving to empty also falls back to the
    /// redeemed operator token, while a literal `address` still wins.
    #[test]
    #[serial_test::serial(home_env)]
    fn resolve_remote_orchestrator_unset_env_token_falls_back_to_nsed() {
        let home = TempDir::new().unwrap();
        let nsed = home.path().join(".nsed");
        std::fs::create_dir_all(&nsed).unwrap();
        std::fs::write(nsed.join("operator.token"), "home-bearer\n").unwrap();

        let ws = TempDir::new().unwrap();
        let ws_path = ws.path().join("nsed.yaml");
        std::fs::write(
            &ws_path,
            r#"
orchestrators:
  prod:
    mode: remote
    address: "https://cfg-orch"
    token: "${QUORUM_TEST_TOKEN_UNSET_XYZ}"
policies:
  default:
    agents: [a, b]
rooms:
  main:
    policy: default
    orchestrator: prod
default_room: main
"#,
        )
        .unwrap();

        let prev_home = std::env::var_os("HOME");
        // SAFETY: serialised via `#[serial(home)]`.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::remove_var("QUORUM_TEST_TOKEN_UNSET_XYZ");
        }
        let resolved = resolve_remote_orchestrator(&ws_path, None);
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        assert_eq!(
            resolved,
            Some(("https://cfg-orch".into(), "home-bearer".into()))
        );
    }

    #[test]
    fn load_fleet_unified_reads_quorum_yml_and_legacy_agent_yml() {
        let tmpdir = TempDir::new().unwrap();
        // Unified quorum.yml: workspace + fleet in one file.
        let q = tmpdir.path().join("quorum.yml");
        std::fs::write(
            &q,
            r#"
orchestrators:
  prod: { mode: remote, address: "https://x", token: "t" }
policies:
  default: { agents: [a, b] }
rooms:
  main: { policy: default, orchestrator: prod }
default_room: main
providers:
  openai: { type: openai, api_key: "${K}" }
agents:
  - name: justindgx
    provider_id: openai
    model_name: gpt-4o
"#,
        )
        .unwrap();
        let fleet = load_fleet_unified(&q).expect("unified fleet");
        assert_eq!(fleet.agents.len(), 1);
        assert_eq!(fleet.agents[0].name, "justindgx");
        assert!(fleet.providers.contains_key("openai"));

        // Legacy agent.yml: orchestrators is a LIST → unified parse fails →
        // falls back to load_config.
        let a = tmpdir.path().join("agent.yml");
        std::fs::write(
            &a,
            r#"
providers:
  openai: { type: openai, api_key: "${K}" }
agents:
  - name: legacy-bot
    provider_id: openai
    model_name: gpt-4o
orchestrators:
  - url: "https://x"
    bearer_token: "t"
"#,
        )
        .unwrap();
        let fleet = load_fleet_unified(&a).expect("legacy fleet");
        assert_eq!(fleet.agents.len(), 1);
        assert_eq!(fleet.agents[0].name, "legacy-bot");
    }

    #[tokio::test]
    async fn register_fleet_agents_empty_bearer_returns_empty_no_network() {
        // Empty operator token → no registration attempted (would 403); the map
        // is empty and the agents fall back to the shared connection. No network.
        let fleet: crate::config::AgentFleetConfig = serde_yaml::from_str("agents: []\n").unwrap();
        let map = register_fleet_agents("https://unused.example", "", &fleet, None).await;
        assert!(map.is_empty());
    }

    #[test]
    fn selfcheck_ok_when_operator_and_tags_present() {
        let agents = vec![agent_info(
            "justindgx",
            Some("dgx-spark-justin"),
            &["noosphera:x"],
        )];
        assert_eq!(evaluate_registration("justindgx", &agents), RegVerdict::Ok);
    }

    #[test]
    fn selfcheck_dropped_when_absent_from_orchestrator() {
        let agents = vec![agent_info("other", Some("op"), &["t"])];
        assert_eq!(
            evaluate_registration("justindgx", &agents),
            RegVerdict::Dropped
        );
    }

    #[test]
    fn selfcheck_unattributed_when_operator_missing_or_empty() {
        let none = vec![agent_info("a", None, &[])];
        assert_eq!(evaluate_registration("a", &none), RegVerdict::Unattributed);
        let empty = vec![agent_info("a", Some(""), &[])];
        assert_eq!(evaluate_registration("a", &empty), RegVerdict::Unattributed);
    }

    #[test]
    fn selfcheck_local_fallback_when_operator_is_local() {
        let agents = vec![agent_info("a", Some("local"), &[])];
        assert_eq!(
            evaluate_registration("a", &agents),
            RegVerdict::LocalFallback
        );
    }

    #[test]
    fn selfcheck_no_tags_when_operator_set_but_tagless() {
        let agents = vec![agent_info("a", Some("real-op"), &[])];
        assert_eq!(
            evaluate_registration("a", &agents),
            RegVerdict::NoOperatorTags
        );
    }

    /// `serve` registers only role-based policies at boot — static agent-list
    /// policies dispatch by name and need no content-hash push (matches `run`).
    #[test]
    fn policies_to_register_selects_role_based_only() {
        let workspace_dir = TempDir::new().unwrap();
        let ws = workspace_dir.path().join("quorum.yml");
        std::fs::write(
            &ws,
            r#"
policies:
  role_based:
    roles:
      - role: r
        count: 2
        capabilities: ["x"]
  static_list:
    agents: ["a", "b"]
"#,
        )
        .unwrap();
        let workspace = QuorumConfig::load_workspace(&ws).unwrap();
        let selected: Vec<&str> = policies_to_register(&workspace)
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(selected, vec!["role_based"]);
    }
}
