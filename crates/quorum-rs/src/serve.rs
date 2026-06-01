//! `serve_fleet` — bring up multiple agents from a fleet config.
//!
//! This is the SDK analog of the proprietary `nsed serve` binary's
//! agent-fleet half: load an [`AgentFleetConfig`], construct one
//! [`NatsNsedWorker`] per agent (using `provider.provider_type` to
//! pick the right [`NsedAgent`] implementation), wire them all into
//! a [`MultiAgentRunner`], and `run().await` until SIGTERM.
//!
//! What's **out of scope** vs. the proprietary `nsed serve`:
//!
//! - **Local orchestrator boot.** `nsed serve` can start an
//!   in-process `nsed-orchestrator` from a `config_file`; that
//!   crate is proprietary. `serve_fleet` always talks to a remote
//!   orchestrator's NATS bus via creds the operator already
//!   redeemed (typically via `quorum redeem`).
//! - **JWT challenge-response registration.** `nsed serve` can
//!   register each agent with each orchestrator over HTTP and
//!   receive per-agent NATS creds back. `serve_fleet` assumes the
//!   operator has already obtained `.creds` (via `quorum redeem`
//!   or otherwise) — one set of creds is used for all agents in
//!   the fleet.
//! - **Workspace policy push.** `nsed serve` writes policies to
//!   the orchestrator's policy registry on startup. `serve_fleet`
//!   assumes policies are configured server-side already.
//!
//! All three of those would be welcome follow-ups but bloat the
//! MVP.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use quorum_rs::config::load_config;
//! use quorum_rs::nats_utils::NatsAuth;
//! use quorum_rs::serve::{ServeOptions, serve_fleet};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let fleet = load_config(Path::new("agent.yml"))?;
//! let opts = ServeOptions {
//!     nats_url: "nats://api.peeramid.xyz:4222".into(),
//!     nats_auth: Some(NatsAuth {
//!         creds_file: Some("/home/me/.nsed/agent.creds".into()),
//!         ..Default::default()
//!     }),
//!     ..Default::default()
//! };
//! serve_fleet(&fleet, opts).await?;
//! # Ok(())
//! # }
//! ```

use crate::agents::ProposerEvaluatorAgent;
use crate::agents::config::AgentConfig;
use crate::agents::exec_agent::ExecAgent;
use crate::agents::mcp_agent::{ClaudeAgent, McpAgent};
use crate::config::{AgentFleetConfig, load_agent_from_config, resolve_agent_names};
use crate::llms::OpenAICompatibleModel;
use crate::multi_agent::MultiAgentRunner;
use crate::nats_utils::NatsAuth;
use crate::prompts::defaults::DefaultPromptSet;
use crate::workers::{NatsNsedWorker, WorkerConfig};
use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{info, warn};

/// Runtime knobs for [`serve_fleet`] that aren't sourced from the
/// fleet YAML. Defaults connect to `nats://localhost:4222` with no
/// auth — fine for a local dev orchestrator, not for production.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// NATS server URL all agents connect to. Typically the URL
    /// returned by `POST /redeem` (operator code with the `agent`
    /// capability) or `POST /redeem-agent`.
    pub nats_url: String,
    /// NATS authentication — usually `creds_file` pointing at
    /// `~/.nsed/agent.creds` from `quorum redeem`. `None` for
    /// unauthenticated dev orchestrators.
    pub nats_auth: Option<NatsAuth>,
    /// Restrict to a subset of agent names from the fleet config.
    /// `None` runs all configured agents. Names are matched
    /// case-insensitively via [`resolve_agent_names`].
    pub agent_filter: Option<Vec<String>>,
    /// JetStream stream the orchestrator publishes work on.
    /// Override only if the orchestrator was deployed with a
    /// non-default stream name (`$NSED_STREAM` on the server side).
    pub stream_name: String,
    /// API subject prefix the orchestrator uses. Override if the
    /// orchestrator was deployed with `$NSED_API_PREFIX` set to a
    /// non-default value.
    pub api_prefix: String,
    /// External shutdown signal. When `cancel.cancelled()` fires,
    /// the runner aborts every worker task and returns `Ok(())`.
    /// The CLI wires this to SIGTERM / SIGINT; library consumers
    /// can clone the token and call `.cancel()` from anywhere.
    ///
    /// `None` uses an internal token that never fires — the
    /// runner only exits when workers exhaust their retry budget
    /// or complete naturally.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// LAN-visible unified dashboard port. When `Some`, starts the
    /// `MultiAgentRunner`'s dashboard control plane on this port —
    /// requires the `status-server` feature compiled in. Overrides
    /// `AgentFleetConfig::dashboard_port`; when both are `None`, no
    /// dashboard is started.
    pub dashboard_port: Option<u16>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            nats_url: "nats://localhost:4222".to_string(),
            nats_auth: None,
            agent_filter: None,
            stream_name: "sphera_jobs".to_string(),
            api_prefix: "sphera".to_string(),
            cancel: None,
            dashboard_port: None,
        }
    }
}

/// Scrub a NATS URL so we can log it without leaking userinfo
/// (`nats://user:password@host:port` → `nats://<redacted>@host:port`).
/// A minimal hand-rolled scrub avoids pulling in the `url` crate
/// just for one log line. Untouched if the URL has no `@`.
fn redact_userinfo(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some(pair) => pair,
        None => return url.to_string(),
    };
    // Find an `@` that occurs BEFORE the first `/` (or end-of-string).
    // An `@` inside the path doesn't carry userinfo.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    match authority.rfind('@') {
        Some(_) => match authority.rsplit_once('@') {
            Some((_userinfo, host)) => format!("{scheme}://<redacted>@{host}{path}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// Build a [`NatsNsedWorker`] for one agent + provider entry,
/// dispatching on `provider.provider_type`. Each provider type
/// constructs a different [`NsedAgent`] impl:
///
/// | `provider_type` | Agent impl | What it does |
/// |---|---|---|
/// | `openai_compat` (or any LLM provider) | [`ProposerEvaluatorAgent`] + [`OpenAICompatibleModel`] | Native ReAct loop driven by an OpenAI-compatible HTTP API |
/// | `exec` | [`ExecAgent`] | Subprocess agent, stdin/stdout-framed (any language) |
/// | `mcp` | [`McpAgent`] | Subprocess agent over the MCP wire protocol |
/// | `claude` | [`ClaudeAgent`] | Claude CLI as the agent runtime |
///
/// Returns `Ok(None)` for provider types this build doesn't yet
/// support (and logs a warning) — keeps the fleet from refusing to
/// boot when ONE agent's config is unsupported.
pub async fn build_worker(
    fleet: &AgentFleetConfig,
    agent_name: &str,
    nats_url: &str,
    nats_auth: Option<&NatsAuth>,
    stream_name: &str,
    api_prefix: &str,
) -> Result<Option<(NatsNsedWorker, AgentConfig)>> {
    let (agent_config, provider) = load_agent_from_config(fleet, agent_name)
        .with_context(|| format!("failed to load agent '{agent_name}' from fleet config"))?;

    let consumer_name = format!("agent_{}", agent_config.name);
    let mut worker_config =
        WorkerConfig::new(nats_url.to_string(), stream_name.to_string(), consumer_name)
            .with_api_prefix(api_prefix.to_string());
    if let Some(auth) = nats_auth {
        worker_config = worker_config.with_nats_auth(auth.clone());
    }

    let agent_name_owned = agent_config.name.clone();
    let worker = match provider.provider_type.as_str() {
        "exec" => {
            let exec_cfg = match agent_config.exec.clone() {
                Some(c) => c,
                None => {
                    warn!(
                        agent = %agent_name_owned,
                        "provider_type=exec but no `exec` section in agent config — skipping"
                    );
                    return Ok(None);
                }
            };
            let agent = ExecAgent::new(agent_name_owned.clone(), exec_cfg);
            NatsNsedWorker::new(agent, agent_config.clone(), worker_config, None).await?
        }
        "mcp" => {
            let mcp_cfg = match agent_config.mcp.clone() {
                Some(c) => c,
                None => {
                    warn!(
                        agent = %agent_name_owned,
                        "provider_type=mcp but no `mcp` section in agent config — skipping"
                    );
                    return Ok(None);
                }
            };
            let agent = McpAgent::new(agent_name_owned.clone(), mcp_cfg);
            NatsNsedWorker::new(agent, agent_config.clone(), worker_config, None).await?
        }
        "claude" => {
            let claude_cfg = agent_config.claude.clone().unwrap_or_default();
            let agent = ClaudeAgent::new(
                agent_config.clone(),
                claude_cfg,
                Arc::new(DefaultPromptSet::new()),
            );
            NatsNsedWorker::new(agent, agent_config.clone(), worker_config, None).await?
        }
        // Other LLM provider types ("openai", "ollama", "simulated",
        // or any other type that's known to speak the OpenAI wire
        // format) go through the OpenAI-compatible client. Anything
        // truly unrecognised (typo in `type`, a custom provider not
        // yet supported by this dispatch) is SKIPPED with an error
        // log rather than silently routed to OpenAI — without this
        // guard a typoed `type: oepnai` would leak the agent's API
        // key to api.openai.com because of the
        // "default base_url = openai" fallback below.
        other if other == "openai" || other == "ollama" || other == "simulated" => {
            let base_url = if provider.base_url.is_empty() {
                if provider.provider_type == "openai" {
                    "https://api.openai.com/v1".to_string()
                } else {
                    warn!(
                        agent = %agent_name_owned,
                        provider_type = %other,
                        "no `base_url` set for non-openai provider type — skipping"
                    );
                    return Ok(None);
                }
            } else {
                provider.base_url.clone()
            };
            let api_key = provider.api_key.clone();
            let llm = OpenAICompatibleModel::new(base_url, api_key, provider.engine.clone());
            let agent = ProposerEvaluatorAgent::new(
                agent_config.clone(),
                Box::new(llm),
                Box::new(DefaultPromptSet::new()),
                vec![],
                vec![],
            );
            NatsNsedWorker::new(agent, agent_config.clone(), worker_config, None).await?
        }
        other => {
            warn!(
                agent = %agent_name_owned,
                provider_type = %other,
                "unknown provider_type — skipping. Supported types: openai, ollama, simulated, exec, mcp, claude"
            );
            return Ok(None);
        }
    };

    Ok(Some((worker, agent_config)))
}

/// Bring up every agent in `fleet` (or every name in
/// `opts.agent_filter`), wire them into a [`MultiAgentRunner`], and
/// run until the runner exits or the process is signalled.
///
/// Returns when the runner returns. The caller is responsible for
/// trapping SIGTERM / SIGINT and propagating shutdown — the SDK
/// stays free of signal handling so library consumers can integrate
/// with whatever runtime they already use (tokio, async-std,
/// systemd, etc.). For a CLI binary, see
/// `quorum_rs::cli::commands::serve::run` for the
/// signal-handling wrapper.
/// Resolve the dashboard port from the CLI flag and the fleet config.
///
/// CLI flag wins so operators can override what they get from the
/// committed `agent.yml` (e.g. a CI run wanting to bind to a fixed
/// port without re-rendering the yaml).
fn resolve_dashboard_port(opt_port: Option<u16>, fleet_port: Option<u16>) -> Option<u16> {
    opt_port.or(fleet_port)
}

pub async fn serve_fleet(fleet: &AgentFleetConfig, opts: ServeOptions) -> Result<()> {
    let filter = opts
        .agent_filter
        .as_ref()
        .map(|v| v.join(","))
        .unwrap_or_else(|| "ALL".to_string());
    let names = resolve_agent_names(&filter, fleet);
    if names.is_empty() {
        anyhow::bail!(
            "no agents to run — fleet config has {} agents but `agent_filter` matched none",
            fleet.agents.len()
        );
    }
    info!(
        agent_count = names.len(),
        nats_url = %redact_userinfo(&opts.nats_url),
        "starting fleet"
    );

    let mut runner = MultiAgentRunner::new();

    // Dashboard wiring: CLI flag (opts) wins over fleet config.
    // The `enable_dashboard` call is a no-op without the
    // `status-server` feature — the inner spawn is feature-gated
    // inside MultiAgentRunner::run — but a warn-log here makes the
    // no-op visible to operators who set a port and see no dashboard.
    if let Some(port) = resolve_dashboard_port(opts.dashboard_port, fleet.dashboard_port) {
        #[cfg(feature = "status-server")]
        {
            info!(dashboard_port = port, "enabling unified dashboard");
            runner.enable_dashboard(port);
        }
        #[cfg(not(feature = "status-server"))]
        {
            tracing::warn!(
                dashboard_port = port,
                "dashboard_port set but `status-server` feature not compiled in — no dashboard will start. \
                 Rebuild with `--features status-server` (or run a build that has it in `default`)."
            );
            let _ = port;
        }
    }

    for name in &names {
        match build_worker(
            fleet,
            name,
            &opts.nats_url,
            opts.nats_auth.as_ref(),
            &opts.stream_name,
            &opts.api_prefix,
        )
        .await
        {
            Ok(Some((worker, agent_config))) => {
                info!(agent = %name, "agent ready");
                runner.add_worker(name.clone(), worker, agent_config);
            }
            Ok(None) => {
                // Already warned inside build_worker — provider
                // type unsupported or missing section.
            }
            Err(e) => {
                warn!(agent = %name, "failed to build agent: {e:#}, skipping");
            }
        }
    }

    if runner.is_empty() {
        anyhow::bail!(
            "no agents successfully started from fleet config (every entry failed or was skipped)"
        );
    }

    let cancel = opts.cancel.unwrap_or_default();
    runner.run_with_cancellation(cancel).await
}

/// Suggest a useful tracing subscriber for the CLI wrapper. Library
/// users typically have their own subscriber configured; this is
/// extracted so the CLI command and any user binary that wants
/// matching log output can share one definition.
#[doc(hidden)]
pub fn install_default_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,quorum_rs=info,async_nats=warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderEntry;

    /// `AgentFleetConfig` doesn't `derive(Default)`. Building one in
    /// the tests via YAML deserialization keeps the fixture
    /// independent of any field additions on the real struct — a
    /// new field with `#[serde(default)]` just slots in.
    fn fleet_yaml(s: &str) -> AgentFleetConfig {
        serde_yaml::from_str(s).expect("fleet yaml must parse")
    }

    /// Filter naming a nonexistent agent → clear "no agents
    /// successfully started" error. `resolve_agent_names` returns
    /// the literal filter name even when it doesn't match; the
    /// downstream `load_agent_from_config` catches it.
    #[tokio::test]
    async fn serve_fleet_rejects_filter_with_unknown_agent() {
        let fleet = fleet_yaml(
            r#"
providers:
  openai:
    type: openai
    base_url: "http://localhost:9999/v1"
    api_key: "sk-test"
agents:
  - name: cortex-a
    provider_id: openai
    model_name: gpt-4o
"#,
        );
        let opts = ServeOptions {
            agent_filter: Some(vec!["does-not-exist".to_string()]),
            ..Default::default()
        };
        let err = serve_fleet(&fleet, opts).await.unwrap_err();
        assert!(
            err.to_string().contains("no agents successfully started"),
            "must surface no-buildable-agents error; got: {err}"
        );
    }

    /// Empty fleet (zero agents) → clear error.
    #[tokio::test]
    async fn serve_fleet_rejects_empty_fleet() {
        let fleet = fleet_yaml("providers: {}\nagents: []\n");
        let err = serve_fleet(&fleet, ServeOptions::default())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no agents to run"),
            "must surface empty-fleet error; got: {err}"
        );
    }

    /// Default options target a local dev orchestrator with no
    /// auth — must be overridden for production.
    #[test]
    fn default_serve_options_target_localhost() {
        let opts = ServeOptions::default();
        assert_eq!(opts.nats_url, "nats://localhost:4222");
        assert!(opts.nats_auth.is_none());
        assert!(opts.agent_filter.is_none());
        assert_eq!(opts.stream_name, "sphera_jobs");
        assert_eq!(opts.api_prefix, "sphera");
    }

    /// `build_worker` short-circuits an `exec` provider that's
    /// missing its `exec:` config block — returns `Ok(None)` BEFORE
    /// any NATS connect, so we use an unbindable URL
    /// (`nats://localhost:0`) to prove the dispatch checks config
    /// shape first.
    #[tokio::test]
    async fn build_worker_skips_exec_without_exec_section() {
        let fleet = fleet_yaml(
            r#"
providers:
  exec_local:
    type: exec
agents:
  - name: broken
    provider_id: exec_local
    model_name: custom
"#,
        );
        let result = build_worker(
            &fleet,
            "broken",
            "nats://localhost:0",
            None,
            "sphera_jobs",
            "sphera",
        )
        .await;
        assert!(
            matches!(result, Ok(None)),
            "exec provider with no exec section must skip cleanly (Ok(None)) before NATS connect; \
             if NATS connection was attempted it would have errored on the unbindable port"
        );
    }

    /// `redact_userinfo` must scrub `user:pass@` from the
    /// authority section of a NATS URL — anything else (the path,
    /// no userinfo at all) goes through unchanged. The CR review
    /// flagged the prior startup log as leaking creds when an
    /// operator passed `nats://user:pass@host` as `--nats-url`.
    #[test]
    fn redact_userinfo_strips_credentials() {
        assert_eq!(
            redact_userinfo("nats://user:pass@example.com:4222"),
            "nats://<redacted>@example.com:4222"
        );
        assert_eq!(
            redact_userinfo("nats://token@example.com:4222"),
            "nats://<redacted>@example.com:4222"
        );
    }

    #[test]
    fn redact_userinfo_leaves_credential_free_urls_alone() {
        for url in [
            "nats://localhost:4222",
            "nats://api.peeramid.xyz:4222",
            "nats://10.0.0.1:4222",
        ] {
            assert_eq!(redact_userinfo(url), url, "{url} must round-trip");
        }
    }

    /// An `@` in the PATH must not be misread as userinfo.
    /// Edge case but the URL parser used here is hand-rolled.
    #[test]
    fn redact_userinfo_ignores_at_sign_in_path() {
        assert_eq!(
            redact_userinfo("nats://example.com:4222/some@path"),
            "nats://example.com:4222/some@path"
        );
    }

    /// Non-URL input (no `://`) passes through unchanged rather
    /// than corrupting the value — better to log "weird-looking
    /// string" than `<redacted>weird-looking-string`.
    #[test]
    fn redact_userinfo_handles_non_url_input() {
        assert_eq!(redact_userinfo("not a url"), "not a url");
        assert_eq!(redact_userinfo(""), "");
    }

    /// Cancel token plumbing — when the caller cancels the token,
    /// `serve_fleet` propagates through `run_with_cancellation`
    /// and returns Ok(()) without orphaning worker tasks. The
    /// test uses an unbindable NATS URL + an immediately-cancelled
    /// token so the runner gives up before any real connection.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn serve_fleet_honours_pre_cancelled_token() {
        let fleet = fleet_yaml(
            r#"
providers:
  exec_local:
    type: exec
agents:
  - name: noop
    provider_id: exec_local
    model_name: custom
"#,
        );
        // exec provider with no `exec:` config => build_worker
        // returns Ok(None) => runner is empty => serve_fleet
        // bails BEFORE reaching the runner. We're only proving the
        // cancel field plumbs through without panicking on
        // construction. (Real-shutdown coverage of the runner
        // itself lives in `multi_agent::tests`.)
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let opts = ServeOptions {
            cancel: Some(token),
            ..Default::default()
        };
        let err = serve_fleet(&fleet, opts).await.unwrap_err();
        assert!(
            err.to_string().contains("no agents successfully started"),
            "must bail with the empty-runner error, not a cancel error: {err}"
        );
    }

    /// Smoke test for [`ProviderEntry`] field names — pins the YAML
    /// shape the dispatch consumes against drift on the real struct.
    #[test]
    fn provider_entry_has_expected_fields() {
        let p: ProviderEntry = serde_yaml::from_str(
            r#"
type: openai
base_url: "http://localhost:9999/v1"
api_key: "sk-test"
"#,
        )
        .expect("provider yaml must parse");
        assert_eq!(p.provider_type, "openai");
        assert_eq!(p.base_url, "http://localhost:9999/v1");
        assert_eq!(p.api_key, "sk-test");
        assert!(p.models.is_empty());
    }

    #[test]
    fn resolve_dashboard_port_cli_flag_wins() {
        assert_eq!(
            super::resolve_dashboard_port(Some(8081), Some(9090)),
            Some(8081)
        );
    }

    #[test]
    fn resolve_dashboard_port_falls_back_to_fleet() {
        assert_eq!(super::resolve_dashboard_port(None, Some(9090)), Some(9090));
    }

    #[test]
    fn resolve_dashboard_port_returns_none_when_both_absent() {
        assert_eq!(super::resolve_dashboard_port(None, None), None);
    }

    /// `dashboard_port` is a top-level optional yaml field — verifies
    /// that an operator who hand-writes `dashboard_port: 8081` in
    /// `agent.yml` actually gets that value out of `AgentFleetConfig`
    /// (regression against the silent-ignore behaviour before this
    /// field existed).
    #[test]
    fn fleet_yaml_carries_dashboard_port() {
        let yaml = "providers: {}\nagents: []\ndashboard_port: 8081\n";
        let cfg: AgentFleetConfig =
            serde_yaml::from_str(yaml).expect("fleet yaml must parse with dashboard_port");
        assert_eq!(cfg.dashboard_port, Some(8081));
    }
}
