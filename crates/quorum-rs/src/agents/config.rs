use crate::nats_utils::OrchestratorEntry;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::HashMap;
use std::path::PathBuf;
use utoipa::ToSchema;

/// Redact env values during serialization to avoid leaking secrets.
/// Keys containing "KEY", "SECRET", "TOKEN", "PASSWORD", or "CREDENTIAL"
/// (case-insensitive) are replaced with `"<redacted>"`. All other values
/// are serialized as-is.
fn serialize_redacted_env<S>(
    env: &HashMap<String, String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeMap;
    const SENSITIVE: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD", "CREDENTIAL"];
    let mut map = serializer.serialize_map(Some(env.len()))?;
    for (k, v) in env {
        let upper = k.to_uppercase();
        let redacted = SENSITIVE.iter().any(|s| upper.contains(s));
        map.serialize_entry(k, if redacted { "<redacted>" } else { v.as_str() })?;
    }
    map.end()
}

/// One layer of a stacked-persona definition. See
/// [`deserialize_persona`] for the yaml-side semantic.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PersonaLayer {
    Text {
        prompt: String,
    },
    /// Markdown file — `prompt` is a filesystem path. The file is read
    /// at parse time and its content stacked into the resolved persona.
    /// Paths resolve relative to the process CWD when `quorum serve`
    /// (or whatever loaded the fleet config) was invoked.
    Md {
        prompt: PathBuf,
    },
}

/// What the yaml field may carry — either a plain string (back-compat)
/// or an ordered array of layers. Internal: the public field type stays
/// `Option<String>` because the layered form is resolved eagerly into
/// a single joined string at parse time.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PersonaInput {
    Inline(String),
    Layered(Vec<PersonaLayer>),
}

/// Custom deserializer attached to [`AgentConfig::persona`].
///
/// Accepts:
///
/// 1. A plain string → returned as-is (`Some(string)`). This is the
///    pre-existing shape; operators with old `agent.yml` files are
///    unaffected.
/// 2. An ordered array of `{type: text|md, prompt: ...}` layer specs.
///    `text` layers contribute their `prompt` string verbatim; `md`
///    layers read the file at `prompt` and contribute its content.
///    Layers are joined with `\n\n` into a single persona string.
/// 3. `null` / absent → `None`.
///
/// File-read failure on an `md` layer surfaces as a parse error
/// (with the failing path named) — operators see the problem at
/// fleet boot, not after the agent is already advertising a partial
/// persona.
fn deserialize_persona<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let opt: Option<PersonaInput> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(PersonaInput::Inline(s)) => Ok(Some(s)),
        Some(PersonaInput::Layered(layers)) => {
            let mut parts: Vec<String> = Vec::with_capacity(layers.len());
            for layer in layers {
                match layer {
                    PersonaLayer::Text { prompt } => parts.push(prompt),
                    PersonaLayer::Md { prompt } => {
                        let content = std::fs::read_to_string(&prompt).map_err(|e| {
                            D::Error::custom(format!(
                                "persona md layer at `{}` could not be read: {e}",
                                prompt.display()
                            ))
                        })?;
                        parts.push(content);
                    }
                }
            }
            Ok(Some(parts.join("\n\n")))
        }
    }
}

/// Configuration for a specific agent.
#[derive(Debug, Deserialize, Clone, Serialize, ToSchema)]
pub struct AgentConfig {
    pub name: String,
    /// Dotpath model reference: `"provider_id.model_key"`.
    /// When set, resolves the provider and merges `ModelDef` fields into this
    /// agent at config load time (`load_agent_from_config`). Replaces the
    /// legacy `provider_id` + `model_name` + flat LLM field pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Legacy provider reference. When `model` is set, this is overwritten
    /// during resolution. Kept for backward compatibility.
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub temperature: f32,
    #[serde(default)]
    pub max_tokens: i32,
    #[serde(default)]
    pub system_prompt_override: Option<String>,
    #[serde(default, deserialize_with = "deserialize_persona")]
    pub persona: Option<String>,
    #[serde(default = "default_max_react_iterations")]
    pub max_react_iterations: Option<i32>,
    #[serde(default = "default_max_scratchpad_size")]
    pub max_scratchpad_size: Option<i32>,
    #[serde(default = "default_max_retries")]
    pub max_retries: Option<i32>,
    #[serde(default)]
    pub supports_native_thinking: bool,
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    /// Presence penalty for the model. Defaults to `Some(1.5)` to encourage
    /// diverse vocabulary in multi-agent deliberation (reduces repetitive
    /// phrasing across rounds). Set to `None` or `0.0` in config to disable.
    #[serde(default = "default_presence_penalty")]
    pub presence_penalty: Option<f32>,
    #[serde(default = "default_textual_feedback")]
    pub textual_feedback: bool,
    #[serde(default = "default_use_streaming")]
    pub use_streaming: bool,
    #[serde(default)]
    pub merge_system_prompt: bool,
    #[serde(default)]
    pub unwrap_hallucinated_tool_calls: bool,
    #[serde(default = "default_repair_invalid_escapes")]
    pub repair_invalid_escapes: bool,
    #[serde(default = "default_scratchpad_limit")]
    pub scratchpad_limit: i32,

    /// Fraction of `max_scratchpad_size` at which `compact_history`
    /// also auto-squeezes the scratchpad. Default 0.95 — leaving 5%
    /// headroom keeps the next tool call from immediately tripping
    /// the persistence cap.
    #[serde(default = "default_scratchpad_squeeze_fraction")]
    pub scratchpad_squeeze_fraction: f64,

    /// Default value of `compact_history(keep_last_n_calls)` when the
    /// model omits the argument. Two recent tool results give the
    /// model enough context to reason while older results fold into
    /// the scratchpad summary.
    #[serde(default = "default_compact_history_keep")]
    pub compact_history_default_keep: usize,
    #[serde(default)]
    pub json_mode: bool,
    #[serde(default)]
    pub disable_native_tools: bool,
    #[serde(default = "default_context_window")]
    pub context_window: i32,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub tool_format: Option<String>,
    /// USD per million input tokens. Used for cost estimation in budget reporting.
    #[serde(default)]
    pub input_price_per_mtok: Option<f64>,
    /// USD per million output tokens. Used for cost estimation in budget reporting.
    #[serde(default)]
    pub output_price_per_mtok: Option<f64>,
    /// Characters per token for heuristic estimation when the provider doesn't return
    /// usage stats. Deserialized as `Option<f64>` (None when absent in config).
    /// The runtime fallback of 4.0 (English approximation) is applied at the call
    /// site via `.unwrap_or(4.0)` in `nsed_agent.rs`; set lower (~1.5) for CJK/code.
    #[serde(default)]
    pub chars_per_token: Option<f64>,
    /// Per-agent orchestrator extensions (additive to the process-wide list).
    /// Only used at agent startup for connection resolution; never serialized
    /// over NATS since this is deployment topology, not agent behavior.
    #[serde(default, skip_serializing)]
    #[schema(ignore)]
    pub orchestrators: Vec<OrchestratorEntry>,
    /// Per-task-category precision parameters for the thermodynamic model.
    /// Map from task category (e.g. "supply", "audit", "quant", "legal") to
    /// `{ pg, pv }` where pg = zero-shot generation precision, pv = verification precision.
    /// Used by the dashboard to compute the NSED utility function:
    ///   U(t) = 1 - (1-pg) * exp(-Lambda*(pv-pg)*t) - beta*t^2
    /// If absent, the dashboard falls back to built-in MODEL_PRECISION defaults.
    #[serde(default)]
    pub task_precision: Option<HashMap<String, TaskPrecision>>,
    /// Controls failure dump output when parse or API errors occur.
    /// Values: `"on"` (default — dump error + raw response), `"full"` (include
    /// system prompt, request body, and messages), `"off"` (disable).
    /// Dumps are written to `failures/<session>_<agent>/`.
    /// Can also be set globally via the `NSED_FAILURE_DUMPS` env var (`1` = on, `full` = full).
    /// The config value takes precedence over the env var.
    #[serde(default = "default_failure_dumps")]
    pub failure_dumps: Option<String>,
    /// Maximum seconds this agent needs to complete a single task (propose or evaluate).
    /// When > 0, this is a hard infrastructure constraint — the orchestrator will never
    /// give this agent less time than this value per phase. Set to `0` to opt out of
    /// SLA reporting (the field is omitted from heartbeats). Defaults to 3600s (1 hour).
    #[serde(default = "default_response_sla_secs")]
    pub response_sla_secs: u64,
    /// Whether to propagate 402 Payment Required errors to the orchestrator.
    /// When `true` (default), an `agent_error` event is published immediately.
    /// When `false`, the agent silently pauses and lets the orchestrator timeout.
    #[serde(default = "default_propagate_payment_error")]
    pub propagate_payment_error: bool,

    // ── Agent metadata (for directory/ranking/dashboard) ──
    /// Free-form capability tags (e.g., `["legal", "audit", "quantitative"]`).
    /// Used for filtering in agent picker and directory.
    #[serde(default)]
    pub capability_tags: Vec<String>,

    /// Short description of the agent's specialization.
    /// Shown in the agent directory and picker UI.
    #[serde(default)]
    pub description: Option<String>,

    /// Signing schemes this agent supports (placeholder for #115).
    /// Values will be validated against `SigningScheme` enum when implemented.
    /// Empty means no signing support (legacy/internal agent).
    #[serde(default)]
    pub signing_schemes: Vec<String>,

    /// When true, buffer entries from this agent are created with `stopped = true`,
    /// preventing auto-release until an external system edits and explicitly
    /// releases them via `POST /buffer/{id}/release`. Used with stub providers
    /// for human-operated agents.
    #[serde(default)]
    pub auto_stop: bool,

    /// Configuration for exec-based external agent providers.
    /// When `provider_type` is `"exec"`, the agent spawns a subprocess instead
    /// of calling an LLM. See `docs/exec-agent-protocol.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecProviderConfig>,

    /// Configuration for MCP-based external agent providers.
    /// When `provider_type` is `"mcp"`, the agent spawns a subprocess and
    /// communicates via the Model Context Protocol (stdio transport).
    /// See `docs/mcp-agent-protocol.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpProviderConfig>,

    /// Configuration for Claude CLI as an agent provider.
    /// When `provider_type` is `"claude"`, automatically constructs `claude`
    /// CLI flags from AgentConfig fields (system prompt, model, session) plus
    /// Claude-specific options (permission mode, budget, MCP tools).
    /// See `docs/mcp-agent-protocol.md#claude-provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeProviderConfig>,

    /// Free-form provider config for **third-party** [`ProviderFactory`]
    /// implementations. Built-in providers (`exec` / `mcp` / `claude`) use
    /// their typed sections above; a custom `provider.type` reads its knobs
    /// from here, so registering a new provider needs no new field on this
    /// core struct.
    ///
    /// Deserialize the whole map into a typed struct with
    /// [`AgentConfig::provider_config_as`], or index the map directly.
    ///
    /// [`ProviderFactory`]: crate::providers::ProviderFactory
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[schema(value_type = Object)]
    pub provider_config: HashMap<String, serde_yaml::Value>,

    /// OpenRouter-specific request extensions (provider routing + ZDR).
    /// Injected into the request body as `"provider": { ... }` when the
    /// underlying base URL is OpenRouter. Non-OpenRouter providers will
    /// ignore or reject the block — only set this for OpenRouter agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openrouter: Option<OpenRouterConfig>,

    /// Per-agent grants for built-in sandboxed tools. Attached to an
    /// agent's tool list **only** for the native-LLM provider branch;
    /// `provider_type: claude` / `exec` / `mcp` route their tools through
    /// provider-native channels (claude sub-agents, the exec subprocess's
    /// own tool surface, MCP server) so grants configured on those agents
    /// are silently ignored at runtime (loaders are expected to warn).
    /// Use this to give native-LLM agents scoped runtime capabilities
    /// (e.g. read files confined to a specific filesystem root) without
    /// going through the user_tools NATS dispatcher pipeline.
    ///
    /// Each grant becomes a tool in the agent's tool list at startup.
    /// See `crate::tools::scoped_read` for the `read_file`
    /// implementation and its security model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_tools: Vec<BuiltinToolGrant>,

    /// Enable the `prompt_exposure` safety guardrail on this agent's LLM
    /// responses. When `true`, the agent scans every terminal tool-call
    /// content (proposal / batch evaluation) for internal-prompt leakage
    /// (XML scaffolding tags, canonical tool names, meta-protocol phrases)
    /// and forces a retry with a block-reason feedback message when a leak
    /// is detected. Defaults to `false` so existing deployments do not
    /// change behavior until explicitly opted in. See
    /// [`docs/middleware.md#prompt_exposure-config`](../../docs/middleware.md)
    /// for the detection heuristics.
    #[serde(default)]
    pub prompt_exposure_guard: bool,

    /// Agent middleware pipelines (`before_prompt` / `on_provider_response` /
    /// `on_completion` / `before_release`). Inert unless configured — the worker
    /// only runs a pipeline when it's non-empty, so existing agents are
    /// unaffected. Deserialize-only (the config carries a non-serializable
    /// runtime `moderation_model`).
    #[serde(default, skip_serializing)]
    #[schema(ignore)]
    pub middleware: crate::middleware::MiddlewareConfig,

    /// Per-agent filesystem roots for the sandboxed `read_file` tool.
    /// Each entry grants the agent permission to read any file under
    /// the canonical path of that root. Symlink targets that resolve
    /// outside the root are rejected. Empty (default) means the tool
    /// isn't activated for this agent.
    ///
    /// Skipped on serialization so host filesystem paths never travel
    /// over the wire (e.g. orchestrator capability advertisements).
    /// Loaded from YAML on the agent host only.
    #[serde(default, skip_serializing)]
    #[schema(value_type = Vec<String>)]
    pub read_file_roots: Vec<PathBuf>,
}

/// SDK-builtin tool grants attached to an agent at startup.
///
/// These differ from `user_tools` (which are job-scoped and forwarded
/// over NATS to a dispatcher process) — `BuiltinToolGrant` entries
/// instantiate concrete in-process `Tool` implementations whose
/// security boundary is the configured root path. Use them when a
/// non-claude agent needs filesystem read access scoped to a
/// documentation corpus.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BuiltinToolGrant {
    /// Read a file whose canonicalized path is contained under one of
    /// the configured roots, capped at `max_bytes`. The agent sees
    /// this as a single `read_file(path)` tool — internally each call
    /// validates the path against every entry in `roots` and accepts
    /// the read iff one of them is a prefix of the canonical target.
    /// Symlinks are followed during canonicalization, so a symlink
    /// pointing outside every root is rejected.
    ReadFile {
        /// One or more allowed root directories. Each root is
        /// canonicalized once at tool construction; the result is the
        /// only filesystem region the tool will serve from for the
        /// lifetime of the agent. Relative paths are resolved against
        /// the agent's CWD at startup.
        roots: Vec<String>,
        /// Per-call file-size cap in bytes. Reads larger than this
        /// return a structured error rather than truncated content,
        /// so the agent can decide what to do (often: ask for a
        /// smaller slice via grep/seek). Default 1 MiB.
        #[serde(default = "default_read_file_max_bytes")]
        max_bytes: usize,
    },
    /// Recursive regex search confined to one of the configured
    /// roots. Wraps `grep -rEn` with the same canonicalize-then-prefix
    /// sandbox the `read_file` grant uses, plus per-call result and
    /// byte caps. The agent sees this as a single
    /// `grep_search(pattern, [path], [include])` tool — useful when a
    /// non-claude agent needs to locate the exact line of a peer's
    /// `file:NNN` citation but doesn't have native Grep.
    Grep {
        /// Allowed root directories. Same semantics as `ReadFile`.
        roots: Vec<String>,
        /// Per-call stdout cap. Output beyond this is truncated and a
        /// `truncated: true` flag returned. Default 1 MiB.
        #[serde(default = "default_read_file_max_bytes")]
        max_bytes: usize,
        /// Per-call match-count cap (passed to `grep -m`). Default 200.
        #[serde(default = "default_grep_max_results")]
        max_results: usize,
        /// Subprocess wall-clock timeout in seconds. Default 10 s.
        /// ReDoS-style patterns can hang grep indefinitely without
        /// this cap.
        #[serde(default = "default_grep_timeout_secs")]
        timeout_secs: u64,
    },
    /// Semantic PDF lookup via PageIndex `pdf_query.py`. The agent
    /// supplies a tree filename (basename — slashes and `..` are
    /// rejected) and a query string; the tool resolves the tree to
    /// `<trees_root>/<tree>`, ensures the canonical path is still
    /// under `trees_root`, then spawns
    /// `<python_bin> <script_path> --tree <abs> --query <q> --top <k>`.
    /// Stdout is JSON-Lines; the tool relays the buffer with the same
    /// truncation + timeout discipline as `Grep`.
    ///
    /// Used to give non-claude aggregators (`provider_type` openai)
    /// hardware-reference-manual lookup parity with the claude
    /// specialists, which already reach pdf_query via the
    /// `coverage_audit` and `hardware_lookup` sub-agents.
    PdfQuery {
        /// Directory holding the PageIndex `tree.json` files. Each
        /// per-call `tree` argument must canonicalize under this root
        /// — any `..`-traversal or out-of-sandbox symlink is rejected.
        trees_root: String,
        /// Absolute path to `pdf_query.py` (or any compatible script
        /// that accepts `--tree`/`--query`/`--top` and prints
        /// JSON-Lines on stdout). Validated at startup; the tool
        /// refuses to instantiate if the script is missing.
        script_path: String,
        /// Interpreter binary. Default `"python3"` — override when the
        /// runtime exposes the script via a venv shim or a wrapper.
        #[serde(default = "default_pdf_query_python_bin")]
        python_bin: String,
        /// Per-call stdout cap. Output beyond this is truncated and a
        /// `truncated: true` flag returned. Default 1 MiB.
        #[serde(default = "default_read_file_max_bytes")]
        max_bytes: usize,
        /// Hard ceiling on the agent-supplied `top_k`. Requests above
        /// this saturate to the cap; absent `top_k` defaults to this
        /// value. Default 10.
        #[serde(default = "default_pdf_query_max_results")]
        max_results: usize,
        /// Subprocess wall-clock timeout in seconds. Default 60 s
        /// (pdf_query keyword scoring on a multi-thousand-node tree
        /// can run for tens of seconds; raise if your trees are
        /// larger).
        #[serde(default = "default_pdf_query_timeout_secs")]
        timeout_secs: u64,
    },
}

fn default_read_file_max_bytes() -> usize {
    1024 * 1024
}

fn default_grep_max_results() -> usize {
    200
}

fn default_grep_timeout_secs() -> u64 {
    10
}

fn default_pdf_query_python_bin() -> String {
    "python3".to_string()
}

fn default_pdf_query_max_results() -> usize {
    10
}

fn default_pdf_query_timeout_secs() -> u64 {
    60
}

impl AgentConfig {
    /// Deserialize the whole [`provider_config`](Self::provider_config) map
    /// into a typed struct `T`. Third-party [`ProviderFactory`] impls use
    /// this to read their bespoke YAML config without adding a typed section
    /// to this core struct:
    ///
    /// ```ignore
    /// #[derive(serde::Deserialize)]
    /// struct CodexConfig { permission_mode: String, sandbox: bool }
    /// let cfg: CodexConfig = agent_config.provider_config_as()?;
    /// ```
    ///
    /// An empty map deserializes to whatever `T` makes of an empty mapping
    /// (e.g. a struct whose fields all have `#[serde(default)]`).
    ///
    /// [`ProviderFactory`]: crate::providers::ProviderFactory
    pub fn provider_config_as<T: serde::de::DeserializeOwned>(
        &self,
    ) -> Result<T, serde_yaml::Error> {
        let mapping: serde_yaml::Mapping = self
            .provider_config
            .iter()
            .map(|(k, v)| (serde_yaml::Value::String(k.clone()), v.clone()))
            .collect();
        serde_yaml::from_value(serde_yaml::Value::Mapping(mapping))
    }

    /// Validate that at most one provider section is populated and, when
    /// `resolved_provider_type` is known, that it matches the populated section.
    pub fn validate_provider_sections(
        &self,
        resolved_provider_type: Option<&str>,
    ) -> Result<(), String> {
        let sections: Vec<&str> = [
            self.exec.as_ref().map(|_| "exec"),
            self.mcp.as_ref().map(|_| "mcp"),
            self.claude.as_ref().map(|_| "claude"),
        ]
        .into_iter()
        .flatten()
        .collect();

        if sections.len() > 1 {
            return Err(format!(
                "agent '{}': multiple provider sections present ({}); exactly one is allowed",
                self.name,
                sections.join(", ")
            ));
        }

        if let Some(ptype) = resolved_provider_type {
            if let Some(&section) = sections.first() {
                if section != ptype {
                    return Err(format!(
                        "agent '{}': provider_type '{}' does not match config section '{}'",
                        self.name, ptype, section
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate compaction knobs land in usable ranges. A
    /// `scratchpad_squeeze_fraction` outside `(0.0, 1.0]` and a
    /// `compact_history_default_keep` of zero would silently produce
    /// degenerate compaction behavior.
    pub fn validate_compaction_knobs(&self) -> Result<(), String> {
        if !(self.scratchpad_squeeze_fraction > 0.0 && self.scratchpad_squeeze_fraction <= 1.0) {
            return Err(format!(
                "agent '{}': scratchpad_squeeze_fraction must be in (0.0, 1.0], got {}",
                self.name, self.scratchpad_squeeze_fraction
            ));
        }
        if self.compact_history_default_keep == 0 {
            return Err(format!(
                "agent '{}': compact_history_default_keep must be >= 1",
                self.name
            ));
        }
        Ok(())
    }
}

/// Configuration for the exec provider, parsed from agent YAML.
/// The agent spawns this command as a subprocess, writes the deliberation
/// context as JSON to stdin, and reads the response JSON from stdout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ExecProviderConfig {
    /// Command and arguments to spawn. First element is the binary.
    /// Example: `["python3", "agents/my_agent.py"]`
    pub command: Vec<String>,

    /// Working directory for the subprocess. Defaults to the current directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub working_dir: Option<PathBuf>,

    /// Extra environment variables passed to the subprocess (additive).
    /// Values containing secrets are redacted during serialization.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        serialize_with = "serialize_redacted_env"
    )]
    #[schema(value_type = HashMap<String, String>)]
    pub env: HashMap<String, String>,

    /// Hard timeout in seconds. Falls back to `phase_budget_remaining_secs`
    /// from the agent context, then 300s if neither is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Configuration for the MCP (Model Context Protocol) provider, parsed from
/// agent YAML. The agent spawns this command as a subprocess and communicates
/// via MCP over stdin/stdout (stdio transport). Unlike exec agents, MCP agents
/// can call deliberation tools (read proposals, search history, update
/// scratchpad) during execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct McpProviderConfig {
    /// Command and arguments to spawn. First element is the binary.
    /// Example: `["python3", "agents/mcp_agent.py"]`
    pub command: Vec<String>,

    /// Working directory for the subprocess. Defaults to the current directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub working_dir: Option<PathBuf>,

    /// Extra environment variables passed to the subprocess (additive).
    /// Values containing secrets are redacted during serialization.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        serialize_with = "serialize_redacted_env"
    )]
    #[schema(value_type = HashMap<String, String>)]
    pub env: HashMap<String, String>,

    /// Hard timeout in seconds. Falls back to `phase_budget_remaining_secs`
    /// from the agent context, then 300s if neither is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Configuration for the Claude CLI provider, parsed from agent YAML.
/// The agent spawns `claude` with flags derived from `AgentConfig` fields
/// (system prompt, model, session persistence) and Claude-specific options
/// (permission mode, budget, MCP config). Delegates to the MCP agent
/// infrastructure for the hybrid stdin+MCP protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ClaudeProviderConfig {
    /// Model override (e.g. "sonnet", "opus", "claude-sonnet-4-6").
    /// Falls back to `AgentConfig.model_name` if not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Working directory for Claude CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub working_dir: Option<PathBuf>,

    /// Extra environment variables passed to the subprocess (additive).
    /// Values containing secrets are redacted during serialization.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        serialize_with = "serialize_redacted_env"
    )]
    #[schema(value_type = HashMap<String, String>)]
    pub env: HashMap<String, String>,

    /// Hard timeout in seconds. Falls back to `phase_budget_remaining_secs`,
    /// then 600s (Claude CLI sessions can be longer than simple scripts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,

    /// Permission mode for automated use. Maps to `--permission-mode`.
    /// Values: `"bypassPermissions"` (default), `"default"`, `"acceptEdits"`, `"plan"`.
    #[serde(default = "default_claude_permission_mode")]
    pub permission_mode: String,

    /// Max USD budget per phase call. Maps to `--max-budget-usd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,

    /// Path(s) to MCP config JSON files for additional tools.
    /// Maps to `--mcp-config`. Use for giving Claude access to external tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<String>)]
    pub mcp_config: Vec<PathBuf>,

    /// Allowed tools filter. Maps to `--allowed-tools`.
    /// Example: `["Bash(git:*)", "Edit", "Read"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,

    /// Disallowed tools filter. Maps to `--disallowed-tools`.
    /// Removed from inherited or allowed tools.
    /// Use `["Write", "Edit"]` to make all `add_dirs` effectively read-only.
    /// Example: `["Write", "Edit", "Bash(rm:*)"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,

    /// Context files injected into Claude's system prompt.
    /// Each file is read by NSED at invocation time and inlined as
    /// `--append-system-prompt "<context_file>...<contents>...</context_file>"`.
    /// No directory access is granted — use `add_dirs` for that.
    /// Example: `["docs/architecture.md", "specs/api-contract.json"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<String>)]
    pub context_files: Vec<PathBuf>,

    /// Allow Claude to write files (Write, Edit, NotebookEdit tools).
    /// Default `false` — Claude gets read-only filesystem access.
    /// Set `true` if Claude needs to create or modify files.
    #[serde(default)]
    pub writable: bool,

    /// Additional directories to grant Claude tool access to.
    /// Maps to `--add-dir`. Read-only unless `writable: true`.
    /// Example: `["/data/shared", "./vendor"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<String>)]
    pub add_dirs: Vec<PathBuf>,

    /// Sub-agent definitions. Maps to `--agents`.
    /// Lets Claude spawn specialized sub-agents during deliberation.
    /// Keys are agent names (lowercase + hyphens), values configure each sub-agent.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agents: HashMap<String, ClaudeSubAgentDef>,

    /// Additional CLI flags passed verbatim to `claude`.
    /// Example: `["--verbose", "--no-session-persistence"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

/// Sub-agent definition for Claude CLI `--agents` flag.
/// Each sub-agent runs in its own context window with a custom prompt,
/// specific tool access, and independent permissions.
///
/// See <https://code.claude.com/docs/en/sub-agents>
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ClaudeSubAgentDef {
    /// When Claude should delegate to this sub-agent.
    pub description: String,

    /// System prompt (the sub-agent's instructions).
    pub prompt: String,

    /// Tool allowlist. Inherits all tools if omitted.
    /// Example: `["Read", "Grep", "Glob", "Bash"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,

    /// Tool denylist. Removed from inherited or allowed tools.
    /// Example: `["Write", "Edit"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "disallowedTools")]
    pub disallowed_tools: Vec<String>,

    /// Model: `"sonnet"`, `"opus"`, `"haiku"`, `"inherit"`, or a full model ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Permission mode: `"default"`, `"acceptEdits"`, `"dontAsk"`,
    /// `"bypassPermissions"`, or `"plan"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,

    /// Maximum number of agentic turns before the sub-agent stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "maxTurns")]
    pub max_turns: Option<u32>,

    /// MCP server definitions or references scoped to this sub-agent.
    /// Each entry is either a server name (string reference) or an
    /// inline definition `{ "name": { "type": "stdio", ... } }`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "mcpServers")]
    pub mcp_servers: Vec<serde_json::Value>,

    /// Effort level: `"low"`, `"medium"`, `"high"`, `"max"` (Opus only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Run as a background sub-agent (concurrent with main conversation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,

    /// Run in a temporary git worktree for isolated file access.
    /// Set to `"worktree"` to enable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,

    /// Persistent memory scope: `"user"`, `"project"`, or `"local"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,

    /// Skills to preload into the sub-agent's context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,

    /// Auto-submitted as the first user turn when running as main agent
    /// via `--agent`. Commands and skills are processed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "initialPrompt")]
    pub initial_prompt: Option<String>,
}

fn default_claude_permission_mode() -> String {
    "bypassPermissions".to_string()
}

impl Default for ClaudeProviderConfig {
    fn default() -> Self {
        Self {
            model: None,
            working_dir: None,
            env: HashMap::new(),
            timeout_secs: None,
            permission_mode: default_claude_permission_mode(),
            max_budget_usd: None,
            mcp_config: Vec::new(),
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            context_files: Vec::new(),
            add_dirs: Vec::new(),
            agents: HashMap::new(),
            extra_args: Vec::new(),
            writable: false,
        }
    }
}

/// OpenRouter provider routing extensions.
/// Docs: <https://openrouter.ai/docs/guides/routing/provider-selection>
///
/// Maps to the `"provider"` object in the request body. Fields are all
/// optional — only set the ones you want to override. Empty struct = no
/// provider block emitted.
#[derive(Debug, Deserialize, Clone, Serialize, Default, PartialEq, ToSchema)]
pub struct OpenRouterConfig {
    /// Provider prioritization: `"throughput"` | `"latency"` | `"price"`.
    /// Defaults to OpenRouter's price-first routing when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_sort: Option<String>,

    /// When `true`, restricts routing to Zero Data Retention endpoints.
    /// Compliance requirement for sensitive workloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,

    /// When `false`, disables automatic fallback to backup providers on
    /// primary failure. Use when you want deterministic routing and
    /// prefer a hard fail over a silent reroute to a slower endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,

    /// Provider slugs to exclude from routing (case-insensitive OpenRouter
    /// slug, e.g. `["nextbit", "ionstream"]`). Useful when pairing
    /// `allow_fallbacks: false` with `provider_sort: "throughput"` —
    /// otherwise the fastest slot is often held by a high-throughput but
    /// low-uptime provider. Injected as `"provider.ignore": [...]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,

    /// Provider allowlist — restrict routing to ONLY these provider
    /// slugs (e.g. `["akashml/fp8", "parasail/fp8"]`). Mutually
    /// exclusive in spirit with `ignore`; setting both means
    /// "allowlist minus the ignore set". Use to pin a model to a
    /// specific provider variant when the default routing yields a
    /// smaller advertised context window than the targeted variant —
    /// e.g. `qwen/qwen3.6-35b-a3b` defaults to a 131k window but the
    /// `akashml/fp8` and `parasail/fp8` variants advertise 262k.
    /// Injected as `"provider.only": [...]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only: Vec<String>,

    /// When `true`, strip reasoning tokens from the visible `content`
    /// stream (maps to OpenRouter's `reasoning.exclude: true`). The model
    /// still reasons internally at the configured `reasoning_effort`; only
    /// the chain-of-thought portion of the output is omitted. Use for
    /// models that otherwise dump reasoning into `content` and leave zero
    /// budget for the final structured-output tool call (observed:
    /// gpt-oss-120b via OR native streaming). When this flag switches the
    /// request to the unified `reasoning: { effort, exclude }` object, the
    /// legacy `reasoning_effort` top-level field is no longer emitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_reasoning: Option<bool>,
}

/// Precision parameters for a specific task category.
/// pg = P(correct answer | single zero-shot generation)
/// pv = P(correct verification | evaluation of proposals)
#[derive(Debug, Deserialize, Clone, Serialize, Default, ToSchema)]
pub struct TaskPrecision {
    /// Zero-shot generation precision (0.0 - 1.0)
    pub pg: f64,
    /// Verification/evaluation precision (0.0 - 1.0, typically > pg)
    pub pv: f64,
}

pub fn default_context_window() -> i32 {
    128_000
}

pub fn default_scratchpad_limit() -> i32 {
    2000
}

pub fn default_scratchpad_squeeze_fraction() -> f64 {
    0.95
}

pub fn default_compact_history_keep() -> usize {
    2
}

pub fn default_repair_invalid_escapes() -> bool {
    true
}

pub fn default_textual_feedback() -> bool {
    true
}

pub fn default_use_streaming() -> bool {
    true
}

pub fn default_presence_penalty() -> Option<f32> {
    Some(1.5)
}

pub fn default_max_retries() -> Option<i32> {
    Some(3)
}

pub fn default_max_react_iterations() -> Option<i32> {
    // Doubled from the historical 10 after prod observation that
    // agents with complex tool-call chains (search_deliberation +
    // update_scratchpad + read_own_proposal before submit_proposal)
    // could legitimately consume 8-12 iterations on a single turn
    // on long prompts, leaving no headroom for retries. 20 is the
    // knee where further increases don't reduce max-iter exhaustion
    // — beyond this the agent is usually stuck in a loop, not making
    // progress, and should fail fast instead.
    Some(20)
}

pub fn default_max_scratchpad_size() -> Option<i32> {
    Some(32_768)
}

pub fn default_failure_dumps() -> Option<String> {
    Some("on".to_string())
}

pub fn default_response_sla_secs() -> u64 {
    3600
}

pub fn default_propagate_payment_error() -> bool {
    true
}

/// Manual Default implementation that matches the serde defaults.
/// `#[derive(Default)]` would set `response_sla_secs` to 0 and
/// `propagate_payment_error` to false, which differs from the documented
/// serde defaults (3600s / 1 hour and true respectively).
impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            model: None,
            provider_id: String::new(),
            model_name: String::new(),
            temperature: 0.0,
            max_tokens: 0,
            system_prompt_override: None,
            persona: None,
            max_react_iterations: default_max_react_iterations(),
            max_scratchpad_size: default_max_scratchpad_size(),
            max_retries: default_max_retries(),
            supports_native_thinking: false,
            frequency_penalty: None,
            presence_penalty: default_presence_penalty(),
            textual_feedback: default_textual_feedback(),
            use_streaming: default_use_streaming(),
            merge_system_prompt: false,
            unwrap_hallucinated_tool_calls: false,
            repair_invalid_escapes: default_repair_invalid_escapes(),
            scratchpad_limit: default_scratchpad_limit(),
            scratchpad_squeeze_fraction: default_scratchpad_squeeze_fraction(),
            compact_history_default_keep: default_compact_history_keep(),
            json_mode: false,
            disable_native_tools: false,
            context_window: default_context_window(),
            reasoning_effort: None,
            tool_format: None,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            chars_per_token: None,
            orchestrators: Vec::new(),
            task_precision: None,
            failure_dumps: default_failure_dumps(),
            response_sla_secs: default_response_sla_secs(),
            propagate_payment_error: default_propagate_payment_error(),
            capability_tags: Vec::new(),
            description: None,
            signing_schemes: Vec::new(),
            auto_stop: false,
            exec: None,
            mcp: None,
            claude: None,
            provider_config: HashMap::new(),
            openrouter: None,
            builtin_tools: Vec::new(),
            prompt_exposure_guard: false,
            read_file_roots: Vec::new(),
            middleware: Default::default(),
        }
    }
}

/// True when the agent runs through the native OpenAI-compatible LLM path
/// (no `exec`, no `mcp`, no `claude` provider section). Used by features
/// like the sandboxed `read_file` tool that don't apply to providers
/// with their own native filesystem affordances.
pub fn is_openai_family_provider(config: &AgentConfig) -> bool {
    config.exec.is_none() && config.mcp.is_none() && config.claude.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_middleware_block_deserializes() {
        // An agent's `middleware:` block now parses into AgentConfig; empty by
        // default so existing agents are unaffected.
        let yaml = r#"
name: PropductBot
provider_id: claude
middleware:
  before_prompt:
    - dylib: ./libpatch_deliberation.dylib
      config:
        patch_deliberation: { upstream: epic }
  on_completion:
    - dylib: ./libpatch_deliberation.dylib
  on_job_complete:
    - dylib: ./libpatch_deliberation.dylib
"#;
        let cfg: AgentConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.middleware.before_prompt.len(), 1);
        assert_eq!(cfg.middleware.on_completion.len(), 1);
        assert_eq!(cfg.middleware.on_job_complete.len(), 1);
        assert!(cfg.middleware.on_provider_response.is_empty());
        // default agent → empty middleware (no behavior change)
        assert!(AgentConfig::default().middleware.is_empty());
    }

    #[test]
    fn provider_config_deserializes_into_typed_struct() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct CodexConfig {
            permission_mode: String,
            sandbox: bool,
            #[serde(default)]
            extra_args: Vec<String>,
        }

        let cfg: AgentConfig = serde_yaml::from_str(
            r#"
name: codex-a
provider_id: my_codex
model_name: codex-mini
provider_config:
  permission_mode: "auto"
  sandbox: true
  extra_args: ["--yolo"]
"#,
        )
        .expect("agent yaml must parse");

        let codex: CodexConfig = cfg.provider_config_as().expect("typed read");
        assert_eq!(
            codex,
            CodexConfig {
                permission_mode: "auto".into(),
                sandbox: true,
                extra_args: vec!["--yolo".into()],
            }
        );
    }

    #[test]
    fn provider_config_empty_yields_all_defaults() {
        #[derive(Debug, serde::Deserialize)]
        struct AllDefault {
            #[serde(default)]
            flag: bool,
        }
        let cfg = AgentConfig::default();
        assert!(cfg.provider_config.is_empty());
        let parsed: AllDefault = cfg.provider_config_as().expect("empty map → defaults");
        assert!(!parsed.flag);
    }

    #[test]
    fn provider_config_omitted_from_serialization_when_empty() {
        let cfg = AgentConfig {
            name: "x".into(),
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        assert!(
            !yaml.contains("provider_config"),
            "empty provider_config must be skipped in serialization"
        );
    }

    #[test]
    fn test_builtin_tools_roundtrip() {
        // Explicit max_bytes — preserved verbatim.
        let json = json!({
            "name": "test",
            "provider_id": "openrouter",
            "model_name": "glm-5.1",
            "builtin_tools": [
                {
                    "type": "read_file",
                    "roots": ["/work/corpus", "/work/linux"],
                    "max_bytes": 2097152
                }
            ]
        });
        let config: AgentConfig = serde_json::from_value(json).expect("deserialize");
        assert_eq!(config.builtin_tools.len(), 1);
        match &config.builtin_tools[0] {
            BuiltinToolGrant::ReadFile { roots, max_bytes } => {
                assert_eq!(
                    roots,
                    &vec!["/work/corpus".to_string(), "/work/linux".to_string()]
                );
                assert_eq!(*max_bytes, 2097152);
            }
            other => panic!("expected ReadFile, got {other:?}"),
        }

        // Default max_bytes — applies the documented 1 MiB fallback.
        let default_json = json!({
            "name": "test2",
            "provider_id": "openrouter",
            "model_name": "glm-5.1",
            "builtin_tools": [{"type": "read_file", "roots": ["/tmp"]}]
        });
        let cfg: AgentConfig = serde_json::from_value(default_json).expect("deserialize");
        match &cfg.builtin_tools[0] {
            BuiltinToolGrant::ReadFile { max_bytes, .. } => {
                assert_eq!(*max_bytes, 1024 * 1024);
            }
            other => panic!("expected ReadFile, got {other:?}"),
        }

        // Empty / absent builtin_tools — no panic, defaults to empty Vec.
        let bare_json = json!({
            "name": "test3",
            "provider_id": "p",
            "model_name": "m"
        });
        let bare: AgentConfig = serde_json::from_value(bare_json).expect("deserialize");
        assert!(bare.builtin_tools.is_empty());
    }

    #[test]
    fn test_pdf_query_grant_roundtrip() {
        // Explicit fields — preserved verbatim.
        let json = json!({
            "name": "agg",
            "provider_id": "openrouter",
            "model_name": "glm-5.1",
            "builtin_tools": [
                {
                    "type": "pdf_query",
                    "trees_root": "/work/corpus/trees",
                    "script_path": "/work/scripts/pdf_query.py",
                    "python_bin": "/opt/pageindex/.venv/bin/python3",
                    "max_bytes": 524288,
                    "max_results": 8,
                    "timeout_secs": 90
                }
            ]
        });
        let cfg: AgentConfig = serde_json::from_value(json).expect("deserialize");
        match &cfg.builtin_tools[0] {
            BuiltinToolGrant::PdfQuery {
                trees_root,
                script_path,
                python_bin,
                max_bytes,
                max_results,
                timeout_secs,
            } => {
                assert_eq!(trees_root, "/work/corpus/trees");
                assert_eq!(script_path, "/work/scripts/pdf_query.py");
                assert_eq!(python_bin, "/opt/pageindex/.venv/bin/python3");
                assert_eq!(*max_bytes, 524288);
                assert_eq!(*max_results, 8);
                assert_eq!(*timeout_secs, 90);
            }
            other => panic!("expected PdfQuery, got {other:?}"),
        }

        // Defaults: omit python_bin/max_bytes/max_results/timeout_secs.
        let defaults_json = json!({
            "name": "agg2",
            "provider_id": "openrouter",
            "model_name": "glm-5.1",
            "builtin_tools": [
                {
                    "type": "pdf_query",
                    "trees_root": "/work/corpus/trees",
                    "script_path": "/work/scripts/pdf_query.py"
                }
            ]
        });
        let cfg: AgentConfig = serde_json::from_value(defaults_json).expect("deserialize");
        match &cfg.builtin_tools[0] {
            BuiltinToolGrant::PdfQuery {
                python_bin,
                max_bytes,
                max_results,
                timeout_secs,
                ..
            } => {
                assert_eq!(python_bin, "python3");
                assert_eq!(*max_bytes, 1024 * 1024);
                assert_eq!(*max_results, 10);
                assert_eq!(*timeout_secs, 60);
            }
            other => panic!("expected PdfQuery, got {other:?}"),
        }
    }

    #[test]
    fn test_agent_config_defaults() {
        let json = json!({
            "name": "test-agent",
            "provider_id": "ollama_local",
            "model_name": "model",
        });

        let config: AgentConfig = serde_json::from_value(json).expect("Deserialization failed");
        assert_eq!(config.max_react_iterations, Some(20));
    }

    #[test]
    fn test_agent_config_model_field_deserialization() {
        let json = json!({
            "name": "dotpath-agent",
            "model": "together_ai.llama-70b",
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.model, Some("together_ai.llama-70b".to_string()));
        // provider_id and model_name should be at their defaults (empty)
        assert!(config.provider_id.is_empty());
        assert!(config.model_name.is_empty());
    }

    #[test]
    fn test_agent_config_model_field_default_none() {
        let json = json!({
            "name": "no-model-field",
            "provider_id": "p1",
            "model_name": "m1",
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert!(config.model.is_none());
    }

    #[test]
    fn test_agent_config_model_field_not_serialized_when_none() {
        let config = AgentConfig::default();
        let serialized = serde_json::to_value(&config).unwrap();
        let obj = serialized.as_object().unwrap();
        assert!(
            !obj.contains_key("model"),
            "model: None should be omitted from serialization"
        );
    }

    #[test]
    fn test_agent_config_pricing_fields_default_none() {
        let json = json!({
            "name": "test-agent",
            "provider_id": "p1",
            "model_name": "m1",
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.input_price_per_mtok, None);
        assert_eq!(config.output_price_per_mtok, None);
        assert_eq!(config.chars_per_token, None);
    }

    #[test]
    fn test_agent_config_pricing_fields_roundtrip() {
        let json = json!({
            "name": "priced-agent",
            "provider_id": "openai",
            "model_name": "gpt-4",
            "input_price_per_mtok": 10.0,
            "output_price_per_mtok": 30.0,
            "chars_per_token": 3.5
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.input_price_per_mtok, Some(10.0));
        assert_eq!(config.output_price_per_mtok, Some(30.0));
        assert_eq!(config.chars_per_token, Some(3.5));

        // Roundtrip
        let serialized = serde_json::to_value(&config).unwrap();
        let deserialized: AgentConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized.input_price_per_mtok, Some(10.0));
        assert_eq!(deserialized.output_price_per_mtok, Some(30.0));
        assert_eq!(deserialized.chars_per_token, Some(3.5));
    }

    #[test]
    fn test_agent_config_chars_per_token_override() {
        let json = json!({
            "name": "cjk-agent",
            "provider_id": "ollama",
            "model_name": "qwen",
            "chars_per_token": 1.5
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.chars_per_token, Some(1.5));
    }

    #[test]
    fn test_agent_config_all_defaults() {
        let json = json!({
            "name": "minimal",
            "provider_id": "p",
            "model_name": "m",
        });
        let config: AgentConfig = serde_json::from_value(json).expect("deserialize");

        assert_eq!(config.name, "minimal");
        assert_eq!(config.provider_id, "p");
        assert_eq!(config.model_name, "m");
        assert_eq!(config.temperature, 0.0);
        assert_eq!(config.max_tokens, 0);
        assert!(config.system_prompt_override.is_none());
        assert!(config.persona.is_none());
        assert_eq!(config.max_react_iterations, Some(20));
        assert_eq!(config.max_scratchpad_size, Some(32768));
        assert_eq!(config.max_retries, Some(3));
        assert!(!config.supports_native_thinking);
        assert!(config.frequency_penalty.is_none());
        assert_eq!(config.presence_penalty, Some(1.5));
        assert!(config.textual_feedback);
        assert!(config.use_streaming);
        assert!(!config.merge_system_prompt);
        assert!(!config.unwrap_hallucinated_tool_calls);
        assert!(config.repair_invalid_escapes);
        assert_eq!(config.scratchpad_limit, 2000);
        assert!(!config.json_mode);
        assert!(!config.disable_native_tools);
        assert_eq!(config.context_window, 128_000);
        assert!(config.reasoning_effort.is_none());
        assert!(config.tool_format.is_none());
        assert!(config.input_price_per_mtok.is_none());
        assert!(config.output_price_per_mtok.is_none());
        assert!(config.chars_per_token.is_none());
        assert!(config.task_precision.is_none());
        assert_eq!(config.failure_dumps, Some("on".to_string()));
        assert_eq!(config.response_sla_secs, 3600);
        assert!(config.propagate_payment_error);
    }

    #[test]
    fn test_agent_config_full_roundtrip() {
        let json = json!({
            "name": "full-agent",
            "provider_id": "openai",
            "model_name": "gpt-4o",
            "temperature": 0.7,
            "max_tokens": 4096,
            "system_prompt_override": "You are helpful.",
            "persona": "expert analyst",
            "max_react_iterations": 5,
            "max_scratchpad_size": 16384,
            "max_retries": 2,
            "supports_native_thinking": true,
            "frequency_penalty": 0.5,
            "presence_penalty": 0.8,
            "textual_feedback": false,
            "use_streaming": false,
            "merge_system_prompt": true,
            "unwrap_hallucinated_tool_calls": true,
            "repair_invalid_escapes": false,
            "scratchpad_limit": 500,
            "json_mode": true,
            "disable_native_tools": true,
            "context_window": 64000,
            "reasoning_effort": "high",
            "tool_format": "json",
            "input_price_per_mtok": 2.5,
            "output_price_per_mtok": 10.0,
            "chars_per_token": 1.5,
            "task_precision": {
                "supply": { "pg": 0.3, "pv": 0.8 },
                "audit": { "pg": 0.5, "pv": 0.9 }
            },
            "failure_dumps": "full",
            "response_sla_secs": 120,
            "propagate_payment_error": false
        });

        let config: AgentConfig = serde_json::from_value(json).expect("deserialize");

        // Verify key fields
        assert_eq!(config.name, "full-agent");
        assert_eq!(config.temperature, 0.7);
        assert_eq!(config.max_tokens, 4096);
        assert_eq!(
            config.system_prompt_override,
            Some("You are helpful.".to_string())
        );
        assert_eq!(config.persona, Some("expert analyst".to_string()));
        assert_eq!(config.max_react_iterations, Some(5));
        assert_eq!(config.max_scratchpad_size, Some(16384));
        assert_eq!(config.max_retries, Some(2));
        assert!(config.supports_native_thinking);
        assert_eq!(config.frequency_penalty, Some(0.5));
        assert_eq!(config.presence_penalty, Some(0.8));
        assert!(!config.textual_feedback);
        assert!(!config.use_streaming);
        assert!(config.merge_system_prompt);
        assert!(config.unwrap_hallucinated_tool_calls);
        assert!(!config.repair_invalid_escapes);
        assert_eq!(config.scratchpad_limit, 500);
        assert!(config.json_mode);
        assert!(config.disable_native_tools);
        assert_eq!(config.context_window, 64000);
        assert_eq!(config.reasoning_effort, Some("high".to_string()));
        assert_eq!(config.tool_format, Some("json".to_string()));
        assert_eq!(config.input_price_per_mtok, Some(2.5));
        assert_eq!(config.output_price_per_mtok, Some(10.0));
        assert_eq!(config.chars_per_token, Some(1.5));
        assert_eq!(config.failure_dumps, Some("full".to_string()));

        // Verify task_precision map
        let tp = config
            .task_precision
            .as_ref()
            .expect("task_precision present");
        assert_eq!(tp.len(), 2);
        let supply = tp.get("supply").expect("supply key");
        assert!((supply.pg - 0.3).abs() < f64::EPSILON);
        assert!((supply.pv - 0.8).abs() < f64::EPSILON);
        let audit = tp.get("audit").expect("audit key");
        assert!((audit.pg - 0.5).abs() < f64::EPSILON);
        assert!((audit.pv - 0.9).abs() < f64::EPSILON);

        // Roundtrip: serialize then deserialize
        let serialized = serde_json::to_value(&config).expect("serialize");
        let roundtripped: AgentConfig =
            serde_json::from_value(serialized).expect("deserialize roundtrip");

        assert_eq!(roundtripped.name, "full-agent");
        assert_eq!(roundtripped.temperature, 0.7);
        assert_eq!(roundtripped.max_tokens, 4096);
        assert_eq!(roundtripped.tool_format, Some("json".to_string()));
        assert_eq!(roundtripped.failure_dumps, Some("full".to_string()));
        assert_eq!(roundtripped.reasoning_effort, Some("high".to_string()));
        assert_eq!(roundtripped.context_window, 64000);
        assert_eq!(config.response_sla_secs, 120);
        assert_eq!(roundtripped.response_sla_secs, 120);
        assert!(!config.propagate_payment_error);
        assert!(!roundtripped.propagate_payment_error);
        let rt_tp = roundtripped.task_precision.as_ref().unwrap();
        assert_eq!(rt_tp.len(), 2);
        assert!((rt_tp["supply"].pg - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn test_agent_config_orchestrators_skip_serializing() {
        let mut config = AgentConfig {
            name: "orch-test".to_string(),
            provider_id: "p".to_string(),
            model_name: "m".to_string(),
            ..AgentConfig::default()
        };
        config.orchestrators = vec![OrchestratorEntry {
            id: Some("local".to_string()),
            url: "http://localhost:8080".to_string(),
            bearer_token: None,
            invite_code: None,
        }];

        let serialized = serde_json::to_value(&config).expect("serialize");
        let obj = serialized.as_object().expect("should be object");
        assert!(
            !obj.contains_key("orchestrators"),
            "orchestrators should be skipped during serialization"
        );
    }

    #[test]
    fn test_task_precision_serde() {
        let tp = TaskPrecision { pg: 0.3, pv: 0.8 };
        let serialized = serde_json::to_value(&tp).expect("serialize");
        assert_eq!(serialized["pg"], 0.3);
        assert_eq!(serialized["pv"], 0.8);

        let roundtripped: TaskPrecision = serde_json::from_value(serialized).expect("deserialize");
        assert!((roundtripped.pg - 0.3).abs() < f64::EPSILON);
        assert!((roundtripped.pv - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_task_precision_default() {
        let tp = TaskPrecision::default();
        assert!((tp.pg - 0.0).abs() < f64::EPSILON);
        assert!((tp.pv - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_function_values() {
        assert_eq!(default_context_window(), 128_000);
        assert_eq!(default_scratchpad_limit(), 2000);
        assert!(default_repair_invalid_escapes());
        assert!(default_textual_feedback());
        assert!(default_use_streaming());
        assert_eq!(default_presence_penalty(), Some(1.5));
        assert_eq!(default_max_retries(), Some(3));
        assert_eq!(default_max_react_iterations(), Some(20));
        assert_eq!(default_max_scratchpad_size(), Some(32_768));
        assert_eq!(default_failure_dumps(), Some("on".to_string()));
        assert_eq!(default_response_sla_secs(), 3600);
        assert!(default_propagate_payment_error());
    }

    #[test]
    fn test_agent_config_propagate_payment_error_default_true() {
        let json = json!({
            "name": "no-config",
            "provider_id": "p",
            "model_name": "m",
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert!(config.propagate_payment_error, "should default to true");
    }

    #[test]
    fn test_agent_config_propagate_payment_error_false() {
        let json = json!({
            "name": "silent-agent",
            "provider_id": "p",
            "model_name": "m",
            "propagate_payment_error": false
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert!(!config.propagate_payment_error);
    }

    #[test]
    fn test_agent_config_response_sla_default() {
        let json = json!({
            "name": "no-sla",
            "provider_id": "p",
            "model_name": "m",
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.response_sla_secs, 3600, "should default to 3600s");
    }

    #[test]
    fn test_agent_config_response_sla_explicit() {
        let json = json!({
            "name": "fast-agent",
            "provider_id": "openai",
            "model_name": "gpt-4o",
            "response_sla_secs": 60
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.response_sla_secs, 60);

        // Roundtrip
        let serialized = serde_json::to_value(&config).unwrap();
        let roundtripped: AgentConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(roundtripped.response_sla_secs, 60);
    }

    #[test]
    fn test_agent_config_failure_dumps_values() {
        // "on" (default)
        let json_on = json!({
            "name": "a", "provider_id": "p", "model_name": "m",
            "failure_dumps": "on"
        });
        let cfg: AgentConfig = serde_json::from_value(json_on).unwrap();
        assert_eq!(cfg.failure_dumps, Some("on".to_string()));

        // "full"
        let json_full = json!({
            "name": "a", "provider_id": "p", "model_name": "m",
            "failure_dumps": "full"
        });
        let cfg: AgentConfig = serde_json::from_value(json_full).unwrap();
        assert_eq!(cfg.failure_dumps, Some("full".to_string()));

        // "off"
        let json_off = json!({
            "name": "a", "provider_id": "p", "model_name": "m",
            "failure_dumps": "off"
        });
        let cfg: AgentConfig = serde_json::from_value(json_off).unwrap();
        assert_eq!(cfg.failure_dumps, Some("off".to_string()));

        // Explicit null → None
        let json_null = json!({
            "name": "a", "provider_id": "p", "model_name": "m",
            "failure_dumps": null
        });
        let cfg: AgentConfig = serde_json::from_value(json_null).unwrap();
        assert!(cfg.failure_dumps.is_none());
    }

    /// Ensures the manual `Default` impl stays in sync with serde defaults.
    /// If a field is added to `AgentConfig` with a `#[serde(default = "...")]`
    /// but the `Default` impl isn't updated (or vice-versa), this test fails.
    #[test]
    fn test_agent_config_default_parity_with_serde() {
        let from_default = AgentConfig::default();

        // Minimal JSON — serde fills every other field from its defaults
        let from_serde: AgentConfig = serde_json::from_value(json!({
            "name": "",
            "provider_id": "",
            "model_name": "",
            "temperature": 0.0,
            "max_tokens": 0,
        }))
        .expect("minimal JSON should deserialize with serde defaults");

        // Compare every field that has a serde(default) function
        assert_eq!(from_default.model, from_serde.model, "model");
        assert_eq!(
            from_default.max_react_iterations, from_serde.max_react_iterations,
            "max_react_iterations"
        );
        assert_eq!(
            from_default.max_scratchpad_size, from_serde.max_scratchpad_size,
            "max_scratchpad_size"
        );
        assert_eq!(
            from_default.max_retries, from_serde.max_retries,
            "max_retries"
        );
        assert_eq!(
            from_default.presence_penalty, from_serde.presence_penalty,
            "presence_penalty"
        );
        assert_eq!(
            from_default.textual_feedback, from_serde.textual_feedback,
            "textual_feedback"
        );
        assert_eq!(
            from_default.use_streaming, from_serde.use_streaming,
            "use_streaming"
        );
        assert_eq!(
            from_default.repair_invalid_escapes, from_serde.repair_invalid_escapes,
            "repair_invalid_escapes"
        );
        assert_eq!(
            from_default.scratchpad_limit, from_serde.scratchpad_limit,
            "scratchpad_limit"
        );
        assert_eq!(
            from_default.context_window, from_serde.context_window,
            "context_window"
        );
        assert_eq!(
            from_default.failure_dumps, from_serde.failure_dumps,
            "failure_dumps"
        );
        assert_eq!(
            from_default.response_sla_secs, from_serde.response_sla_secs,
            "response_sla_secs"
        );
        assert_eq!(
            from_default.propagate_payment_error, from_serde.propagate_payment_error,
            "propagate_payment_error"
        );
        assert_eq!(
            from_default.capability_tags, from_serde.capability_tags,
            "capability_tags"
        );
        assert_eq!(
            from_default.description, from_serde.description,
            "description"
        );
        assert_eq!(
            from_default.signing_schemes, from_serde.signing_schemes,
            "signing_schemes"
        );
        assert_eq!(from_default.exec, from_serde.exec, "exec");
        assert_eq!(from_default.mcp, from_serde.mcp, "mcp");
        assert_eq!(from_default.claude, from_serde.claude, "claude");
        assert_eq!(from_default.openrouter, from_serde.openrouter, "openrouter");
        assert_eq!(
            from_default.prompt_exposure_guard, from_serde.prompt_exposure_guard,
            "prompt_exposure_guard"
        );
    }

    /// `openrouter: Option<OpenRouterConfig>` must omit from the wire
    /// when `None`. Skipping this check let the test pass while the
    /// field silently serialised as `null` — which broke forward
    /// compat on orchestrator deployments that pre-dated the field.
    #[test]
    fn test_openrouter_field_omitted_when_none() {
        let cfg = AgentConfig {
            name: "test".into(),
            provider_id: "openai".into(),
            model_name: "gpt-4".into(),
            temperature: 0.0,
            max_tokens: 0,
            ..Default::default()
        };
        assert!(cfg.openrouter.is_none(), "sanity: default is None");
        let serialized = serde_json::to_string(&cfg).unwrap();
        assert!(
            !serialized.contains("openrouter"),
            "openrouter=None should be omitted, got: {serialized}"
        );

        // Round-trip a minimal JSON with no openrouter field — the
        // field must arrive as `None`, not as a synthetic default.
        let json = json!({
            "name": "test",
            "provider_id": "openai",
            "model_name": "gpt-4",
            "temperature": 0.0,
            "max_tokens": 0,
        });
        let parsed: AgentConfig = serde_json::from_value(json).unwrap();
        assert!(parsed.openrouter.is_none());
    }

    /// Round-trip with a populated `OpenRouterConfig` to lock the
    /// wire shape. If any field is dropped by serde or the default
    /// impl drifts, this catches it at build time.
    #[test]
    fn test_openrouter_field_roundtrip_populated() {
        let cfg = AgentConfig {
            name: "test".into(),
            provider_id: "openrouter".into(),
            model_name: "google/gemma-4-26b-a4b-it".into(),
            temperature: 0.7,
            max_tokens: 16384,
            openrouter: Some(OpenRouterConfig {
                provider_sort: Some("throughput".into()),
                zdr: Some(true),
                allow_fallbacks: Some(false),
                ignore: vec!["nextbit".into()],
                only: vec!["akashml/fp8".into()],
                exclude_reasoning: Some(true),
            }),
            ..Default::default()
        };
        let serialized = serde_json::to_string(&cfg).unwrap();
        assert!(serialized.contains(r#""openrouter""#));
        let parsed: AgentConfig = serde_json::from_str(&serialized).unwrap();
        let or = parsed.openrouter.expect("openrouter must round-trip");
        assert_eq!(or.provider_sort.as_deref(), Some("throughput"));
        assert_eq!(or.zdr, Some(true));
        assert_eq!(or.allow_fallbacks, Some(false));
        assert_eq!(or.ignore, vec!["nextbit".to_string()]);
        assert_eq!(or.only, vec!["akashml/fp8".to_string()]);
        assert_eq!(or.exclude_reasoning, Some(true));
    }

    #[test]
    fn test_capability_tags_roundtrip() {
        let json = json!({
            "name": "test",
            "provider_id": "openai",
            "model_name": "gpt-4",
            "temperature": 0.7,
            "max_tokens": 1000,
            "capability_tags": ["legal", "audit", "compliance"],
            "description": "Legal audit specialist",
            "signing_schemes": ["eip712", "ed25519"]
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.capability_tags, vec!["legal", "audit", "compliance"]);
        assert_eq!(
            config.description.as_deref(),
            Some("Legal audit specialist")
        );
        assert_eq!(config.signing_schemes, vec!["eip712", "ed25519"]);

        // Roundtrip
        let serialized = serde_json::to_string(&config).unwrap();
        let deserialized: AgentConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.capability_tags, config.capability_tags);
        assert_eq!(deserialized.description, config.description);
        assert_eq!(deserialized.signing_schemes, config.signing_schemes);
    }

    #[test]
    fn test_new_fields_default_to_empty() {
        let json = json!({
            "name": "minimal",
            "provider_id": "test",
            "model_name": "test",
            "temperature": 0.0,
            "max_tokens": 0,
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        assert!(config.capability_tags.is_empty());
        assert!(config.description.is_none());
        assert!(config.signing_schemes.is_empty());
        assert!(config.exec.is_none());
    }

    #[test]
    fn test_agent_config_exec_roundtrip() {
        let json = json!({
            "name": "py-agent",
            "provider_id": "exec_local",
            "model_name": "custom",
            "exec": {
                "command": ["python3", "agent.py"],
                "working_dir": "/opt/agents",
                "env": {"MY_VAR": "value"},
                "timeout_secs": 120
            }
        });
        let config: AgentConfig = serde_json::from_value(json).unwrap();
        let exec = config.exec.as_ref().expect("exec should be present");
        assert_eq!(exec.command, vec!["python3", "agent.py"]);
        assert_eq!(
            exec.working_dir.as_ref().map(|p| p.to_str().unwrap()),
            Some("/opt/agents")
        );
        assert_eq!(exec.env.get("MY_VAR").unwrap(), "value");
        assert_eq!(exec.timeout_secs, Some(120));

        // Roundtrip
        let serialized = serde_json::to_value(&config).unwrap();
        let deserialized: AgentConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized.exec, config.exec);
    }

    #[test]
    fn test_agent_config_exec_none_omitted_from_json() {
        let config = AgentConfig::default();
        let serialized = serde_json::to_value(&config).unwrap();
        let obj = serialized.as_object().unwrap();
        assert!(
            !obj.contains_key("exec"),
            "exec: None should be omitted from serialization"
        );
        assert!(
            !obj.contains_key("mcp"),
            "mcp: None should be omitted from serialization"
        );
        assert!(
            !obj.contains_key("claude"),
            "claude: None should be omitted from serialization"
        );
    }

    #[test]
    fn redacted_env_hides_sensitive_keys() {
        let mut env = HashMap::new();
        env.insert("SAFE_VAR".to_string(), "visible".to_string());
        env.insert("API_KEY".to_string(), "super-secret".to_string());
        env.insert("db_password".to_string(), "pass123".to_string());
        env.insert("AUTH_TOKEN".to_string(), "tok_abc".to_string());
        env.insert("MY_CREDENTIAL_ID".to_string(), "cred".to_string());
        env.insert("AWS_SECRET_ACCESS_KEY".to_string(), "aws123".to_string());

        let config = ExecProviderConfig {
            command: vec!["test".into()],
            working_dir: None,
            env,
            timeout_secs: None,
        };
        let serialized = serde_json::to_value(&config).unwrap();
        let env_obj = serialized["env"].as_object().unwrap();

        assert_eq!(env_obj["SAFE_VAR"], "visible");
        assert_eq!(env_obj["API_KEY"], "<redacted>");
        assert_eq!(env_obj["db_password"], "<redacted>");
        assert_eq!(env_obj["AUTH_TOKEN"], "<redacted>");
        assert_eq!(env_obj["MY_CREDENTIAL_ID"], "<redacted>");
        assert_eq!(env_obj["AWS_SECRET_ACCESS_KEY"], "<redacted>");
    }

    #[test]
    fn validate_provider_sections_rejects_multiple() {
        let config: AgentConfig = serde_json::from_value(json!({
            "name": "a", "provider_id": "p", "model_name": "m",
            "exec": { "command": ["test"] },
            "mcp": { "command": ["test"] }
        }))
        .unwrap();
        let err = config.validate_provider_sections(None).unwrap_err();
        assert!(err.contains("multiple provider sections"), "{err}");
    }

    #[test]
    fn validate_provider_sections_rejects_mismatch() {
        let config: AgentConfig = serde_json::from_value(json!({
            "name": "a", "provider_id": "p", "model_name": "m",
            "exec": { "command": ["test"] }
        }))
        .unwrap();
        let err = config.validate_provider_sections(Some("mcp")).unwrap_err();
        assert!(err.contains("does not match"), "{err}");
    }

    #[test]
    fn validate_provider_sections_accepts_matching() {
        let config: AgentConfig = serde_json::from_value(json!({
            "name": "a", "provider_id": "p", "model_name": "m",
            "exec": { "command": ["test"] }
        }))
        .unwrap();
        config.validate_provider_sections(Some("exec")).unwrap();
    }

    #[test]
    fn validate_provider_sections_accepts_none() {
        let config: AgentConfig = serde_json::from_value(json!({
            "name": "a", "provider_id": "p", "model_name": "m"
        }))
        .unwrap();
        config.validate_provider_sections(Some("exec")).unwrap();
    }

    #[test]
    fn validate_compaction_knobs_accepts_defaults() {
        AgentConfig::default().validate_compaction_knobs().unwrap();
    }

    #[test]
    fn validate_compaction_knobs_rejects_zero_keep() {
        let cfg = AgentConfig {
            compact_history_default_keep: 0,
            ..AgentConfig::default()
        };
        let err = cfg.validate_compaction_knobs().unwrap_err();
        assert!(err.contains("compact_history_default_keep"));
    }

    #[test]
    fn validate_compaction_knobs_rejects_out_of_range_fraction() {
        for bad in [0.0, 1.5, -0.1] {
            let cfg = AgentConfig {
                scratchpad_squeeze_fraction: bad,
                ..AgentConfig::default()
            };
            assert!(
                cfg.validate_compaction_knobs().is_err(),
                "fraction {bad} must be rejected"
            );
        }
    }

    #[test]
    fn validate_compaction_knobs_accepts_boundary_one() {
        let cfg = AgentConfig {
            scratchpad_squeeze_fraction: 1.0,
            compact_history_default_keep: 1,
            ..AgentConfig::default()
        };
        cfg.validate_compaction_knobs().unwrap();
    }

    // ── persona deserialization ─────────────────────────────────────

    fn parse_agent_persona(yaml_fragment: &str) -> Option<String> {
        let yaml = format!("name: test\n{yaml_fragment}");
        let cfg: AgentConfig = serde_yaml::from_str(&yaml).expect("agent yaml must parse");
        cfg.persona
    }

    /// Plain-string persona — back-compat path. Operators with old
    /// `agent.yml` files must keep parsing as if this PR never landed.
    #[test]
    fn persona_inline_string_back_compat() {
        let persona = parse_agent_persona("persona: \"you are a careful reviewer\"");
        assert_eq!(persona.as_deref(), Some("you are a careful reviewer"));
    }

    #[test]
    fn persona_absent_stays_none() {
        let persona = parse_agent_persona("");
        assert!(persona.is_none());
    }

    #[test]
    fn persona_layered_text_joins_with_double_newline() {
        let persona = parse_agent_persona(
            "persona:\n\
             - type: text\n  prompt: \"a\"\n\
             - type: text\n  prompt: \"b\"\n",
        );
        assert_eq!(persona.as_deref(), Some("a\n\nb"));
    }

    /// Md layer reads the referenced file. Mixed with text layers in
    /// order produces the expected stacked string.
    #[test]
    fn persona_layered_md_reads_file_and_stacks_with_text() {
        let sandbox_dir = tempfile::tempdir().unwrap();
        let md_path = sandbox_dir.path().join("body.md");
        std::fs::write(&md_path, "from-md\n").unwrap();
        let yaml = format!(
            "persona:\n\
             - type: text\n  prompt: \"lead\"\n\
             - type: md\n  prompt: \"{}\"\n\
             - type: text\n  prompt: \"tail\"\n",
            md_path.display()
        );
        let persona = parse_agent_persona(&yaml);
        assert_eq!(persona.as_deref(), Some("lead\n\nfrom-md\n\n\ntail"));
    }

    /// Missing md file → parse error naming the path. Operators see
    /// the failure at fleet boot rather than at agent advertisement.
    #[test]
    fn persona_layered_md_missing_file_errors_with_path() {
        let yaml = "name: test\n\
                    persona:\n\
                    - type: md\n  prompt: \"/path/does/not/exist/persona.md\"\n";
        let err = serde_yaml::from_str::<AgentConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("/path/does/not/exist/persona.md") && msg.contains("could not be read"),
            "error must name the missing path; got: {msg}"
        );
    }
}
