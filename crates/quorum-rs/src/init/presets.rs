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
        }
    }

    /// Apply preset defaults for known agent names (from default.yml).
    pub(super) fn apply_preset(&mut self) {
        if let Some(preset) = PRESET_CONFIGS.iter().find(|p| p.name == self.name) {
            self.persona = Some(preset.persona.to_string());
            self.temperature = preset.temperature;
            self.max_tokens = preset.max_tokens;
            if let Some(cw) = preset.context_window {
                self.context_window = Some(cw);
            }
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
            if !preset.capability_tags.is_empty() {
                self.capability_tags = preset
                    .capability_tags
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect();
            }
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
    // ── Security Audit ensemble ─────────────────────────────────────────
    default_preset(
        "REENTRY",
        "You are a re-entrancy vulnerability specialist analyzing recursive call patterns and cross-function re-entrancy vectors in smart contracts.",
        0.7,
        4096,
        Some(131072),
        None,
        &["security:reentrancy", "security:*"],
        Some("Re-entrancy vulnerability specialist"),
    ),
    default_preset(
        "STATIC",
        "You are a static analysis expert running Slither, Mythril, and Securify against Solidity bytecode.",
        0.5,
        4096,
        Some(32768),
        None,
        &["security:static-analysis", "security:*"],
        Some("Static analysis (Slither/Mythril/Securify)"),
    ),
    default_preset(
        "FUZZ",
        "You are a fuzz-testing specialist using Echidna and Foundry invariant tests for property-based testing.",
        0.7,
        4096,
        Some(262144),
        None,
        &["security:fuzzing", "security:*"],
        Some("Fuzz-testing (Echidna/Foundry invariants)"),
    ),
    default_preset(
        "REGULAT",
        "You are a regulatory and compliance analyst assessing smart contracts against SEC, MiCA, and DeFi regulatory frameworks.",
        0.7,
        4096,
        Some(32768),
        None,
        &["security:compliance", "regulatory"],
        Some("Regulatory & compliance analysis"),
    ),
    default_preset(
        "ARCHIT",
        "You are a smart contract architect synthesizing all security findings into a prioritized remediation roadmap.",
        0.5,
        4096,
        Some(131072),
        Some("medium"),
        &["security:architecture", "security:*"],
        Some("Architecture synthesis & remediation roadmap"),
    ),
    // ── Quant Strategy ensemble ─────────────────────────────────────────
    default_preset(
        "MOMENTUM",
        "You are a momentum and trend-following strategist analyzing cross-sectional and time-series momentum signals.",
        0.7,
        4096,
        Some(131072),
        Some("medium"),
        &["quant:momentum", "quant:*"],
        Some("Trend-following & momentum signals"),
    ),
    default_preset(
        "MEANREV",
        "You are a mean-reversion specialist identifying over-extended moves and statistical arbitrage opportunities.",
        0.7,
        4096,
        Some(131072),
        None,
        &["quant:mean-reversion", "quant:*"],
        Some("Mean-reversion & statistical arbitrage"),
    ),
    default_preset(
        "VOLATIL",
        "You are a volatility specialist structuring options strategies and tail-risk hedges using VIX derivatives.",
        0.5,
        4096,
        Some(32768),
        None,
        &["quant:volatility", "quant:*"],
        Some("Volatility & options strategy"),
    ),
    default_preset(
        "MACRO",
        "You are a macro strategist analyzing yield curves, rate environments, and geopolitical regime shifts.",
        0.7,
        4096,
        Some(32768),
        None,
        &["quant:macro", "quant:*"],
        Some("Macro strategy & yield curve analysis"),
    ),
    default_preset(
        "RISKMOD",
        "You are a portfolio risk modeler enforcing drawdown limits, VaR constraints, and rebalancing triggers.",
        0.5,
        4096,
        Some(262144),
        None,
        &["quant:risk", "quant:*"],
        Some("Portfolio risk modeling & VaR constraints"),
    ),
    // ── Supply Chain ensemble ───────────────────────────────────────────
    default_preset(
        "DEMAND",
        "You are a demand forecasting analyst who triages stock levels and prioritizes customer orders based on SLA penalty exposure.",
        0.7,
        4096,
        Some(32768),
        None,
        &["supply:demand", "supply:*"],
        Some("Demand forecasting & order prioritization"),
    ),
    default_preset(
        "LOGISTICS",
        "You are a logistics routing specialist focused on freight optimization, carrier selection, and warehouse network planning.",
        0.7,
        4096,
        Some(131072),
        Some("medium"),
        &["supply:logistics", "supply:*"],
        Some("Freight routing & warehouse planning"),
    ),
    default_preset(
        "PROCURE",
        "You are a procurement strategist focused on alternate suppliers, dual-sourcing de-risking, and spot market procurement.",
        0.7,
        4096,
        Some(131072),
        None,
        &["supply:procurement", "supply:*"],
        Some("Procurement & dual-sourcing strategy"),
    ),
    default_preset(
        "QUALCON",
        "You are a quality control and risk assessment lead synthesizing demand forecasts, logistics plans, and procurement options.",
        0.5,
        4096,
        Some(262144),
        None,
        &["supply:quality", "supply:*"],
        Some("Quality control & risk assessment"),
    ),
    // ── Legal Review ensemble ───────────────────────────────────────────
    default_preset(
        "CLAUSAN",
        "You are a clause analysis specialist dissecting IP ownership, licensing terms, and data rights in commercial agreements.",
        0.7,
        4096,
        Some(262144),
        None,
        &["legal:clause-analysis", "legal:*"],
        Some("Clause analysis & IP/licensing review"),
    ),
    default_preset(
        "RISKLEG",
        "You are a legal risk analyst reviewing limitation of liability, indemnification clauses, and data breach carve-outs.",
        0.7,
        4096,
        Some(32768),
        None,
        &["legal:risk", "legal:*"],
        Some("Legal risk & indemnification review"),
    ),
    default_preset(
        "JURISD",
        "You are a jurisdiction and negotiation strategist synthesizing clause analysis into redlines, leverage points, and enforceability assessments.",
        0.7,
        4096,
        Some(131072),
        None,
        &["legal:jurisdiction", "legal:*"],
        Some("Jurisdiction strategy & enforceability"),
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
pub(super) const AGENT_PRESETS: &[AgentPreset] = &[
    // General Assistant ensemble
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
    // Security Audit ensemble
    AgentPreset {
        name: "REENTRY",
        desc: "re-entrancy vulnerability specialist",
        ensemble: "Security",
    },
    AgentPreset {
        name: "STATIC",
        desc: "static analysis (Slither/Mythril/Securify)",
        ensemble: "Security",
    },
    AgentPreset {
        name: "FUZZ",
        desc: "fuzz-testing (Echidna/Foundry invariants)",
        ensemble: "Security",
    },
    AgentPreset {
        name: "REGULAT",
        desc: "regulatory & compliance analysis",
        ensemble: "Security",
    },
    AgentPreset {
        name: "ARCHIT",
        desc: "architecture synthesis & remediation roadmap",
        ensemble: "Security",
    },
    // Quant Strategy ensemble
    AgentPreset {
        name: "MOMENTUM",
        desc: "trend-following & momentum signals",
        ensemble: "Quant",
    },
    AgentPreset {
        name: "MEANREV",
        desc: "mean-reversion & statistical arbitrage",
        ensemble: "Quant",
    },
    AgentPreset {
        name: "VOLATIL",
        desc: "volatility & options strategy",
        ensemble: "Quant",
    },
    AgentPreset {
        name: "MACRO",
        desc: "macro strategy & yield curve analysis",
        ensemble: "Quant",
    },
    AgentPreset {
        name: "RISKMOD",
        desc: "portfolio risk modeling & VaR constraints",
        ensemble: "Quant",
    },
    // Supply Chain ensemble
    AgentPreset {
        name: "DEMAND",
        desc: "demand forecasting & order prioritization",
        ensemble: "Supply",
    },
    AgentPreset {
        name: "LOGISTICS",
        desc: "freight routing & warehouse planning",
        ensemble: "Supply",
    },
    AgentPreset {
        name: "PROCURE",
        desc: "procurement & dual-sourcing strategy",
        ensemble: "Supply",
    },
    AgentPreset {
        name: "QUALCON",
        desc: "quality control & risk assessment",
        ensemble: "Supply",
    },
    // Legal Review ensemble
    AgentPreset {
        name: "CLAUSAN",
        desc: "clause analysis & IP/licensing review",
        ensemble: "Legal",
    },
    AgentPreset {
        name: "RISKLEG",
        desc: "legal risk & indemnification review",
        ensemble: "Legal",
    },
    AgentPreset {
        name: "JURISD",
        desc: "jurisdiction strategy & enforceability",
        ensemble: "Legal",
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
    match p {
        Some(0.0) => "free".to_string(),
        Some(v) => format!("${:.2}", v),
        None => "\u{2014}".to_string(),
    }
}
