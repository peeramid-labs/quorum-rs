//! Agent presets, preset configs, and tested models.

// ── Agent assignment ───────────────────────────────────────────────────────

/// A named agent that will run as its own container.
#[derive(Debug, Clone)]
pub(super) struct AgentSlot {
    /// The `NSED_AGENT_NAME` value — must match an entry in `config/default.yml`.
    pub name: String,
    pub provider_id: String,
    /// Model identifier (e.g. `meta-llama/Llama-3-70b-chat-hf`).
    pub model_name: String,
    /// USD per million input tokens (`None` = unknown).
    pub input_price: Option<f64>,
    /// USD per million output tokens (`None` = unknown).
    pub output_price: Option<f64>,
    /// Sampling temperature (0.0–2.0).
    pub temperature: f32,
    /// Maximum output tokens.
    pub max_tokens: i32,
    /// Presence penalty [-2.0–2.0].
    pub presence_penalty: f32,
    /// System-level persona instructions.
    pub persona: Option<String>,
    /// Model context window size.
    pub context_window: Option<i32>,
    /// Reasoning effort level ("low"/"medium"/"high").
    pub reasoning_effort: Option<String>,
    // ── Tool / repair / streaming strategy flags ─────────────────────────
    /// Stream responses (default: true).
    pub use_streaming: Option<bool>,
    /// Merge system prompt with initial user message.
    pub merge_system_prompt: Option<bool>,
    /// Parse tool calls from conversational text.
    pub unwrap_hallucinated_tool_calls: Option<bool>,
    /// Fix invalid JSON escape sequences in tool call arguments.
    pub repair_invalid_escapes: Option<bool>,
    /// Request JSON mode output from provider.
    pub json_mode: Option<bool>,
    /// Disable native tool definitions in API requests.
    pub disable_native_tools: Option<bool>,
    /// Maximum scratchpad entries shown in context.
    pub scratchpad_limit: Option<i32>,
    /// Capability tags advertised via heartbeat for policy-based scheduling.
    pub capability_tags: Vec<String>,
    /// Short description of the agent's specialization.
    pub description: Option<String>,
    /// Exec provider command (e.g. `["python3", "agent.py"]`).
    pub exec_command: Option<Vec<String>>,
    /// Files/dirs this agent may read for context. Rendered per provider:
    /// `builtin_tools: read_file` roots (native LLM), `add_dirs` (Claude),
    /// or `working_dir` (exec). Empty (default) grants no file access.
    pub read_paths: Vec<String>,
    /// Directories this agent may write to / manage. Only Claude agents get
    /// write tools (`add_dirs` + `writable: true`); exec agents use the first
    /// as `working_dir`; native-LLM agents have no write tool (rendered as a
    /// note). Empty (default) grants no write access.
    pub write_dirs: Vec<String>,
}

impl AgentSlot {
    /// Create an AgentSlot with only core fields; all strategy flags default to `None`.
    pub(super) fn new(
        name: String,
        provider_id: String,
        model_name: String,
        input_price: Option<f64>,
        output_price: Option<f64>,
    ) -> Self {
        Self {
            name,
            provider_id,
            model_name,
            input_price,
            output_price,
            temperature: 0.7,
            max_tokens: 4096,
            presence_penalty: 1.5,
            persona: None,
            context_window: None,
            reasoning_effort: None,
            use_streaming: None,
            merge_system_prompt: None,
            unwrap_hallucinated_tool_calls: None,
            repair_invalid_escapes: None,
            json_mode: None,
            disable_native_tools: None,
            scratchpad_limit: None,
            capability_tags: vec![],
            description: None,
            exec_command: None,
            read_paths: vec![],
            write_dirs: vec![],
        }
    }

    /// Apply preset defaults for known agent names (from default.yml).
    pub(super) fn apply_preset(&mut self) {
        // TODO(slop): add test for new `if` branch (no paired test file in this patch)
        if let Some(preset) = PRESET_CONFIGS.iter().find(|p| p.name == self.name) {
            self.persona = Some(preset.persona.to_string());
            self.temperature = preset.temperature;
            self.max_tokens = preset.max_tokens;
            // TODO(slop): add test for new `if` branch (no paired test file in this patch)
            if let Some(cw) = preset.context_window {
                self.context_window = Some(cw);
            }
            // TODO(slop): add test for new `if` branch (no paired test file in this patch)
            if let Some(re) = preset.reasoning_effort {
                self.reasoning_effort = Some(re.to_string());
            }
            // Tool/repair strategy flags
            self.use_streaming = preset.use_streaming;
            self.merge_system_prompt = preset.merge_system_prompt;
            self.unwrap_hallucinated_tool_calls = preset.unwrap_hallucinated_tool_calls;
            self.repair_invalid_escapes = preset.repair_invalid_escapes;
            self.json_mode = preset.json_mode;
            self.disable_native_tools = preset.disable_native_tools;
            self.scratchpad_limit = preset.scratchpad_limit;
            // TODO(slop): add test for new `if` branch (no paired test file in this patch)
            if !preset.capability_tags.is_empty() {
                self.capability_tags = preset
                    .capability_tags
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect();
            }
            // TODO(slop): add test for new `if` branch (no paired test file in this patch)
            if let Some(desc) = preset.description {
                self.description = Some(desc.to_string());
            }
        }
    }
}

// ── Preset configs ─────────────────────────────────────────────────────────

/// Per-agent defaults sourced from `config/default.yml` in the Docker image.
pub(super) struct PresetConfig {
    pub name: &'static str,
    pub persona: &'static str,
    pub temperature: f32,
    pub max_tokens: i32,
    pub context_window: Option<i32>,
    pub reasoning_effort: Option<&'static str>,
    // Tool/repair strategy overrides (None = use framework defaults)
    pub use_streaming: Option<bool>,
    pub merge_system_prompt: Option<bool>,
    pub unwrap_hallucinated_tool_calls: Option<bool>,
    pub repair_invalid_escapes: Option<bool>,
    pub json_mode: Option<bool>,
    pub disable_native_tools: Option<bool>,
    pub scratchpad_limit: Option<i32>,
    /// Capability tags for policy-based scheduling.
    pub capability_tags: &'static [&'static str],
    /// Short description of the agent's specialization.
    pub description: Option<&'static str>,
}

/// Default preset config values — all strategy flags use framework defaults.
#[allow(clippy::too_many_arguments)]
pub(super) const fn default_preset(
    name: &'static str,
    persona: &'static str,
    temperature: f32,
    max_tokens: i32,
    context_window: Option<i32>,
    reasoning_effort: Option<&'static str>,
    capability_tags: &'static [&'static str],
    description: Option<&'static str>,
) -> PresetConfig {
    PresetConfig {
        name,
        persona,
        temperature,
        max_tokens,
        context_window,
        reasoning_effort,
        use_streaming: None,
        merge_system_prompt: None,
        unwrap_hallucinated_tool_calls: None,
        repair_invalid_escapes: None,
        json_mode: None,
        disable_native_tools: None,
        scratchpad_limit: None,
        capability_tags,
        description,
    }
}

/// All agent presets from `config/default.yml` with their full config.
pub(super) const PRESET_CONFIGS: &[PresetConfig] = &[
    // ── General Assistant ensemble ──────────────────────────────────────
    default_preset(
        "DEFAULT",
        "You are a helpful, accurate, and thorough AI assistant. You answer user queries clearly and honestly. You critically evaluate requests before responding and double-check your reasoning for potential errors.",
        0.7,
        8096,
        Some(131072),
        Some("medium"),
        &["general", "reasoning"],
        Some("General-purpose helpful assistant"),
    ),
    default_preset(
        "REASON",
        "You are a structured reasoning specialist. You break complex problems into logical steps, identify assumptions, and build rigorous arguments. You excel at analysis, planning, and decision-making under uncertainty.",
        0.7,
        12096,
        Some(202800),
        None,
        &["reasoning", "analysis"],
        Some("Structured reasoning & logical analysis"),
    ),
    default_preset(
        "CREATE",
        "You are a creative problem-solver and generalist writer. You excel at brainstorming, drafting content, synthesizing information from multiple domains, and presenting ideas clearly. You balance thoroughness with conciseness.",
        0.7,
        12096,
        Some(262144),
        None,
        &["creative", "writing"],
        Some("Creative writing & brainstorming"),
    ),
    default_preset(
        "VERIFY",
        "You are a detail-oriented fact-checker and quality reviewer. You verify claims, catch logical errors, ensure consistency, and stress-test arguments. You are the last line of defense before a final answer is delivered.",
        0.5,
        12096,
        Some(131072),
        None,
        &["verification", "quality"],
        Some("Fact-checking & quality review"),
    ),
];

// ── Agent presets ──────────────────────────────────────────────────────────

/// An agent ensemble grouping with a human-readable description.
#[allow(dead_code)]
pub(super) struct AgentPreset {
    /// The `NSED_AGENT_NAME` value (must match `config/default.yml`).
    pub name: &'static str,
    /// One-line description for the CLI picker.
    pub desc: &'static str,
    /// Ensemble label (for grouping in the MultiSelect).
    pub ensemble: &'static str,
}

/// Curated agent presets defined in `config/default.yml` inside the Docker image.
/// Curated set of generally-useful agent personas. Domain-specific
/// ensembles (Security / Quant / Supply / Legal) used to live here
/// but were noise for the common case — operators picking a custom
/// persona via free-text or the structured `claude:` YAML block are
/// the path forward for non-general flows.
pub(super) const AGENT_PRESETS: &[AgentPreset] = &[
    AgentPreset {
        name: "DEFAULT",
        desc: "general-purpose helpful assistant",
        ensemble: "General",
    },
    AgentPreset {
        name: "REASON",
        desc: "structured reasoning & logical analysis",
        ensemble: "General",
    },
    AgentPreset {
        name: "CREATE",
        desc: "creative writing & brainstorming",
        ensemble: "General",
    },
    AgentPreset {
        name: "VERIFY",
        desc: "fact-checking & quality review",
        ensemble: "General",
    },
];

/// Models with dedicated NSED integration tests — known to work well.
/// Sorted to the top of the model picker and tagged "tested".
pub(super) const TESTED_MODELS: &[&str] = &[
    "openai/gpt-oss-120b",
    "moonshotai/Kimi-K2.5",
    "Qwen/Qwen3-Coder-Next-FP8",
    "essentialai/rnj-1-instruct",
    "mistralai/Mistral-Small-24B-Instruct-2501",
    "MiniMaxAI/MiniMax-M2.5",
    "Qwen/Qwen3-Next-80B-A3B-Thinking",
    "zai-org/GLM-4.7",
    "meta-llama/Llama-Guard-4-12B",
];

pub(super) fn is_tested_model(id: &str) -> bool {
    TESTED_MODELS.contains(&id)
}

/// Format a price as $/Mtok for display, e.g. `$0.15`.
pub(super) fn fmt_price(p: Option<f64>) -> String {
    // TODO(slop): add test for new `match` branch (no paired test file in this patch)
    match p {
        // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
        Some(0.0) => "free".to_string(),
        // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
        Some(v) => format!("${:.2}", v),
        // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
        None => "\u{2014}".to_string(),
    }
}
