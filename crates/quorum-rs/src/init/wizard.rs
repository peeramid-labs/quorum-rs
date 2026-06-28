// ── Interactive wizard functions ────────────────────────────────────────────

use std::collections::HashMap;

use anyhow::Result;
use inquire::{Confirm, MultiSelect, Select, Text};

use super::ask;
use super::model_recommendations::{note_line, recommend, repair_default_indices};
use super::presets::{AGENT_PRESETS, AgentSlot, is_tested_model};

/// Engine-strategy labels (how the runtime drives the model).
const ENGINE_STREAMING: &str = "stream responses";
const ENGINE_MERGE_SYS: &str = "merge system prompt into first user message";
const ENGINE_DISABLE_NATIVE_TOOLS: &str = "disable native tool definitions";

/// LLM-repair pass labels (the only ones backed by a real strategy flag).
const REPAIR_ESCAPES: &str = "fix invalid JSON escapes";
const REPAIR_HALLUCINATED: &str = "parse hallucinated tool-calls";
const REPAIR_JSON_MODE: &str = "request JSON mode output";

/// Map selected engine labels to flags, in order:
/// `(use_streaming, merge_system_prompt, disable_native_tools)`.
fn engine_flags_from_selection(selected: &[&str]) -> (Option<bool>, Option<bool>, Option<bool>) {
    (
        Some(selected.contains(&ENGINE_STREAMING)),
        Some(selected.contains(&ENGINE_MERGE_SYS)),
        Some(selected.contains(&ENGINE_DISABLE_NATIVE_TOOLS)),
    )
}

/// Map selected repair-pass labels to the three strategy flags, in order:
/// `(repair_invalid_escapes, unwrap_hallucinated_tool_calls, json_mode)`.
fn repair_flags_from_selection(selected: &[&str]) -> (Option<bool>, Option<bool>, Option<bool>) {
    (
        Some(selected.contains(&REPAIR_ESCAPES)),
        Some(selected.contains(&REPAIR_HALLUCINATED)),
        Some(selected.contains(&REPAIR_JSON_MODE)),
    )
}
use super::providers::{
    DetectedTool, FetchedModel, ModelInfo, Provider, build_claude_exec_provider,
    build_exec_provider, build_ollama_provider, build_provider_env_key, build_simulated_provider,
    derive_provider_pricing, fetch_models, fetched_to_model_infos, sanitize_provider_id,
};
use crate::brand;

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

/// Split a comma-separated path list into trimmed, non-empty entries.
fn split_paths(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Suggest a default agent name from a template, de-duplicated against names
/// already in use (lower-cased; numeric suffix on clash).
fn suggest_agent_name(template: Option<&str>, used: &std::collections::HashSet<String>) -> String {
    let base = template
        .map(|t| t.to_ascii_lowercase())
        .unwrap_or_else(|| "agent".to_string());
    if !used.contains(&base) {
        return base;
    }
    let mut i = 2;
    loop {
        let candidate = format!("{base}-{i}");
        if !used.contains(&candidate) {
            return candidate;
        }
        i += 1;
    }
}

/// Validate an agent name: non-empty, ASCII alphanumeric + `-` `_` `.`.
/// Returns the trimmed name when valid.
fn sanitize_agent_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Per-agent default writable memories directory (relative to the workspace).
fn default_memories_dir(name: &str) -> String {
    format!("./.nsed/memories/{name}")
}

/// Starter content for an agent-maintained `memories.md`.
fn memories_starter(name: &str) -> String {
    format!(
        "# Memories — {name}\n\n\
         Durable notes this agent maintains across deliberations. The agent may\n\
         append facts, decisions, and context it wants to recall next time.\n\
         Keep entries concise; prune what's stale.\n"
    )
}

/// Create `<dir>/memories.md` with starter content. Returns `Ok(true)` when a
/// new file was written, `Ok(false)` when one already existed (left untouched).
fn seed_memories_md(dir: &std::path::Path, name: &str) -> std::io::Result<bool> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("memories.md");
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(&path, memories_starter(name))?;
    Ok(true)
}

pub(super) fn wizard_agents(
    providers: &[Provider],
    exec_tools: &[DetectedTool],
) -> Result<Option<Vec<AgentSlot>>> {
    brand::section("agent assignment");
    let rc = brand::render_config();

    let model_options = build_model_options(providers);
    if model_options.is_empty() {
        brand::warn("No models discovered — using a single DEFAULT agent.");
        if let Some(slot) = build_fallback_agent(providers) {
            brand::warn("Set model_name in config/agent.yml after init.");
            return Ok(Some(vec![slot]));
        }
        brand::warn("No providers configured — cannot create fallback agent.");
        return Ok(Some(Vec::new()));
    }
    let display_options: Vec<String> = model_options.iter().map(|(d, ..)| d.clone()).collect();

    // Personas are starting templates: every agent picks one, then gets its
    // own name, capability tags, and file access. Loop to add as many as you
    // want — deliberation wants 2+ agents, ideally with diverse models.
    let template_options = build_persona_options();

    let mut agents: Vec<AgentSlot> = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let n = agents.len() + 1;

        // 1. Template (persona seed: system prompt + default tags)
        let Some(tmpl) = ask(Select::new(
            &format!("Agent {n} — start from template:"),
            template_options.clone(),
        )
        .with_help_message("Rename and retag it below — the template is just a starting point.")
        .with_render_config(rc)
        .prompt())?
        else {
            return Ok(None);
        };
        let template_name = resolve_selected_names(&[tmpl], &template_options)
            .into_iter()
            .next();

        // 2. Name (unique)
        let default_name = suggest_agent_name(template_name.as_deref(), &used_names);
        let name = loop {
            let Some(raw) = ask(Text::new(&format!("Agent {n} — name:"))
                .with_default(&default_name)
                .with_help_message("Unique. ASCII letters, digits, -, _, .")
                .with_render_config(rc)
                .prompt())?
            else {
                return Ok(None);
            };
            match sanitize_agent_name(&raw) {
                None => brand::warn("Invalid name — ASCII letters, digits, -, _, . only."),
                Some(valid) if used_names.contains(&valid) => {
                    brand::warn(&format!("'{valid}' already used — pick another."))
                }
                Some(valid) => break valid,
            }
        };

        // 3. Model (provider follows automatically)
        let Some(choice) = ask(Select::new(
            &format!("Model for {name}:"),
            display_options.clone(),
        )
        .with_starting_cursor((n - 1) % model_options.len())
        .with_help_message("✓ = integration-tested  ·  prices: input/output $/Mtok")
        .with_render_config(rc)
        .prompt())?
        else {
            return Ok(None);
        };

        let Some(entry) = model_options.iter().find(|(d, ..)| *d == choice) else {
            brand::warn("Internal error: model choice not found.");
            return Ok(None);
        };
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

        let provider_type = providers
            .iter()
            .find(|p| p.id == provider_id)
            .map(|p| p.provider_type.as_str())
            .unwrap_or("openai");

        // apply_preset keys on the slot name, so construct with the TEMPLATE
        // name to inherit its persona + tags, then rename to the user's choice.
        let mut slot = AgentSlot::new(
            template_name.clone().unwrap_or_else(|| name.clone()),
            provider_id.clone(),
            model_name,
            input_price,
            output_price,
        );
        slot.apply_preset();
        slot.name = name.clone();

        // 4. Capability tags (prefilled from template, editable). Rooms and
        //    policies schedule agents by matching these.
        let tag_default = slot.capability_tags.join(", ");
        let Some(tags_str) = ask(Text::new(&format!("Capability tags for {name}:"))
            .with_default(&tag_default)
            .with_help_message(
                "Comma-separated. Rooms/policies target these, e.g. general, planning",
            )
            .with_render_config(rc)
            .prompt())?
        else {
            return Ok(None);
        };
        slot.capability_tags = split_paths(&tags_str);

        // 5. File access — modelled to each provider's real mechanism.
        match provider_type {
            "exec" => {
                let default_cmd = if exec_tools.iter().any(|t| t.name == "python3") {
                    "python3 my_agent.py"
                } else {
                    "bash agent.sh"
                };
                let parts: Vec<String> = loop {
                    let Some(cmd_str) = ask(Text::new(&format!("Exec command for {name}:"))
                        .with_default(default_cmd)
                        .with_help_message("Shell-style command, e.g. \"python3 agent.py\"")
                        .with_render_config(rc)
                        .prompt())?
                    else {
                        return Ok(None);
                    };
                    match split_shell_command(&cmd_str) {
                        Ok(p) if !p.is_empty() => break p,
                        Ok(_) => brand::warn("Command cannot be empty — enter a runnable command."),
                        Err(e) => brand::warn(&format!("Invalid command — {e}")),
                    }
                };
                slot.exec_command = Some(parts);

                let Some(wd) = ask(Text::new(&format!(
                    "Working directory for {name} (optional):"
                ))
                .with_default("")
                .with_help_message("Subprocess runs here; also its read/write root.")
                .with_render_config(rc)
                .prompt())?
                else {
                    return Ok(None);
                };
                slot.write_dirs = split_paths(&wd);
            }
            "claude" => {
                // Read context → context_files: inlined, read-only, per-file.
                let Some(ctx) = ask(Text::new(&format!(
                    "Context files for {name} — read-only, inlined (comma-separated, optional):"
                ))
                .with_default("")
                .with_help_message(
                    "Read as context, never writable. Example: ./README.md, ./docs/spec.md",
                )
                .with_render_config(rc)
                .prompt())?
                else {
                    return Ok(None);
                };
                slot.read_paths = split_paths(&ctx);

                // Writable scope → add_dirs + writable:true. Each agent manages
                // its OWN memories dir here (default suggested per name).
                let mem_default = default_memories_dir(&name);
                let Some(wr) = ask(Text::new(&format!(
                    "Writable dirs for {name} — agent creates/edits files here (comma-separated):"
                ))
                .with_default(&mem_default)
                .with_help_message(
                    "Each agent manages its own memories here. Blank = read-only agent.",
                )
                .with_render_config(rc)
                .prompt())?
                else {
                    return Ok(None);
                };
                slot.write_dirs = split_paths(&wr);

                // Offer to seed a starter memories.md in each writable dir.
                for dir in &slot.write_dirs {
                    let Some(seed) =
                        ask(Confirm::new(&format!("Seed {dir}/memories.md for {name}?"))
                            .with_default(true)
                            .with_render_config(rc)
                            .prompt())?
                    else {
                        return Ok(None);
                    };
                    if seed {
                        match seed_memories_md(std::path::Path::new(dir), &name) {
                            Ok(true) => brand::success(&format!("Wrote {dir}/memories.md")),
                            Ok(false) => {
                                brand::info(&format!("{dir}/memories.md exists — left as-is"))
                            }
                            Err(e) => {
                                brand::warn(&format!("Could not seed {dir}/memories.md: {e}"))
                            }
                        }
                    }
                }
            }
            _ => {
                // Native LLM: read roots via builtin_tools read_file; no write tool.
                let Some(rd) = ask(Text::new(&format!(
                    "Read access for {name} — files/dirs for context (comma-separated, optional):"
                ))
                .with_default("")
                .with_help_message("Granted to the agent's read_file tool. Example: ./docs, ./src")
                .with_render_config(rc)
                .prompt())?
                else {
                    return Ok(None);
                };
                slot.read_paths = split_paths(&rd);
            }
        }

        let recommendation = recommend(&slot.model_name);
        print!("{}", note_line(&recommendation));

        // ── Engine strategies (multi-select) ──────────────────────────────
        let engine_choices = vec![
            ENGINE_STREAMING,
            ENGINE_MERGE_SYS,
            ENGINE_DISABLE_NATIVE_TOOLS,
        ];
        let Some(engine_sel) = ask(MultiSelect::new(
            &format!("Engine strategies for {name} (space toggles):"),
            engine_choices,
        )
        .with_default(&[0])
        .with_help_message(
            "How the runtime drives the model. Default: streaming on. Enable the others only \
             for models that need them.",
        )
        .with_render_config(rc)
        .prompt())?
        else {
            return Ok(None);
        };
        let (stream, merge_sys, disable_native) = engine_flags_from_selection(&engine_sel);
        slot.use_streaming = stream;
        slot.merge_system_prompt = merge_sys;
        slot.disable_native_tools = disable_native;

        // ── LLM-repair passes (multi-select) ──────────────────────────────
        let repair_choices = vec![REPAIR_ESCAPES, REPAIR_HALLUCINATED, REPAIR_JSON_MODE];
        let Some(selected) = ask(MultiSelect::new(
            &format!("LLM-repair passes for {name} (space toggles):"),
            repair_choices,
        )
        .with_default(&repair_default_indices(&recommendation))
        .with_help_message(
            "Repairs malformed output from smaller models. Defaults track the model's tested config.",
        )
        .with_render_config(rc)
        .prompt())?
        else {
            return Ok(None);
        };
        let (esc, hall, jm) = repair_flags_from_selection(&selected);
        slot.repair_invalid_escapes = esc;
        slot.unwrap_hallucinated_tool_calls = hall;
        slot.json_mode = jm;

        let Some(fd) = ask(Select::new(
            &format!("Failure dumps for {name} (on parse/API errors):"),
            vec!["on", "full", "off"],
        )
        .with_help_message("`on` = metadata, `full` = raw payloads, `off` = nothing written.")
        .with_render_config(rc)
        .prompt())?
        else {
            return Ok(None);
        };
        slot.failure_dumps = Some(fd.to_string());

        used_names.insert(name);
        agents.push(slot);

        let Some(more) = ask(Confirm::new("Add another agent?")
            .with_default(agents.len() < 2)
            .with_help_message("Deliberation needs 2+ agents; 3+ with diverse models is best.")
            .with_render_config(rc)
            .prompt())?
        else {
            return Ok(None);
        };
        if !more {
            break;
        }
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
    use super::{
        ENGINE_DISABLE_NATIVE_TOOLS, ENGINE_MERGE_SYS, ENGINE_STREAMING, REPAIR_ESCAPES,
        REPAIR_HALLUCINATED, REPAIR_JSON_MODE, default_memories_dir, engine_flags_from_selection,
        memories_starter, repair_flags_from_selection, sanitize_agent_name, seed_memories_md,
        split_paths, split_shell_command, suggest_agent_name,
    };
    use std::collections::HashSet;

    #[test]
    fn engine_flags_map_selection_to_each_flag() {
        // Only streaming selected → stream on, others off (explicit Some(false)).
        let (stream, merge, disable) = engine_flags_from_selection(&[ENGINE_STREAMING]);
        assert_eq!(
            (stream, merge, disable),
            (Some(true), Some(false), Some(false))
        );
        // All three.
        let (stream, merge, disable) = engine_flags_from_selection(&[
            ENGINE_STREAMING,
            ENGINE_MERGE_SYS,
            ENGINE_DISABLE_NATIVE_TOOLS,
        ]);
        assert_eq!(
            (stream, merge, disable),
            (Some(true), Some(true), Some(true))
        );
    }

    #[test]
    fn repair_flags_map_selection_to_each_flag() {
        let (esc, hall, jm) = repair_flags_from_selection(&[REPAIR_ESCAPES, REPAIR_HALLUCINATED]);
        assert_eq!((esc, hall, jm), (Some(true), Some(true), Some(false)));
        let (esc, hall, jm) = repair_flags_from_selection(&[REPAIR_JSON_MODE]);
        assert_eq!((esc, hall, jm), (Some(false), Some(false), Some(true)));
        // Nothing selected → all explicitly off.
        let (esc, hall, jm) = repair_flags_from_selection(&[]);
        assert_eq!((esc, hall, jm), (Some(false), Some(false), Some(false)));
    }

    #[test]
    fn suggest_agent_name_lowercases_template_and_dedupes() {
        let mut used = HashSet::new();
        assert_eq!(suggest_agent_name(Some("DEFAULT"), &used), "default");
        used.insert("default".to_string());
        assert_eq!(suggest_agent_name(Some("DEFAULT"), &used), "default-2");
        used.insert("default-2".to_string());
        assert_eq!(suggest_agent_name(Some("DEFAULT"), &used), "default-3");
        assert_eq!(suggest_agent_name(None, &used), "agent");
    }

    #[test]
    fn sanitize_agent_name_accepts_valid_rejects_bad() {
        assert_eq!(
            sanitize_agent_name("  planner_1.v2-x "),
            Some("planner_1.v2-x".to_string())
        );
        assert_eq!(sanitize_agent_name(""), None);
        assert_eq!(sanitize_agent_name("   "), None);
        assert_eq!(sanitize_agent_name("bad name"), None);
        assert_eq!(sanitize_agent_name("has:colon"), None);
    }

    #[test]
    fn default_memories_dir_is_per_agent() {
        assert_eq!(default_memories_dir("planner"), "./.nsed/memories/planner");
        assert_eq!(default_memories_dir("scribe"), "./.nsed/memories/scribe");
    }

    #[test]
    fn seed_memories_md_writes_then_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("planner");
        // first call writes a new file
        assert!(seed_memories_md(&target, "planner").unwrap());
        let path = target.join("memories.md");
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Memories — planner"));
        assert_eq!(body, memories_starter("planner"));
        // second call leaves the existing file untouched
        std::fs::write(&path, "user edits").unwrap();
        assert!(!seed_memories_md(&target, "planner").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user edits");
    }

    #[test]
    fn split_paths_trims_and_drops_empties() {
        assert_eq!(
            split_paths(" ./docs , src/api.rs ,, "),
            vec!["./docs".to_string(), "src/api.rs".to_string()]
        );
        assert!(split_paths("").is_empty());
        assert!(split_paths("  ,  ").is_empty());
    }

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
