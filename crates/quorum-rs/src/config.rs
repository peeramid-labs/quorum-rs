//! Agent fleet configuration loading and resolution.
//!
//! Types and helpers for loading agent configuration from YAML files,
//! so any tooling can parse and use the same agent configuration shape.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::agents::AgentConfig;
use crate::nats_utils::OrchestratorEntry;
use crate::telemetry::{TelemetryConfig, TelemetryEndpointConfig};

/// LLM model definition within a provider.
///
/// Contains model-specific parameters (temperature, max_tokens, etc.) that
/// were previously scattered across `AgentConfig`. Agents reference a model
/// via a dotpath `"provider_id.model_key"` in their `model` field; at load
/// time, these values are merged into the agent config.
///
/// All fields are `Option<T>` — only present values override agent defaults.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ModelDef {
    /// The actual model name sent in API requests (e.g. "meta-llama/Llama-3-70b").
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<i32>,
    #[serde(default)]
    pub context_window: Option<i32>,
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    #[serde(default)]
    pub presence_penalty: Option<f32>,
    #[serde(default)]
    pub use_streaming: Option<bool>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub repair_invalid_escapes: Option<bool>,
    #[serde(default)]
    pub tool_format: Option<String>,
    #[serde(default)]
    pub disable_native_tools: Option<bool>,
    #[serde(default)]
    pub merge_system_prompt: Option<bool>,
    #[serde(default)]
    pub unwrap_hallucinated_tool_calls: Option<bool>,
    #[serde(default)]
    pub json_mode: Option<bool>,
    #[serde(default)]
    pub chars_per_token: Option<f64>,
    #[serde(default)]
    pub scratchpad_limit: Option<i32>,
    #[serde(default)]
    pub max_scratchpad_size: Option<i32>,
    #[serde(default)]
    pub supports_native_thinking: Option<bool>,
    #[serde(default)]
    pub input_price_per_mtok: Option<f64>,
    #[serde(default)]
    pub output_price_per_mtok: Option<f64>,
}

/// Minimal provider config — just enough to extract base_url, api_key, engine.
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderEntry {
    /// Provider type: "openai" (default), "ollama", or "simulated"
    #[serde(rename = "type", default = "default_provider_type")]
    pub provider_type: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub engine: Option<String>,
    /// Simulated provider: artificial latency per response (milliseconds)
    #[serde(default)]
    pub latency_ms: u64,
    /// Named model definitions for this provider. Agents reference these
    /// via the dotpath syntax `model: "provider_id.model_key"`.
    #[serde(default)]
    pub models: HashMap<String, ModelDef>,
}

fn default_provider_type() -> String {
    "openai".to_string()
}

/// Overlay variant of [`ProviderEntry`] where every field is optional.
/// Used during config merge so a single-field overlay (e.g. just `api_key`)
/// does not wipe inherited fields.
#[derive(Debug, Deserialize)]
struct ProviderOverlay {
    #[serde(rename = "type")]
    provider_type: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    engine: Option<String>,
    latency_ms: Option<u64>,
    models: Option<HashMap<String, ModelDef>>,
}

impl From<ProviderOverlay> for ProviderEntry {
    fn from(o: ProviderOverlay) -> Self {
        ProviderEntry {
            provider_type: o.provider_type.unwrap_or_else(default_provider_type),
            base_url: o.base_url.unwrap_or_default(),
            api_key: o.api_key.unwrap_or_default(),
            engine: o.engine,
            latency_ms: o.latency_ms.unwrap_or(0),
            models: o.models.unwrap_or_default(),
        }
    }
}

/// Merge overlay fields into an existing provider entry.
/// Only fields explicitly present in the overlay (`Some`) are applied.
fn merge_provider(base: &mut ProviderEntry, overlay: &ProviderOverlay) {
    if let Some(ref pt) = overlay.provider_type {
        base.provider_type = pt.clone();
    }
    if let Some(ref url) = overlay.base_url {
        base.base_url = url.clone();
    }
    if let Some(ref key) = overlay.api_key {
        base.api_key = key.clone();
    }
    if let Some(ref eng) = overlay.engine {
        base.engine = Some(eng.clone());
    }
    if let Some(ms) = overlay.latency_ms {
        base.latency_ms = ms;
    }
    if let Some(ref models) = overlay.models {
        for (key, model) in models {
            base.models.insert(key.clone(), model.clone());
        }
    }
}

/// Overlay counterpart for [`TelemetryConfig`]. Every field is
/// `Option` so a partial overlay merges field-by-field: missing keys
/// preserve the base, present keys replace it. Without this a
/// `telemetry:` block in an env overlay that only sets `endpoints`
/// would deserialize `enabled` to its default `true` and silently
/// flip a base `enabled: false` back on (non-monotonic opt-out).
///
/// `endpoints` overlay semantics: `Some(list)` replaces the base
/// list **wholesale** (not element-wise by name). Operators who want
/// to add a single endpoint must restate the full list. This is
/// intentional — element-wise merge by name would silently leave
/// stale endpoints in production after a rename, which is a much
/// worse failure mode than a verbose overlay.
#[derive(Debug, Deserialize)]
struct TelemetryConfigOverlay {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    endpoints: Option<Vec<TelemetryEndpointConfig>>,
}

/// Internal overlay struct for [`AgentFleetConfig`] that uses [`ProviderOverlay`]
/// for field-level merging of provider entries.
#[derive(Debug, Deserialize)]
struct FleetConfigOverlay {
    #[serde(default)]
    providers: HashMap<String, ProviderOverlay>,
    #[serde(default)]
    agents: Vec<AgentConfig>,
    #[serde(default)]
    orchestrators: Vec<OrchestratorEntry>,
    #[serde(default)]
    response_sla_secs: Option<u64>,
    #[serde(default)]
    telemetry: Option<TelemetryConfigOverlay>,
}

/// Top-level agent fleet config structure matching agent `config/default.yml`.
///
/// NATS URLs are obtained dynamically from orchestrators via JWT registration.
/// The `orchestrators` list defines which orchestrators this agent process
/// connects to. Per-agent extensions are defined inline on each agent entry
/// (`AgentConfig.orchestrators`) and are **additive** to this process-wide list.
#[derive(Debug, Deserialize)]
pub struct AgentFleetConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderEntry>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    /// Process-wide orchestrator list. Each agent registers with all of these
    /// plus any per-agent extensions from `agents[].orchestrators`.
    #[serde(default)]
    pub orchestrators: Vec<OrchestratorEntry>,
    /// Global response SLA in seconds. Applies as the HITL buffer hold duration
    /// for all agents.
    #[serde(default)]
    pub response_sla_secs: Option<u64>,
    /// Operational telemetry configuration. Defaults to enabled.
    /// Acts as the opt-out point for operators who do not want their
    /// agent process to publish the metrics-only telemetry events
    /// described in `docs/agent-telemetry.md`.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Optional unified-dashboard port. When set, `quorum serve`
    /// (with the `status-server` feature compiled in) starts a
    /// LAN-visible HTTP control plane on this port exposing
    /// per-agent status, chat capture, buffer inspection, and live
    /// config tuning. A CLI `--dashboard-port` flag overrides this
    /// value at runtime; absent both, no dashboard is started.
    #[serde(default)]
    pub dashboard_port: Option<u16>,
}

/// Load and merge config files from a config path (file or directory).
///
/// **File mode**: If `config_path` is a file, load it directly.
/// **Directory mode**: Load `default.yml` + `{NSED_ENV}.yml` overlay.
pub fn load_config(config_path: &Path) -> anyhow::Result<AgentFleetConfig> {
    // File mode: load the file directly, no overlay
    if config_path.is_file() {
        tracing::info!(config = %config_path.display(), "Loading config from file");
        let contents = std::fs::read_to_string(config_path)?;
        let config: AgentFleetConfig = serde_yaml::from_str(&contents)?;
        return Ok(config);
    }

    // Directory mode: default.yml + {env}.yml overlay
    let default_path = config_path.join("default.yml");
    if !default_path.exists() {
        anyhow::bail!("Config file not found: {}", default_path.display());
    }

    let default_contents = std::fs::read_to_string(&default_path)?;
    let mut config: AgentFleetConfig = serde_yaml::from_str(&default_contents)?;

    // Determine overlay: NSED_ENV env var (default: "local")
    let env_name = std::env::var("NSED_ENV").unwrap_or_else(|_| "local".into());
    let overlay_path = config_path.join(format!("{}.yml", env_name));
    if overlay_path.exists() {
        tracing::info!(overlay = %overlay_path.display(), "Merging overlay config");
        let overlay_contents = std::fs::read_to_string(&overlay_path)?;
        let overlay: FleetConfigOverlay = serde_yaml::from_str(&overlay_contents)?;

        // Merge: overlay providers patch existing entries field-by-field.
        // This prevents a one-field overlay from wiping inherited fields.
        for (id, overlay_provider) in overlay.providers {
            config
                .providers
                .entry(id)
                .and_modify(|existing| {
                    merge_provider(existing, &overlay_provider);
                })
                .or_insert_with(|| overlay_provider.into());
        }
        // Overlay agents replace default agents (if any specified)
        if !overlay.agents.is_empty() {
            config.agents = overlay.agents;
        }
        // Overlay orchestrators replace default orchestrators (if any specified)
        if !overlay.orchestrators.is_empty() {
            config.orchestrators = overlay.orchestrators;
        }
        // Overlay response SLA overrides default when provided
        if overlay.response_sla_secs.is_some() {
            config.response_sla_secs = overlay.response_sla_secs;
        }
        // Overlay telemetry merges field-by-field — a partial overlay
        // (e.g. tenant-specific endpoint list) must not clobber
        // `enabled: false` from the base back to `true` by virtue of
        // serde filling in defaults for absent fields. Each `Some`
        // replaces the base; each `None` preserves it. `endpoints` is
        // replaced wholesale per the overlay-struct doc above.
        if let Some(telemetry) = overlay.telemetry {
            if let Some(enabled) = telemetry.enabled {
                config.telemetry.enabled = enabled;
            }
            if let Some(endpoints) = telemetry.endpoints {
                config.telemetry.endpoints = endpoints;
            }
        }
    } else {
        tracing::debug!(path = %overlay_path.display(), "No overlay config found, using defaults only");
    }

    Ok(config)
}

/// Merge a [`ModelDef`] into an [`AgentConfig`].
///
/// Fields from `model_def` are applied only when the agent's current value
/// matches the `AgentConfig::default()` value (i.e., was not explicitly set
/// in the agent YAML). This lets agent-level overrides take precedence.
fn merge_model_def_into_agent(agent: &mut AgentConfig, model_def: &ModelDef) {
    let defaults = AgentConfig::default();

    if let Some(ref name) = model_def.model_name {
        // model_name from ModelDef always applies (it's the resolved model)
        // unless the agent explicitly set a different model_name
        if agent.model_name.is_empty() || agent.model_name == defaults.model_name {
            agent.model_name = name.clone();
        }
    }
    if let Some(temp) = model_def.temperature {
        if (agent.temperature - defaults.temperature).abs() < f32::EPSILON {
            agent.temperature = temp;
        }
    }
    if let Some(mt) = model_def.max_tokens {
        if agent.max_tokens == defaults.max_tokens {
            agent.max_tokens = mt;
        }
    }
    if let Some(cw) = model_def.context_window {
        if agent.context_window == defaults.context_window {
            agent.context_window = cw;
        }
    }
    if let Some(fp) = model_def.frequency_penalty {
        if agent.frequency_penalty == defaults.frequency_penalty {
            agent.frequency_penalty = Some(fp);
        }
    }
    if let Some(pp) = model_def.presence_penalty {
        if agent.presence_penalty == defaults.presence_penalty {
            agent.presence_penalty = Some(pp);
        }
    }
    if let Some(us) = model_def.use_streaming {
        if agent.use_streaming == defaults.use_streaming {
            agent.use_streaming = us;
        }
    }
    if let Some(ref re) = model_def.reasoning_effort {
        if agent.reasoning_effort == defaults.reasoning_effort {
            agent.reasoning_effort = Some(re.clone());
        }
    }
    if let Some(rie) = model_def.repair_invalid_escapes {
        if agent.repair_invalid_escapes == defaults.repair_invalid_escapes {
            agent.repair_invalid_escapes = rie;
        }
    }
    if let Some(ref tf) = model_def.tool_format {
        if agent.tool_format == defaults.tool_format {
            agent.tool_format = Some(tf.clone());
        }
    }
    if let Some(dnt) = model_def.disable_native_tools {
        if agent.disable_native_tools == defaults.disable_native_tools {
            agent.disable_native_tools = dnt;
        }
    }
    if let Some(msp) = model_def.merge_system_prompt {
        if agent.merge_system_prompt == defaults.merge_system_prompt {
            agent.merge_system_prompt = msp;
        }
    }
    if let Some(uhtc) = model_def.unwrap_hallucinated_tool_calls {
        if agent.unwrap_hallucinated_tool_calls == defaults.unwrap_hallucinated_tool_calls {
            agent.unwrap_hallucinated_tool_calls = uhtc;
        }
    }
    if let Some(jm) = model_def.json_mode {
        if agent.json_mode == defaults.json_mode {
            agent.json_mode = jm;
        }
    }
    if let Some(cpt) = model_def.chars_per_token {
        if agent.chars_per_token == defaults.chars_per_token {
            agent.chars_per_token = Some(cpt);
        }
    }
    if let Some(sl) = model_def.scratchpad_limit {
        if agent.scratchpad_limit == defaults.scratchpad_limit {
            agent.scratchpad_limit = sl;
        }
    }
    if let Some(mss) = model_def.max_scratchpad_size {
        if agent.max_scratchpad_size == defaults.max_scratchpad_size {
            agent.max_scratchpad_size = Some(mss);
        }
    }
    if let Some(snt) = model_def.supports_native_thinking {
        if agent.supports_native_thinking == defaults.supports_native_thinking {
            agent.supports_native_thinking = snt;
        }
    }
    if let Some(ip) = model_def.input_price_per_mtok {
        if agent.input_price_per_mtok == defaults.input_price_per_mtok {
            agent.input_price_per_mtok = Some(ip);
        }
    }
    if let Some(op) = model_def.output_price_per_mtok {
        if agent.output_price_per_mtok == defaults.output_price_per_mtok {
            agent.output_price_per_mtok = Some(op);
        }
    }
}

/// Load an agent config from the fleet config by name. Returns `(AgentConfig, ProviderEntry)`.
///
/// Resolves the provider entry for the agent's `provider_id`, applying env var
/// overrides for API keys. When an agent uses the dotpath `model` field
/// (`"provider_id.model_key"`), the provider and model definition are resolved
/// and merged into the agent config before returning.
pub fn load_agent_from_config(
    config: &AgentFleetConfig,
    agent_name: &str,
) -> anyhow::Result<(AgentConfig, ProviderEntry)> {
    let mut agent = config
        .agents
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(agent_name))
        .ok_or_else(|| {
            let available: Vec<&str> = config.agents.iter().map(|a| a.name.as_str()).collect();
            anyhow::anyhow!(
                "Agent '{}' not found in config. Available agents:\n  {}",
                agent_name,
                available.join("\n  ")
            )
        })?
        .clone();

    // Dotpath resolution: "provider_id.model_key" → resolve provider + model
    // Clone to avoid holding an immutable borrow on `agent` during mutation.
    if let Some(model_dotpath) = agent.model.clone() {
        let (provider_id, model_key) = model_dotpath.split_once('.').ok_or_else(|| {
            anyhow::anyhow!(
                "Agent '{}': `model` must be 'provider_id.model_key', got '{}'",
                agent.name,
                model_dotpath
            )
        })?;

        let provider = config.providers.get(provider_id).ok_or_else(|| {
            anyhow::anyhow!(
                "Agent '{}': provider '{}' (from model '{}') not found in config. Available: {}",
                agent.name,
                provider_id,
                model_dotpath,
                config
                    .providers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        let model_def = provider.models.get(model_key).ok_or_else(|| {
            let available: Vec<&str> = provider.models.keys().map(|k| k.as_str()).collect();
            anyhow::anyhow!(
                "Agent '{}': model '{}' not found in provider '{}'. Available models: [{}]",
                agent.name,
                model_key,
                provider_id,
                available.join(", ")
            )
        })?;

        // Set provider_id from dotpath (overwrite legacy field)
        agent.provider_id = provider_id.to_string();

        // Merge model definition into agent config
        merge_model_def_into_agent(&mut agent, model_def);

        // Fallback: if model_name is still empty after merge, use the model_key
        if agent.model_name.is_empty() {
            agent.model_name = model_key.to_string();
        }
    }

    let provider = config.providers.get(&agent.provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider '{}' (used by agent '{}') not found in config. Available: {}",
            agent.provider_id,
            agent.name,
            config
                .providers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    // Expand ${VAR} references in api_key, then allow env var override.
    let mut provider_entry = provider.clone();
    provider_entry.api_key = resolve_env_token("api_key", &provider_entry.api_key);
    provider_entry.base_url = resolve_env_token("base_url", &provider_entry.base_url);

    let env_key_name = format!(
        "APP_PROVIDERS__{}__API_KEY",
        agent.provider_id.to_uppercase().replace('-', "_")
    );
    if let Ok(key) = std::env::var(&env_key_name) {
        provider_entry.api_key = key;
    } else if is_openai_compatible_provider(&agent.provider_id, &provider_entry)
        && is_placeholder_key(&provider_entry.api_key)
    {
        // Only fall back to OPENAI_API_KEY for canonical OpenAI providers
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            provider_entry.api_key = key;
        }
    }

    // Validate provider section consistency before anything else can bail
    agent
        .validate_provider_sections(Some(provider_entry.provider_type.as_str()))
        .map_err(|e| anyhow::anyhow!(e))?;
    agent
        .validate_compaction_knobs()
        .map_err(|e| anyhow::anyhow!(e))?;

    // Only require API keys for remote providers
    let is_local_url = provider_entry.base_url.starts_with("http://localhost")
        || provider_entry.base_url.starts_with("http://127.0.0.1");
    let is_local_provider = matches!(
        provider_entry.provider_type.as_str(),
        "ollama" | "vllm" | "local" | "simulated" | "stub" | "exec" | "mcp" | "claude"
    ) || is_local_provider_id(&agent.provider_id);
    if !is_local_url && !is_local_provider && is_placeholder_key(&provider_entry.api_key) {
        anyhow::bail!(
            "No API key for provider '{}'. Set {} env var.",
            agent.provider_id,
            env_key_name
        );
    }

    Ok((agent, provider_entry))
}

/// Check whether an API key is empty or a placeholder value.
fn is_placeholder_key(key: &str) -> bool {
    key.trim().is_empty() || key == "ollama" || key.starts_with("your-")
}

/// Check whether a provider is compatible with the OpenAI API key fallback.
/// Only canonical OpenAI providers (provider_id "openai" or base_url pointing
/// to `api.openai.com`) qualify for the `OPENAI_API_KEY` env var fallback.
fn is_openai_compatible_provider(provider_id: &str, entry: &ProviderEntry) -> bool {
    let id = provider_id.to_ascii_lowercase();
    id == "openai" || id.starts_with("openai_") || entry.base_url.contains("api.openai.com")
}

/// Check whether a provider_id indicates a local inference provider (e.g.
/// "ollama_default", "vllm_local") that does not require an API key.
pub fn is_local_provider_id(provider_id: &str) -> bool {
    const LOCAL_PREFIXES: &[&str] = &["ollama", "vllm", "local", "lmstudio"];
    let lower = provider_id.to_ascii_lowercase();
    LOCAL_PREFIXES
        .iter()
        .any(|prefix| lower == *prefix || lower.starts_with(&format!("{prefix}_")))
}

/// Resolve agent names from a name specification string.
/// - `"DEFAULT"` → `vec!["DEFAULT"]`
/// - `"ALL"` → all agents from config
/// - `"AGENT1,AGENT2,AGENT3"` → split by comma
pub fn resolve_agent_names(agent_name_spec: &str, config: &AgentFleetConfig) -> Vec<String> {
    if agent_name_spec.eq_ignore_ascii_case("ALL") {
        config.agents.iter().map(|a| a.name.clone()).collect()
    } else if agent_name_spec.contains(',') {
        agent_name_spec
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec![agent_name_spec.to_string()]
    }
}

/// Expand `${VAR_NAME}` references in a string with environment variable values.
/// Returns the raw string unchanged if it doesn't contain `${...}`.
///
/// `field` names the config field being resolved (e.g. `"api_key"`,
/// `"bearer_token"`) so the warning log points to the correct location.
pub fn resolve_env_token(field: &str, raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("${") {
        if let Some(var_name) = rest.strip_suffix('}') {
            return std::env::var(var_name).unwrap_or_else(|_| {
                tracing::warn!(
                    "Environment variable {} not set (referenced in {})",
                    var_name,
                    field,
                );
                String::new()
            });
        }
    }
    raw.to_string()
}

/// Derive a short orchestrator ID from its URL (hostname + optional port).
pub fn derive_orch_id(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("unknown")
        .replace(':', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ─── ProviderEntry deserialization ───────────────────────────────────

    #[test]
    fn test_provider_entry_defaults() {
        let yaml = r#"
base_url: "http://localhost:11434/v1"
"#;
        let entry: ProviderEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(entry.provider_type, "openai");
        assert_eq!(entry.base_url, "http://localhost:11434/v1");
        assert!(entry.api_key.is_empty());
        assert!(entry.engine.is_none());
        assert_eq!(entry.latency_ms, 0);
    }

    #[test]
    fn test_provider_entry_full() {
        let yaml = r#"
type: ollama
base_url: "http://localhost:11434/v1"
api_key: "test-key"
engine: "llama3"
latency_ms: 100
"#;
        let entry: ProviderEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(entry.provider_type, "ollama");
        assert_eq!(entry.api_key, "test-key");
        assert_eq!(entry.engine, Some("llama3".to_string()));
        assert_eq!(entry.latency_ms, 100);
    }

    // ─── AgentFleetConfig deserialization ────────────────────────────────

    #[test]
    fn test_fleet_config_minimal() {
        let yaml = r#"
providers:
  test_provider:
    type: stub
    base_url: "http://localhost:11434/v1"
agents:
  - name: ALPHA
    provider_id: test_provider
    model_name: test-model
"#;
        let config: AgentFleetConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "ALPHA");
        assert!(config.orchestrators.is_empty());
        assert!(config.response_sla_secs.is_none());
    }

    #[test]
    fn test_fleet_config_empty() {
        let yaml = "{}";
        let config: AgentFleetConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.providers.is_empty());
        assert!(config.agents.is_empty());
        assert!(config.orchestrators.is_empty());
    }

    // ─── load_config ────────────────────────────────────────────────────

    #[test]
    fn test_load_config_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("agents.yml");
        std::fs::write(
            &config_file,
            r#"
providers:
  stub:
    type: stub
    base_url: "http://stub"
agents:
  - name: AGENT_A
    provider_id: stub
    model_name: stub-model
"#,
        )
        .unwrap();

        let config = load_config(&config_file).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "AGENT_A");
    }

    #[test]
    #[serial]
    fn test_overlay_single_field_preserves_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers:
  foo:
    type: openai
    base_url: "https://api.example.com/v1"
    api_key: "default-key"
    engine: "gpt-4"
    latency_ms: 42
agents:
  - name: A
    provider_id: foo
    model_name: m1
"#,
        )
        .unwrap();

        // Overlay only sets api_key — all other fields must survive.
        unsafe { std::env::set_var("NSED_ENV", "overlay_test") };
        std::fs::write(
            dir.path().join("overlay_test.yml"),
            r#"
providers:
  foo:
    api_key: "new-secret"
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        let foo = config.providers.get("foo").unwrap();
        assert_eq!(foo.api_key, "new-secret", "overlay api_key should apply");
        assert_eq!(
            foo.base_url, "https://api.example.com/v1",
            "base_url preserved"
        );
        assert_eq!(foo.provider_type, "openai", "provider_type preserved");
        assert_eq!(foo.engine, Some("gpt-4".to_string()), "engine preserved");
        assert_eq!(foo.latency_ms, 42, "latency_ms preserved");

        unsafe { std::env::remove_var("NSED_ENV") };
    }

    #[test]
    #[serial]
    fn test_overlay_telemetry_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers: {}
agents: []
telemetry:
  enabled: true
"#,
        )
        .unwrap();

        unsafe { std::env::set_var("NSED_ENV", "telemetry_opt_out") };
        std::fs::write(
            dir.path().join("telemetry_opt_out.yml"),
            r#"
telemetry:
  enabled: false
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert!(
            !config.telemetry.enabled,
            "overlay telemetry block must override default"
        );

        unsafe { std::env::remove_var("NSED_ENV") };
    }

    /// Regression: a partial overlay that only tunes `endpoints`
    /// must NOT silently re-enable emission by clobbering the base
    /// `enabled: false` via whole-block replacement. Models the
    /// operator workflow where a production overlay declares
    /// tenant-specific destinations without wanting to reverse a
    /// per-env opt-out.
    #[test]
    #[serial]
    fn test_overlay_endpoints_preserves_base_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers: {}
agents: []
telemetry:
  enabled: false
"#,
        )
        .unwrap();

        unsafe { std::env::set_var("NSED_ENV", "endpoints_only_overlay") };
        std::fs::write(
            dir.path().join("endpoints_only_overlay.yml"),
            r#"
telemetry:
  endpoints:
    - name: tenant
      nats_url: "nats://tenant.example.com:4222"
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert!(
            !config.telemetry.enabled,
            "partial overlay must not flip enabled=false back to true"
        );
        assert_eq!(config.telemetry.endpoints.len(), 1);
        assert_eq!(config.telemetry.endpoints[0].name, "tenant");
        assert_eq!(
            config.telemetry.endpoints[0].nats_url.as_deref(),
            Some("nats://tenant.example.com:4222"),
        );

        unsafe { std::env::remove_var("NSED_ENV") };
    }

    /// `endpoints` overlay replaces the base list wholesale. Adding a
    /// new endpoint means restating any existing ones — element-wise
    /// merge would silently keep stale destinations after a rename,
    /// which is a worse failure mode than a verbose overlay.
    #[test]
    #[serial]
    fn test_overlay_endpoints_replaces_base_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers: {}
agents: []
telemetry:
  enabled: true
  endpoints:
    - name: stale
      nats_url: "nats://stale.example.com:4222"
"#,
        )
        .unwrap();

        unsafe { std::env::set_var("NSED_ENV", "endpoints_replace_overlay") };
        std::fs::write(
            dir.path().join("endpoints_replace_overlay.yml"),
            r#"
telemetry:
  endpoints:
    - name: prod
      nats_url: "nats://prod.example.com:4222"
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert_eq!(config.telemetry.endpoints.len(), 1);
        assert_eq!(
            config.telemetry.endpoints[0].name, "prod",
            "overlay endpoints must replace base wholesale, not merge by name"
        );

        unsafe { std::env::remove_var("NSED_ENV") };
    }

    #[test]
    #[serial]
    fn test_default_telemetry_enabled_when_block_absent() {
        let dir = tempfile::tempdir().unwrap();
        // No `telemetry:` key in either file — must fall back to the
        // default-on policy from `TelemetryConfig::default()`.
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers: {}
agents: []
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert!(
            config.telemetry.enabled,
            "missing telemetry block must default to enabled"
        );
    }

    #[test]
    #[serial]
    fn test_overlay_new_provider_added() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers:
  existing:
    type: stub
    base_url: "http://stub"
agents: []
"#,
        )
        .unwrap();

        unsafe { std::env::set_var("NSED_ENV", "new_provider_test") };
        std::fs::write(
            dir.path().join("new_provider_test.yml"),
            r#"
providers:
  brand_new:
    type: ollama
    base_url: "http://localhost:11434/v1"
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert!(config.providers.contains_key("existing"));
        let new = config.providers.get("brand_new").unwrap();
        assert_eq!(new.provider_type, "ollama");
        assert_eq!(new.base_url, "http://localhost:11434/v1");

        unsafe { std::env::remove_var("NSED_ENV") };
    }

    #[test]
    fn test_load_config_directory_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers:
  p1:
    type: stub
    base_url: "http://default"
agents:
  - name: DEFAULT_AGENT
    provider_id: p1
    model_name: m1
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "DEFAULT_AGENT");
    }

    #[test]
    fn test_load_config_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent.yml");
        let err = load_config(&missing).unwrap_err();
        assert!(err.to_string().contains("not found") || err.to_string().contains("No such file"));
    }

    // ─── load_agent_from_config ─────────────────────────────────────────

    fn stub_provider() -> ProviderEntry {
        ProviderEntry {
            provider_type: "stub".to_string(),
            base_url: "http://localhost".to_string(),
            api_key: String::new(),
            engine: None,
            latency_ms: 0,
            models: HashMap::new(),
        }
    }

    #[test]
    fn test_load_agent_from_config_found() {
        let config = AgentFleetConfig {
            providers: HashMap::from([("p1".to_string(), stub_provider())]),
            agents: vec![AgentConfig {
                name: "ALPHA".to_string(),
                provider_id: "p1".to_string(),
                model_name: "m1".to_string(),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (agent, provider) = load_agent_from_config(&config, "alpha").unwrap();
        assert_eq!(agent.name, "ALPHA");
        assert_eq!(provider.provider_type, "stub");
    }

    #[test]
    fn test_load_agent_from_config_not_found() {
        let config = AgentFleetConfig {
            providers: HashMap::new(),
            agents: vec![AgentConfig {
                name: "BETA".to_string(),
                provider_id: "p1".to_string(),
                model_name: "m1".to_string(),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let err = load_agent_from_config(&config, "MISSING").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_load_agent_from_config_missing_provider() {
        let config = AgentFleetConfig {
            providers: HashMap::new(),
            agents: vec![AgentConfig {
                name: "ALPHA".to_string(),
                provider_id: "nonexistent".to_string(),
                model_name: "m1".to_string(),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let err = load_agent_from_config(&config, "ALPHA").unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    #[serial]
    fn test_load_agent_expands_env_in_api_key() {
        unsafe { std::env::set_var("SDK_TEST_API_KEY_777", "secret-key-value") };
        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "p1".to_string(),
                ProviderEntry {
                    provider_type: "openai".to_string(),
                    base_url: "https://api.example.com".to_string(),
                    api_key: "${SDK_TEST_API_KEY_777}".to_string(),
                    engine: None,
                    latency_ms: 0,
                    models: HashMap::new(),
                },
            )]),
            agents: vec![AgentConfig {
                name: "AGENT".to_string(),
                provider_id: "p1".to_string(),
                model_name: "m1".to_string(),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (_, provider) = load_agent_from_config(&config, "AGENT").unwrap();
        assert_eq!(provider.api_key, "secret-key-value");
        unsafe { std::env::remove_var("SDK_TEST_API_KEY_777") };
    }

    #[test]
    #[serial]
    fn test_openai_key_not_inherited_by_non_openai_provider() {
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-should-not-leak") };
        // Remove any provider-specific override
        unsafe { std::env::remove_var("APP_PROVIDERS__ANTHROPIC__API_KEY") };

        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "anthropic".to_string(),
                ProviderEntry {
                    provider_type: "openai".to_string(),
                    base_url: "https://api.anthropic.com/v1".to_string(),
                    api_key: String::new(), // empty — should NOT fall back to OPENAI_API_KEY
                    engine: None,
                    latency_ms: 0,
                    models: HashMap::new(),
                },
            )]),
            agents: vec![AgentConfig {
                name: "CLAUDE".to_string(),
                provider_id: "anthropic".to_string(),
                model_name: "claude-3".to_string(),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        // Non-OpenAI provider should NOT get OPENAI_API_KEY fallback
        let result = load_agent_from_config(&config, "CLAUDE");
        assert!(
            result.is_err(),
            "Non-OpenAI provider should fail without its own API key, not inherit OPENAI_API_KEY"
        );

        unsafe { std::env::remove_var("OPENAI_API_KEY") };
    }

    // ─── resolve_agent_names ────────────────────────────────────────────

    fn make_fleet_config(agent_names: &[&str]) -> AgentFleetConfig {
        AgentFleetConfig {
            providers: HashMap::new(),
            agents: agent_names
                .iter()
                .map(|name| AgentConfig {
                    name: name.to_string(),
                    provider_id: "test".into(),
                    model_name: "test-model".into(),
                    ..Default::default()
                })
                .collect(),
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        }
    }

    #[test]
    fn test_resolve_agent_names_single() {
        let config = make_fleet_config(&["DEFAULT", "REENTRY"]);
        assert_eq!(resolve_agent_names("DEFAULT", &config), vec!["DEFAULT"]);
    }

    #[test]
    fn test_resolve_agent_names_all() {
        let config = make_fleet_config(&["ALPHA", "BETA", "GAMMA"]);
        assert_eq!(
            resolve_agent_names("ALL", &config),
            vec!["ALPHA", "BETA", "GAMMA"]
        );
    }

    #[test]
    fn test_resolve_agent_names_all_case_insensitive() {
        let config = make_fleet_config(&["A", "B"]);
        assert_eq!(resolve_agent_names("all", &config), vec!["A", "B"]);
    }

    #[test]
    fn test_resolve_agent_names_comma_separated() {
        let config = make_fleet_config(&[]);
        assert_eq!(
            resolve_agent_names("REENTRY,STATIC,FUZZ", &config),
            vec!["REENTRY", "STATIC", "FUZZ"]
        );
    }

    #[test]
    fn test_resolve_agent_names_comma_with_whitespace() {
        let config = make_fleet_config(&[]);
        assert_eq!(
            resolve_agent_names(" A , B , C ", &config),
            vec!["A", "B", "C"]
        );
    }

    #[test]
    fn test_resolve_agent_names_comma_skips_empty() {
        let config = make_fleet_config(&[]);
        assert_eq!(resolve_agent_names("A,,B,", &config), vec!["A", "B"]);
    }

    // ─── is_local_provider_id ───────────────────────────────────────────

    #[test]
    fn test_is_local_provider_id_known_prefixes() {
        assert!(is_local_provider_id("ollama_default"));
        assert!(is_local_provider_id("vllm_local"));
        assert!(is_local_provider_id("local_gpu"));
        assert!(is_local_provider_id("lmstudio_7b"));
    }

    #[test]
    fn test_is_local_provider_id_exact_match() {
        assert!(is_local_provider_id("ollama"));
        assert!(is_local_provider_id("OLLAMA"));
    }

    #[test]
    fn test_is_local_provider_id_remote() {
        assert!(!is_local_provider_id("together_ai"));
        assert!(!is_local_provider_id("openai"));
    }

    // ─── resolve_env_token ──────────────────────────────────────────────

    #[test]
    fn test_resolve_env_token_literal() {
        assert_eq!(resolve_env_token("test", "literal_value"), "literal_value");
    }

    #[test]
    #[serial]
    fn test_resolve_env_token_expansion() {
        unsafe { std::env::set_var("SDK_TEST_TOKEN_VAR", "expanded") };
        assert_eq!(
            resolve_env_token("test", "${SDK_TEST_TOKEN_VAR}"),
            "expanded"
        );
        unsafe { std::env::remove_var("SDK_TEST_TOKEN_VAR") };
    }

    #[test]
    #[serial]
    fn test_resolve_env_token_missing_var() {
        unsafe { std::env::remove_var("NONEXISTENT_SDK_VAR_99") };
        assert_eq!(resolve_env_token("test", "${NONEXISTENT_SDK_VAR_99}"), "");
    }

    #[test]
    fn test_resolve_env_token_empty_braces() {
        assert_eq!(resolve_env_token("test", "${}"), "");
    }

    // ─── derive_orch_id ─────────────────────────────────────────────────

    #[test]
    fn test_derive_orch_id_localhost() {
        assert_eq!(derive_orch_id("http://localhost:8080"), "localhost_8080");
    }

    #[test]
    fn test_derive_orch_id_domain() {
        assert_eq!(
            derive_orch_id("https://orch.example.com/api"),
            "orch.example.com"
        );
    }

    #[test]
    fn test_derive_orch_id_ip() {
        assert_eq!(
            derive_orch_id("http://192.168.1.1:9090/path"),
            "192.168.1.1_9090"
        );
    }

    #[test]
    fn test_exec_provider_skips_api_key_check() {
        // An exec provider with no API key should load without error.
        let yaml = r#"
providers:
  exec_local:
    type: exec
agents:
  - name: PY_AGENT
    provider_id: exec_local
    model_name: custom
"#;
        let config: AgentFleetConfig = serde_yaml::from_str(yaml).unwrap();
        let (agent, provider) = load_agent_from_config(&config, "PY_AGENT").unwrap();
        assert_eq!(provider.provider_type, "exec");
        assert_eq!(agent.name, "PY_AGENT");
    }

    #[test]
    fn test_load_agent_rejects_mismatched_provider_section() {
        use crate::agents::config::ExecProviderConfig;
        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "p1".to_string(),
                ProviderEntry {
                    provider_type: "mcp".to_string(),
                    base_url: "http://localhost".to_string(),
                    api_key: String::new(),
                    engine: None,
                    latency_ms: 0,
                    models: HashMap::new(),
                },
            )]),
            agents: vec![AgentConfig {
                name: "BAD".to_string(),
                provider_id: "p1".to_string(),
                model_name: "m".to_string(),
                exec: Some(ExecProviderConfig {
                    command: vec!["echo".into()],
                    working_dir: None,
                    env: HashMap::new(),
                    timeout_secs: None,
                }),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let err = load_agent_from_config(&config, "BAD").unwrap_err();
        assert!(
            err.to_string().contains("does not match"),
            "Expected mismatch error, got: {err}"
        );
    }

    // ─── ModelDef + dotpath resolution ──────────────────────────────────

    #[test]
    fn test_model_def_deserialization() {
        let yaml = r#"
model_name: "meta-llama/Llama-3-70b"
temperature: 0.7
max_tokens: 8192
context_window: 131072
frequency_penalty: 0.2
presence_penalty: 0.8
use_streaming: true
reasoning_effort: "medium"
repair_invalid_escapes: false
tool_format: "nous"
disable_native_tools: true
merge_system_prompt: true
unwrap_hallucinated_tool_calls: true
json_mode: true
chars_per_token: 3.5
scratchpad_limit: 500
max_scratchpad_size: 16384
supports_native_thinking: true
input_price_per_mtok: 0.90
output_price_per_mtok: 0.90
"#;
        let def: ModelDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(def.model_name, Some("meta-llama/Llama-3-70b".to_string()));
        assert_eq!(def.temperature, Some(0.7));
        assert_eq!(def.max_tokens, Some(8192));
        assert_eq!(def.context_window, Some(131072));
        assert_eq!(def.frequency_penalty, Some(0.2));
        assert_eq!(def.presence_penalty, Some(0.8));
        assert_eq!(def.use_streaming, Some(true));
        assert_eq!(def.reasoning_effort, Some("medium".to_string()));
        assert_eq!(def.repair_invalid_escapes, Some(false));
        assert_eq!(def.tool_format, Some("nous".to_string()));
        assert_eq!(def.disable_native_tools, Some(true));
        assert_eq!(def.merge_system_prompt, Some(true));
        assert_eq!(def.unwrap_hallucinated_tool_calls, Some(true));
        assert_eq!(def.json_mode, Some(true));
        assert_eq!(def.chars_per_token, Some(3.5));
        assert_eq!(def.scratchpad_limit, Some(500));
        assert_eq!(def.max_scratchpad_size, Some(16384));
        assert_eq!(def.supports_native_thinking, Some(true));
        assert_eq!(def.input_price_per_mtok, Some(0.90));
        assert_eq!(def.output_price_per_mtok, Some(0.90));
    }

    #[test]
    fn test_model_def_defaults_to_all_none() {
        let def = ModelDef::default();
        assert!(def.model_name.is_none());
        assert!(def.temperature.is_none());
        assert!(def.max_tokens.is_none());
        assert!(def.context_window.is_none());
    }

    #[test]
    fn test_provider_entry_with_models() {
        let yaml = r#"
type: openai
base_url: "https://api.together.xyz/v1"
api_key: "test-key"
models:
  llama-70b:
    model_name: "meta-llama/Llama-3-70b"
    temperature: 0.7
    max_tokens: 8192
    context_window: 131072
  llama-8b:
    model_name: "meta-llama/Llama-3-8b"
    temperature: 0.3
"#;
        let entry: ProviderEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(entry.models.len(), 2);
        let llama70b = entry.models.get("llama-70b").unwrap();
        assert_eq!(
            llama70b.model_name,
            Some("meta-llama/Llama-3-70b".to_string())
        );
        assert_eq!(llama70b.temperature, Some(0.7));
        assert_eq!(llama70b.max_tokens, Some(8192));
        let llama8b = entry.models.get("llama-8b").unwrap();
        assert_eq!(llama8b.temperature, Some(0.3));
        assert!(llama8b.max_tokens.is_none());
    }

    #[test]
    fn test_provider_entry_without_models_default() {
        let yaml = r#"
type: stub
base_url: "http://localhost"
"#;
        let entry: ProviderEntry = serde_yaml::from_str(yaml).unwrap();
        assert!(entry.models.is_empty());
    }

    #[test]
    fn test_dotpath_resolution_basic() {
        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "p1".to_string(),
                ProviderEntry {
                    provider_type: "stub".to_string(),
                    base_url: "http://localhost".to_string(),
                    api_key: String::new(),
                    engine: None,
                    latency_ms: 0,
                    models: HashMap::from([(
                        "llama".to_string(),
                        ModelDef {
                            model_name: Some("meta-llama/Llama-3-70b".to_string()),
                            temperature: Some(0.7),
                            max_tokens: Some(8192),
                            ..Default::default()
                        },
                    )]),
                },
            )]),
            agents: vec![AgentConfig {
                name: "Researcher".to_string(),
                model: Some("p1.llama".to_string()),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (agent, provider) = load_agent_from_config(&config, "Researcher").unwrap();
        assert_eq!(agent.provider_id, "p1");
        assert_eq!(agent.model_name, "meta-llama/Llama-3-70b");
        assert_eq!(agent.temperature, 0.7);
        assert_eq!(agent.max_tokens, 8192);
        assert_eq!(provider.provider_type, "stub");
    }

    #[test]
    fn test_dotpath_resolution_merges_all_fields() {
        let model_def = ModelDef {
            model_name: Some("test-model".to_string()),
            temperature: Some(0.5),
            max_tokens: Some(4096),
            context_window: Some(65536),
            frequency_penalty: Some(0.1),
            presence_penalty: Some(0.3),
            use_streaming: Some(false),
            reasoning_effort: Some("high".to_string()),
            repair_invalid_escapes: Some(false),
            tool_format: Some("nous".to_string()),
            disable_native_tools: Some(true),
            merge_system_prompt: Some(true),
            unwrap_hallucinated_tool_calls: Some(true),
            json_mode: Some(true),
            chars_per_token: Some(1.5),
            scratchpad_limit: Some(500),
            max_scratchpad_size: Some(16384),
            supports_native_thinking: Some(true),
            input_price_per_mtok: Some(2.5),
            output_price_per_mtok: Some(10.0),
        };
        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "p1".to_string(),
                ProviderEntry {
                    provider_type: "stub".to_string(),
                    base_url: "http://localhost".to_string(),
                    api_key: String::new(),
                    engine: None,
                    latency_ms: 0,
                    models: HashMap::from([("full".to_string(), model_def)]),
                },
            )]),
            agents: vec![AgentConfig {
                name: "FULL".to_string(),
                model: Some("p1.full".to_string()),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (a, _) = load_agent_from_config(&config, "FULL").unwrap();
        assert_eq!(a.model_name, "test-model");
        assert_eq!(a.temperature, 0.5);
        assert_eq!(a.max_tokens, 4096);
        assert_eq!(a.context_window, 65536);
        assert_eq!(a.frequency_penalty, Some(0.1));
        assert_eq!(a.presence_penalty, Some(0.3));
        assert!(!a.use_streaming);
        assert_eq!(a.reasoning_effort, Some("high".to_string()));
        assert!(!a.repair_invalid_escapes);
        assert_eq!(a.tool_format, Some("nous".to_string()));
        assert!(a.disable_native_tools);
        assert!(a.merge_system_prompt);
        assert!(a.unwrap_hallucinated_tool_calls);
        assert!(a.json_mode);
        assert_eq!(a.chars_per_token, Some(1.5));
        assert_eq!(a.scratchpad_limit, 500);
        assert_eq!(a.max_scratchpad_size, Some(16384));
        assert!(a.supports_native_thinking);
        assert_eq!(a.input_price_per_mtok, Some(2.5));
        assert_eq!(a.output_price_per_mtok, Some(10.0));
    }

    #[test]
    fn test_dotpath_agent_level_override() {
        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "p1".to_string(),
                ProviderEntry {
                    provider_type: "stub".to_string(),
                    base_url: "http://localhost".to_string(),
                    api_key: String::new(),
                    engine: None,
                    latency_ms: 0,
                    models: HashMap::from([(
                        "m1".to_string(),
                        ModelDef {
                            model_name: Some("base-model".to_string()),
                            temperature: Some(0.7),
                            max_tokens: Some(8192),
                            ..Default::default()
                        },
                    )]),
                },
            )]),
            agents: vec![AgentConfig {
                name: "OVERRIDER".to_string(),
                model: Some("p1.m1".to_string()),
                temperature: 0.9, // agent-level override — NOT the default 0.0
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (agent, _) = load_agent_from_config(&config, "OVERRIDER").unwrap();
        // Agent explicitly set temperature=0.9, so ModelDef's 0.7 should NOT override
        assert_eq!(agent.temperature, 0.9);
        // But model_name and max_tokens should come from ModelDef
        assert_eq!(agent.model_name, "base-model");
        assert_eq!(agent.max_tokens, 8192);
    }

    #[test]
    fn test_dotpath_invalid_no_dot() {
        let config = AgentFleetConfig {
            providers: HashMap::from([("p1".to_string(), stub_provider())]),
            agents: vec![AgentConfig {
                name: "BAD".to_string(),
                model: Some("nodot".to_string()),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let err = load_agent_from_config(&config, "BAD").unwrap_err();
        assert!(
            err.to_string().contains("provider_id.model_key"),
            "Expected dotpath format error, got: {err}"
        );
    }

    #[test]
    fn test_dotpath_provider_not_found() {
        let config = AgentFleetConfig {
            providers: HashMap::from([("p1".to_string(), stub_provider())]),
            agents: vec![AgentConfig {
                name: "BAD".to_string(),
                model: Some("missing.model".to_string()),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let err = load_agent_from_config(&config, "BAD").unwrap_err();
        assert!(
            err.to_string().contains("missing"),
            "Expected provider not found, got: {err}"
        );
    }

    #[test]
    fn test_dotpath_model_not_found() {
        let config = AgentFleetConfig {
            providers: HashMap::from([("p1".to_string(), stub_provider())]),
            agents: vec![AgentConfig {
                name: "BAD".to_string(),
                model: Some("p1.nonexistent".to_string()),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let err = load_agent_from_config(&config, "BAD").unwrap_err();
        assert!(
            err.to_string().contains("nonexistent"),
            "Expected model not found, got: {err}"
        );
    }

    #[test]
    fn test_dotpath_sets_provider_id_and_model_name() {
        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "together".to_string(),
                ProviderEntry {
                    provider_type: "openai".to_string(),
                    base_url: "https://api.together.xyz/v1".to_string(),
                    api_key: "test-key".to_string(),
                    engine: None,
                    latency_ms: 0,
                    models: HashMap::from([(
                        "llama".to_string(),
                        ModelDef {
                            model_name: Some("meta-llama/Llama-3-70b".to_string()),
                            ..Default::default()
                        },
                    )]),
                },
            )]),
            agents: vec![AgentConfig {
                name: "AGENT".to_string(),
                model: Some("together.llama".to_string()),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (agent, _) = load_agent_from_config(&config, "AGENT").unwrap();
        assert_eq!(agent.provider_id, "together");
        assert_eq!(agent.model_name, "meta-llama/Llama-3-70b");
    }

    #[test]
    fn test_backward_compat_flat_fields() {
        // Old-style config with provider_id + model_name + flat LLM fields
        let config = AgentFleetConfig {
            providers: HashMap::from([("p1".to_string(), stub_provider())]),
            agents: vec![AgentConfig {
                name: "OLD_STYLE".to_string(),
                provider_id: "p1".to_string(),
                model_name: "gpt-4".to_string(),
                temperature: 0.8,
                max_tokens: 4096,
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (agent, _) = load_agent_from_config(&config, "OLD_STYLE").unwrap();
        assert_eq!(agent.model_name, "gpt-4");
        assert_eq!(agent.temperature, 0.8);
        assert_eq!(agent.max_tokens, 4096);
        assert!(agent.model.is_none());
    }

    #[test]
    fn test_dotpath_preferred_over_flat() {
        // Agent has both `model` (new) and `provider_id`+`model_name` (old)
        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "p1".to_string(),
                ProviderEntry {
                    models: HashMap::from([(
                        "llama".to_string(),
                        ModelDef {
                            model_name: Some("resolved-llama".to_string()),
                            temperature: Some(0.3),
                            ..Default::default()
                        },
                    )]),
                    ..stub_provider()
                },
            )]),
            agents: vec![AgentConfig {
                name: "BOTH".to_string(),
                model: Some("p1.llama".to_string()),
                provider_id: "old_provider".to_string(),
                model_name: "old-model".to_string(),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (agent, _) = load_agent_from_config(&config, "BOTH").unwrap();
        // Dotpath wins: provider_id is overwritten
        assert_eq!(agent.provider_id, "p1");
        // model_name was "old-model" (not default ""), so agent override wins
        assert_eq!(agent.model_name, "old-model");
    }

    #[test]
    fn test_fleet_config_yaml_with_models() {
        let yaml = r#"
providers:
  together_ai:
    type: openai
    base_url: "https://api.together.xyz/v1"
    api_key: "test-key"
    models:
      llama-70b:
        model_name: "meta-llama/Llama-3-70b"
        temperature: 0.7
        max_tokens: 8192
        context_window: 131072
      llama-8b:
        model_name: "meta-llama/Llama-3-8b"
  claude_cli:
    type: claude
    models:
      opus:
        model_name: "opus"
      sonnet:
        model_name: "sonnet"
agents:
  - name: Researcher
    model: "together_ai.llama-70b"
    persona: "Research specialist"
  - name: PL_Product
    model: "claude_cli.opus"
    persona: "Product expert"
"#;
        let config: AgentFleetConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.agents.len(), 2);

        let together = config.providers.get("together_ai").unwrap();
        assert_eq!(together.models.len(), 2);

        let claude = config.providers.get("claude_cli").unwrap();
        assert_eq!(claude.models.len(), 2);

        // Verify dotpath fields on agents
        assert_eq!(
            config.agents[0].model,
            Some("together_ai.llama-70b".to_string())
        );
        assert_eq!(config.agents[1].model, Some("claude_cli.opus".to_string()));
    }

    #[test]
    #[serial]
    fn test_overlay_preserves_models() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers:
  p1:
    type: stub
    base_url: "http://localhost"
    models:
      m1:
        model_name: "base-model"
        temperature: 0.5
agents:
  - name: A
    model: "p1.m1"
"#,
        )
        .unwrap();

        unsafe { std::env::set_var("NSED_ENV", "overlay_models_test") };
        std::fs::write(
            dir.path().join("overlay_models_test.yml"),
            r#"
providers:
  p1:
    api_key: "new-key"
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        let p1 = config.providers.get("p1").unwrap();
        assert_eq!(p1.api_key, "new-key", "overlay api_key should apply");
        assert_eq!(p1.models.len(), 1, "models should be preserved");
        assert_eq!(
            p1.models.get("m1").unwrap().model_name,
            Some("base-model".to_string())
        );

        unsafe { std::env::remove_var("NSED_ENV") };
    }

    #[test]
    #[serial]
    fn test_overlay_adds_models() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("default.yml"),
            r#"
providers:
  p1:
    type: stub
    base_url: "http://localhost"
    models:
      m1:
        model_name: "base-model"
agents: []
"#,
        )
        .unwrap();

        unsafe { std::env::set_var("NSED_ENV", "overlay_add_models_test") };
        std::fs::write(
            dir.path().join("overlay_add_models_test.yml"),
            r#"
providers:
  p1:
    models:
      m2:
        model_name: "new-model"
"#,
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        let p1 = config.providers.get("p1").unwrap();
        assert_eq!(
            p1.models.len(),
            2,
            "should have both base and overlay models"
        );
        assert!(p1.models.contains_key("m1"));
        assert!(p1.models.contains_key("m2"));

        unsafe { std::env::remove_var("NSED_ENV") };
    }

    #[test]
    fn test_dotpath_model_without_model_name_uses_key() {
        // When ModelDef.model_name is None, the agent.model_name stays at default
        let config = AgentFleetConfig {
            providers: HashMap::from([(
                "p1".to_string(),
                ProviderEntry {
                    models: HashMap::from([(
                        "fast".to_string(),
                        ModelDef {
                            temperature: Some(0.3),
                            ..Default::default()
                        },
                    )]),
                    ..stub_provider()
                },
            )]),
            agents: vec![AgentConfig {
                name: "AGENT".to_string(),
                model: Some("p1.fast".to_string()),
                ..Default::default()
            }],
            orchestrators: vec![],
            response_sla_secs: None,
            telemetry: Default::default(),
            dashboard_port: None,
        };

        let (agent, _) = load_agent_from_config(&config, "AGENT").unwrap();
        // model_name not set in ModelDef, falls back to model_key "fast"
        assert_eq!(agent.model_name, "fast");
        assert_eq!(agent.temperature, 0.3);
    }
}
