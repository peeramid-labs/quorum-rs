//! File I/O, re-init handling, and `.gitignore` management.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use inquire::Select;

use super::ask;
use crate::brand;

// ── Re-init handling ───────────────────────────────────────────────────────

pub(super) enum ReinitChoice {
    /// Re-run the wizard; preserve existing secrets from `.env`.
    Merge(HashMap<String, String>),
    /// Back up existing files and re-run from scratch.
    Overwrite,
    /// Ignore existing files — overwrite without backup.
    MakeNew,
    /// Abort — leave everything as-is.
    Cancel,
}

pub(super) fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    content
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            // Strip surrounding matching quotes so `KEY="value"` and
            // `KEY='value'` both round-trip to `value` — otherwise the
            // quotes leak into the merged output as `KEY=""value""`.
            let trimmed = v.trim();
            let unquoted = {
                let bytes = trimmed.as_bytes();
                if bytes.len() >= 2
                    && (bytes[0] == b'"' || bytes[0] == b'\'')
                    && bytes[0] == bytes[bytes.len() - 1]
                {
                    &trimmed[1..trimmed.len() - 1]
                } else {
                    trimmed
                }
            };
            Some((k.trim().to_string(), unquoted.to_string()))
        })
        .collect()
}

/// Check which output files already exist in the target directory.
///
/// Returns a list of file names that were found (e.g. `["docker-compose.yml", ".env"]`).
/// An empty list means a fresh start (no existing files).
pub(super) fn detect_existing_files<'a>(output_dir: &Path, compose_name: &'a str) -> Vec<&'a str> {
    let mut found: Vec<&str> = Vec::new();
    if output_dir.join(compose_name).exists() {
        found.push(compose_name);
    }
    if output_dir.join(".env").exists() {
        found.push(".env");
    }
    if output_dir.join("config/agent.yml").exists() {
        found.push("config/agent.yml");
    }
    if output_dir.join("config/orchestrator.yml").exists() {
        found.push("config/orchestrator.yml");
    }
    found
}

/// Process a reinit choice string and return the appropriate `ReinitChoice`.
///
/// If the choice is "Overwrite", existing files are backed up before returning.
/// If the choice is "Merge", existing `.env` secrets are read and preserved.
pub(super) fn process_reinit_choice(
    choice: &str,
    output_dir: &Path,
    compose_name: &str,
) -> Result<ReinitChoice> {
    if choice.starts_with("Cancel") {
        return Ok(ReinitChoice::Cancel);
    }

    if choice.starts_with("Overwrite") {
        backup_files(output_dir, compose_name)?;
        return Ok(ReinitChoice::Overwrite);
    }

    if choice.starts_with("Make new") {
        return Ok(ReinitChoice::MakeNew);
    }

    // Merge: read existing secrets
    let env_path = output_dir.join(".env");
    let existing = parse_env_file(&env_path);
    Ok(ReinitChoice::Merge(existing))
}

pub(super) fn check_existing(output_dir: &Path, compose_name: &str) -> Result<ReinitChoice> {
    let found = detect_existing_files(output_dir, compose_name);

    if found.is_empty() {
        return Ok(ReinitChoice::Overwrite); // fresh start
    }

    // Tell the user exactly which files were found.
    println!();
    brand::warn(&format!("Found existing: {}", found.join(", ")));
    println!();

    let opts = vec![
        "Merge      \u{2014} keep existing secrets, update provider/agent config",
        "Overwrite  \u{2014} start fresh  (existing files backed up to .nsed-backup/)",
        "Make new   \u{2014} ignore existing files, write fresh (no backup)",
        "Cancel",
    ];
    let rc = brand::render_config();
    let choice = match ask(Select::new("How would you like to proceed?", opts)
        .with_render_config(rc)
        .prompt())?
    {
        Some(c) => c,
        None => return Ok(ReinitChoice::Cancel),
    };

    process_reinit_choice(choice, output_dir, compose_name)
}

pub(super) fn backup_files(output_dir: &Path, compose_name: &str) -> Result<()> {
    let backup = output_dir.join(".nsed-backup");
    std::fs::create_dir_all(&backup).context("creating .nsed-backup/")?;

    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let files_to_backup = [
        compose_name,
        ".env",
        "config/agent.yml",
        "config/orchestrator.yml",
    ];
    for name in &files_to_backup {
        let src = output_dir.join(name);
        if src.exists() {
            // Flatten nested paths for the backup filename (config/agent.yml → config_agent.yml)
            let safe_name = name.replace('/', "_");
            let dst = backup.join(format!("{safe_name}.{timestamp}"));
            std::fs::copy(&src, &dst).with_context(|| format!("backing up {name}"))?;
        }
    }
    brand::info(&format!(
        "Existing files backed up to {}",
        brand::teal(".nsed-backup/")
    ));
    Ok(())
}

// ── File writing ───────────────────────────────────────────────────────────

pub(super) fn ensure_gitignore(output_dir: &Path) -> Result<()> {
    let gi = output_dir.join(".gitignore");
    let entries = [".env", ".nsed-backup/"];

    if gi.exists() {
        let content = std::fs::read_to_string(&gi).unwrap_or_default();
        let mut f = std::fs::OpenOptions::new().append(true).open(&gi)?;
        use std::io::Write;
        for entry in &entries {
            if !content.lines().any(|l| l.trim() == *entry) {
                write!(f, "\n{entry}\n")?;
            }
        }
    } else {
        let content = entries.join("\n") + "\n";
        std::fs::write(&gi, content)?;
    }
    Ok(())
}

/// Extract non-comment key names from generated `.env` content.
///
/// Currently only consumed from the test suite — the production
/// merge path no longer needs to enumerate template keys upfront
/// because `merge_env` falls back to the template default per-line.
/// Kept here (under `#[cfg(test)]`) so the test surface that verifies
/// template-key extraction lives next to the production parser.
#[cfg(test)]
pub(super) fn env_keys(env_content: &str) -> std::collections::HashSet<String> {
    env_content
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
        .collect()
}

/// Configuration files to write alongside compose + .env.
pub(super) struct ConfigFiles {
    /// Agent `config/default.yml` content (always present when agents are configured).
    pub agent_config: Option<String>,
    /// Orchestrator `config/default.yml` content (only for owner persona).
    pub orchestrator_config: Option<String>,
}

/// Key substrings that identify secret/credential env vars whose values
/// should be preserved from an existing `.env` during a merge-reinit.
///
/// Note: "TOKEN" is intentionally excluded — `NSED_BEARER_TOKEN` is a
/// credential the user explicitly enters during the wizard, so it must
/// not be silently preserved when re-running init with a new value.
/// Actual auth secrets are already covered by "SECRET" (e.g.,
/// `APP_AUTH__TOKENS__0__SECRET`).
const SECRET_KEY_PATTERNS: &[&str] = &["SECRET", "API_KEY", "PASSWORD", "SEED"];

/// Returns `true` if the key looks like a secret that should be preserved
/// from the user's existing `.env` rather than overwritten by the template.
pub(super) fn is_secret_key(key: &str) -> bool {
    let upper = key.to_uppercase();
    SECRET_KEY_PATTERNS.iter().any(|pat| upper.contains(pat))
}

/// Merge preserved secret values from `existing_env` into `env_content`.
///
/// For each `KEY=VALUE` line in the new template, if the key is a secret
/// (matches [`SECRET_KEY_PATTERNS`]) and appears in `existing_env`, the
/// existing value is substituted.  Non-secret keys (ports, URLs, comments)
/// always use the new template value.
fn merge_env(env_content: &str, existing_env: &HashMap<String, String>) -> String {
    env_content
        .lines()
        .map(|line| {
            // Comment or blank → pass through unchanged
            if line.trim_start().starts_with('#') || line.trim().is_empty() {
                return line.to_string();
            }
            if let Some((key, _value)) = line.split_once('=') {
                let key = key.trim();
                if is_secret_key(key) {
                    if let Some(old_val) = existing_env.get(key) {
                        return format!("{key}={old_val}");
                    }
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Write compose + .env + .gitignore + config/ YAML files.
///
/// When `existing_env` is non-empty and already contains every key the new
/// template needs, `.env` is still rewritten (picking up any new comments,
/// reordering, etc.) but secret values (`*SECRET*`, `*TOKEN*`, `*API_KEY*`,
/// `*PASSWORD*`, `*SEED*`) are preserved from the existing file.  Returns
/// `true` when secret values were merged, `false` when a fresh `.env` was
/// written with no merge.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_files(
    output_dir: &Path,
    compose_name: &str,
    compose_content: &str,
    env_content: &str,
    existing_env: &HashMap<String, String>,
    configs: &ConfigFiles,
) -> Result<bool> {
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("creating directory {}", output_dir.display()))?;

    let compose_path = output_dir.join(compose_name);
    let env_path = output_dir.join(".env");

    std::fs::write(&compose_path, compose_content)
        .with_context(|| format!("writing {}", compose_path.display()))?;

    // Merge: write the new template, preserving non-empty values
    // from the existing .env so rotated tokens / API keys aren't lost.
    // Always merge when an existing .env is present — the previous
    // "merge only if every new key already exists" rule wiped
    // existing secrets whenever a fresh provider was added.
    // `merge_env` already falls back to the template default for any
    // key the existing file lacks.
    let env_skipped = if !existing_env.is_empty() {
        let merged = merge_env(env_content, existing_env);
        std::fs::write(&env_path, merged)
            .with_context(|| format!("writing {}", env_path.display()))?;
        true
    } else {
        std::fs::write(&env_path, env_content)
            .with_context(|| format!("writing {}", env_path.display()))?;
        false
    };
    ensure_gitignore(output_dir).context("updating .gitignore")?;

    // Write config/ YAML files
    if configs.agent_config.is_some() || configs.orchestrator_config.is_some() {
        let config_dir = output_dir.join("config");
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating directory {}", config_dir.display()))?;

        if let Some(agent_yml) = &configs.agent_config {
            let agent_config_path = config_dir.join("agent.yml");
            std::fs::write(&agent_config_path, agent_yml)
                .with_context(|| format!("writing {}", agent_config_path.display()))?;
        }
        if let Some(orch_yml) = &configs.orchestrator_config {
            let orch_config_path = config_dir.join("orchestrator.yml");
            std::fs::write(&orch_config_path, orch_yml)
                .with_context(|| format!("writing {}", orch_config_path.display()))?;
        }
    }

    Ok(env_skipped)
}
