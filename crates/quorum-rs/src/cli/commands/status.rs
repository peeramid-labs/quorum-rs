use std::path::Path;
use std::process::ExitCode;

use crate::cli::workspace::WorkspaceConfig;

use super::common::{build_remote, resolve_orchestrators};

pub async fn run(config_path: &Path, orchestrator: Option<&str>) -> ExitCode {
    let config = match WorkspaceConfig::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let targets = match resolve_orchestrators(&config, orchestrator) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut any_failed = false;

    for (name, orch) in &targets {
        let client = match build_remote(name, orch) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                any_failed = true;
                continue;
            }
        };

        let address = orch.address.as_deref().unwrap_or("?");
        println!("Orchestrator: {name} ({address})");

        match client.health().await {
            Ok(h) => println!("  Health: {} (NATS: {})", h.status, h.nats_connection),
            Err(e) => {
                println!("  Health: unreachable ({e})");
                any_failed = true;
                continue;
            }
        }

        match client.agents().await {
            Ok(agents) => {
                if agents.is_empty() {
                    println!("  Agents: (none registered)");
                } else {
                    println!("  Agents ({}):", agents.len());
                    for a in &agents {
                        let icon = if a.is_online { "\u{2713}" } else { "\u{2717}" };
                        let cost = a
                            .estimated_cost_per_round
                            .map(|c| format!("${c:.2}/round"))
                            .unwrap_or_else(|| "-".into());
                        println!("    {icon} {:<20} {:<16} {cost}", a.agent_id, a.model_name);
                    }
                }
            }
            Err(e) => {
                println!("  Agents: failed to fetch ({e})");
                any_failed = true;
            }
        }
    }

    if any_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
