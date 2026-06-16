//! `quorum` binary entry point.
//!
//! Thin dispatch shim — clap parses the `Cli` struct below, the
//! `match cli.command` arm forwards to the corresponding
//! `quorum_rs::cli::commands::<subcommand>::run` function, and the
//! returned [`ExitCode`] becomes the process exit status. Every
//! subcommand owns its own argument plumbing, error printing, and
//! signal handling inside the SDK so library consumers can drive
//! the same flow from their own binary without depending on this
//! main.
//!
//! See [`quorum_rs::cli::commands`] for the subcommand map.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use quorum_rs::cli::commands;

#[derive(Parser)]
#[command(
    name = "quorum",
    version,
    about = "Multi-agent deliberation from the command line"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to workspace config file [default: ./nsed.yaml, then ./nsed.yml]
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a deliberation task.
    Run {
        /// The task or question to deliberate on. Mutually exclusive
        /// with `--task-file`.
        #[arg(conflicts_with = "task_file")]
        task: Option<String>,

        /// Read the task from a file. Path resolves relative to the
        /// workspace config's directory.
        #[arg(long, value_name = "PATH")]
        task_file: Option<PathBuf>,

        /// Room to use (overrides default_room in config).
        #[arg(short, long)]
        room: Option<String>,

        /// Policy ID or name (ad-hoc run, no room needed).
        #[arg(short, long)]
        policy: Option<String>,

        /// Override shared context files.
        #[arg(long, num_args = 1..)]
        files: Vec<PathBuf>,

        /// Capture verdict + prompt + dispatch.json into the given
        /// directory.
        #[arg(long, value_name = "DIR")]
        output_dir: Option<PathBuf>,

        /// Write only the verdict markdown to this path.
        #[arg(long, value_name = "PATH")]
        output_file: Option<PathBuf>,

        /// Allow `--output-dir` to overwrite an existing verdict.md.
        #[arg(long)]
        force_output: bool,

        /// Launch an interactive TUI for live deliberation view.
        #[cfg(feature = "tui")]
        #[arg(long, conflicts_with = "files")]
        tui: bool,
    },

    /// Health check and agent status.
    Status {
        /// Target a specific orchestrator by name.
        #[arg(short = 'o', long)]
        orchestrator: Option<String>,
    },

    /// Launch the interactive terminal UI.
    #[cfg(feature = "tui")]
    Tui,

    /// Display a deliberation trace.
    Trace {
        /// Job ID to display trace for.
        job_id: String,

        /// Target a specific orchestrator by name.
        #[arg(short = 'o', long)]
        orchestrator: Option<String>,

        /// Show full evaluation justifications.
        #[arg(short, long)]
        verbose: bool,
    },

    /// Bootstrap a workspace config (nsed.yaml or agent.yml).
    ///
    /// Default mode is the interactive wizard — prompts for the
    /// orchestrator, redeem flow, providers, presets, agents, and
    /// rooms; writes a fully-wired workspace from operator answers.
    ///
    /// `--non-interactive` (or any of `--agent-fleet` /
    /// `--orchestrator-url` / `--room` / `--token-env` /
    /// `--agents`, all of which imply non-interactive) drops to the
    /// one-shot template renderer for the cases where prompting
    /// isn't possible (scripts, Docker entrypoints, CI).
    Init {
        /// Skip the interactive wizard and run the one-shot
        /// template renderer. Implied when stdin is not a TTY or
        /// when any one-shot-only flag is set.
        #[arg(long)]
        non_interactive: bool,

        /// Write an `agent.yml` fleet config (consumed by `quorum
        /// serve`) instead of the client-side `nsed.yaml` (consumed
        /// by `quorum run`/`status`/`trace`/`tui`). The two YAMLs
        /// configure distinct concerns:
        ///
        /// - Without this flag → `nsed.yaml`: which orchestrator to
        ///   talk to, which room, which agents make up the room's
        ///   policy. Read when SUBMITTING tasks.
        /// - With this flag → `agent.yml`: providers (LLM endpoints
        ///   / Claude CLI / exec subprocess), agents (the
        ///   per-worker config that `quorum serve` instantiates).
        ///   Read when RUNNING agents.
        #[arg(long)]
        agent_fleet: bool,

        /// Orchestrator base URL. Only embedded in `nsed.yaml`
        /// (client config); ignored when `--agent-fleet` is set
        /// because `quorum serve` learns the NATS URL from
        /// `--nats-url` at runtime.
        #[arg(long, default_value = commands::init::DEFAULT_ORCHESTRATOR_URL)]
        orchestrator_url: String,

        /// Room name (becomes both the room key + `default_room`).
        /// Only used by the `nsed.yaml` template.
        #[arg(short, long, default_value = commands::init::DEFAULT_ROOM)]
        room: String,

        /// Env-var name the generated YAML interpolates for the
        /// bearer token. The value of this variable is NOT read by
        /// `init` itself — only its name is embedded in the config.
        /// Only used by the `nsed.yaml` template.
        #[arg(long, default_value = commands::init::DEFAULT_TOKEN_ENV)]
        token_env: String,

        /// Agent names. For `nsed.yaml` (no `--agent-fleet`):
        /// listed under the room's policy. For `agent.yml`
        /// (`--agent-fleet`): each becomes an agent entry the
        /// runner instantiates. Repeat or comma-separate
        /// (`--agents cortex-a,cortex-b`).
        #[arg(long, value_delimiter = ',', num_args = 0..)]
        agents: Vec<String>,

        /// Overwrite an existing workspace file.
        #[arg(long)]
        force: bool,
    },

    /// Redeem a JWT invite code for NATS credentials. Generates a
    /// fresh NKey on this host, POSTs `{code, user_pub_key}` to the
    /// orchestrator's `/redeem-agent`, writes both `.creds` and
    /// `.seed` to disk (mode 0600 on Unix), and prints a summary.
    Redeem {
        /// The invite code provided by the admin (a JWT string,
        /// `eyJhbGc...`). Pass as a single positional argument.
        code: String,

        /// Orchestrator base URL.
        ///
        /// Resolution order: `--url` > `$ORCH_URL` env > built-in
        /// default. The default is `https://api.peeramid.xyz`;
        /// setting `NSED_ENV=local` (or `dev` / `development`)
        /// flips it to `http://localhost:8080` so working against
        /// a locally-running orchestrator doesn't need the flag.
        #[arg(long)]
        url: Option<String>,

        /// Path to write the `.seed` file to. Defaults to
        /// `~/.nsed/agent.seed`.
        #[arg(long, value_name = "PATH")]
        seed_out: Option<PathBuf>,

        /// Path to write the `.creds` file to. Defaults to
        /// `~/.nsed/agent.creds`. Only written when the code grants
        /// NATS credentials (any code minted via
        /// `/admin/api/agent-invites`, or operator codes with the
        /// `agent` capability in `capabilities`).
        #[arg(long, value_name = "PATH")]
        creds_out: Option<PathBuf>,

        /// Path to write the HTTP bearer token to. Defaults to
        /// `~/.nsed/operator.token`. Only written for operator
        /// codes (those minted via `/admin/api/invites` — chat
        /// users + operators). Agent-only codes don't carry a
        /// bearer token.
        #[arg(long, value_name = "PATH")]
        token_out: Option<PathBuf>,

        /// Overwrite existing creds / seed / token files.
        #[arg(long)]
        force: bool,

        /// Optional path to an existing NKey seed file (the `.seed`
        /// from a prior `quorum redeem`, or any `SU…` seed produced
        /// elsewhere). When set, the seed is loaded and reused
        /// instead of generating a fresh keypair. Lets an operator
        /// keep the same NATS identity across re-runs (e.g. when a
        /// transient redeem failed AFTER the orchestrator marked
        /// the JTI consumed — pre-stage the seed once, then redeem
        /// fresh invites with `--seed-in agent.seed` to keep the
        /// same pubkey). When absent, a fresh NKey is generated.
        #[arg(long, value_name = "PATH")]
        seed_in: Option<PathBuf>,

        /// Maximum retry attempts on transient failures
        /// (5xx, `kv_unavailable`, network blips).
        #[arg(long, default_value_t = 5)]
        max_attempts: u32,
    },

    /// Run a fleet of agents from a YAML config. Boots one
    /// `NatsNsedWorker` per agent, wires them into a
    /// `MultiAgentRunner`, and runs until SIGTERM.
    ///
    /// Operator pre-flight: `quorum redeem <invite>` to write
    /// `~/.nsed/agent.creds`, then point this command at your
    /// `agent.yml`. The NATS URL is resolved from the orchestrator
    /// at startup — no `agent.yml` field stashes it.
    Serve {
        /// Path to the fleet config (`agent.yml`). When omitted,
        /// searches `./agent.yml` then `./config/default.yml`.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,

        /// Workspace yaml (`nsed.yaml`). Defaults to top-level
        /// `--config` (i.e. `./nsed.yaml`). Used to look up the
        /// orchestrator address for NATS-URL discovery.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,

        /// Room to serve. Selects which orchestrator entry from
        /// the workspace config is queried for the NATS URL. When
        /// omitted, falls back to `default_room` then to "the only
        /// room defined" — same resolution rules `quorum run` uses.
        #[arg(long, value_name = "NAME")]
        room: Option<String>,

        /// NATS server URL. When set, skips the orchestrator query
        /// entirely — the runner connects to this URL with the
        /// configured creds. Intended as an explicit operator
        /// override for offline / dev clusters where the runtime
        /// discovery path can't reach the orchestrator. Production
        /// operators should leave this unset; `quorum serve`
        /// resolves the URL from the orchestrator via
        /// `GET /api/runtime/nats`.
        #[arg(long, value_name = "URL")]
        nats_url: Option<String>,

        /// Path to a `.creds` file (NKey User JWT) the agents
        /// authenticate with. Defaults to `~/.nsed/agent.creds`
        /// (where `quorum redeem` writes). Omit for unauthenticated
        /// dev orchestrators.
        #[arg(long, value_name = "PATH")]
        nats_creds: Option<PathBuf>,

        /// Restrict to a subset of agent names from the fleet
        /// config. Repeatable; matches case-insensitively. Omit
        /// (or pass `--agent ALL`) to run every configured agent.
        #[arg(long = "agent", value_name = "NAME")]
        agents: Vec<String>,

        /// JetStream stream name the orchestrator publishes work
        /// on. Override only if the orchestrator was deployed with
        /// `$NSED_STREAM` set to a non-default value.
        #[arg(long, value_name = "NAME")]
        stream_name: Option<String>,

        /// API subject prefix the orchestrator uses. Override only
        /// if the orchestrator was deployed with `$NSED_API_PREFIX`
        /// set to a non-default value.
        #[arg(long, value_name = "PREFIX")]
        api_prefix: Option<String>,

        /// LAN-visible unified dashboard port. Starts an HTTP
        /// control plane exposing per-agent status, chat capture,
        /// buffer inspection, and live config tuning. Falls back to
        /// `dashboard_port` in `agent.yml`. Requires the binary
        /// built with the `status-server` feature; a warning is
        /// logged when the flag is set but the feature is missing.
        #[arg(long, value_name = "PORT")]
        dashboard_port: Option<u16>,

        /// Address the dashboard binds to. Defaults to `127.0.0.1`
        /// (loopback only — invisible from LAN). Pass `0.0.0.0`
        /// (or a specific interface IP) to make the dashboard
        /// reachable from other hosts. Also configurable via the
        /// `QUORUM_DASHBOARD_BIND` env var; the flag wins when
        /// both are set.
        #[arg(long, value_name = "ADDR")]
        dashboard_bind: Option<String>,
    },

    /// Validate a workspace yaml against the `WorkspaceConfig` schema.
    /// Reports a one-line summary on success, exits non-zero on parse
    /// failure. Pure CLI helper — no network, no LLM, no mutation.
    Validate,
}

impl Cli {
    fn config_path(&self) -> PathBuf {
        resolve_config_path(self.config.as_deref(), |p| p.exists())
    }
}

/// Resolve the workspace config path: an explicit `--config` wins;
/// otherwise prefer `nsed.yaml`, then `nsed.yml`, falling back to
/// `nsed.yaml` (so a not-found error names the canonical file).
///
/// `exists` is injected so this stays a pure, fs-free unit.
fn resolve_config_path(explicit: Option<&Path>, exists: impl Fn(&Path) -> bool) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    for candidate in ["nsed.yaml", "nsed.yml"] {
        if exists(Path::new(candidate)) {
            return PathBuf::from(candidate);
        }
    }
    PathBuf::from("nsed.yaml")
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            ref task,
            ref task_file,
            ref room,
            ref policy,
            ref files,
            ref output_dir,
            ref output_file,
            force_output,
            #[cfg(feature = "tui")]
            tui,
        } => {
            let config_path = cli.config_path();
            let config_dir = config_path.parent().unwrap_or(Path::new("."));
            let resolved_task = match commands::run::resolve_task_input(
                task.as_deref(),
                task_file.as_deref(),
                config_dir,
            ) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            #[cfg(feature = "tui")]
            if tui {
                return quorum_rs::cli::tui::run_tui_with_task(
                    &config_path,
                    Some(&resolved_task),
                    room.as_deref(),
                    policy.as_deref(),
                )
                .await;
            }
            commands::run::run(
                &config_path,
                &resolved_task,
                room.as_deref(),
                policy.as_deref(),
                files,
                output_dir.as_deref(),
                output_file.as_deref(),
                force_output,
            )
            .await
        }
        #[cfg(feature = "tui")]
        Commands::Tui => quorum_rs::cli::tui::run_tui(&cli.config_path()).await,
        Commands::Status { ref orchestrator } => {
            commands::status::run(&cli.config_path(), orchestrator.as_deref()).await
        }
        Commands::Trace {
            ref job_id,
            ref orchestrator,
            verbose,
        } => {
            commands::trace::run(&cli.config_path(), job_id, orchestrator.as_deref(), verbose).await
        }
        Commands::Redeem {
            ref code,
            ref url,
            ref seed_out,
            ref creds_out,
            ref token_out,
            force,
            ref seed_in,
            max_attempts,
        } => {
            // Trim whitespace and ignore empty strings from `--url` /
            // `$ORCH_URL` so a stray blank doesn't beat the production
            // default and silently redeem against an unparseable URL.
            let resolved_url = url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    std::env::var("ORCH_URL")
                        .ok()
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_else(commands::redeem::default_orchestrator_url);
            match commands::redeem::run(
                code,
                &resolved_url,
                seed_out.as_deref(),
                creds_out.as_deref(),
                token_out.as_deref(),
                force,
                seed_in.as_deref(),
                max_attempts,
            )
            .await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Init {
            non_interactive,
            agent_fleet,
            ref orchestrator_url,
            ref room,
            ref token_env,
            ref agents,
            force,
        } => {
            // Decide between the interactive wizard and the
            // one-shot template renderer. The wizard only runs when
            // every condition is met: stdin is a TTY, no
            // `--non-interactive`, no one-shot-only flag set,
            // and the agent-fleet renderer wasn't requested.
            //
            // Any flag implies the operator wants the one-shot
            // path (scripted / Docker / CI), so the wizard is
            // skipped without forcing them to add
            // `--non-interactive` too.
            let one_shot_flags_set = !orchestrator_url.is_empty()
                && orchestrator_url != commands::init::DEFAULT_ORCHESTRATOR_URL
                || room != commands::init::DEFAULT_ROOM
                || token_env != commands::init::DEFAULT_TOKEN_ENV
                || !agents.is_empty();
            let stdin_is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
            let use_wizard =
                !non_interactive && !agent_fleet && !one_shot_flags_set && stdin_is_tty;

            if use_wizard {
                commands::init_wizard::run(&cli.config_path()).await
            } else if agent_fleet {
                // `agent.yml` is the conventional name `quorum
                // serve` looks for; fall back to it when --config
                // wasn't passed. Reusing `cli.config_path()` would
                // emit `nsed.yaml` here which is the wrong file
                // for the agent runner to read.
                let default_path = std::path::Path::new("agent.yml");
                let resolved = cli.config_path();
                let target = if cli.config.is_some() {
                    resolved.as_path()
                } else {
                    default_path
                };
                commands::init::run_agent_fleet(target, agents, force)
            } else {
                commands::init::run(
                    &cli.config_path(),
                    orchestrator_url,
                    room,
                    token_env,
                    agents,
                    force,
                )
            }
        }

        Commands::Serve {
            ref config,
            ref workspace,
            ref room,
            ref nats_url,
            ref nats_creds,
            ref agents,
            ref stream_name,
            ref api_prefix,
            dashboard_port,
            ref dashboard_bind,
        } => {
            let agents_filter: Option<&[String]> = if agents.is_empty() {
                None
            } else {
                Some(agents.as_slice())
            };
            let resolved_workspace = cli.config_path();
            let workspace_path = workspace.as_deref().unwrap_or(&resolved_workspace);
            match commands::serve::run(
                config.as_deref(),
                workspace_path,
                room.as_deref(),
                nats_url.as_deref(),
                nats_creds.as_deref(),
                agents_filter,
                stream_name.as_deref(),
                api_prefix.as_deref(),
                dashboard_port,
                dashboard_bind.as_deref(),
            )
            .await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }

        Commands::Validate => commands::validate::run(&cli.config_path()),
    }
}

#[cfg(test)]
mod config_path_tests {
    use super::resolve_config_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn explicit_path_wins_even_if_absent() {
        let got = resolve_config_path(Some(Path::new("custom.yml")), |_| false);
        assert_eq!(got, PathBuf::from("custom.yml"));
    }

    #[test]
    fn prefers_yaml_when_present() {
        let got = resolve_config_path(None, |p| p == Path::new("nsed.yaml"));
        assert_eq!(got, PathBuf::from("nsed.yaml"));
    }

    #[test]
    fn falls_back_to_yml_when_only_yml_present() {
        let got = resolve_config_path(None, |p| p == Path::new("nsed.yml"));
        assert_eq!(got, PathBuf::from("nsed.yml"));
    }

    #[test]
    fn yaml_wins_when_both_present() {
        let got = resolve_config_path(None, |_| true);
        assert_eq!(got, PathBuf::from("nsed.yaml"));
    }

    #[test]
    fn defaults_to_yaml_when_neither_present() {
        let got = resolve_config_path(None, |_| false);
        assert_eq!(got, PathBuf::from("nsed.yaml"));
    }
}
