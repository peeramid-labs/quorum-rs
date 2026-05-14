use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use quorum_cli::commands;

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
                return quorum_cli::tui::run_tui_with_task(
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
        Commands::Tui => quorum_cli::tui::run_tui(cli.config_path()).await,
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
    }
}
