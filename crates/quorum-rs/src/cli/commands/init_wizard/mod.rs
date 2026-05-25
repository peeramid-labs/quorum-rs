//! `nsed init` — interactive workspace setup wizard.
//!
//! Generates `nsed.yaml` (and optionally `config/agent.yml`) by guiding the
//! user through:
//! 1. Orchestrator configuration (one or more, remote or embedded)
//! 2. Agent setup (provider discovery + agent presets via nsed-cli-common)
//! 3. Policy creation (deliberation rules: rounds, convergence, SLA, mode)
//! 4. Agent → policy assignment (for static-agent policies)
//! 5. Room + default_room wiring
//! 6. Write, validate, and print summary

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

use inquire::{Confirm, CustomType, InquireError, MultiSelect, Select, Text};

use crate::cli::remote::{AgentInfo, RemoteOrchestrator};
use crate::cli::workspace::{
    AgentsConfig, ContextRef, OrchestratorConfig, OrchestratorMode, PolicyConfig, RoleConfig,
    RoomConfig, WorkspaceConfig,
};
use crate::config::resolve_env_token;
use crate::init::AgentSummary;
use crate::scheduling::PolicySla;

// ── Cancellation helper ─────────────────────────────────────────────────────

/// Wraps an `inquire` result: cancellation → `Ok(None)`, other errors bubble.
fn ask<T>(r: Result<T, InquireError>) -> Result<Option<T>, InquireError> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(e),
    }
}

// ── Pure helpers (testable without interactive prompts) ──────────────────────

/// Sanitize an orchestrator/room/policy name to a valid slug.
/// Keeps alphanumeric, dashes, underscores. Lowercases. Returns `None` if empty.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let slug: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Collapse consecutive underscores into a single one
    let mut collapsed = String::with_capacity(slug.len());
    for c in slug.chars() {
        if c == '_' && collapsed.ends_with('_') {
            continue;
        }
        collapsed.push(c);
    }
    let slug = collapsed.trim_matches('_').to_string();
    if slug.is_empty() { None } else { Some(slug) }
}

/// Build the YAML string for a `WorkspaceConfig`.
pub fn render_yaml(config: &WorkspaceConfig) -> Result<String, String> {
    serde_yaml::to_string(config).map_err(|e| format!("failed to serialize config: {e}"))
}

/// Render YAML for terminal preview, redacting any literal (non-env-ref) tokens.
pub fn render_yaml_redacted(config: &WorkspaceConfig) -> Result<String, String> {
    let mut preview = config.clone();
    for orch in preview.orchestrators.values_mut() {
        if let Some(ref t) = orch.token
            && !t.starts_with("${")
        {
            orch.token = Some("<redacted>".into());
        }
    }
    render_yaml(&preview)
}

/// Build a `WorkspaceConfig` from collected wizard inputs.
pub fn build_config(
    orchestrators: HashMap<String, OrchestratorConfig>,
    policies: HashMap<String, PolicyConfig>,
    rooms: HashMap<String, RoomConfig>,
    default_room: Option<String>,
    agent_config_file: Option<String>,
    dashboard_port: Option<u16>,
) -> WorkspaceConfig {
    WorkspaceConfig {
        orchestrators,
        policies,
        rooms,
        shared: None,
        default_room,
        agents: agent_config_file.map(|f| AgentsConfig {
            config_file: f,
            dashboard_port,
        }),
    }
}

/// Build agent display options from `AgentInfo` list (remote-discovered agents).
#[cfg(test)]
pub fn agent_display_options(agents: &[AgentInfo]) -> Vec<String> {
    agents
        .iter()
        .map(|a| {
            let status = if a.is_online { "●" } else { "○" };
            format!("{status} {:<20} {}", a.agent_id, a.model_name)
        })
        .collect()
}

/// Parse selected agent display strings back to agent IDs.
#[cfg(test)]
pub fn parse_selected_agents(selected: &[String], all_agents: &[AgentInfo]) -> Vec<String> {
    let display = agent_display_options(all_agents);
    selected
        .iter()
        .filter_map(|sel| {
            let idx = display.iter().position(|d| d == sel)?;
            Some(all_agents[idx].agent_id.clone())
        })
        .collect()
}

/// Build display options for locally-created agent summaries.
#[cfg(test)]
pub fn created_agent_display(agents: &[AgentSummary]) -> Vec<String> {
    agents
        .iter()
        .map(|a| format!("{:<20} {} ({})", a.name, a.model_name, a.provider_id))
        .collect()
}

/// Parse selected created-agent display strings back to agent names.
#[cfg(test)]
pub fn parse_selected_created(selected: &[String], all: &[AgentSummary]) -> Vec<String> {
    let display = created_agent_display(all);
    selected
        .iter()
        .filter_map(|sel| {
            let idx = display.iter().position(|d| d == sel)?;
            Some(all[idx].name.clone())
        })
        .collect()
}

// ── Prompt helper — ask for a unique name ───────────────────────────────────

fn ask_unique_name(
    prompt: &str,
    default: &str,
    existing: &[String],
) -> Result<Option<String>, String> {
    loop {
        let raw = match ask(Text::new(prompt).with_default(default).prompt())
            .map_err(|e| e.to_string())?
        {
            Some(r) => r,
            None => return Ok(None),
        };
        let name = match sanitize_name(&raw) {
            Some(s) => s,
            None => {
                eprintln!("  Invalid name — use alphanumeric, dashes, underscores.");
                continue;
            }
        };
        if existing.contains(&name) {
            eprintln!("  '{name}' already exists — pick a different name.");
            continue;
        }
        return Ok(Some(name));
    }
}

// ── Interactive wizard ──────────────────────────────────────────────────────

pub async fn run(output_path: &Path) -> ExitCode {
    if output_path.exists() {
        eprintln!("warning: {} already exists", output_path.display());
        match ask(Confirm::new("Overwrite?").with_default(false).prompt()) {
            Ok(Some(true)) => {}
            Ok(_) => {
                eprintln!("Cancelled.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("Prompt failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    eprintln!("nsed init — workspace setup wizard\n");

    // ── Step 1: Orchestrators (loop) ────────────────────────────────────────

    eprintln!("─── Orchestrators ─────────────────────────────────────────");
    eprintln!("  Where deliberation runs. Add one or more (remote API or local embedded).\n");

    let mut orchestrators: HashMap<String, OrchestratorConfig> = HashMap::new();
    let mut discovered_agents: HashMap<String, Vec<AgentInfo>> = HashMap::new();

    loop {
        let existing_names: Vec<String> = orchestrators.keys().cloned().collect();

        // Happy-path shortcut: first iteration auto-picks Remote
        // (the overwhelmingly common case) and auto-names it
        // `remote`. After the first orchestrator is registered the
        // loop asks a single Y/N "add another?" (default N) instead
        // of forcing operators through a Remote/Local/Done menu —
        // most setups have exactly one orchestrator.
        let is_first = orchestrators.is_empty();
        let (is_remote, orch_name) = if is_first {
            (true, "remote".to_string())
        } else {
            eprintln!("Orchestrators: {}", existing_names.join(", "));
            let add_more = match ask(Confirm::new("Add another orchestrator?")
                .with_default(false)
                .prompt())
            {
                Ok(Some(v)) => v,
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("Prompt failed: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if !add_more {
                break;
            }

            // Explicit menu for the rare add-another flow.
            let add_opts = vec![
                "Remote  — connect to an existing orchestrator",
                "Embedded — run orchestrator locally with nsed serve",
            ];
            let choice = match ask(Select::new("Add orchestrator:", add_opts).prompt()) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("Prompt failed: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let is_remote = choice.starts_with("Remote");
            let default_name = if is_remote {
                if orchestrators.contains_key("remote") {
                    format!("remote_{}", orchestrators.len() + 1)
                } else {
                    "remote".into()
                }
            } else if orchestrators.contains_key("local") {
                format!("local_{}", orchestrators.len() + 1)
            } else {
                "local".into()
            };

            let name = match ask_unique_name("Orchestrator name:", &default_name, &existing_names) {
                Ok(Some(n)) => n,
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            (is_remote, name)
        };

        if is_remote {
            match wizard_remote_orchestrator().await {
                Ok(Some((orch, agents))) => {
                    if !agents.is_empty() {
                        discovered_agents.insert(orch_name.clone(), agents);
                    }
                    orchestrators.insert(orch_name, orch);
                }
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            match wizard_embedded_orchestrator() {
                Ok(Some(orch)) => {
                    orchestrators.insert(orch_name, orch);
                }
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    // Merge all discovered agents for display in later steps.
    let all_discovered: Vec<AgentInfo> = discovered_agents
        .values()
        .flat_map(|v| v.iter().cloned())
        .collect();

    // ── Step 2: Agent Setup (optional) ──────────────────────────────────────

    eprintln!("\n─── Agents ────────────────────────────────────────────────");
    eprintln!("  Create agent personas for deliberation (detects Ollama, prompts for providers).");
    if !all_discovered.is_empty() {
        eprintln!(
            "  {} agent(s) already discovered from remote orchestrator(s).",
            all_discovered.len()
        );
    }
    eprintln!();

    let mut created_agents: Vec<AgentSummary> = Vec::new();
    let mut agent_config_yaml: Option<String> = None;

    let setup_agents = match ask(Confirm::new("Set up local agents now?")
        .with_default(true)
        .prompt())
    {
        Ok(Some(v)) => v,
        Ok(None) => {
            eprintln!("Cancelled.");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("Prompt failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if setup_agents {
        // Determine orchestrator URL for the agent config file
        let orch_url = resolve_orchestrator_url(&orchestrators);

        match crate::init::run_agent_setup(&orch_url).await {
            Ok(Some(result)) => {
                if !result.agents.is_empty() {
                    eprintln!(
                        "\n  ✓ {} agent(s) configured: {}",
                        result.agents.len(),
                        result
                            .agents
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    created_agents = result.agents;
                }
                if !result.agent_config_yaml.is_empty() {
                    agent_config_yaml = Some(result.agent_config_yaml);
                }
            }
            Ok(None) => {
                eprintln!("  Skipped agent setup.");
            }
            Err(e) => {
                eprintln!("error: agent setup failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // ── Dashboard port (optional, only when agents are configured) ──────────
    // Track whether the user was prompted so we can distinguish "said No"
    // (don't inherit old config) from "not prompted" (preserve old config).
    // On re-init, use the existing config's dashboard_port as prompt defaults.

    let existing_dashboard_port: Option<u16> = std::fs::read_to_string(output_path)
        .ok()
        .and_then(|c| serde_yaml::from_str::<WorkspaceConfig>(&c).ok())
        .and_then(|w| w.agents)
        .and_then(|a| a.dashboard_port);

    let mut dashboard_port: Option<u16> = None;
    let mut dashboard_prompted = false;

    if agent_config_yaml.is_some() || !created_agents.is_empty() {
        dashboard_prompted = true;
        match ask(Confirm::new("Enable agent dashboard?")
            .with_default(true)
            .prompt())
        {
            Ok(Some(true)) => {
                let port_default = existing_dashboard_port.unwrap_or(8081);
                match ask(CustomType::<u16>::new("Dashboard port:")
                    .with_default(port_default)
                    .prompt())
                {
                    Ok(Some(0)) => {
                        eprintln!("error: port must be between 1 and 65535");
                        return ExitCode::FAILURE;
                    }
                    Ok(Some(port)) => {
                        dashboard_port = Some(port);
                        eprintln!("  ✓ Dashboard on port {port}");
                    }
                    Ok(None) => {
                        eprintln!("Cancelled.");
                        return ExitCode::SUCCESS;
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            Ok(Some(false)) => {} // User explicitly said "No" — dashboard_port stays None
            Ok(None) => {
                eprintln!("Cancelled.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // ── Step 3: Policies (loop) ─────────────────────────────────────────────

    eprintln!("\n─── Policies ──────────────────────────────────────────────");
    eprintln!("  Deliberation rules: how many rounds, when to stop, which agents participate.\n");

    let mut policies: HashMap<String, PolicyConfig> = HashMap::new();
    // Track which policies use static mode and need agent assignment later.
    let mut static_policies: Vec<String> = Vec::new();

    loop {
        let existing_names: Vec<String> = policies.keys().cloned().collect();
        if !existing_names.is_empty() {
            eprintln!("Policies: {}", existing_names.join(", "));
        }

        if !policies.is_empty() {
            let add_more = match ask(Confirm::new("Add another policy?")
                .with_default(false)
                .prompt())
            {
                Ok(Some(v)) => v,
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("Prompt failed: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if !add_more {
                break;
            }
        }

        let default_name = if policies.is_empty() {
            "default".into()
        } else {
            format!("policy_{}", policies.len() + 1)
        };

        let policy_name = match ask_unique_name("Policy name:", &default_name, &existing_names) {
            Ok(Some(n)) => n,
            Ok(None) => {
                eprintln!("Cancelled.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };

        let policy = match wizard_policy() {
            Ok(Some((p, is_static))) => {
                if is_static {
                    static_policies.push(policy_name.clone());
                }
                p
            }
            Ok(None) => {
                eprintln!("Cancelled.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };

        policies.insert(policy_name, policy);
    }

    // ── Step 4: Agent Assignment (for static policies) ──────────────────────

    if !static_policies.is_empty() {
        eprintln!("\n─── Agent Assignment ──────────────────────────────────────");
        eprintln!("  Assign agents to each static policy (minimum 2 per policy).\n");

        for policy_name in &static_policies {
            let agents = match wizard_assign_agents(policy_name, &created_agents, &all_discovered) {
                Ok(Some(a)) => a,
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };

            if let Some(policy) = policies.get_mut(policy_name) {
                policy.agents = Some(agents);
            }
        }
    }

    // ── Step 5: Rooms (loop) ────────────────────────────────────────────────

    eprintln!("\n─── Rooms ─────────────────────────────────────────────────");
    eprintln!("  A room links a policy to an orchestrator. `nsed run` uses the default room.\n");

    let orch_names: Vec<String> = orchestrators.keys().cloned().collect();
    let policy_names: Vec<String> = policies.keys().cloned().collect();
    let mut rooms: HashMap<String, RoomConfig> = HashMap::new();

    loop {
        let existing_names: Vec<String> = rooms.keys().cloned().collect();
        if !existing_names.is_empty() {
            eprintln!("Rooms: {}", existing_names.join(", "));
        }

        if !rooms.is_empty() {
            let add_more = match ask(Confirm::new("Add another room?")
                .with_default(false)
                .prompt())
            {
                Ok(Some(v)) => v,
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("Prompt failed: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if !add_more {
                break;
            }
        }

        let default_name = if rooms.is_empty() {
            "main".into()
        } else {
            format!("room_{}", rooms.len() + 1)
        };

        let room_name = match ask_unique_name("Room name:", &default_name, &existing_names) {
            Ok(Some(n)) => n,
            Ok(None) => {
                eprintln!("Cancelled.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };

        // Pick policy
        let policy_ref = if policy_names.len() == 1 {
            eprintln!("  Using policy '{}' (only one defined)", policy_names[0]);
            policy_names[0].clone()
        } else {
            match ask(Select::new("Policy for this room:", policy_names.clone()).prompt()) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("Prompt failed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        };

        // Pick orchestrator
        let orch_ref = if orch_names.len() == 1 {
            eprintln!(
                "  Using orchestrator '{}' (only one defined)",
                orch_names[0]
            );
            Some(orch_names[0].clone())
        } else {
            match ask(Select::new("Orchestrator for this room:", orch_names.clone()).prompt()) {
                Ok(Some(o)) => Some(o),
                Ok(None) => {
                    eprintln!("Cancelled.");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("Prompt failed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        };

        rooms.insert(
            room_name,
            RoomConfig {
                policy: policy_ref,
                orchestrator: orch_ref,
            },
        );
    }

    // ── Default room ────────────────────────────────────────────────────────

    let room_names: Vec<String> = rooms.keys().cloned().collect();
    let default_room = if room_names.len() == 1 {
        Some(room_names[0].clone())
    } else {
        match ask(Select::new("Default room:", room_names).prompt()) {
            Ok(Some(r)) => Some(r),
            Ok(None) => {
                eprintln!("Cancelled.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    // ── Build config ────────────────────────────────────────────────────────

    let has_remote = orchestrators.values().any(|o| {
        o.mode
            .as_ref()
            .is_some_and(|m| *m == OrchestratorMode::Remote)
    });
    let agent_config_ref = agent_config_yaml
        .as_ref()
        .map(|_| "config/agent.yml".to_string())
        .or_else(|| {
            // Preserve existing agent config reference on re-init:
            // read the current nsed.yaml and reuse its agents.config_file path.
            let dir = output_path.parent().unwrap_or(Path::new("."));
            if let Ok(contents) = std::fs::read_to_string(output_path)
                && let Ok(existing) = serde_yaml::from_str::<WorkspaceConfig>(&contents)
                && let Some(agents) = existing.agents
            {
                let resolved = dir.join(&agents.config_file);
                if resolved.exists() {
                    return Some(agents.config_file);
                }
            }
            // Fall back: check default path on disk.
            let default_path = dir.join("config/agent.yml");
            default_path
                .exists()
                .then(|| "config/agent.yml".to_string())
        });
    // Preserve existing dashboard_port on re-init only when the user was not
    // prompted (e.g. no agents configured). If they were prompted and said "No",
    // don't silently re-enable from the old config.
    let dashboard_port = if dashboard_prompted {
        dashboard_port
    } else {
        dashboard_port.or(existing_dashboard_port)
    };

    let config = build_config(
        orchestrators,
        policies,
        rooms,
        default_room,
        agent_config_ref,
        dashboard_port,
    );

    if let Err(e) = config.validate() {
        eprintln!("error: generated config is invalid: {e}");
        return ExitCode::FAILURE;
    }

    let yaml = match render_yaml(&config) {
        Ok(y) => y,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // ── Summary (tokens redacted for terminal safety) ─────────────────────

    let preview = render_yaml_redacted(&config).unwrap_or_else(|_| yaml.clone());
    eprintln!("\n--- Generated nsed.yaml ---");
    eprintln!("{preview}");

    let agent_config_path = output_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("config/agent.yml");
    let mut files_to_write = vec![output_path.display().to_string()];
    if agent_config_yaml.is_some() {
        files_to_write.push(agent_config_path.display().to_string());
    }
    eprintln!("Files: {}", files_to_write.join(", "));

    match ask(Confirm::new("Write these files?")
        .with_default(true)
        .prompt())
    {
        Ok(Some(true)) => {}
        Ok(_) => {
            eprintln!("Cancelled.");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("Prompt failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    if let Err(e) = std::fs::write(output_path, &yaml) {
        eprintln!("error: failed to write {}: {e}", output_path.display());
        return ExitCode::FAILURE;
    }
    eprintln!("✓ Wrote {}", output_path.display());

    // Write agent config if generated — path already resolved above
    if let Some(ref agent_yaml) = agent_config_yaml {
        if let Some(parent) = agent_config_path.parent()
            && !parent.exists()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!("error: failed to create {}: {e}", parent.display());
            return ExitCode::FAILURE;
        }
        if let Err(e) = std::fs::write(&agent_config_path, agent_yaml) {
            eprintln!(
                "error: failed to write {}: {e}",
                agent_config_path.display()
            );
            return ExitCode::FAILURE;
        }
        eprintln!("✓ Wrote config/agent.yml");
    }

    eprint!("{}", format_next_steps(has_remote));

    ExitCode::SUCCESS
}

// ── Next-steps message ─────────────────────────────────────────────────────

/// Formats the post-init "Next steps" message.
/// When `has_remote` is true, appends a hint about running `nsed serve` first.
fn format_next_steps(has_remote: bool) -> String {
    let mut msg = String::from("\nNext steps:\n");
    msg.push_str("  nsed config validate        # verify config\n");
    msg.push_str("  nsed serve                  # start agents & register policies\n");
    msg.push_str("  nsed run \"your question\"    # start a deliberation\n");
    if has_remote {
        msg.push('\n');
        msg.push_str(
            "  \u{2139}  Remote orchestrator detected \u{2014} run `nsed serve` to register\n",
        );
        msg.push_str("     your agents and policies before starting deliberation.\n");
    }
    msg
}

// ── Orchestrator URL resolver ───────────────────────────────────────────────

/// Determine the best orchestrator URL for agent config.
/// Prefers orchestrators with an explicit address (embedded stores its API
/// port there too), falling back to `http://localhost:8080`.
/// When multiple orchestrators have addresses, picks the one with the
/// lexicographically smallest key for deterministic output.
pub fn resolve_orchestrator_url(orchestrators: &HashMap<String, OrchestratorConfig>) -> String {
    let mut keys: Vec<&String> = orchestrators.keys().collect();
    keys.sort();
    for key in keys {
        if let Some(ref addr) = orchestrators[key].address {
            return addr.clone();
        }
    }
    "http://localhost:8080".to_string()
}

// ── Sub-wizards ─────────────────────────────────────────────────────────────

async fn wizard_remote_orchestrator() -> Result<Option<(OrchestratorConfig, Vec<AgentInfo>)>, String>
{
    let address = match ask(Text::new("Orchestrator URL:")
        .with_default("https://api.peeramid.xyz")
        .prompt())
    .map_err(|e| e.to_string())?
    {
        Some(a) => {
            let trimmed = a.trim().to_string();
            if trimmed.is_empty() {
                return Err("address cannot be empty".into());
            }
            trimmed
        }
        None => return Ok(None),
    };

    // Branch on how the operator authenticates: a long-lived bearer
    // token they've been given, OR a single-use invite code we
    // redeem here-and-now to mint a fresh bearer token. Invite-code
    // path is the lowest-friction onboarding for "trying the tech"
    // — operator pastes one string and gets a working workspace.
    let auth_method = match ask(Select::new(
        "How do you want to authenticate?",
        vec![
            "Bearer token (long-lived, pre-issued)",
            "Invite code (single-use, redeem now)",
        ],
    )
    .prompt())
    .map_err(|e| e.to_string())?
    {
        Some(m) => m,
        None => return Ok(None),
    };

    let token_raw = if auth_method.starts_with("Invite") {
        match redeem_operator_invite_in_wizard(&address).await? {
            Some(t) => t,
            None => return Ok(None),
        }
    } else {
        match ask(Text::new("Bearer token (or ${ENV_VAR}):")
            .with_default("${NSED_BEARER_TOKEN}")
            .prompt())
        .map_err(|e| e.to_string())?
        {
            Some(t) => {
                let trimmed = t.trim().to_string();
                if trimmed.is_empty() {
                    return Err("token cannot be empty".into());
                }
                trimmed
            }
            None => return Ok(None),
        }
    };

    let orch = OrchestratorConfig {
        mode: Some(OrchestratorMode::Remote),
        address: Some(address.clone()),
        token: Some(token_raw.clone()),
        nats_url: None,
        config_file: None,
    };

    // Probe the orchestrator for agents (best-effort)
    let resolved_token = resolve_env_token("token", &token_raw);
    let mut agents = Vec::new();

    if !resolved_token.trim().is_empty() {
        eprintln!("Probing {address} ...");
        match RemoteOrchestrator::new(&address, &resolved_token) {
            Ok(client) => {
                match client.health().await {
                    Ok(h) => eprintln!("  ✓ Health: {} (NATS: {})", h.status, h.nats_connection),
                    Err(e) => eprintln!("  ✗ Health check failed: {e}"),
                }
                match client.agents().await {
                    Ok(a) => {
                        eprintln!("  ✓ {} agent(s) discovered", a.len());
                        agents = a;
                    }
                    Err(e) => eprintln!("  ✗ Agent discovery failed: {e}"),
                }
            }
            Err(e) => eprintln!("  ✗ Could not create client: {e}"),
        }
    } else {
        eprintln!("  ⚠ Token not resolved — skipping probe (set env var and re-run)");
    }

    Ok(Some((orch, agents)))
}

fn wizard_embedded_orchestrator() -> Result<Option<OrchestratorConfig>, String> {
    let config_file = match ask(Text::new("Config file path (will be created if missing):")
        .with_default("./config/orchestrator.yml")
        .prompt())
    .map_err(|e| e.to_string())?
    {
        Some(f) => {
            let trimmed = f.trim().to_string();
            if trimmed.is_empty() {
                return Err("config file path cannot be empty".into());
            }
            trimmed
        }
        None => return Ok(None),
    };

    // Prompt for key fields used in the full config template
    let app_port: u16 = match ask(Text::new("API port:").with_default("8080").prompt())
        .map_err(|e| e.to_string())?
    {
        Some(p) => match p.trim().parse::<u16>() {
            Ok(v) if v > 0 => v,
            _ => return Err("port must be 1–65535".into()),
        },
        None => return Ok(None),
    };

    let nats_port: u16 = match ask(Text::new("NATS port:").with_default("4222").prompt())
        .map_err(|e| e.to_string())?
    {
        Some(p) => match p.trim().parse::<u16>() {
            Ok(v) if v > 0 => v,
            _ => return Err("port must be 1–65535".into()),
        },
        None => return Ok(None),
    };

    let nats_url = format!("nats://127.0.0.1:{nats_port}");

    // Scaffold the config file if it doesn't exist
    let config_path = Path::new(&config_file);
    if !config_path.exists() {
        eprintln!("  Config file '{}' does not exist.", config_file);
        let create = match ask(Confirm::new("Create orchestrator config from template?")
            .with_default(true)
            .prompt())
        .map_err(|e| e.to_string())?
        {
            Some(v) => v,
            None => return Ok(None),
        };
        if create {
            if let Some(parent) = config_path.parent()
                && !parent.exists()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
            }
            // Use the full template from nsed-cli-common (same as `nsed-orchestrator init`)
            let content = crate::init::render_default_orchestrator_config(app_port, nats_port);
            std::fs::write(config_path, &content)
                .map_err(|e| format!("failed to write {}: {e}", config_file))?;
            eprintln!("  ✓ Created {config_file} (edit to customise auth, credentials, providers)");
        }
    } else {
        eprintln!("  ✓ Using existing {config_file}");
    }

    Ok(Some(OrchestratorConfig {
        mode: Some(OrchestratorMode::Embedded),
        address: Some(format!("http://localhost:{app_port}")),
        token: None,
        nats_url: Some(nats_url),
        config_file: Some(config_file),
    }))
}

/// Policy wizard — collects deliberation rules and mode choice.
/// Returns `(PolicyConfig, is_static)` where `is_static` means agents need
/// assignment in a later step (the `agents` field is left as a placeholder).
fn wizard_policy() -> Result<Option<(PolicyConfig, bool)>, String> {
    let mode_opts = vec![
        "Static agents — explicitly assign agents to this policy",
        "Role-based   — define roles with required capabilities; agents matched at runtime",
    ];
    let mode_choice =
        match ask(Select::new("Policy mode:", mode_opts).prompt()).map_err(|e| e.to_string())? {
            Some(c) => c,
            None => return Ok(None),
        };
    let is_static = mode_choice.starts_with("Static");

    // ── Deliberation parameters ─────────────────────────────────────────────

    let rounds = match ask(
        Text::new("Max deliberation rounds (upper bound):")
            .with_default("3")
            .with_help_message(
                "upper bound on iterations; the orchestrator may finish earlier when convergence is reached",
            )
            .prompt(),
    )
    .map_err(|e| e.to_string())?
    {
        Some(r) => match r.trim().parse::<u32>() {
            Ok(n) if n >= 1 => n,
            _ => return Err("max_rounds must be >= 1".into()),
        },
        None => return Ok(None),
    };

    let effort = match ask(Text::new("Effort (0.0–1.0):").with_default("0.6").prompt())
        .map_err(|e| e.to_string())?
    {
        Some(c) => match c.trim().parse::<f32>() {
            Ok(v) if (0.0..=1.0).contains(&v) => v,
            _ => return Err("effort must be 0.0–1.0".into()),
        },
        None => return Ok(None),
    };

    // ── SLA — all fields ────────────────────────────────────────────────────

    let sla = match wizard_sla()? {
        Some(s) => s,
        None => return Ok(None),
    };

    // ── Agent capability requirements ───────────────────────────────────────
    // These filter which agents are eligible. Each agent must advertise
    // matching capability tags. Format: `lang:rust`, `security:*`, `*`.

    let capabilities = if is_static {
        eprintln!(
            "  Agent capabilities filter which agents are eligible for assignment in the next step."
        );
        match ask(
            Text::new("Agent capabilities required (comma-separated, empty = any agent):")
                .with_default("")
                .with_help_message(
                    "e.g. lang:rust, security:owasp — each assigned agent must have these",
                )
                .prompt(),
        )
        .map_err(|e| e.to_string())?
        {
            Some(c) => parse_comma_list(&c),
            None => return Ok(None),
        }
    } else {
        // Role-based mode defines capabilities per role, but policy-level caps
        // are an additional global filter.
        match ask(Text::new(
            "Global agent capabilities required (comma-separated, empty = per-role only):",
        )
        .with_default("")
        .with_help_message("applied in addition to per-role capabilities")
        .prompt())
        .map_err(|e| e.to_string())?
        {
            Some(c) => parse_comma_list(&c),
            None => return Ok(None),
        }
    };

    // ── Discovery tags ──────────────────────────────────────────────────────

    let tags = match ask(Text::new(
        "Discovery tags (comma-separated, empty = none, e.g. domain:security, public):",
    )
    .with_default("")
    .with_help_message("tags for policy discovery and matching")
    .prompt())
    .map_err(|e| e.to_string())?
    {
        Some(t) => parse_comma_list(&t),
        None => return Ok(None),
    };

    if is_static {
        // Placeholder — agents assigned in step 4.
        Ok(Some((
            PolicyConfig {
                agents: Some(vec!["__placeholder__".into(), "__placeholder__".into()]),
                roles: None,
                max_rounds: rounds,
                effort,
                sla,
                capabilities,
                tags,
                mode: Default::default(),
            },
            true,
        )))
    } else {
        // Role-based mode — collect roles (each role has count + capabilities)
        eprintln!();
        eprintln!("  Each role defines a capability requirement and how many agents fill it.");
        let roles = match wizard_roles() {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        };

        Ok(Some((
            PolicyConfig {
                agents: None,
                roles: Some(roles),
                max_rounds: rounds,
                effort,
                sla,
                capabilities,
                tags,
                mode: Default::default(),
            },
            false,
        )))
    }
}

/// Prompt the operator for an invite code, redeem it against
/// `orchestrator_url` (#307 operator-redeem endpoint), and return the
/// freshly minted bearer token. The token is embedded in the
/// workspace YAML as a literal (not an env-var template) since the
/// operator only sees this value once — they have nothing to
/// substitute later.
///
/// A fresh NATS User NKey is generated locally and the public half is
/// presented to the orchestrator at redeem time. When the redeemed
/// code carries the agent grant (#444), the orchestrator also returns
/// a scoped User JWT + NATS URL — the wizard then persists `.creds` +
/// `.seed` to `~/.nsed/` so the agent process can pick them up via
/// `NATS_CREDS=~/.nsed/agent.creds`. Chat-only codes simply ignore
/// the pubkey and return the bearer token alone.
///
/// `Ok(None)` = operator cancelled / pressed Esc. `Err(_)` =
/// orchestrator rejected the code or transport failure. In either
/// case the caller bails out of the wizard cleanly.
async fn redeem_operator_invite_in_wizard(
    orchestrator_url: &str,
) -> Result<Option<String>, String> {
    use crate::nats_utils::{
        RedeemInviteError, format_nats_creds, redeem_operator_invite_with_orchestrator,
    };

    let code = match ask(Text::new("Paste invite code:").prompt()).map_err(|e| e.to_string())? {
        Some(c) => {
            let trimmed = c.trim().to_string();
            if trimmed.is_empty() {
                return Err("invite code cannot be empty".into());
            }
            trimmed
        }
        None => return Ok(None),
    };

    let keypair = nkeys::KeyPair::new_user();
    let pub_key = keypair.public_key();

    eprintln!("Redeeming invite at {orchestrator_url}…");
    match redeem_operator_invite_with_orchestrator(
        orchestrator_url,
        &code,
        Some(&pub_key),
        Some("nsed init wizard"),
    )
    .await
    {
        Ok(resp) => {
            eprintln!(
                "  ✓ Redeemed as `{}` (token saved into workspace YAML)",
                resp.name
            );
            if let Some(budget) = resp.budget {
                eprintln!("  ✓ Initial budget: {budget} credits");
            }
            if let (Some(user_jwt), Some(nats_url)) =
                (resp.user_jwt.as_ref(), resp.nats_url.as_ref())
            {
                let seed = keypair
                    .seed()
                    .map_err(|e| format!("Failed to extract NKey seed: {e}"))?;
                let (creds_path, seed_path) =
                    persist_agent_creds(&format_nats_creds(user_jwt, &seed), &seed)?;
                eprintln!("  ✓ NATS creds : {}", creds_path.display());
                eprintln!("  ✓ NATS seed  : {}", seed_path.display());
                eprintln!("  ✓ NATS URL   : {nats_url}");
                eprintln!(
                    "  → Point your agent at the creds file (e.g. `NATS_CREDS={}` or the YAML \
                     `nats.auth.creds_file` field).",
                    creds_path.display()
                );
            }
            Ok(Some(resp.token))
        }
        Err(RedeemInviteError::Expired) => {
            Err("This invite code has expired. Ask the admin for a fresh code.".into())
        }
        Err(RedeemInviteError::Replayed) => Err(
            "This invite code was already redeemed. Each code is single-use — ask the admin \
             for a fresh code."
                .into(),
        ),
        Err(RedeemInviteError::Revoked) => Err("The admin revoked this invite code.".into()),
        Err(RedeemInviteError::InvalidCode) => Err(
            "This invite code is invalid. Common causes: tampered during copy/paste, wrong \
             code type (agent-credential vs operator-token), or signing-secret mismatch."
                .into(),
        ),
        Err(RedeemInviteError::NotConfigured) => Err(
            "The orchestrator does not have invite codes configured. Ask the admin to set \
             APP_INVITES__SIGNING_SECRET on the orchestrator."
                .into(),
        ),
        Err(RedeemInviteError::KvUnavailable) => Err(
            "The orchestrator's backing store is temporarily unreachable. Try again in a minute."
                .into(),
        ),
        Err(RedeemInviteError::Unexpected { status, body }) => {
            Err(format!("Unexpected response: HTTP {status} body={body:?}"))
        }
        Err(RedeemInviteError::Transport(e)) => Err(format!("Failed to reach orchestrator: {e:#}")),
        Err(RedeemInviteError::Decode(e)) => Err(format!(
            "Orchestrator accepted the invite but the SDK couldn't process the response \
             ({e:#}). The invite is now consumed — ask the admin for a fresh code; the \
             orchestrator may be misconfigured."
        )),
    }
}

/// Persist a fresh `.creds` + `.seed` pair to `~/.nsed/agent.{creds,seed}`
/// after a unified-grant redeem. Files are written mode 0600 on Unix.
///
/// If a file already exists at either path, it's rotated to a
/// timestamped `.bak-<unix-ts>` sidecar before the new write —
/// previously the wizard refused, which lost the freshly-minted
/// token (the orchestrator had already burned the JTI by the time
/// this function ran, so failing here meant the operator had to ask
/// for a new invite). The old creds are never deleted, just renamed,
/// so an operator who needs the previous identity can recover it
/// from `.bak-<ts>`.
///
/// Returns the resolved paths so the caller can echo them to the user.
fn persist_agent_creds(
    creds_content: &str,
    user_seed: &str,
) -> std::result::Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| "Cannot determine $HOME — set HOME or USERPROFILE.".to_string())?;
    let mut creds_path = std::path::PathBuf::from(&home);
    creds_path.push(".nsed");
    let dir = creds_path.clone();
    creds_path.push("agent.creds");
    let mut seed_path = std::path::PathBuf::from(&home);
    seed_path.push(".nsed");
    seed_path.push("agent.seed");

    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for path in [&creds_path, &seed_path] {
        if path.exists() {
            let bak = path.with_extension(format!(
                "{}.bak-{}",
                path.extension().and_then(|s| s.to_str()).unwrap_or(""),
                ts
            ));
            std::fs::rename(path, &bak).map_err(|e| {
                format!(
                    "Failed to rotate existing {} to {}: {e}",
                    path.display(),
                    bak.display()
                )
            })?;
            eprintln!(
                "  ↻ Rotated existing {} → {} (recoverable if needed)",
                path.display(),
                bak.display()
            );
        }
    }

    write_secret_file(&creds_path, creds_content)
        .map_err(|e| format!("Failed to write {}: {e}", creds_path.display()))?;
    write_secret_file(&seed_path, user_seed)
        .map_err(|e| format!("Failed to write {}: {e}", seed_path.display()))?;

    Ok((creds_path, seed_path))
}

fn write_secret_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(content.as_bytes())?;
        if !content.ends_with('\n') {
            writeln!(f)?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        // Atomic create-or-fail so a racing process that materialises
        // the path between our existence check and this write can't
        // be silently clobbered. Same TOCTOU guarantee as the Unix
        // branch above (`create_new(true)`).
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        f.write_all(content.as_bytes())?;
        if !content.ends_with('\n') {
            writeln!(f)?;
        }
        Ok(())
    }
}

/// Parse a comma-separated string into `Option<Vec<String>>`.
/// Returns `None` if empty.
fn parse_comma_list(raw: &str) -> Option<Vec<String>> {
    let items: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if items.is_empty() { None } else { Some(items) }
}

/// Collect SLA parameters: job timeout, response SLA (HITL buffer), max tokens.
///
/// - **Job timeout** (`job_timeout_secs`): whole-job wall-clock budget for the
///   entire deliberation. The BudgetManager divides this adaptively across
///   rounds and phases — when a phase's derived budget runs out, stragglers
///   are timed out.
/// - **Response SLA (HITL buffer)**: after the LLM produces a response, it's
///   held in a buffer for this duration, giving a human operator time to inspect,
///   edit, annotate, or reject before auto-release to the orchestrator.
///   Set to 0 to disable (pass-through). Operators can also pause, stop, or
///   auto-approve via the agent control plane API.
/// - **Max tokens**: caps each agent's LLM response length (maps to `max_tokens`).
fn wizard_sla() -> Result<Option<Option<PolicySla>>, String> {
    let job_timeout = match ask(Text::new(
        "Job timeout — whole-job wall-clock budget divided across rounds/phases (seconds, 0 = skip):",
    )
    .with_default("600")
    .with_help_message("the BudgetManager distributes this across rounds; stragglers are timed out when a phase's derived budget expires")
    .prompt())
    .map_err(|e| e.to_string())?
    {
        Some(t) => match t.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => return Err("invalid timeout".into()),
        },
        None => return Ok(None),
    };

    // Only ask advanced SLA fields if the user set a job timeout
    let mut response_sla: Option<u64> = None;
    let mut max_tokens: Option<u32> = None;

    if let Some(job_secs) = job_timeout {
        let configure_advanced = match ask(Confirm::new(
            "Configure operator review window (HITL) and max tokens?",
        )
        .with_default(false)
        .with_help_message(
            "response SLA = buffer time for human review before auto-release to orchestrator",
        )
        .prompt())
        .map_err(|e| e.to_string())?
        {
            Some(v) => v,
            None => return Ok(None),
        };

        if configure_advanced {
            // response_sla_secs is the HITL operator review window:
            // after the LLM produces a response, it's held in a buffer for
            // this duration, giving a human operator time to inspect, edit,
            // annotate, or reject before it's auto-released to the orchestrator.
            // 0 = pass-through (no buffer).
            response_sla = match ask(Text::new(
                "Response SLA — operator review window (seconds, 0 = no buffer):",
            )
            .with_default("0")
            .with_help_message(&format!(
                "HITL buffer: LLM responses held for review before auto-release (≤ {job_secs}s job budget)"
            ))
            .prompt())
            .map_err(|e| e.to_string())?
            {
                Some(t) => match t.trim().parse::<u64>() {
                    Ok(0) => None,
                    Ok(n) if n <= job_secs => Some(n),
                    Ok(n) => {
                        eprintln!(
                            "  warning: {n}s exceeds job timeout {job_secs}s, clamping to {job_secs}s"
                        );
                        Some(job_secs)
                    }
                    Err(_) => return Err("invalid response SLA".into()),
                },
                None => return Ok(None),
            };

            max_tokens = match ask(Text::new("Max tokens per agent response (0 = no limit):")
                .with_default("0")
                .with_help_message("caps LLM output length (maps to max_tokens parameter)")
                .prompt())
            .map_err(|e| e.to_string())?
            {
                Some(t) => match t.trim().parse::<u32>() {
                    Ok(0) => None,
                    Ok(n) => Some(n),
                    Err(_) => return Err("invalid max_tokens".into()),
                },
                None => return Ok(None),
            };
        }
    }

    if job_timeout.is_none() && response_sla.is_none() && max_tokens.is_none() {
        return Ok(Some(None));
    }

    // job_timeout_secs is a required field on PolicySla and must be > 0
    // (workspace validation rejects 0). If user skipped job timeout but set
    // other SLA fields, use a sensible default.
    let job_timeout_secs = job_timeout.unwrap_or(600);

    Ok(Some(Some(PolicySla {
        job_timeout_secs,
        response_sla_secs: response_sla,
        max_tokens,
    })))
}

/// Collect role definitions for a role-based policy.
fn wizard_roles() -> Result<Option<Vec<RoleConfig>>, String> {
    let mut roles: Vec<RoleConfig> = Vec::new();
    eprintln!("  Define roles (at least 2 total agents across all roles).\n");

    loop {
        let existing: Vec<String> = roles.iter().map(|r| r.role.clone()).collect();
        if !existing.is_empty() {
            let total: u32 = roles.iter().map(|r| r.count as u32).sum();
            eprintln!("  Roles so far: {} ({total} agent(s))", existing.join(", "));
        }

        if !roles.is_empty() {
            let total: u32 = roles.iter().map(|r| r.count as u32).sum();
            if total < 2 {
                eprintln!(
                    "  Need at least 2 total agents across roles (have {total}). Adding another role.\n"
                );
            } else {
                let add_more = match ask(Confirm::new("Add another role?")
                    .with_default(false)
                    .prompt())
                .map_err(|e| e.to_string())?
                {
                    Some(v) => v,
                    None => return Ok(None),
                };
                if !add_more {
                    break;
                }
            }
        }

        let default_name = if roles.is_empty() {
            "analyst".into()
        } else {
            format!("role_{}", roles.len() + 1)
        };

        let role_name = match ask_unique_name("Role name:", &default_name, &existing) {
            Ok(Some(n)) => n,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        };

        let count: u8 = match ask(Text::new("How many agents for this role?")
            .with_default("1")
            .with_help_message("each agent filling this role must match the required capabilities")
            .prompt())
        .map_err(|e| e.to_string())?
        {
            Some(c) => match c.trim().parse::<u8>() {
                Ok(n) if n >= 1 => n,
                _ => return Err("count must be >= 1".into()),
            },
            None => return Ok(None),
        };

        let caps_raw = match ask(Text::new(
            "Agent capabilities required (comma-separated, e.g. lang:rust, security:*):",
        )
        .with_default("*")
        .with_help_message("agents must advertise these capability tags to fill this role")
        .prompt())
        .map_err(|e| e.to_string())?
        {
            Some(c) => c,
            None => return Ok(None),
        };

        let capabilities: Vec<String> = caps_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if capabilities.is_empty() {
            return Err("at least one capability required per role".into());
        }

        // Optional context files for this role
        let context = match ask(Text::new(
            "Context files for this role (name=path pairs, comma-separated, empty = none):",
        )
        .with_default("")
        .with_help_message("e.g. spec=docs/spec.md, code=src/main.rs")
        .prompt())
        .map_err(|e| e.to_string())?
        {
            Some(raw) => {
                let mut refs = Vec::new();
                for pair in raw.split(',') {
                    let pair = pair.trim();
                    if pair.is_empty() {
                        continue;
                    }
                    match pair.split_once('=') {
                        Some((name, path))
                            if !name.trim().is_empty() && !path.trim().is_empty() =>
                        {
                            refs.push(ContextRef {
                                name: name.trim().to_string(),
                                path: path.trim().to_string(),
                            });
                        }
                        _ => {
                            eprintln!(
                                "  ⚠ Skipping malformed context pair: {pair:?} (expected name=path)"
                            );
                        }
                    }
                }
                if refs.is_empty() { None } else { Some(refs) }
            }
            None => return Ok(None),
        };

        roles.push(RoleConfig {
            role: role_name,
            count,
            capabilities,
            context,
            pinned_agents: None,
            moderator: false,
        });
    }

    Ok(Some(roles))
}

/// Build a unified display list combining created and discovered agents.
/// Returns `(display_strings, agent_ids, default_indices)`.
pub fn combined_agent_display(
    created: &[AgentSummary],
    discovered: &[AgentInfo],
) -> (Vec<String>, Vec<String>) {
    let mut display = Vec::new();
    let mut ids = Vec::new();

    for a in created {
        let caps = if a.capability_tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", a.capability_tags.join(", "))
        };
        display.push(format!(
            "★ {:<20} {} ({}){}",
            a.name, a.model_name, a.provider_id, caps
        ));
        ids.push(a.name.clone());
    }

    for a in discovered {
        let status = if a.is_online { "●" } else { "○" };
        let caps = if a.capability_tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", a.capability_tags.join(", "))
        };
        display.push(format!(
            "{status} {:<20} {} ({}){}",
            a.agent_id, a.model_name, a.provider_id, caps
        ));
        ids.push(a.agent_id.clone());
    }

    (display, ids)
}

/// Parse selected display strings back to agent IDs using the combined list.
pub fn parse_combined_selection(
    selected: &[String],
    display: &[String],
    ids: &[String],
) -> Vec<String> {
    selected
        .iter()
        .filter_map(|sel| {
            let idx = display.iter().position(|d| d == sel)?;
            Some(ids[idx].clone())
        })
        .collect()
}

/// Assign agents to a static policy from available sources.
/// Combines locally-created and remote-discovered agents in a single list.
fn wizard_assign_agents(
    policy_name: &str,
    created: &[AgentSummary],
    discovered: &[AgentInfo],
) -> Result<Option<Vec<String>>, String> {
    eprintln!("  Policy '{policy_name}' — assign agents:");

    let has_agents = !created.is_empty() || !discovered.is_empty();

    if has_agents {
        let (display, ids) = combined_agent_display(created, discovered);

        // Default: select all created agents + online discovered agents
        let defaults: Vec<usize> = (0..display.len())
            .filter(|&i| {
                if i < created.len() {
                    true // all created agents selected by default
                } else {
                    discovered[i - created.len()].is_online
                }
            })
            .collect();

        loop {
            let selected = match ask(MultiSelect::new("Select agents:", display.clone())
                .with_default(&defaults)
                .with_help_message("★ = created locally, ● = online remote, ○ = offline remote")
                .prompt())
            .map_err(|e| e.to_string())?
            {
                Some(s) => s,
                None => return Ok(None),
            };

            let names = parse_combined_selection(&selected, &display, &ids);
            if names.len() >= 2 {
                return Ok(Some(names));
            }
            eprintln!("  Need at least 2 agents, got {}. Try again.", names.len());
        }
    }

    // Fallback: manual entry (no agents available from setup or discovery)
    loop {
        let raw = match ask(Text::new("Agent names (comma-separated, min 2):")
            .with_default("DEFAULT, FriendlyAssistant, CapableAnalyst")
            .prompt())
        .map_err(|e| e.to_string())?
        {
            Some(r) => r,
            None => return Ok(None),
        };

        let names: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            raw.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
                .collect()
        };

        if names.len() >= 2 {
            return Ok(Some(names));
        }
        eprintln!("  Need at least 2 distinct agents, got {}.", names.len());
    }
}
