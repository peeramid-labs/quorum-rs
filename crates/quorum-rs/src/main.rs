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

    /// Path to workspace config file [default: ./nsed.yaml]
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

    /// Bootstrap a workspace config (nsed.yaml) or agent fleet
    /// (agent.yml). Default: interactive wizard for provider /
    /// preset discovery — same flow as legacy `nsed init`. Falls
    /// through to the one-shot non-interactive renderer when any
    /// flag is set, when `--non-interactive` is passed, or when
    /// stdin is not a TTY (piped / CI).
    Init {
        /// Skip the interactive wizard and run the non-interactive
        /// one-shot template renderer instead. Implied when stdin is
        /// not a TTY (piped / CI) or when any of `--orchestrator-url`
        /// / `--room` / `--token-env` / `--agents` are passed. Use
        /// this flag to force the one-shot from a TTY without
        /// supplying any other flag (rare; mostly for scripts and
        /// docker entrypoints).
        #[arg(long, conflicts_with_all = ["agent_fleet"])]
        non_interactive: bool,

        /// Write an `agent.yml` fleet config (consumed by `quorum
        /// serve`) instead of the client-side `nsed.yaml` (consumed
        /// by `quorum run`/`status`/`trace`/`tui`). One-shot, never
        /// interactive. The two YAMLs configure distinct concerns:
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

        /// Orchestrator base URL. Only meaningful for the one-shot
        /// non-interactive path (the wizard prompts for it). Setting
        /// this auto-selects non-interactive mode.
        #[arg(long)]
        orchestrator_url: Option<String>,

        /// Room name. Setting it auto-selects non-interactive mode.
        #[arg(short, long)]
        room: Option<String>,

        /// Env-var name the generated YAML interpolates for the
        /// bearer token. Setting it auto-selects non-interactive
        /// mode.
        #[arg(long)]
        token_env: Option<String>,

        /// Agent names. For `nsed.yaml` (no `--agent-fleet`):
        /// listed under the room's policy. For `agent.yml`
        /// (`--agent-fleet`): each becomes an agent entry the
        /// runner instantiates. Repeat or comma-separate
        /// (`--agents cortex-a,cortex-b`). Setting it auto-selects
        /// non-interactive mode.
        #[arg(long, value_delimiter = ',', num_args = 0..)]
        agents: Vec<String>,

        /// Overwrite an existing workspace file (non-interactive
        /// modes only). The wizard refuses to clobber and asks first.
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

    /// Run a fleet of agents from a YAML config. The SDK analog of
    /// the proprietary `nsed serve` binary — boots one
    /// `NatsNsedWorker` per agent, wires them into a
    /// `MultiAgentRunner`, and runs until SIGTERM.
    ///
    /// Operator pre-flight: `quorum redeem <invite>` to write
    /// `~/.nsed/agent.creds`, then point this command at your
    /// `agent.yml` and the orchestrator's NATS URL.
    Serve {
        /// Path to the fleet config (`agent.yml`). When omitted,
        /// searches `./agent.yml` then `./config/default.yml`.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,

        /// NATS server URL. Falls back to `$NATS_URL`, then
        /// `nats://localhost:4222`. Production deployments should
        /// always pass this explicitly — the orchestrator returns
        /// the right URL in the `/redeem` response.
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
    },

    /// Validate a workspace yaml against the `WorkspaceConfig` schema.
    /// Reports a one-line summary on success, exits non-zero on parse
    /// failure. Pure CLI helper — no network, no LLM, no mutation.
    Validate,
}

impl Cli {
    fn config_path(&self) -> &Path {
        self.config.as_deref().unwrap_or(Path::new("nsed.yaml"))
    }
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
                    config_path,
                    Some(&resolved_task),
                    room.as_deref(),
                    policy.as_deref(),
                )
                .await;
            }
            commands::run::run(
                config_path,
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
        Commands::Tui => quorum_rs::cli::tui::run_tui(cli.config_path()).await,
        Commands::Status { ref orchestrator } => {
            commands::status::run(cli.config_path(), orchestrator.as_deref()).await
        }
        Commands::Trace {
            ref job_id,
            ref orchestrator,
            verbose,
        } => {
            commands::trace::run(cli.config_path(), job_id, orchestrator.as_deref(), verbose).await
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
            if agent_fleet {
                // `agent.yml` is the conventional name `quorum
                // serve` looks for; fall back to it when --config
                // wasn't passed. Reusing `cli.config_path()` would
                // emit `nsed.yaml` here which is the wrong file
                // for the agent runner to read.
                let default_path = std::path::Path::new("agent.yml");
                let target = if cli.config.is_some() {
                    cli.config_path()
                } else {
                    default_path
                };
                commands::init::run_agent_fleet(target, agents, force)
            } else {
                // Wizard is the default. Bail to the one-shot
                // renderer when EITHER (a) any non-interactive flag
                // was set, (b) `--non-interactive` was passed
                // explicitly, or (c) stdin is not a TTY (piped / CI
                // entrypoint). Otherwise launch the interactive
                // provider-discovery flow.
                use std::io::IsTerminal;
                // `--force` is NOT in this set: it's a confirmation
                // override that can apply to either path. The wizard
                // itself can use it to skip the "file exists, overwrite?"
                // prompt without dropping the rest of the interactive flow.
                let one_shot_flags_set = orchestrator_url.is_some()
                    || room.is_some()
                    || token_env.is_some()
                    || !agents.is_empty();
                let stdin_is_tty = std::io::stdin().is_terminal();
                let use_wizard = !non_interactive && !one_shot_flags_set && stdin_is_tty;
                if use_wizard {
                    commands::init_wizard::run(cli.config_path()).await
                } else {
                    commands::init::run(
                        cli.config_path(),
                        orchestrator_url
                            .as_deref()
                            .unwrap_or(commands::init::DEFAULT_ORCHESTRATOR_URL),
                        room.as_deref().unwrap_or(commands::init::DEFAULT_ROOM),
                        token_env
                            .as_deref()
                            .unwrap_or(commands::init::DEFAULT_TOKEN_ENV),
                        agents,
                        force,
                    )
                }
            }
        }

        Commands::Serve {
            ref config,
            ref nats_url,
            ref nats_creds,
            ref agents,
            ref stream_name,
            ref api_prefix,
        } => {
            let agents_filter: Option<&[String]> = if agents.is_empty() {
                None
            } else {
                Some(agents.as_slice())
            };
            match commands::serve::run(
                config.as_deref(),
                nats_url.as_deref(),
                nats_creds.as_deref(),
                agents_filter,
                stream_name.as_deref(),
                api_prefix.as_deref(),
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

        Commands::Validate => commands::validate::run(cli.config_path()),
    }
}
