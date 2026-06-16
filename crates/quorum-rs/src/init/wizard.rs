// ── Interactive wizard functions ────────────────────────────────────────────

use std::collections::HashMap;

use anyhow::Result;
use inquire::{Confirm, MultiSelect, Select, Text};

use super::ask;
use super::presets::{AGENT_PRESETS, AgentSlot, is_tested_model};
use super::providers::{
    DetectedTool, FetchedModel, ModelInfo, Provider, build_claude_exec_provider,
    build_exec_provider, build_ollama_provider, build_openai_oauth_provider,
    build_provider_env_key, build_simulated_provider, derive_provider_pricing, fetch_models,
    fetched_to_model_infos, sanitize_provider_id,
};
use crate::brand;
use crate::llms::openai_codex::{OpenAICodexAuthStore, login_and_store_openai_codex_device_code};

/// Split a command string into tokens, respecting single and double quotes.
///
/// Handles basic quoting (e.g. `python3 "my agent.py"` → `["python3", "my agent.py"]`)
/// without full shell expansion. Sufficient for exec provider commands.
///
/// Returns `Err` when the input has an unterminated quote — previously
/// the function silently returned partial tokens, which let a typo
/// like `python3 "agent.py` produce a single-token command of
/// `["python3", "agent.py"]` with the rest of the line glued on.
fn split_shell_command(cmd: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for ch in cmd.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if in_single {
        return Err(format!("unterminated single quote in command: {cmd}"));
    }
    if in_double {
        return Err(format!("unterminated double quote in command: {cmd}"));
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Resolve `${VAR}` references against process environment (which includes
/// values loaded from `.env` by `dotenvy`).  Returns the raw string unchanged
/// if it doesn't match the `${…}` pattern.
fn resolve_env_ref(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix("${")?.strip_suffix('}')?;
    if inner.is_empty() {
        return None;
    }
    std::env::var(inner).ok()
}

/// Load `.env` once (best-effort, errors silently if missing).
fn try_load_dotenv() {
    let _ = dotenvy::dotenv();
}

// ── Phase 2: provider configuration ───────────────────────────────────────

async fn ensure_openai_oauth_auth() -> Result<bool> {
    let rc = brand::render_config();
    let store = OpenAICodexAuthStore::default()?;
    if store.read()?.is_some() {
        brand::success(&format!(
            "OpenAI OAuth auth found at {}.",
            store.path().display()
        ));
        return Ok(true);
    }

    if store.import_from_codex_cli()? {
        brand::success(&format!(
            "Imported OpenAI OAuth auth from Codex CLI into {}.",
            store.path().display()
        ));
        return Ok(true);
    }

    let Some(sign_in) = ask(Confirm::new("Sign in to OpenAI OAuth now?")
        .with_default(true)
        .with_render_config(rc)
        .prompt())?
    else {
        return Ok(false);
    };
    if !sign_in {
        return Ok(false);
    }

    login_and_store_openai_codex_device_code(&store, |prompt| async move {
        println!();
        println!("Open this URL in your browser:");
        println!("  {}", prompt.verification_url);
        println!();
        println!("Enter this code:");
        println!("  {}", prompt.user_code);
        println!();
        println!("Waiting for OpenAI authorization...");
    })
    .await?;
    brand::success(&format!(
        "OpenAI OAuth auth saved to {}.",
        store.path().display()
    ));
    Ok(true)
}

pub(super) async fn wizard_providers(
    ollama_models: &Option<Vec<FetchedModel>>,
    exec_tools: &[DetectedTool],
    preserved: &HashMap<String, String>,
) -> Result<Option<Vec<Provider>>> {
    let rc = brand::render_config();

    // Pre-fill the first provider if Ollama is running locally
    let mut providers: Vec<Provider> = Vec::new();
    if let Some(ref models) = *ollama_models {
        println!(
            "  {} Ollama detected at {} ({} model(s) available).",
            brand::teal("✓"),
            // TODO(slop): inline placeholder URL (localhost / example.com / YOUR_...) — route through config / env before shipping
            brand::teal("http://localhost:11434"),
            models.len()
        );
        let add_ollama = match ask(Confirm::new("Pre-fill Ollama as the first provider?")
            .with_default(true)
            .with_render_config(rc)
            .prompt())?
        {
            Some(v) => v,
            None => return Ok(None),
        };
        if add_ollama {
            providers.push(build_ollama_provider(models));
        }
    }

    // Auto-offer Claude CLI as exec provider if detected
    let has_claude = exec_tools.iter().any(|t| t.name == "claude");
    if has_claude && !providers.iter().any(|p| p.provider_type == "exec") {
        let add_claude = match ask(Confirm::new("Add Claude CLI as agent provider?")
            .with_default(true)
            .with_render_config(rc)
            .prompt())?
        {
            Some(v) => v,
            None => return Ok(None),
        };
        if add_claude {
            providers.push(build_claude_exec_provider());
            brand::success("Claude CLI provider added (exec, subscription pricing).");
        }
    }

    // Provider-type menu
    let type_opts = vec![
        "Together AI   (https://api.together.xyz/v1)",
        "OpenRouter    (https://openrouter.ai/api/v1)",
        "Ollama        (local, http://localhost:11434/v1)",
        "OpenAI        (https://api.openai.com/v1)",
        "OpenAI OAuth  (ChatGPT/Codex subscription)",
        "OpenAI-compatible  (enter URL manually)",
        "Exec          (external process, e.g. Python/Claude CLI)",
        "Simulated     (testing/CI, no real LLM calls)",
        "Done — no more providers",
    ];

    loop {
        brand::section(&format!(
            "providers  {}",
            brand::dim(&format!("({} configured)", providers.len()))
        ));

        let Some(choice) = ask(Select::new("Add a provider:", type_opts.clone())
            .with_render_config(rc)
            .prompt())?
        else {
            return Ok(None);
        };

        if choice.starts_with("Done") {
            if providers.is_empty() {
                brand::warn("At least one provider is required.");
                continue;
            }
            break;
        }

        if choice.starts_with("Simulated") {
            if providers.iter().any(|p| p.provider_type == "simulated") {
                brand::warn("A simulated provider already exists — skipping.");
            } else {
                providers.push(build_simulated_provider());
                brand::success("Simulated provider added.");
            }
            continue;
        }

        if choice.starts_with("Exec") {
            if providers.iter().any(|p| p.provider_type == "exec") {
                brand::warn("An exec provider already exists — skipping.");
            } else {
                providers.push(build_exec_provider());
                brand::success(
                    "Exec provider added. Configure command per agent in the next step.",
                );
            }
            continue;
        }

        if choice.starts_with("OpenAI OAuth") {
            if providers.iter().any(|p| p.provider_type == "openai-oauth") {
                brand::warn("An OpenAI OAuth provider already exists — skipping.");
                continue;
            }
            match ensure_openai_oauth_auth().await? {
                true => {
                    providers.push(build_openai_oauth_provider());
                    brand::success("OpenAI OAuth provider added.");
                }
                false => {
                    brand::warn("OpenAI OAuth provider skipped.");
                }
            }
            continue;
        }

        // Determine defaults for each preset
        let (default_url, needs_key, preset_id) = resolve_provider_preset(choice);

        // Provider ID
        let suggested_id = suggest_provider_id(preset_id, &providers);
        let id = loop {
            let Some(raw_candidate) = ask(Text::new("Provider ID (slug):")
                .with_default(&suggested_id)
                .with_render_config(rc)
                .prompt())?
            else {
                return Ok(None);
            };
            let candidate = match sanitize_provider_id(&raw_candidate) {
                Some(s) => s,
                None => {
                    brand::warn("Provider ID must contain at least one alphanumeric character.");
                    continue;
                }
            };
            if candidate != raw_candidate {
                println!(
                    "  {} Normalized provider ID to \"{}\"",
                    brand::dim("ℹ"),
                    candidate
                );
            }
            if providers.iter().any(|p| p.id == candidate) {
                brand::warn(&format!(
                    "Provider ID \"{candidate}\" already exists — pick another."
                ));
                continue;
            }
            break candidate;
        };

        // Base URL — must be a valid http(s) URL
        let base_url = loop {
            let Some(url_input) = ask(Text::new("Base URL:")
                .with_default(default_url)
                .with_render_config(rc)
                .prompt())?
            else {
                return Ok(None);
            };
            let trimmed = url_input.trim().to_string();
            if trimmed.is_empty() {
                brand::warn("Base URL cannot be empty.");
                continue;
            }
            if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
                brand::warn("Base URL must start with http:// or https://");
                continue;
            }
            break trimmed;
        };

        let (api_key, resolved_key) = if needs_key {
            let env_key = build_provider_env_key(&id);
            let preserved_key = preserved.get(&env_key).cloned();

            if let Some(ref existing) = preserved_key {
                println!(
                    "  {} Existing key for {} preserved.",
                    brand::teal("✓"),
                    brand::teal(&id)
                );
                // Resolve ${VAR} references so fetch_models gets the real key
                try_load_dotenv();
                let resolved = resolve_env_ref(existing).unwrap_or_else(|| existing.clone());
                (Some(existing.clone()), Some(resolved))
            } else {
                // Load .env so ${VAR} references can resolve
                try_load_dotenv();

                let default_env_var = format!("${{{env_key}}}");
                let raw = match ask(Text::new("API key (or ${ENV_VAR}):")
                    .with_default(&default_env_var)
                    .with_help_message(
                        "Enter key directly, or ${VAR} to read from environment / .env",
                    )
                    .with_render_config(rc)
                    .prompt())?
                {
                    Some(v) => v.trim().to_string(),
                    None => return Ok(None),
                };
                if raw.is_empty() {
                    (None, None)
                } else if let Some(resolved) = resolve_env_ref(&raw) {
                    brand::success(&format!("Resolved {} from environment.", brand::teal(&raw)));
                    // Store the ${VAR} reference in config, use resolved value for model fetch
                    (Some(raw), Some(resolved))
                } else if raw.starts_with("${") {
                    brand::warn(&format!(
                        "{} not found in environment — models will be entered manually.",
                        raw
                    ));
                    (Some(raw), None)
                } else {
                    (Some(raw.clone()), Some(raw))
                }
            }
        } else {
            (None, None)
        };

        // Fetch models for pricing (non-fatal) — use resolved key for API auth
        brand::info("Fetching available models…");
        let fetched = fetch_models(&base_url, resolved_key.as_deref()).await;

        let (input_price, output_price) =
            derive_provider_pricing(choice.starts_with("Ollama"), &fetched);

        if fetched.is_some() {
            let n = fetched.as_ref().map(|v| v.len()).unwrap_or(0);
            brand::success(&format!("{n} models discovered."));
        } else {
            brand::warn("Could not reach provider endpoint — pricing shown as unknown.");
        }

        let model_infos: Vec<ModelInfo> = fetched
            .as_ref()
            .map(|m| fetched_to_model_infos(m))
            .unwrap_or_default();

        providers.push(Provider {
            id,
            base_url,
            api_key,
            input_price,
            output_price,
            provider_type: "openai".to_string(),
            models: model_infos,
            engine: None,
        });
    }

    Ok(Some(providers))
}

// ── Phase 3: agent assignment ──────────────────────────────────────────────

/// `(display_string, provider_id, model_name, input_price, output_price)`.
pub(super) type ModelOption = (String, String, String, Option<f64>, Option<f64>);

/// Build a combined model list across all providers, tested models sorted first.
///
/// Returns a list of `ModelOption` tuples suitable for display in a picker UI.
pub(super) fn build_model_options(providers: &[Provider]) -> Vec<ModelOption> {
    let mut tested_options: Vec<ModelOption> = Vec::new();
    let mut other_options: Vec<ModelOption> = Vec::new();
    for p in providers {
        if p.provider_type == "exec" {
            // Exec providers show model name with subscription tag
            for mi in &p.models {
                let label = if p.id == "claude_cli" {
                    format!(
                        "{} {}  {}",
                        // TODO(slop): emoji in source — almost never deliberate in code
                        brand::teal("⚡"),
                        brand::teal("Claude CLI"),
                        brand::dim(&format!("({})  subscription", p.id)),
                    )
                } else {
                    format!(
                        "  {} (exec)  {}",
                        mi.name,
                        brand::dim(&format!("({})", p.id)),
                    )
                };
                if p.id == "claude_cli" {
                    tested_options.push((
                        label,
                        p.id.clone(),
                        mi.name.clone(),
                        mi.input_price,
                        mi.output_price,
                    ));
                } else {
                    other_options.push((
                        label,
                        p.id.clone(),
                        mi.name.clone(),
                        mi.input_price,
                        mi.output_price,
                    ));
                }
            }
        } else if p.provider_type == "simulated" {
            other_options.push((
                format!("simulated  {}", brand::dim(&format!("({})", p.id))),
                p.id.clone(),
                "simulated-default".to_string(),
                Some(0.0),
                Some(0.0),
            ));
        } else if p.models.is_empty() {
            other_options.push((
                format!(
                    "(enter model manually)  {}",
                    brand::dim(&format!("({})", p.id))
                ),
                p.id.clone(),
                String::new(),
                p.input_price,
                p.output_price,
            ));
        } else {
            for mi in &p.models {
                let price_tag = match (mi.input_price, mi.output_price) {
                    (Some(i), Some(o)) if i == 0.0 && o == 0.0 => brand::dim("free"),
                    (Some(i), Some(o)) => brand::dim(&format!("${:.2}/${:.2}", i, o)),
                    _ => String::new(),
                };
                let suffix = if price_tag.is_empty() {
                    brand::dim(&format!("({})", p.id))
                } else {
                    brand::dim(&format!("({})  {}", p.id, price_tag))
                };
                if is_tested_model(&mi.name) {
                    tested_options.push((
                        format!(
                            "{} {}  {}",
                            brand::green("✓"),
                            brand::green(&mi.name),
                            suffix
                        ),
                        p.id.clone(),
                        mi.name.clone(),
                        mi.input_price,
                        mi.output_price,
                    ));
                } else {
                    other_options.push((
                        format!("  {}  {}", mi.name, suffix),
                        p.id.clone(),
                        mi.name.clone(),
                        mi.input_price,
                        mi.output_price,
                    ));
                }
            }
        }
    }
    let mut model_options: Vec<ModelOption> = Vec::new();
    model_options.append(&mut tested_options);
    model_options.append(&mut other_options);
    model_options
}

pub(super) fn build_persona_options() -> Vec<String> {
    AGENT_PRESETS
        .iter()
        .map(|a| format!("{:<10} {} {}", a.name, brand::dim("—"), brand::dim(a.desc)))
        .collect()
}

/// Map selected display strings back to preset names.
pub(super) fn resolve_selected_names(selected: &[String], options: &[String]) -> Vec<String> {
    selected
        .iter()
        .filter_map(|display| {
            let idx = options.iter().position(|o| o == display)?;
            Some(AGENT_PRESETS[idx].name.to_string())
        })
        .collect()
}

/// Create a fallback DEFAULT agent when no agents were selected.
///
/// Finds the first provider with a discovered model; falls back to the first
/// provider with empty model if no models are discovered.
///
/// Returns `None` if `providers` is empty (should not happen in normal flow,
/// since the wizard requires at least one provider before reaching here).
pub(super) fn build_fallback_agent(providers: &[Provider]) -> Option<AgentSlot> {
    let first_provider = providers.first()?;
    let fallback_mi = ModelInfo {
        name: String::new(),
        input_price: None,
        output_price: None,
    };
    let (p, mi) = providers
        .iter()
        .find_map(|p| p.models.first().map(|m| (p, m)))
        .unwrap_or((first_provider, &fallback_mi));
    let mut slot = AgentSlot::new(
        "DEFAULT".to_string(),
        p.id.clone(),
        mi.name.clone(),
        mi.input_price.or(p.input_price),
        mi.output_price.or(p.output_price),
    );
    slot.apply_preset();
    Some(slot)
}

/// Check if agent diversity is low (< 3 distinct models for 3+ agents).
///
/// Returns true if a diversity warning should be shown.
pub(super) fn check_model_diversity(agents: &[AgentSlot]) -> bool {
    let distinct_models: std::collections::HashSet<_> =
        agents.iter().map(|a| &a.model_name).collect();
    agents.len() >= 3 && distinct_models.len() < 3
}

/// Resolve provider defaults from a menu choice string.
///
/// Returns `(default_url, needs_api_key, preset_id)`.
pub(super) fn resolve_provider_preset(choice: &str) -> (&'static str, bool, &'static str) {
    if choice.starts_with("Together AI") {
        ("https://api.together.xyz/v1", true, "together_ai")
    } else if choice.starts_with("OpenRouter") {
        ("https://openrouter.ai/api/v1", true, "openrouter")
    } else if choice.starts_with("Ollama") {
        // TODO(slop): inline placeholder URL (localhost / example.com / YOUR_...) — route through config / env before shipping
        ("http://localhost:11434/v1", false, "ollama_local")
    } else if choice.starts_with("OpenAI-compatible") {
        ("", true, "custom")
    } else if choice.starts_with("Simulated") {
        ("", false, "simulated")
    } else {
        // OpenAI
        ("https://api.openai.com/v1", true, "openai")
    }
}

/// Generate a unique suggested provider ID, appending a suffix if the preset
/// ID already exists in the list.
pub(super) fn suggest_provider_id(preset_id: &str, existing: &[Provider]) -> String {
    if existing.iter().any(|p| p.id == preset_id) {
        let mut suffix = existing.len() + 1;
        loop {
            let candidate = format!("{preset_id}_{suffix}");
            if !existing.iter().any(|p| p.id == candidate) {
                return candidate;
            }
            suffix += 1;
        }
    } else {
        preset_id.to_string()
    }
}

pub(super) fn wizard_agents(
    providers: &[Provider],
    exec_tools: &[DetectedTool],
) -> Result<Option<Vec<AgentSlot>>> {
    brand::section("agent assignment");
    let rc = brand::render_config();

    let model_options = build_model_options(providers);

    // Build described persona options grouped by ensemble.
    let options = build_persona_options();
    // Default: select the first 3 (General ensemble: DEFAULT, REASON, CREATE)
    let defaults: Vec<usize> = (0..3.min(AGENT_PRESETS.len())).collect();
    let Some(selected) = ask(MultiSelect::new("Select agent personas:", options.clone())
        .with_default(&defaults)
        .with_help_message("Space = toggle, Enter = confirm  ·  ✓ = integration-tested model")
        .with_render_config(rc)
        .prompt())?
    else {
        return Ok(None);
    };

    // Map selected display strings back to preset names.
    let selected_names = resolve_selected_names(&selected, &options);

    if selected_names.is_empty() {
        brand::warn("No agents selected — using DEFAULT.");
        if let Some(slot) = build_fallback_agent(providers) {
            if slot.model_name.is_empty() {
                brand::warn(
                    "No models discovered — set model_name in config/agent.yml after init.",
                );
            }
            return Ok(Some(vec![slot]));
        }
        brand::warn("No providers configured — cannot create fallback agent.");
        return Ok(Some(Vec::new()));
    }

    // For each agent, pick a model (provider follows automatically)
    let display_options: Vec<String> = model_options.iter().map(|(d, ..)| d.clone()).collect();
    let mut agents = Vec::new();
    for (i, name) in selected_names.iter().enumerate() {
        let Some(choice) = ask(Select::new(
            &format!("Model for {name}:"),
            display_options.clone(),
        )
        .with_starting_cursor(i % model_options.len())
        .with_help_message("✓ = integration-tested  ·  prices: input/output $/Mtok")
        .with_render_config(rc)
        .prompt())?
        else {
            return Ok(None);
        };

        // Find the matching entry
        // TODO(slop): `.unwrap()` / `.expect()` outside tests — propagate the error with `?` or handle it
        let entry = model_options.iter().find(|(d, ..)| *d == choice).unwrap();
        let provider_id = entry.1.clone();
        let mut model_name = entry.2.clone();
        let input_price = entry.3;
        let output_price = entry.4;

        if model_name.is_empty() {
            model_name = loop {
                let Some(manual) =
                    ask(Text::new(&format!("Model name for {name} (e.g. gpt-4o):"))
                        .with_render_config(rc)
                        .prompt())?
                else {
                    return Ok(None);
                };
                let trimmed = manual.trim().to_string();
                if trimmed.is_empty() {
                    brand::warn("Model name cannot be empty.");
                    continue;
                }
                break trimmed;
            };
        }

        let exec_command = if let Some(p) = providers
            .iter()
            .find(|p| p.id == provider_id && p.provider_type == "exec")
        {
            if p.id == "claude_cli" {
                // Claude CLI provider — pick model + optional
                // context dirs/files, then assemble the exec
                // command. Operators wanting the rich `claude:`
                // YAML block (subagents, per-tool permissions)
                // edit the file by hand after init.
                let model_opts = vec!["sonnet", "opus", "haiku"];
                let Some(model) = ask(Select::new("Claude model:", model_opts)
                    .with_help_message(
                        "haiku = cheap+fast, sonnet = balanced (default), opus = strongest",
                    )
                    .with_render_config(rc)
                    .prompt())?
                else {
                    return Ok(None);
                };

                let Some(ctx_paths_str) = ask(Text::new(&format!(
                    "Context paths for {name} (comma-separated, optional):"
                ))
                .with_default("")
                .with_help_message(
                    "Files or dirs claude reads as additional context (--add-dir). \
                     Example: ./docs, ./README.md",
                )
                .with_render_config(rc)
                .prompt())?
                else {
                    return Ok(None);
                };
                let context_paths: Vec<String> = ctx_paths_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                let mut parts: Vec<String> = vec![
                    "claude".into(),
                    "-p".into(),
                    "--output-format".into(),
                    "json".into(),
                    "--model".into(),
                    model.into(),
                    "--verbose".into(),
                ];
                for path in &context_paths {
                    parts.push("--add-dir".into());
                    parts.push(path.clone());
                }
                Some(parts)
            } else {
                // Generic exec provider — prompt for command
                let default_cmd = if exec_tools.iter().any(|t| t.name == "python3") {
                    "python3 my_agent.py"
                } else {
                    "bash agent.sh"
                };
                // Re-prompt locally until the operator supplies a
                // non-empty, properly-quoted command. Previous
                // fallback returned a phony `["echo", "hello"]`
                // which silently shipped a useless agent.
                let parts: Vec<String> = loop {
                    let Some(cmd_str) = ask(Text::new(&format!("Exec command for {name}:"))
                        .with_default(default_cmd)
                        .with_help_message(
                            "Shell-style command, e.g. \"python3 agent.py\" or \"claude -p --output-format json\"",
                        )
                        .with_render_config(rc)
                        .prompt())?
                    else {
                        return Ok(None);
                    };
                    let parts = match split_shell_command(&cmd_str) {
                        Ok(p) => p,
                        Err(e) => {
                            brand::warn(&format!("Invalid command — {e}"));
                            continue;
                        }
                    };
                    if parts.is_empty() {
                        brand::warn("Command cannot be empty — please enter a runnable command.");
                        continue;
                    }
                    break parts;
                };
                Some(parts)
            }
        } else {
            None
        };

        let mut slot = AgentSlot::new(
            name.clone(),
            provider_id,
            model_name,
            input_price,
            output_price,
        );
        slot.exec_command = exec_command;
        slot.apply_preset();
        agents.push(slot);
    }

    if check_model_diversity(&agents) {
        println!();
        brand::warn("Fewer than 3 distinct models selected.");
        brand::warn("Deliberation quality improves with diverse models.");
        println!();
    }

    Ok(Some(agents))
}

#[cfg(test)]
mod tests {
    use super::split_shell_command;

    #[test]
    fn simple_command() {
        assert_eq!(
            split_shell_command("python3 agent.py").unwrap(),
            vec!["python3", "agent.py"]
        );
    }
    // TODO(slop): placeholder identifier — pick a name that says what this is

    #[test]
    fn double_quoted_arg() {
        assert_eq!(
            split_shell_command(r#"python3 "my agent.py" --flag"#).unwrap(),
            vec!["python3", "my agent.py", "--flag"]
        );
    }

    #[test]
    fn single_quoted_arg() {
        assert_eq!(
            split_shell_command("bash -c 'echo hello world'").unwrap(),
            vec!["bash", "-c", "echo hello world"]
        );
    }

    #[test]
    fn claude_cli_command() {
        assert_eq!(
            split_shell_command("claude -p --output-format json --model sonnet --verbose").unwrap(),
            vec![
                "claude",
                "-p",
                "--output-format",
                "json",
                "--model",
                "sonnet",
                "--verbose"
            ]
        );
    }

    #[test]
    fn empty_string() {
        let result: Vec<String> = split_shell_command("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn unterminated_double_quote_returns_err() {
        let err = split_shell_command(r#"python3 "agent.py"#).unwrap_err();
        assert!(
            err.contains("unterminated double quote"),
            "expected unterminated-quote message, got: {err}"
        );
    }

    #[test]
    fn unterminated_single_quote_returns_err() {
        let err = split_shell_command("bash -c 'echo hello").unwrap_err();
        assert!(
            err.contains("unterminated single quote"),
            "expected unterminated-quote message, got: {err}"
        );
    }

    #[test]
    fn extra_whitespace() {
        assert_eq!(
            split_shell_command("  python3   agent.py  ").unwrap(),
            vec!["python3", "agent.py"]
        );
    }
}
