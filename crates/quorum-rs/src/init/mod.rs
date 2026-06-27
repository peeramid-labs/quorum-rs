//! Interactive first-run setup wizard shared by `nsed-orchestrator init` and
//! `nsed-agent init`.
//!
//! Produces:
//!   `./docker-compose.yml`   — orchestrator (+ embedded NATS) and/or N agent services
//!   `./.env`                 — all secrets and tunables
//!   `./.gitignore`           — `.env` appended when not already present
//!
//! # Entry points
//!
//! - [`run_owner`] — full wizard with persona choice (workspace owner / worker).
//! - [`run_worker`] — worker-only wizard (`nsed-agent init --orchestrator <url>`).
//!
//! # Module structure
//!
//! Production code is split into focused submodules; all tests live in `tests.rs`.
//!
//! - [`providers`] — Provider/model types, model discovery, pricing helpers
//! - [`presets`]   — Agent presets, preset configs, tested-model list
//! - [`render`]    — Output rendering (`.env`, Docker Compose, config YAML)
//! - [`files`]     — File I/O, re-init handling, `.gitignore` management
//! - [`wizard`]    — Interactive wizard flows (provider + agent assignment)
//! - [`guidance`]  — Post-init console guidance (pricing table, next steps)

mod files;
mod guidance;
mod presets;
mod providers;
mod render;
mod wizard;

// Re-export submodule items into this module's namespace so that:
//   1. Entry-point functions below can use them unqualified.
//   2. `tests.rs` can reach everything via `use super::*`.
use files::*;
use guidance::*;
use presets::*;
use providers::*;
use render::*;
use wizard::*;

// Public API: reusable rendering functions for other crates (e.g. `nsed init`).
pub use render::render_default_orchestrator_config;

use crate::brand;
use anyhow::{Context, Result};
use inquire::{Confirm, InquireError, Select, Text};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

// ── Public types for agent setup ─────────────────────────────────────────

/// Summary of a created agent — enough info for policy assignment and display.
#[derive(Debug, Clone)]
pub struct AgentSummary {
    pub name: String,
    pub capability_tags: Vec<String>,
    pub model_name: String,
    pub provider_id: String,
}

/// Result of the interactive agent setup wizard.
#[derive(Debug)]
pub struct AgentSetupResult {
    /// Rendered `config/agent.yml` content. Caller decides where to write it.
    pub agent_config_yaml: String,
    /// Created agents with summary info for policy assignment.
    pub agents: Vec<AgentSummary>,
}

/// Display detected exec tools with check/cross indicators.
fn print_exec_tools_status(exec_tools: &[DetectedTool]) {
    for tool in exec_tools {
        println!(
            "  {} {} found ({})",
            brand::teal("✓"),
            brand::teal(tool.name),
            tool.version,
        );
    }
    for name in &["claude", "python3", "docker"] {
        if !exec_tools.iter().any(|t| t.name == *name) {
            println!("  {} {} not found", brand::dim("✗"), brand::dim(name));
        }
    }
}

/// Run the interactive agent setup wizard (provider discovery → agent presets
/// → model assignment). Returns `None` if the user cancels.
///
/// Does NOT write files — the caller decides where to save `agent_config_yaml`.
pub async fn run_agent_setup(
    orchestrator_url: &str,
    bearer_ref: &str,
) -> Result<Option<AgentSetupResult>> {
    brand::section("provider discovery");
    brand::info("Checking for local Ollama instance…");
    let exec_tools = detect_exec_tools();
    let ollama_models = detect_ollama().await;

    print_exec_tools_status(&exec_tools);

    let providers = match wizard_providers(&ollama_models, &exec_tools, &HashMap::new()).await? {
        Some(p) => p,
        None => return Ok(None),
    };

    let agents = match wizard_agents(&providers, &exec_tools)? {
        Some(a) => a,
        None => return Ok(None),
    };

    if agents.is_empty() {
        return Ok(Some(AgentSetupResult {
            agent_config_yaml: String::new(),
            agents: Vec::new(),
        }));
    }

    let agent_config_yaml = render_agent_config(orchestrator_url, bearer_ref, &providers, &agents);

    let summaries: Vec<AgentSummary> = agents
        .iter()
        .map(|a| AgentSummary {
            name: a.name.clone(),
            capability_tags: a.capability_tags.clone(),
            model_name: a.model_name.clone(),
            provider_id: a.provider_id.clone(),
        })
        .collect();

    Ok(Some(AgentSetupResult {
        agent_config_yaml,
        agents: summaries,
    }))
}

// ── Cancellation helper ────────────────────────────────────────────────────

/// Wraps an `inquire` result: cancellation → `Ok(None)`, other errors bubble.
fn ask<T>(r: std::result::Result<T, InquireError>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ── Content generation (extracted for testability) ─────────────────────────

/// Generate all output content (compose, env, config files) for a given persona.
///
/// Returns `(compose_content, env_content, configs)`.
#[allow(clippy::too_many_arguments)]
fn generate_output_content(
    persona: &str,
    app_port: u16,
    nats_port: u16,
    api_secret: &str,
    worker_secret: &str,
    account_seed: &str,
    providers: &[Provider],
    agents: &[AgentSlot],
    operator_name: Option<&str>,
) -> (String, String, ConfigFiles) {
    if persona == "owner" {
        let compose = render_compose_owner(app_port, nats_port, providers, agents);
        let env = render_env(
            "owner",
            app_port,
            nats_port,
            api_secret,
            worker_secret,
            account_seed,
            providers,
            None,
            None, // owner doesn't use operator_name
        );
        let agent_cfg = if !agents.is_empty() {
            let orch_url = format!("http://nsed:${{APP_PORT:-{app_port}}}");
            Some(render_agent_config(
                &orch_url,
                "${NSED_BEARER_TOKEN}",
                providers,
                agents,
            ))
        } else {
            None
        };
        let orch_cfg = Some(render_orchestrator_config(app_port, nats_port, providers));
        (
            compose,
            env,
            ConfigFiles {
                agent_config: agent_cfg,
                orchestrator_config: orch_cfg,
            },
        )
    } else {
        let orch_url = account_seed;
        let compose = render_compose_worker(agents, providers);
        let env = render_env(
            "worker",
            app_port,
            nats_port,
            api_secret,
            worker_secret,
            "",
            providers,
            Some(orch_url),
            operator_name,
        );
        let agent_cfg = if !agents.is_empty() {
            Some(render_agent_config(
                orch_url,
                "${NSED_BEARER_TOKEN}",
                providers,
                agents,
            ))
        } else {
            None
        };
        (
            compose,
            env,
            ConfigFiles {
                agent_config: agent_cfg,
                orchestrator_config: None,
            },
        )
    }
}

/// Print the write summary after files have been written.
fn print_write_summary(
    output_dir: &Path,
    compose_name: &str,
    env_skipped: bool,
    configs: &ConfigFiles,
) {
    brand::section("done");
    brand::success(&format!(
        "{} written to {}",
        brand::teal(compose_name),
        brand::teal(output_dir.display().to_string().as_str())
    ));
    if env_skipped {
        brand::info(".env merged — secret values preserved from existing file.");
    } else {
        brand::success(&format!("{} written", brand::teal(".env")));
    }
    if configs.agent_config.is_some() {
        brand::success(&format!("{} written", brand::teal("config/agent.yml")));
    }
    if configs.orchestrator_config.is_some() {
        brand::success(&format!(
            "{} written",
            brand::teal("config/orchestrator.yml")
        ));
    }
    println!();
}

fn print_next_steps(
    persona: &str,
    compose_name: &str,
    app_port: u16,
    api_secret: &str,
    worker_secret: &str,
    agents: &[AgentSlot],
    output_dir: &Path,
) {
    let cf = compose_flag(compose_name);

    // If output_dir is not ".", show a cd hint
    let dir_display = output_dir.display().to_string();
    let show_cd = dir_display != "." && !dir_display.is_empty();

    println!("  {} Next steps:", brand::dim("❯"));
    println!();
    if show_cd {
        println!("  {} Change to your project directory:", brand::dim("0."));
        println!(
            "       {}",
            brand::teal(&format!("cd {}", shell_quote(&dir_display)))
        );
        println!();
    }
    println!("  {} Start your stack:", brand::dim("1."));
    println!(
        "       {}",
        brand::teal(&format!("docker compose{cf} up -d"))
    );
    println!();
    println!("  {} Verify it's running:", brand::dim("2."));
    println!("       {}", brand::teal(&format!("docker compose{cf} ps")));
    println!();
    if persona == "owner" {
        println!("  {} Open in your browser:", brand::dim("3."));
        println!(
            "       Dashboard  {}",
            // TODO(slop): inline placeholder URL (localhost / example.com / YOUR_...) — route through config / env before shipping
            brand::teal(&format!("http://localhost:{app_port}/dashboard"))
        );
        println!(
            "       Swagger UI {}",
            // TODO(slop): inline placeholder URL (localhost / example.com / YOUR_...) — route through config / env before shipping
            brand::teal(&format!("http://localhost:{app_port}/swagger-ui/"))
        );
        println!();
        print_share_with_workers(app_port, worker_secret);
        print_list_agents(app_port, api_secret);
        print_submit_guidance(app_port, api_secret, agents);
        print_security_notice();
    } else {
        println!("  {} Check agent logs:", brand::dim("3."));
        println!(
            "       {}",
            brand::teal(&format!("docker compose{cf} logs -f"))
        );
    }
    println!();
}

// ── Main entry point ───────────────────────────────────────────────────────

/// Run the interactive setup wizard (full mode — persona choice, orchestrator config, providers, agents).
///
/// `output_dir`  — directory where `docker-compose.yml` and `.env` are written (default: `.`)
/// `worker_url`  — when `Some`, skip the orchestrator phase and configure worker-only output
/// `auth_token`  — bearer token for worker mode; when `Some`, skips the interactive token prompt
pub async fn run_owner(
    output_dir: PathBuf,
    worker_url: Option<String>,
    auth_token: Option<String>,
) -> Result<()> {
    brand::print_banner();

    let rc = brand::render_config();

    // ── Output file name ─────────────────────────────────────────────────
    let Some(compose_name) = ask(Text::new("Compose file name:")
        .with_default("docker-compose.yml")
        .with_render_config(rc)
        .prompt())?
    else {
        println!("\n{}", brand::dim("  Cancelled. No files written."));
        return Ok(());
    };

    // ── Check for existing files ──────────────────────────────────────────
    let reinit = check_existing(&output_dir, &compose_name)?;
    let preserved_env: HashMap<String, String> = match &reinit {
        ReinitChoice::Cancel => {
            println!("\n{}", brand::dim("  Cancelled. No files written."));
            return Ok(());
        }
        ReinitChoice::Merge(existing) => existing.clone(),
        ReinitChoice::Overwrite | ReinitChoice::MakeNew => HashMap::new(),
    };

    // ── Persona ───────────────────────────────────────────────────────────
    let (persona, include_agents) = if worker_url.is_some() {
        ("worker", true)
    } else {
        brand::section("setup");
        let persona_opts = vec![
            "Workspace Owner + Agents — orchestrator, NATS, and agent containers",
            "Workspace Owner only     — orchestrator + NATS (add agents later)",
            "Worker Node              — agent containers for an existing workspace",
        ];
        let Some(choice) = ask(Select::new("What do you want to set up?", persona_opts)
            .with_render_config(rc)
            .prompt())?
        else {
            println!("\n{}", brand::dim("  Cancelled. No files written."));
            return Ok(());
        };
        if choice.starts_with("Worker") {
            ("worker", true)
        } else if choice.contains("only") {
            ("owner", false)
        } else {
            ("owner", true)
        }
    };

    // ── Owner: orchestrator config ────────────────────────────────────────
    let (app_port, nats_port, api_secret, worker_secret, account_seed) = if persona == "owner" {
        brand::section("orchestrator");

        let app_port: u16 = loop {
            let Some(port_str) = ask(Text::new("API port:")
                .with_default("8080")
                .with_render_config(rc)
                .prompt())?
            else {
                println!("\n{}", brand::dim("  Cancelled. No files written."));
                return Ok(());
            };
            match port_str.parse::<u16>() {
                Ok(p) if p > 0 => break p,
                _ => {
                    brand::warn(&format!(
                        "Invalid port '{port_str}' — must be 1–65535. Try again."
                    ));
                }
            }
        };

        let nats_port: u16 = loop {
            let Some(nats_str) = ask(Text::new("NATS port (embedded):")
                .with_default("4222")
                .with_render_config(rc)
                .prompt())?
            else {
                println!("\n{}", brand::dim("  Cancelled. No files written."));
                return Ok(());
            };
            match nats_str.parse::<u16>() {
                Ok(p) if p > 0 => {
                    if p == app_port {
                        brand::warn(&format!(
                            "Port {p} is already used for the API. Pick a different port."
                        ));
                        continue;
                    }
                    break p;
                }
                _ => {
                    brand::warn(&format!(
                        "Invalid port '{nats_str}' — must be 1–65535. Try again."
                    ));
                }
            }
        };

        // Reuse or generate admin API secret
        let api_secret = preserved_env
            .get("APP_AUTH__TOKENS__0__SECRET")
            .cloned()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Reuse or generate worker token (user role only, safe to share)
        let worker_secret = preserved_env
            .get("APP_AUTH__TOKENS__1__SECRET")
            .cloned()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let account_seed = if let Some(existing) =
            preserved_env.get("APP_CREDENTIALS__ACCOUNT_SEED")
        {
            let preview: String = existing.chars().take(12).collect();
            brand::info(&format!(
                "Reusing account seed from existing .env ({preview}…)",
            ));
            existing.clone()
        } else {
            let seed_opts = vec![
                "Generate new  — create a fresh NKey account seed",
                "Paste existing — I already have a seed from another deployment",
            ];
            let Some(seed_choice) = ask(Select::new("NKey account seed:", seed_opts)
                .with_render_config(rc)
                .prompt())?
            else {
                println!("\n{}", brand::dim("  Cancelled. No files written."));
                return Ok(());
            };

            if seed_choice.starts_with("Paste") {
                loop {
                    let Some(pasted) = ask(Text::new("Account seed (starts with SA…):")
                        .with_render_config(rc)
                        .prompt())?
                    else {
                        println!("\n{}", brand::dim("  Cancelled. No files written."));
                        return Ok(());
                    };
                    let trimmed = pasted.trim();
                    match nkeys::KeyPair::from_seed(trimmed) {
                        Ok(_) => break trimmed.to_string(),
                        Err(e) => {
                            brand::warn(&format!("Invalid NKey seed: {e}"));
                            brand::warn("Account seeds start with 'SA' and are ~58 characters.");
                            continue;
                        }
                    }
                }
            } else {
                let generated = nkeys::KeyPair::new_account()
                    .seed()
                    .context("failed to generate NKey account seed")?;
                println!();
                brand::warn("Save this seed — if it changes, all agents must re-authenticate:");
                println!("  {}", brand::teal(&generated));
                println!();
                generated
            }
        };

        (app_port, nats_port, api_secret, worker_secret, account_seed)
    } else {
        let orchestrator_url = if let Some(url) = worker_url.clone() {
            url
        } else {
            let Some(url) = ask(Text::new("Orchestrator URL:")
                // TODO(slop): inline placeholder URL (localhost / example.com / YOUR_...) — route through config / env before shipping
                .with_default("http://localhost:8080")
                .with_render_config(rc)
                .prompt())?
            else {
                println!("\n{}", brand::dim("  Cancelled. No files written."));
                return Ok(());
            };
            url
        };

        let token = if let Some(t) = auth_token.clone() {
            let trimmed = t.trim().to_string();
            if trimmed.is_empty() {
                brand::warn("--auth token is empty; please provide a non-empty token.");
                return Ok(());
            }
            brand::success(&format!("Using token from {}", brand::teal("--auth")));
            trimmed
        } else {
            loop {
                let Some(t) = ask(Text::new(
                    "Worker token (APP_AUTH__TOKENS__1__SECRET from owner's .env):",
                )
                .with_render_config(rc)
                .prompt())?
                else {
                    println!("\n{}", brand::dim("  Cancelled. No files written."));
                    return Ok(());
                };
                let trimmed = t.trim().to_string();
                if trimmed.is_empty() {
                    brand::warn("Token cannot be empty.");
                    continue;
                }
                break trimmed;
            }
        };

        (8080_u16, 4222_u16, token.clone(), token, orchestrator_url)
    };

    // ── Phase 2: Providers ────────────────────────────────────────────────
    let (providers, agents) = if include_agents {
        brand::section("provider discovery");
        brand::info("Checking for local Ollama instance…");
        let exec_tools = detect_exec_tools();
        let ollama_models = detect_ollama().await;
        for tool in &exec_tools {
            println!(
                "  {} {} found ({})",
                brand::teal("✓"),
                brand::teal(tool.name),
                tool.version,
            );
        }
        for name in &["claude", "python3", "docker"] {
            if !exec_tools.iter().any(|t| t.name == *name) {
                println!("  {} {} not found", brand::dim("✗"), brand::dim(name));
            }
        }

        let provs = match wizard_providers(&ollama_models, &exec_tools, &preserved_env).await? {
            Some(p) => p,
            None => {
                println!("\n{}", brand::dim("  Cancelled. No files written."));
                return Ok(());
            }
        };

        // ── Phase 3: Agent assignment ─────────────────────────────────────
        let agts = match wizard_agents(&provs, &exec_tools)? {
            Some(a) => a,
            None => {
                println!("\n{}", brand::dim("  Cancelled. No files written."));
                return Ok(());
            }
        };

        // ── Cost estimate ─────────────────────────────────────────────────
        print_pricing_table(&agts);

        (provs, agts)
    } else {
        (Vec::new(), Vec::new())
    };

    // ── Confirm ───────────────────────────────────────────────────────────
    brand::section("summary");
    println!(
        "  Will write to {}:",
        brand::teal(output_dir.display().to_string().as_str())
    );
    let svc_count = agents.len() + if persona == "owner" { 1 } else { 0 };
    println!(
        "    {} {}  ({} service(s))",
        brand::teal("→"),
        compose_name,
        svc_count
    );
    println!("    {} .env", brand::teal("→"));
    println!("    {} .gitignore  (updated)", brand::teal("→"));
    if !agents.is_empty() {
        println!("    {} config/agent.yml", brand::teal("→"));
    }
    if persona == "owner" {
        println!("    {} config/orchestrator.yml", brand::teal("→"));
    }
    println!();

    let Some(confirmed) = ask(Confirm::new("Write these files?")
        .with_default(true)
        .with_render_config(rc)
        .prompt())?
    else {
        println!("\n{}", brand::dim("  Cancelled. No files written."));
        return Ok(());
    };
    if !confirmed {
        println!("\n{}", brand::dim("  Cancelled. No files written."));
        return Ok(());
    }

    // ── Generate content ──────────────────────────────────────────────────
    let (compose_content, env_content, configs) = generate_output_content(
        persona,
        app_port,
        nats_port,
        &api_secret,
        &worker_secret,
        &account_seed,
        &providers,
        &agents,
        None, // owner persona doesn't use operator_name
    );

    // ── Write ─────────────────────────────────────────────────────────────
    let env_skipped = write_files(
        &output_dir,
        &compose_name,
        &compose_content,
        &env_content,
        &preserved_env,
        &configs,
    )?;

    // ── Next steps ────────────────────────────────────────────────────────
    print_write_summary(&output_dir, &compose_name, env_skipped, &configs);
    print_next_steps(
        persona,
        &compose_name,
        app_port,
        &api_secret,
        &worker_secret,
        &agents,
        &output_dir,
    );

    Ok(())
}

/// Worker-only setup wizard (`nsed-agent init --orchestrator <url>`).
///
/// Skips persona choice and orchestrator configuration — goes straight to
/// bearer token → provider discovery → agent assignment → worker compose.
///
/// `output_dir`       — directory where `docker-compose.yml` and `.env` are written
/// `orchestrator_url` — the orchestrator URL (required for worker mode)
/// `auth_token`       — bearer token; when `Some`, skips the interactive prompt
pub async fn run_worker(
    output_dir: PathBuf,
    orchestrator_url: String,
    auth_token: Option<String>,
) -> Result<()> {
    brand::print_banner();

    let rc = brand::render_config();

    // ── Output file name ─────────────────────────────────────────────────
    let Some(compose_name) = ask(Text::new("Compose file name:")
        .with_default("docker-compose.yml")
        .with_render_config(rc)
        .prompt())?
    else {
        println!("\n{}", brand::dim("  Cancelled. No files written."));
        return Ok(());
    };

    // ── Check for existing files ──────────────────────────────────────────
    let reinit = check_existing(&output_dir, &compose_name)?;
    let preserved_env: HashMap<String, String> = match &reinit {
        ReinitChoice::Cancel => {
            println!("\n{}", brand::dim("  Cancelled. No files written."));
            return Ok(());
        }
        ReinitChoice::Merge(existing) => existing.clone(),
        ReinitChoice::Overwrite | ReinitChoice::MakeNew => HashMap::new(),
    };

    // ── Bearer token ──────────────────────────────────────────────────────
    brand::section("worker setup");
    brand::info(&format!("Orchestrator: {}", brand::teal(&orchestrator_url)));
    println!();

    let token = if let Some(t) = auth_token {
        let trimmed = t.trim().to_string();
        if trimmed.is_empty() {
            brand::warn("--auth token is empty; please provide a non-empty token.");
            return Ok(());
        }
        brand::success(&format!("Using token from {}", brand::teal("--auth")));
        trimmed
    } else {
        loop {
            let Some(t) = ask(Text::new(
                "Worker token (APP_AUTH__TOKENS__1__SECRET from owner's .env):",
            )
            .with_render_config(rc)
            .prompt())?
            else {
                println!("\n{}", brand::dim("  Cancelled. No files written."));
                return Ok(());
            };
            let trimmed = t.trim().to_string();
            if trimmed.is_empty() {
                brand::warn("Token cannot be empty.");
                continue;
            }
            break trimmed;
        }
    };

    let Some(operator_raw) = ask(Text::new(
        "Operator display name (optional, shown in dashboard):",
    )
    .with_default("")
    .with_render_config(rc)
    .prompt())?
    else {
        println!("\n{}", brand::dim("  Cancelled. No files written."));
        return Ok(());
    };
    let operator_name: Option<String> = if operator_raw.trim().is_empty() {
        None
    } else {
        Some(operator_raw.trim().to_string())
    };
    if let Some(ref name) = operator_name {
        brand::success(&format!("Operator name: {}", brand::teal(name)));
    }
    println!();

    // ── Provider discovery ────────────────────────────────────────────────
    brand::section("provider discovery");
    brand::info("Checking for local Ollama instance…");
    let exec_tools = detect_exec_tools();
    let ollama_models = detect_ollama().await;
    for tool in &exec_tools {
        println!(
            "  {} {} found ({})",
            brand::teal("✓"),
            brand::teal(tool.name),
            tool.version,
        );
    }
    for name in &["claude", "python3", "docker"] {
        if !exec_tools.iter().any(|t| t.name == *name) {
            println!("  {} {} not found", brand::dim("✗"), brand::dim(name));
        }
    }

    let providers = match wizard_providers(&ollama_models, &exec_tools, &preserved_env).await? {
        Some(p) => p,
        None => {
            println!("\n{}", brand::dim("  Cancelled. No files written."));
            return Ok(());
        }
    };

    // ── Agent assignment ──────────────────────────────────────────────────
    let agents = match wizard_agents(&providers, &exec_tools)? {
        Some(a) => a,
        None => {
            println!("\n{}", brand::dim("  Cancelled. No files written."));
            return Ok(());
        }
    };

    // ── Cost estimate ─────────────────────────────────────────────────────
    print_pricing_table(&agents);

    // ── Confirm ───────────────────────────────────────────────────────────
    brand::section("summary");
    println!(
        "  Will write to {}:",
        brand::teal(output_dir.display().to_string().as_str())
    );
    println!(
        "    {} {}  ({} service(s))",
        brand::teal("→"),
        compose_name,
        agents.len()
    );
    println!("    {} .env", brand::teal("→"));
    println!("    {} .gitignore  (updated)", brand::teal("→"));
    if !agents.is_empty() {
        println!("    {} config/agent.yml", brand::teal("→"));
    }
    println!();

    let Some(confirmed) = ask(Confirm::new("Write these files?")
        .with_default(true)
        .with_render_config(rc)
        .prompt())?
    else {
        println!("\n{}", brand::dim("  Cancelled. No files written."));
        return Ok(());
    };
    if !confirmed {
        println!("\n{}", brand::dim("  Cancelled. No files written."));
        return Ok(());
    }

    // ── Generate content ──────────────────────────────────────────────────
    let (compose_content, env_content, configs) = generate_output_content(
        "worker",
        8080,
        4222,
        &token,
        &token,
        &orchestrator_url,
        &providers,
        &agents,
        operator_name.as_deref(),
    );

    // ── Write ─────────────────────────────────────────────────────────────
    let env_skipped = write_files(
        &output_dir,
        &compose_name,
        &compose_content,
        &env_content,
        &preserved_env,
        &configs,
    )?;

    // ── Next steps ────────────────────────────────────────────────────────
    print_write_summary(&output_dir, &compose_name, env_skipped, &configs);
    print_next_steps(
        "worker",
        &compose_name,
        8080,
        &token,
        &token,
        &agents,
        &output_dir,
    );

    Ok(())
}

#[cfg(test)]
mod tests;
