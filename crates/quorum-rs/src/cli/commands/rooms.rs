use std::path::Path;
use std::process::ExitCode;

use crate::cli::workspace::WorkspaceConfig;

use super::common::{build_remote, resolve_orchestrators};

/// `quorum rooms` — list the rooms you can submit to. The orchestrator
/// returns its grant-filtered view (`GET /rooms`): public rooms plus any
/// whose tags glob-match your token's grants.
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

        match client.discover_rooms().await {
            Ok(rooms) => {
                if rooms.is_empty() {
                    println!("  Rooms: (none available to you)");
                } else {
                    println!("  Rooms ({}):", rooms.len());
                    for r in &rooms {
                        let tags = if r.tags.is_empty() {
                            "-".to_string()
                        } else {
                            r.tags.join(",")
                        };
                        println!(
                            "    {:<24} {:<9} {} agent(s)  tags: {tags}",
                            r.id, r.visibility, r.eligible_agent_count
                        );
                    }
                }
            }
            Err(e) => {
                println!("  Rooms: failed to fetch ({e})");
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
