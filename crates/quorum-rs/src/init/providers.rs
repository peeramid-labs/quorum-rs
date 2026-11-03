//! Provider types, model discovery, and API helpers.

use serde::Deserialize;

// ── Provider catalogue ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(super) struct Provider {
    /// Slug used as the key in `providers:` blocks (e.g. `together_ai`).
    pub id: String,
    pub base_url: String,
    /// `None` for Ollama-local and Simulated providers.
    pub api_key: Option<String>,
    /// USD per million input tokens; `None` for local/simulated.
    pub input_price: Option<f64>,
    /// USD per million output tokens; `None` for local/simulated.
    pub output_price: Option<f64>,
    /// Provider type tag written to YAML: `"openai"` or `"simulated"`.
    pub provider_type: String,
    /// Discovered models from `/v1/models` (empty if fetch failed).
    pub models: Vec<ModelInfo>,
    /// LLM strategy engine override (e.g. `"harmony"`, `"vllm_responses"`).
    pub engine: Option<String>,
}

/// Discovered model with optional per-token pricing.
#[derive(Debug, Clone)]
pub(super) struct ModelInfo {
    pub name: String,
    /// USD per million input tokens.
    pub input_price: Option<f64>,
    /// USD per million output tokens.
    pub output_price: Option<f64>,
}

impl Provider {
    pub(super) fn is_local(&self) -> bool {
        self.api_key.is_none() && self.provider_type != "simulated"
    }
}

// ── Model discovery ────────────────────────────────────────────────────────

/// Model info returned by `/v1/models`.
#[derive(Debug, Deserialize)]
pub(super) struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
    /// Model type (Together AI: "chat", "language", "code", "image",
    /// "embedding", "moderation", "rerank").  Not all providers set this.
    #[serde(rename = "type", default)]
    pub model_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ModelPricing {
    /// USD per million input tokens (Together AI format).
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
}

/// OpenAI-style response: `{ "data": [...] }`
#[derive(Debug, Deserialize)]
pub(super) struct ModelsResponseWrapped {
    pub data: Vec<ModelEntry>,
}

/// Model IDs containing these substrings are excluded from the list
/// (embedding / reranking / vision models not suitable for deliberation).
pub(super) const MODEL_FILTER_KEYWORDS: &[&str] =
    &["embed", "rerank", "bge", "e5-", "clip", "vision", "whisper"];

/// Model types that are NOT suitable for deliberation.
pub(super) const EXCLUDED_MODEL_TYPES: &[&str] = &["embedding", "rerank", "moderation", "image"];

pub(super) fn is_inference_model(entry: &ModelEntry) -> bool {
    // If the provider gives us a model type, use it for filtering
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if let Some(ref t) = entry.model_type {
        let lower = t.to_lowercase();
        // TODO(slop): add test for new `if` branch (no paired test file in this patch)
        if EXCLUDED_MODEL_TYPES.iter().any(|kw| lower == *kw) {
            return false;
        }
        return true;
    }
    // Fallback: keyword-based filtering on model ID
    let lower = entry.id.to_lowercase();
    !MODEL_FILTER_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Parse raw bytes from a `/v1/models` response into filtered `FetchedModel` entries.
///
/// Supports both OpenAI-style `{ "data": [...] }` and flat-array `[...]`
/// formats.  Non-inference models (embeddings, rerankers, etc.) are removed.
pub(super) fn parse_models_response(bytes: &[u8]) -> Option<Vec<FetchedModel>> {
    // Strategy 1: OpenAI-style `{ "data": [...] }`
    let entries: Vec<ModelEntry> =
// TODO(slop): add test for new `if` branch (no paired test file in this patch)
        if let Ok(wrapped) = serde_json::from_slice::<ModelsResponseWrapped>(bytes) {
            wrapped.data
// TODO(slop): add test for new `if` branch (no paired test file in this patch)
        } else if let Ok(flat) = serde_json::from_slice::<Vec<ModelEntry>>(bytes) {
            // Strategy 2: flat array `[...]`
            flat
        } else {
            return None;
        };

    let models = entries
        .into_iter()
        .filter(is_inference_model)
        .map(|m| {
            let (inp, out) = extract_pricing(m.pricing.as_ref());
            (m.id, inp, out)
        })
        .collect();

    Some(models)
}

/// Build the URL for a `/v1/models` request, normalizing trailing slashes.
pub(super) fn build_models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Attempt to fetch models from a provider endpoint.
///
/// Supports both OpenAI-style `{ "data": [...] }` and flat-array `[...]`
/// responses (some providers like Together AI may use either format).
pub(super) async fn fetch_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Option<Vec<FetchedModel>> {
    let url = build_models_url(base_url);
    // TODO(slop): add test for new `try_q` branch (no paired test file in this patch)
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;

    let mut req = client.get(&url);
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if let Some(key) = api_key {
        // TODO(slop): add test for new `if` branch (no paired test file in this patch)
        if !key.is_empty() {
            req = req.bearer_auth(key);
        }
    }

    // TODO(slop): add test for new `try_q` branch (no paired test file in this patch)
    let resp = req.send().await.ok()?;
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if !resp.status().is_success() {
        return None;
    }

    // Read the raw body so we can try two different parse strategies.
    // TODO(slop): add test for new `try_q` branch (no paired test file in this patch)
    let bytes = resp.bytes().await.ok()?;
    parse_models_response(&bytes)
}

pub(super) fn extract_pricing(p: Option<&ModelPricing>) -> (Option<f64>, Option<f64>) {
    // TODO(slop): add test for new `match` branch (no paired test file in this patch)
    let p = match p {
        // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
        Some(p) => p,
        // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
        None => return (None, None),
    };
    let parse = |v: &serde_json::Value| -> Option<f64> {
        // TODO(slop): add test for new `match` branch (no paired test file in this patch)
        match v {
            // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
            serde_json::Value::Number(n) => n.as_f64(),
            // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
            serde_json::Value::String(s) => s.parse().ok(),
            // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
            _ => None,
        }
    };
    let inp = p.input.as_ref().and_then(parse);
    let out = p.output.as_ref().and_then(parse);
    (inp, out)
}

/// `(model_id, input_price_per_mtok, output_price_per_mtok)`.
pub(super) type FetchedModel = (String, Option<f64>, Option<f64>);

/// Check if Ollama is running at the default local port.
pub(super) async fn detect_ollama() -> Option<Vec<FetchedModel>> {
    // TODO(slop): inline placeholder URL (localhost / example.com / YOUR_...) — route through config / env before shipping
    fetch_models("http://localhost:11434/v1", None).await
}

// ── Provider builder functions ──────────────────────────────────────────────

pub(super) fn build_ollama_provider(models: &[FetchedModel]) -> Provider {
    let model_infos = fetched_to_model_infos(models);
    Provider {
        id: "ollama_local".to_string(),
        // TODO(slop): inline placeholder URL (localhost / example.com / YOUR_...) — route through config / env before shipping
        base_url: "http://localhost:11434/v1".to_string(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "openai".to_string(),
        models: model_infos,
        engine: None,
    }
}

pub(super) fn build_simulated_provider() -> Provider {
    Provider {
        id: "simulated".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "simulated".to_string(),
        models: vec![ModelInfo {
            name: "simulated-default".to_string(),
            input_price: Some(0.0),
            output_price: Some(0.0),
        }],
        engine: None,
    }
}

pub(super) fn build_openai_oauth_provider() -> Provider {
    Provider {
        id: "openai_oauth".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: None,
        output_price: None,
        provider_type: "openai-oauth".to_string(),
        models: vec![ModelInfo {
            name: "gpt-5.5".to_string(),
            input_price: None,
            output_price: None,
        }],
        engine: None,
    }
}

/// Detected exec-capable tool with version info.
#[derive(Debug, Clone)]
pub(super) struct DetectedTool {
    pub name: &'static str,
    pub version: String,
}

/// Detect exec-capable tools in the local environment (claude, python3, docker).
///
/// Uses blocking `std::process::Command` — fast enough for version checks.
pub(super) fn detect_exec_tools() -> Vec<DetectedTool> {
    let mut tools = Vec::new();

    // Claude CLI
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if let Ok(output) = std::process::Command::new("claude")
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        // TODO(slop): add test for new `if` branch (no paired test file in this patch)
        if output.status.success() {
            let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
            tools.push(DetectedTool {
                name: "claude",
                version: ver,
            });
        }
    }

    // python3
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if let Ok(output) = std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        // TODO(slop): add test for new `if` branch (no paired test file in this patch)
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout);
            let ver = raw.trim().replace("Python ", "");
            tools.push(DetectedTool {
                name: "python3",
                version: ver,
            });
        }
    }

    // docker
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if let Ok(output) = std::process::Command::new("docker")
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        // TODO(slop): add test for new `if` branch (no paired test file in this patch)
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout);
            // "Docker version 24.0.7, build afdd53b"
            let ver = raw
                .trim()
                .strip_prefix("Docker version ")
                .and_then(|s| s.split(',').next())
                .unwrap_or("unknown")
                .to_string();
            tools.push(DetectedTool {
                name: "docker",
                version: ver,
            });
        }
    }

    tools
}

/// Build a Claude CLI provider (auto-detected).
/// Uses `type: claude` which auto-constructs CLI flags from AgentConfig
/// (system prompt, model, session persistence, MCP tools, permissions).
pub(super) fn build_claude_exec_provider() -> Provider {
    Provider {
        id: "claude_cli".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "claude".to_string(),
        models: vec![
            ModelInfo {
                name: "sonnet".to_string(),
                input_price: Some(0.0),
                output_price: Some(0.0),
            },
            ModelInfo {
                name: "opus".to_string(),
                input_price: Some(0.0),
                output_price: Some(0.0),
            },
            ModelInfo {
                name: "haiku".to_string(),
                input_price: Some(0.0),
                output_price: Some(0.0),
            },
        ],
        engine: None,
    }
}

pub(super) fn build_exec_provider() -> Provider {
    Provider {
        id: "exec_local".to_string(),
        base_url: String::new(),
        api_key: None,
        input_price: Some(0.0),
        output_price: Some(0.0),
        provider_type: "exec".to_string(),
        models: vec![ModelInfo {
            name: "custom".to_string(),
            input_price: Some(0.0),
            output_price: Some(0.0),
        }],
        engine: None,
    }
}

/// Derive provider-level pricing from fetched models.
///
/// - Ollama providers get (0.0, 0.0)
/// - For remote providers: scans for the first model with any pricing data
/// - When no models are fetched or none have pricing: (None, None)
pub(super) fn derive_provider_pricing(
    is_ollama: bool,
    fetched: &Option<Vec<FetchedModel>>,
) -> (Option<f64>, Option<f64>) {
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if is_ollama {
        (Some(0.0), Some(0.0))
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    } else if let Some(models) = fetched {
        // Find the first model that has *any* pricing, not just the first model.
        models
            .iter()
            .find(|(_, i, o)| i.is_some() || o.is_some())
            .map(|(_, i, o)| (*i, *o))
            .unwrap_or((None, None))
    } else {
        (None, None)
    }
}

/// Normalize a provider ID into a strict shell-safe slug: lowercase,
/// only `[a-z0-9_]`, guaranteed to start with a letter or underscore.
///
/// Spaces and hyphens become underscores; all other non-alphanumeric/underscore
/// characters are removed.  A leading digit is prefixed with `p_` so the
/// resulting slug is safe for env-var names and YAML keys (e.g. `"01.ai"` →
/// `"p_01ai"`).  Returns `None` if the result is empty after sanitization.
pub fn sanitize_provider_id(raw: &str) -> Option<String> {
    let slug: String = raw
        .to_lowercase()
        .chars()
        // TODO(slop): add test for new `if` branch (no paired test file in this patch)
        .map(|c| if c == ' ' || c == '-' { '_' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if slug.is_empty() {
        None
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    } else if slug.starts_with(|c: char| c.is_ascii_digit()) {
        Some(format!("p_{slug}"))
    } else {
        Some(slug)
    }
}

/// Build the environment variable key for a provider's API key.
///
/// Sanitizes the provider ID first, then converts to uppercase.
pub(super) fn build_provider_env_key(provider_id: &str) -> String {
    let sanitized = sanitize_provider_id(provider_id).unwrap_or_else(|| provider_id.to_string());
    format!("{}_API_KEY", sanitized.to_uppercase())
}

/// Convert fetched model tuples into `ModelInfo` structs.
pub(super) fn fetched_to_model_infos(models: &[FetchedModel]) -> Vec<ModelInfo> {
    models
        .iter()
        .map(|(n, inp, out)| ModelInfo {
            name: n.clone(),
            input_price: *inp,
            output_price: *out,
        })
        .collect()
}
