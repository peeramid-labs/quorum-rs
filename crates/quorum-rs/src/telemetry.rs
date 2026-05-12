//! Operational telemetry — independent log streams for orchestrator and
//! agent processes.
//!
//! This module ships the event catalog (as a Rust type), subject
//! derivation, trace correlation, and a fire-and-forget emission
//! helper.
//!
//! # Design principles (operator-facing contract lives in
//! `docs/agent-telemetry.md`)
//!
//! 1. **Metrics only, no content.** Every variant in [`TelemetryEvent`]
//!    carries IDs, durations, counts, enums, and boolean flags —
//!    never prompt text, proposal bodies, evaluation justifications,
//!    raw LLM outputs, thought process, or secret material. Redaction
//!    is enforced at the *type* layer: fields that could carry text
//!    simply do not exist on these structs.
//! 2. **Role-scoped subjects.** Orchestrator publishes under
//!    `telemetry.orch.*`; each agent publishes under
//!    `telemetry.agent.{agent_id}.*`. The agent_id position is
//!    JWT-bound so one agent cannot forge another agent's subtree.
//! 3. **Fire-and-forget, zero-cost when disabled.** [`emit`] returns
//!    immediately on success, silently drops on serialization or
//!    publish failure. Telemetry must never gate critical-path work.
//!    When [`TelemetryConfig::enabled`] is `false`, no events are
//!    constructed.
//! 4. **Trace correlation without coordination.** Orchestrator and
//!    agent independently derive the same [`trace_id`] from
//!    `(job_id, round, phase, agent_id)` via the shared
//!    [`derive_trace_id`] function. No protocol message carries the
//!    trace id — it is a pure function of public identifiers.
//!
//! Consumers that do not construct a [`TelemetryEmitter`] pay zero
//! runtime cost.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agents::DeliberationPhase;

/// Default subject prefix for the per-agent telemetry tree.
/// The `{agent_id}` position is JWT-bound so agents cannot cross tenants.
pub const TELEMETRY_AGENT_PREFIX: &str = "telemetry.agent";

/// Length (in hex chars) of the correlation id derived by
/// [`derive_trace_id`]. 32 hex chars = 128 bits of identifier space —
/// enough to keep birthday-collision probability negligible at
/// telemetry scale (one event per LLM attempt, per tool call, per
/// retry, across multiple agents and long-lived jobs). Do not narrow
/// below 32 without reviewing the collision math; the unit test
/// `trace_id_width_is_at_least_128_bits` is a regression guard.
pub const TRACE_ID_LEN: usize = 32;

/// Configuration for telemetry emission.
///
/// Defaults to **enabled**; an operator who wants to opt their agent
/// out of telemetry sets `telemetry.enabled: false` in the agent YAML.
/// The orchestrator block mirrors the same shape for symmetry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// Master switch. When `false`, no events are emitted and no
    /// emitter is constructed. Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Telemetry destinations. One entry per destination — typically
    /// the service operator's NATS plus the agent operator's own
    /// dashboard NATS for OSS-split deployments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<TelemetryEndpointConfig>,
}

/// One telemetry destination.
///
/// Each endpoint owns its own NATS connection, credentials, and
/// subject prefix. The SDK's [`TelemetryEmitterMux`] fans events out
/// to every configured endpoint; targeted emission for one
/// destination uses [`TelemetryEmitterMux::emit_for`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelemetryEndpointConfig {
    /// Operator-readable name used by [`TelemetryEmitterMux::emit_for`]
    /// and shown in `dropped_count` per-endpoint reports. Must be
    /// unique within the `endpoints` list.
    pub name: String,
    /// NATS server URL for this endpoint. `None` means "reuse the
    /// agent's primary NATS connection" — the legacy single-endpoint
    /// path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nats_url: Option<String>,
    /// Path to a NATS `.creds` file authorising this agent to publish
    /// to its telemetry subtree on the endpoint's NATS account. `None`
    /// when reusing the primary connection (which already has
    /// credentials from the orchestrator-issued JWT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creds: Option<String>,
    /// Subject prefix override for this endpoint. `None` falls back
    /// to [`TELEMETRY_AGENT_PREFIX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_prefix: Option<String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoints: Vec::new(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

/// Which process is emitting an event.
///
/// Determines the NATS subject prefix. Currently only supports agent
/// sources; the orchestrator defines its own source type in its crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetrySource {
    /// Agent-side event. Subject: `telemetry.agent.<agent_id>.<event>`.
    Agent {
        /// Agent id as it appears in the JWT subject bind.
        agent_id: String,
    },
}

impl TelemetrySource {
    /// Construct an `Agent` source after validating that `agent_id`
    /// is a NATS-safe subject token. Returns `Err` if the id contains
    /// `.` / `*` / `>` / whitespace / control characters or is empty
    /// — using such an id would otherwise reshape the subject
    /// hierarchy and silently break the JWT-bound `agent_id`
    /// position contract.
    ///
    /// Direct struct-literal construction (`TelemetrySource::Agent { … }`)
    /// is still permitted for tests and for callers that have
    /// already validated their input upstream (e.g. orchestrator
    /// JWT-issuance flow), but [`subject`](Self::subject) re-validates
    /// as defence-in-depth and refuses to emit malformed subjects.
    pub fn agent(agent_id: impl Into<String>) -> Result<Self, String> {
        let agent_id = agent_id.into();
        crate::nats_utils::validate_nats_name(&agent_id, "agent_id")?;
        Ok(TelemetrySource::Agent { agent_id })
    }

    /// Compute the NATS subject for a given event kind, optionally using a
    /// custom prefix (for testing / tenanted deployments).
    ///
    /// The trailing path segment is the event's snake_case kind (e.g.
    /// `"task_accepted"`). No event-payload fields appear in the subject,
    /// so there is no injection surface from the event data. The `agent_id`
    /// and any `custom_prefix` are validated against
    /// [`crate::nats_utils::validate_nats_name`]; an invalid token
    /// produces `Err` and the caller (typically [`TelemetryEmitter::emit`])
    /// must drop the event rather than ship a malformed subject.
    pub fn subject(&self, kind: &str, custom_prefix: Option<&str>) -> Result<String, String> {
        if let Some(prefix) = custom_prefix {
            for segment in prefix.split('.') {
                crate::nats_utils::validate_nats_name(segment, "telemetry custom_prefix segment")?;
            }
        }
        match self {
            TelemetrySource::Agent { agent_id } => {
                crate::nats_utils::validate_nats_name(agent_id, "agent_id")?;
                let prefix = custom_prefix.unwrap_or(TELEMETRY_AGENT_PREFIX);
                Ok(format!("{prefix}.{agent_id}.{kind}"))
            }
        }
    }
}

/// Deterministic 32-char (128-bit) hex correlation id.
///
/// Orchestrator and agent compute the same id from the same
/// `(job_id, round, phase, agent_id)` tuple. A forwarder / sink can
/// stitch per-process spans into a single trace without any runtime
/// coordination protocol.
///
/// Input encoding is **length-prefixed** for each variable-length
/// component (`{len}:{bytes}`) so two distinct input tuples can never
/// collide via boundary ambiguity, regardless of which characters
/// (including `':'`) the identifiers contain. Length is encoded in
/// decimal ASCII — the value is the byte length of the UTF-8 form.
/// Fixed-width fields (`round` u32, `phase` discriminant) are
/// appended without prefixes since their boundaries are
/// unambiguous from the schema.
pub fn derive_trace_id(
    job_id: &str,
    round: u32,
    phase: DeliberationPhase,
    agent_id: &str,
) -> String {
    let input = format!(
        "{}:{job_id}|{round}|{}|{}:{agent_id}",
        job_id.len(),
        phase.as_str(),
        agent_id.len(),
    );
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(TRACE_ID_LEN);
    for byte in digest.iter().take(TRACE_ID_LEN / 2) {
        use std::fmt::Write;
        write!(out, "{byte:02x}").expect("writing to String never fails");
    }
    debug_assert_eq!(out.len(), TRACE_ID_LEN);
    out
}

/// Convenience wrapper around [`derive_trace_id`] with consistent naming
/// for telemetry callers. All agent-side telemetry uses this function so
/// the correlation-id derivation is uniform across modules.
pub fn trace_id_for(job_id: &str, round: u32, phase: DeliberationPhase, agent_id: &str) -> String {
    derive_trace_id(job_id, round, phase, agent_id)
}

/// `trace_id` for events that have no task scope (e.g. the worker's
/// NATS connection-state monitor). Same `TRACE_ID_LEN` lowercase-hex
/// shape `derive_trace_id` produces, so consumers can parse `trace_id`
/// uniformly across the catalog. Derives the digest from a
/// length-prefixed `(agent_id, uuid_v4)` input — UUIDv4 supplies
/// 122 bits of entropy, length-prefixing the agent_id matches the
/// boundary-disambiguation property `derive_trace_id` already uses.
fn session_less_trace_id(agent_id: &str) -> String {
    let uuid = uuid::Uuid::new_v4();
    let input = format!("nosess|{}:{agent_id}|{}", agent_id.len(), uuid.as_simple());
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(TRACE_ID_LEN);
    for byte in digest.iter().take(TRACE_ID_LEN / 2) {
        use std::fmt::Write;
        write!(out, "{byte:02x}").expect("writing to String never fails");
    }
    debug_assert_eq!(out.len(), TRACE_ID_LEN);
    out
}

// ---------------------------------------------------------------------------
// Common envelope
// ---------------------------------------------------------------------------

/// Fields that every agent-side event carries.
///
/// Orchestrator events reuse a subset (no `agent_id`, no `phase` for
/// cross-round events) — structured here as a helper rather than a
/// forced mixin so each variant keeps its own flat JSON shape.
///
/// `job_id`, `round`, and `phase` are `Option` so that session-less
/// events (e.g. [`NatsConnectionStateChanged`]) can omit them cleanly
/// rather than fabricating placeholder values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEventCommon {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<DeliberationPhase>,
    /// Unix milliseconds at emission time.
    pub ts: i64,
    /// 32-char (128-bit) hex produced by [`derive_trace_id`].
    pub trace_id: String,
}

// ---------------------------------------------------------------------------
// TelemetryContext + emit_event! macro
// ---------------------------------------------------------------------------

/// Lightweight context for telemetry emission.
///
/// Carries the identifiers common to all agent-side events and
/// produces an [`AgentEventCommon`] via [`Self::common`].
/// `job_id`, `round`, and `phase` are `Option` so that session-less
/// callers (e.g. NATS connection state events) can leave them unset.
#[derive(Debug, Clone)]
pub struct TelemetryContext {
    agent_id: String,
    job_id: Option<String>,
    round: Option<u32>,
    phase: Option<DeliberationPhase>,
    trace_id: String,
}

impl TelemetryContext {
    /// Construct a new context.
    ///
    /// `job_id`, `round`, and `phase` may be `None` for events that
    /// genuinely have no task scope — the worker's NATS connection-
    /// state monitor is the canonical example. In that case
    /// `trace_id` is a per-event UUID (`nosess-{agent}-{uuid}`)
    /// rather than a `derive_trace_id(...)` derivation, because
    /// deriving from a constant tuple (e.g. `("nosess", 0, Proposing)`)
    /// would alias every session-less event for the same agent under
    /// a single trace, defeating correlation.
    ///
    /// For task-scoped emission, callers go through
    /// [`AgentContext::telemetry_for`](crate::agents::AgentContext::telemetry_for),
    /// which enforces the `session_id` invariant and never reaches
    /// the no-session branch here.
    pub fn new(
        agent_id: &str,
        job_id: Option<&str>,
        round: Option<u32>,
        phase: Option<DeliberationPhase>,
    ) -> Self {
        let trace_id = match (job_id, round, phase) {
            (Some(j), Some(r), Some(p)) => derive_trace_id(j, r, p, agent_id),
            _ => session_less_trace_id(agent_id),
        };
        Self {
            agent_id: agent_id.to_string(),
            job_id: job_id.map(|s| s.to_string()),
            round,
            phase,
            trace_id,
        }
    }

    /// Produce the shared envelope fields for a telemetry event.
    pub fn common(&self) -> AgentEventCommon {
        AgentEventCommon {
            agent_id: self.agent_id.clone(),
            job_id: self.job_id.clone(),
            round: self.round,
            phase: self.phase,
            ts: chrono::Utc::now().timestamp_millis(),
            trace_id: self.trace_id.clone(),
        }
    }
}

/// Emit a telemetry event when an emitter is available.
///
/// Reduces boilerplate at emit sites from ~12 lines to 1:
/// ```ignore
/// emit_event!(Some(&emitter), ctx, LlmRequestStart {
///     request_id: "r1".into(),
///     model: "gpt-4".into(),
///     provider_id: "openai".into(),
///     attempt: 1,
///     estimated_input_tokens: 100,
/// });
/// ```
#[macro_export]
macro_rules! emit_event {
    // Low-level: caller has `Option<&TelemetryEmitter>` and a
    // `TelemetryContext` directly. Used in process-level paths
    // without an `AgentContext` (e.g. the worker's NATS
    // connection-state monitor).
    ($emitter:expr, $ctx:expr, $variant:ident { $($field:ident $(: $value:expr)?),* $(,)? }) => {
        if let Some(emitter) = $emitter {
            let event = $crate::telemetry::TelemetryEvent::$variant($crate::telemetry::$variant {
                common: $ctx.common(),
                $($field $(: $value)?),*
            });
            emitter.emit(&event);
        }
    };
}

/// Emit a telemetry event scoped to the task on the given context.
///
/// Pulls the emitter from `context.telemetry` and derives the
/// `TelemetryContext` envelope via `context.telemetry_for()` —
/// `agent_id` is taken from `context.agent_id` (populated by the
/// orchestrator at dispatch and by the worker after deserialize).
/// One argument-list, no double-`context.` plumbing at the call
/// site:
///
/// ```ignore
/// emit_for!(context, ToolCallExecuted {
///     tool_name: name,
///     latency_ms: 42,
///     success: true,
/// });
/// ```
///
/// Use [`emit_event!`] for the lower-level form when no
/// [`AgentContext`](crate::agents::AgentContext) is in scope (e.g.
/// process-level connection-state events).
#[macro_export]
macro_rules! emit_for {
    ($context:expr, $variant:ident { $($field:ident $(: $value:expr)?),* $(,)? }) => {
        if let Some(ref emitter) = $context.telemetry {
            let envelope = $context.telemetry_for();
            let event = $crate::telemetry::TelemetryEvent::$variant($crate::telemetry::$variant {
                common: envelope.common(),
                $($field $(: $value)?),*
            });
            emitter.emit(&event);
        }
    };
}

// ---------------------------------------------------------------------------
// Failure / error taxonomy
// ---------------------------------------------------------------------------

/// Error classification emitted on [`LlmRequestFailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorClass {
    Transport,
    RateLimit,
    PaymentRequired,
    ServerError,
    ContextOverflow,
    Parse,
    Other,
}

// `LlmError` (the typed error from `AiModel::chat_completion`) lives
// in `crate::llms::error` because it's an AiModel concern, not a
// telemetry concept. Re-exported here for ergonomic backward
// compatibility — existing call sites that imported via
// `crate::telemetry::LlmError` keep compiling.
pub use crate::llms::LlmError;

/// Terminal finish reason reported by the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    Error,
}

/// Reason a structured-output retry was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryReason {
    EmptyContent,
    SchemaError,
    Truncated,
    HallucinatedTool,
}

/// Terminal failure class on [`TaskFailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFailureClass {
    LlmExhausted,
    ToolError,
    Timeout,
    ContextOverflow,
    ParseRetryExhausted,
    EmptyContentAfterRetries,
}

/// State transition on the agent's NATS client (G6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatsConnectionState {
    Connected,
    Disconnected,
    Reconnecting,
    Closed,
}

impl From<&async_nats::connection::State> for NatsConnectionState {
    fn from(s: &async_nats::connection::State) -> Self {
        match s {
            async_nats::connection::State::Connected => Self::Connected,
            async_nats::connection::State::Disconnected => Self::Disconnected,
            async_nats::connection::State::Pending => Self::Reconnecting,
        }
    }
}

// ---------------------------------------------------------------------------
// Agent events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmRequestStart {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    /// Opaque id used to correlate with [`LlmRequestComplete`] /
    /// [`LlmRequestFailed`] / [`LlmRequestStalled`] events from the
    /// same in-flight call.
    pub request_id: String,
    pub model: String,
    pub provider_id: String,
    pub attempt: u32,
    pub estimated_input_tokens: u32,
    /// `100 * estimated_input_tokens / context_window`, clamped to
    /// `[0, 100]`. `0.0` when `context_window` is unknown.
    #[serde(default)]
    pub context_utilization_pct: f64,
    /// Sum of [`ToolCallExecuted::output_bytes`] for this task so far.
    #[serde(default)]
    pub recent_tool_output_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmRequestComplete {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub request_id: String,
    pub latency_ms: u64,
    /// G1 — time-to-first-token when the provider streams. `None`
    /// when the provider response is non-streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// G1 — ms from first token to finish. `None` on non-streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_ms: Option<u64>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: u32,
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub cost_usd: f64,
    pub finish_reason: FinishReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_backend: Option<String>,
    /// G7 — evaluate-phase only: structured-output array lengths.
    /// `None` on propose phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_assessments_emitted: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disagreements_emitted: Option<u32>,
    /// Char count of the serialized input messages JSON. Tokenizer-
    /// agnostic request-size proxy that survives provider differences;
    /// pairs with `input_tokens` to surface tokenization anomalies.
    #[serde(default)]
    pub messages_chars: u32,
    /// `max_tokens` value the agent asked the provider for at this
    /// request. Useful when reactive context-shrink kicks in: lets
    /// dashboards correlate `finish_reason == Length` with the
    /// shrink-retry path. `None` when the provider strategy doesn't
    /// surface a max-tokens cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_requested: Option<u32>,
    /// Char count of the generated content (the `message.content`
    /// across choices, post-strip-reasoning). Pairs with
    /// `output_tokens` to spot tokenization anomalies.
    #[serde(default)]
    pub response_chars: u32,
    /// Number of tool calls the model emitted on this request.
    /// Distinct from [`ToolCallExecuted`] (per-call telemetry) and
    /// [`TaskCompleted::tool_call_count`] (cumulative across the
    /// whole task). Captures the per-turn fan-out.
    #[serde(default)]
    pub tool_calls_emitted: u32,
    /// `true` only when `available < floor` — distinct from healthy
    /// `available > floor` adaptive shrinks.
    #[serde(default)]
    pub max_tokens_shrunk_to_floor: bool,
    /// Raw headroom at dispatch (`context_window - estimated_input`,
    /// saturating non-negative). `None` when `context_window` is
    /// unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_space_at_dispatch: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmRequestFailed {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub request_id: String,
    pub error_class: LlmErrorClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    pub latency_ms: u64,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_backend: Option<String>,
}

/// 30s-cadence heartbeat for in-flight LLM requests. Fired by the
/// agent on a timer while an [`LlmRequestStart`] has no matching
/// terminal event; cancelled automatically when the request
/// completes or fails.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmRequestStalled {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub request_id: String,
    pub elapsed_ms: u64,
    pub ttft_received: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_token_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallExecuted {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub tool_name: String,
    pub latency_ms: u64,
    pub success: bool,
    /// Length of the tool result payload in bytes.
    #[serde(default)]
    pub output_bytes: u64,
    /// `ceil(output_bytes / 4)` from the built-in agent. `None` is
    /// reserved for callers without a tokenizer hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_estimated: Option<u32>,
    /// `true` when the tool's `max_bytes` cap clipped the result.
    #[serde(default)]
    pub truncated: bool,
    /// `true` when the tool emitted a `next_offset` cursor.
    #[serde(default)]
    pub paginated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryLoopAttempt {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub attempt: u32,
    pub reason: RetryReason,
    pub cumulative_latency_ms: u64,
    /// G4 — rollup cost across this task's attempts so far.
    #[serde(default)]
    pub cumulative_cost_usd: f64,
    #[serde(default)]
    pub cumulative_input_tokens: u32,
    #[serde(default)]
    pub cumulative_output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskAccepted {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub dispatch_delay_ms: u64,
    /// Unix ms the orchestrator stamped on the envelope at publish.
    /// `None` on payloads from a publisher that didn't stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_publish_ts: Option<i64>,
    /// `agent_receive_ts - task_publish_ts`, clamped non-negative.
    /// `None` when `task_publish_ts` is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_age_at_accept_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskCompleted {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub duration_ms: u64,
    pub dispatch_delay_ms: u64,
    /// `Some(n)` once first-`llm_request_start` timing is recorded;
    /// `None` until the per-task counter wiring lands. Distinguishes
    /// "task ran but we didn't measure" from "task ran with zero
    /// queue wait".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<u64>,
    pub phase_budget_remaining_ms: i64,
    /// `Some(n)` once each `NsedAgent` impl exposes its retry
    /// counter; `None` until that wiring lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_attempts: Option<u32>,
    /// `Some(n)` once `AgentResponse.tool_usage` is summed at task
    /// boundary; `None` until that wiring lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_count: Option<u32>,
    /// G6 — NATS client local buffer depth at submit time. `> 0`
    /// signals a forwarder / connection issue holding the agent's
    /// submission on-process. `None` until `async_nats::Client`
    /// introspection is wired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_publish_depth: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskFailed {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub duration_ms: u64,
    pub dispatch_delay_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<u64>,
    pub phase_budget_remaining_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_count: Option<u32>,
    pub failure_class: TaskFailureClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_publish_depth: Option<u32>,
}

/// G6 — NATS client transition on the agent side.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NatsConnectionStateChanged {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub state: NatsConnectionState,
    pub reconnects_so_far: u32,
    /// `None` until `async_nats::Client` introspection is wired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_publish_depth: Option<u32>,
    /// `None` until `async_nats::Client` introspection is wired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_bytes: Option<u64>,
}

/// Prompt-exposure guardrail fired on an agent's terminal-tool content.
///
/// Mirrors the `PromptExposureMiddleware` in
/// `crates/nsed-agent/src/middleware/builtin/prompt_exposure.rs`: it
/// reports that the guardrail saw one or more dictionary hits on the
/// agent's final response content, whether that tripped the retry
/// threshold, and how many hits landed in each category.
///
/// **Redaction posture.** The only text this event carries is
/// `sample_hits`, which is drawn from the guardrail's *fixed
/// dictionaries* (`xml-tag <tag_name>`, `tool-name <name>`,
/// `instruction "phrase"`, `wrong-acronym "acronym"`) — public
/// identifiers that already appear in the source tree. No portion of
/// the scanned response content is emitted. `sample_hits` is capped
/// by the guardrail at the same length as the retry-reason string so
/// the two surfaces cannot diverge.
///
/// **Threshold semantics.** `blocked = true` means the guardrail
/// rejected the response and forced a retry; `blocked = false` means
/// hits were observed but fell under the `min_suspicion_score` /
/// `min_matches` / `min_answer_length_chars` gates so the response
/// was allowed through. Dashboards use the `(blocked, hit_count)`
/// tuple to compute false-positive rates and tune thresholds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptExposureDetected {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    /// The terminal tool whose content was scanned
    /// (`submit_proposal`, `submit_batch_evaluation`, or any other
    /// terminal tool the agent was using for this task).
    pub terminal_tool: String,
    /// `true` when the guardrail rejected the response and triggered a
    /// retry; `false` when hits were observed but fell under threshold.
    pub blocked: bool,
    /// Total hits across all categories. Invariant:
    /// `hit_count == xml_tag_hits + tool_name_hits + instruction_hits
    /// + wrong_acronym_hits`.
    pub hit_count: u32,
    /// Length of the scanned content in chars. Feeds the suspicion
    /// score computation on the detector side.
    pub response_length_chars: u32,
    /// Log-scaled suspicion score the guardrail computed for this
    /// response (`hit_count * log2(1 + len / unit_chars)`), so
    /// operators can reproduce the guardrail's threshold logic from
    /// telemetry alone.
    pub suspicion_score: f64,
    pub xml_tag_hits: u32,
    pub tool_name_hits: u32,
    pub instruction_hits: u32,
    pub wrong_acronym_hits: u32,
    /// Sample of the first few dictionary-sourced hit labels in the
    /// same format the guardrail uses internally
    /// (`xml-tag <working_memory>`, `tool-name submit_proposal`,
    /// `instruction "Proposing Phase"`, `wrong-acronym "Neural
    /// Swarm"`). Capped so the payload stays bounded; never contains
    /// scanned-content fragments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_hits: Vec<String>,
}

impl PromptExposureDetected {
    /// Known dictionary prefixes for sample_hits labels. Each entry is a
    /// short, fixed dictionary identifier used by the guardrail — no free-
    /// form user content.
    const ALLOWED_PREFIXES: &'static [&'static str] =
        &["xml-tag ", "tool-name ", "instruction ", "wrong-acronym "];

    /// Validate structural invariants before serialization.
    ///
    /// 1. `hit_count` must equal the sum of all category counters.
    /// 2. `sample_hits` must not contain raw scanned-content fragments
    ///    (they should only be dictionary-sourced labels like
    ///    `"xml-tag <working_memory>"`).
    ///
    /// Returns a descriptive error when any invariant is violated.
    /// Callers should drop the event and increment a drop counter.
    pub fn validate(&self) -> Result<(), String> {
        let sum = self
            .xml_tag_hits
            .saturating_add(self.tool_name_hits)
            .saturating_add(self.instruction_hits)
            .saturating_add(self.wrong_acronym_hits);
        if self.hit_count != sum {
            return Err(format!(
                "hit_count {} != sum of category hits {} \
                 (xml={}: tool={}: instruction={}: acronym={})",
                self.hit_count,
                sum,
                self.xml_tag_hits,
                self.tool_name_hits,
                self.instruction_hits,
                self.wrong_acronym_hits
            ));
        }
        // Reject sample_hits that look like raw content. Labels are
        // short token-like strings starting with a known dictionary
        // prefix; anything > 64 chars is suspicious.
        for (i, hit) in self.sample_hits.iter().enumerate() {
            if hit.len() > 64 {
                return Err(format!(
                    "sample_hits[{i}] exceeds 64 chars ({}); \
                     may contain raw content",
                    hit.len()
                ));
            }
            // Each entry must start with a known dictionary prefix.
            if !Self::ALLOWED_PREFIXES.iter().any(|p| hit.starts_with(p)) {
                return Err(format!(
                    "sample_hits[{i}] does not start with a known dictionary prefix: {hit:?}"
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Context-emergency shrink
// ---------------------------------------------------------------------------

/// One contributor to the running tool-output total carried by
/// [`ContextEmergencyShrink::recent_tool_outputs`]. Only the top-N
/// (default 5) are emitted so the payload stays bounded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentToolOutput {
    pub tool: String,
    pub bytes: u64,
}

/// Fires once per task per shrink-to-floor with the bloat
/// attribution payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextEmergencyShrink {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub available_space: u32,
    pub requested_max: u32,
    /// The floor the SDK clamped to (typically 200).
    pub floor_used: u32,
    pub estimated_input: u32,
    pub context_window: u32,
    /// Top contributors to bloat in this task (size-bounded; limit
    /// 5). `tool` (public name) + `bytes` only — content is never
    /// disclosed.
    #[serde(default)]
    pub recent_tool_outputs: Vec<RecentToolOutput>,
}

// ---------------------------------------------------------------------------
// claude_cli subprocess lifecycle
// ---------------------------------------------------------------------------

/// Fires for `provider_type: claude_cli` only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaudeSubprocessSpawn {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    /// Stable UUID under `~/.claude/projects/-work/<sid>/`.
    pub session_id: String,
    /// Previous run leaked the lock; this spawn will collide.
    pub lock_present_at_spawn: bool,
}

/// Pairs with [`ClaudeSubprocessSpawn`] via `session_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaudeSubprocessExit {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub session_id: String,
    pub exit_code: i32,
    pub wallclock_ms: u64,
    /// `false` = leaked lock; next spawn collides.
    pub session_lock_released: bool,
}

/// Fired by spawn when it discovers a prior lock file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaudeSessionLockCollision {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    pub session_id: String,
    /// mtime delta on the lock file.
    pub prior_lock_age_secs: u64,
    /// `None` when claude doesn't write the PID into the lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_pid: Option<i32>,
}

// ---------------------------------------------------------------------------
// Event union
// ---------------------------------------------------------------------------

/// All telemetry event variants. The serde `type` tag makes the event
/// identifiable without subject parsing (the forwarder re-encodes
/// into its sink's schema).
///
/// HTTP error emitted by axum middleware on every 4xx/5xx response
/// from the agent's status server. Mirrors the orchestrator-side
/// `OrchestratorTelemetryEvent::ApiError` so dashboards can join on
/// `agent_id` (agent only) or `operator_principal` (orch only) to
/// attribute the failure.
///
/// `operator_principal` is intentionally absent — agent-side requests
/// are not authenticated as operators. The orch
/// catalog carries the principal field on its own `ApiError` variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiError {
    #[serde(flatten)]
    pub common: AgentEventCommon,
    /// HTTP status code returned to the client.
    pub http_status: u16,
    /// Stable error code emitted by the handler when one is available
    /// (e.g. `"job_not_found"`); `None` for raw 4xx/5xx with no
    /// programmer-supplied code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Path template the request matched (e.g. `"/api/operators/{name}"`).
    /// Falls back to the raw path when the framework can't supply a
    /// route template.
    pub endpoint: String,
    pub method: String,
    pub duration_ms: u64,
}

/// Adding a variant: also update [`TelemetryEvent::kind`] so the
/// subject-derivation table stays in sync. The unit test
/// `event_kind_covers_every_variant` guards this invariant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TelemetryEvent {
    LlmRequestStart(LlmRequestStart),
    LlmRequestComplete(LlmRequestComplete),
    LlmRequestFailed(LlmRequestFailed),
    LlmRequestStalled(LlmRequestStalled),
    ToolCallExecuted(ToolCallExecuted),
    RetryLoopAttempt(RetryLoopAttempt),
    TaskAccepted(TaskAccepted),
    TaskCompleted(TaskCompleted),
    TaskFailed(TaskFailed),
    #[serde(rename = "nats_connection_state")]
    NatsConnectionStateChanged(NatsConnectionStateChanged),
    PromptExposureDetected(PromptExposureDetected),
    ApiError(ApiError),
    ContextEmergencyShrink(ContextEmergencyShrink),
    ClaudeSubprocessSpawn(ClaudeSubprocessSpawn),
    ClaudeSubprocessExit(ClaudeSubprocessExit),
    ClaudeSessionLockCollision(ClaudeSessionLockCollision),
}

impl TelemetryEvent {
    /// snake_case discriminant used as the final subject segment.
    ///
    /// Matches the serde `type` tag value 1:1.
    pub fn kind(&self) -> &'static str {
        match self {
            TelemetryEvent::LlmRequestStart(_) => "llm_request_start",
            TelemetryEvent::LlmRequestComplete(_) => "llm_request_complete",
            TelemetryEvent::LlmRequestFailed(_) => "llm_request_failed",
            TelemetryEvent::LlmRequestStalled(_) => "llm_request_stalled",
            TelemetryEvent::ToolCallExecuted(_) => "tool_call_executed",
            TelemetryEvent::RetryLoopAttempt(_) => "retry_loop_attempt",
            TelemetryEvent::TaskAccepted(_) => "task_accepted",
            TelemetryEvent::TaskCompleted(_) => "task_completed",
            TelemetryEvent::TaskFailed(_) => "task_failed",
            TelemetryEvent::NatsConnectionStateChanged(_) => "nats_connection_state",
            TelemetryEvent::PromptExposureDetected(_) => "prompt_exposure_detected",
            TelemetryEvent::ApiError(_) => "api_error",
            TelemetryEvent::ContextEmergencyShrink(_) => "context_emergency_shrink",
            TelemetryEvent::ClaudeSubprocessSpawn(_) => "claude_subprocess_spawn",
            TelemetryEvent::ClaudeSubprocessExit(_) => "claude_subprocess_exit",
            TelemetryEvent::ClaudeSessionLockCollision(_) => "claude_session_lock_collision",
        }
    }

    /// Returns the `agent_id` carried by this event.
    pub fn agent_id(&self) -> &str {
        match self {
            TelemetryEvent::LlmRequestStart(e) => &e.common.agent_id,
            TelemetryEvent::LlmRequestComplete(e) => &e.common.agent_id,
            TelemetryEvent::LlmRequestFailed(e) => &e.common.agent_id,
            TelemetryEvent::LlmRequestStalled(e) => &e.common.agent_id,
            TelemetryEvent::ToolCallExecuted(e) => &e.common.agent_id,
            TelemetryEvent::RetryLoopAttempt(e) => &e.common.agent_id,
            TelemetryEvent::TaskAccepted(e) => &e.common.agent_id,
            TelemetryEvent::TaskCompleted(e) => &e.common.agent_id,
            TelemetryEvent::TaskFailed(e) => &e.common.agent_id,
            TelemetryEvent::NatsConnectionStateChanged(e) => &e.common.agent_id,
            TelemetryEvent::PromptExposureDetected(e) => &e.common.agent_id,
            TelemetryEvent::ApiError(e) => &e.common.agent_id,
            TelemetryEvent::ContextEmergencyShrink(e) => &e.common.agent_id,
            TelemetryEvent::ClaudeSubprocessSpawn(e) => &e.common.agent_id,
            TelemetryEvent::ClaudeSubprocessExit(e) => &e.common.agent_id,
            TelemetryEvent::ClaudeSessionLockCollision(e) => &e.common.agent_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Returns `true` when the event's `agent_id` matches the source's
/// identity. For `TelemetrySource::Agent` this is a direct string
/// comparison. Since `TelemetrySource` currently has only the `Agent`
/// variant, this always returns `true` — but the function exists so
/// adding `Orchestrator` or sub-tenant variants in the future forces
/// an exhaustiveness update here.
fn source_agent_matches(source: &TelemetrySource, event: &TelemetryEvent) -> bool {
    match source {
        TelemetrySource::Agent { agent_id: src_id } => event.agent_id() == *src_id,
    }
}

/// Fire-and-forget telemetry publisher.
///
/// Zero-await on the hot path: the NATS client's internal buffer
/// absorbs transient spikes. Serialization or publish failure is
/// **silently dropped** — telemetry must never gate critical-path
/// work. All failures are counted via atomics so tests + dashboards
/// can still observe drop rates.
///
/// Cloning is cheap: the inner [`async_nats::Client`] is already
/// `Clone` and the counter is an `Arc`.
///
/// `Debug` skips the NATS client (its impl is verbose and not useful
/// for our purposes); only the source identity and drop counter are
/// printed so an `AgentContext` Debug-format is concise.
#[derive(Clone)]
pub struct TelemetryEmitter {
    client: async_nats::Client,
    source: TelemetrySource,
    custom_prefix: Option<String>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl std::fmt::Debug for TelemetryEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryEmitter")
            .field("source", &self.source)
            .field("custom_prefix", &self.custom_prefix)
            .field("dropped", &self.dropped_count())
            .finish()
    }
}

impl TelemetryEmitter {
    pub fn new(client: async_nats::Client, source: TelemetrySource) -> Self {
        Self {
            client,
            source,
            custom_prefix: None,
            dropped: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Override the subject prefix (forwarder tenanted deployments).
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.custom_prefix = Some(prefix.into());
        self
    }

    /// Number of events dropped due to serialization or publish
    /// failure since this emitter was constructed. Exposed for
    /// tests and operator visibility.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Publish an event. Returns immediately; publish happens inside
    /// the `async_nats` client's outbound queue. The event is
    /// **silently dropped** (counter increments) on any of:
    ///
    /// - Subject derivation failure (invalid `agent_id` or
    ///   `custom_prefix`).
    /// - Serialization failure (programmer error — should not happen
    ///   for well-typed `TelemetryEvent` variants).
    /// - No active Tokio runtime — calling `emit()` from a thread
    ///   that has no current runtime increments `dropped` instead of
    ///   panicking. Telemetry must never crash the caller.
    /// - The async-nats client's `publish()` future returning `Err`.
    pub fn emit(&self, event: &TelemetryEvent) {
        // When emitting as an agent, the event's payload agent_id
        // must match the emitter's source identity.
        if !source_agent_matches(&self.source, event) {
            let src_id = match &self.source {
                TelemetrySource::Agent { agent_id } => agent_id.as_str(),
            };
            tracing::warn!(
                event_kind = event.kind(),
                emitter_agent_id = src_id,
                event_agent_id = event.agent_id(),
                "dropping telemetry event: payload agent_id does not match emitter"
            );
            self.dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }

        // Validate PromptExposureDetected structural invariants before
        // serialization. Invalid events are dropped with a counted drop.
        if let TelemetryEvent::PromptExposureDetected(detected) = event {
            if let Err(e) = detected.validate() {
                tracing::warn!(
                    error = %e,
                    "dropping invalid PromptExposureDetected event"
                );
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }

        let subject = match self
            .source
            .subject(event.kind(), self.custom_prefix.as_deref())
        {
            Ok(s) => s,
            Err(_) => {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        let payload = match serde_json::to_vec(event) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        // `try_publish` when available in the NATS client would be
        // strictly preferable (no await at all); `publish` returns a
        // future we intentionally fire-and-forget. Spawning onto the
        // current runtime is acceptable because the queue is bounded
        // and backpressure is absorbed in the NATS client's internal
        // buffer, not propagated to the caller.
        //
        // `tokio::spawn` panics outside an active runtime; guard with
        // `try_current()` so `emit()` from a non-Tokio thread degrades
        // to a counted drop rather than crashing the caller.
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        let client = self.client.clone();
        let dropped = self.dropped.clone();
        handle.spawn(async move {
            if client.publish(subject, payload.into()).await.is_err() {
                dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
}

/// Fan-out wrapper over multiple [`TelemetryEmitter`]s — one per
/// configured destination. Lets a single agent publish telemetry to
/// the service operator's NATS *and* the agent operator's own NATS
/// in parallel, without coupling the SDK to a single shared bus.
///
/// `emit()` fans an event to every endpoint (default operational
/// shape — same event, every dashboard); `emit_for(name, &event)`
/// targets one endpoint by name for cases where an event genuinely
/// belongs to one destination only.
///
/// Each endpoint independently absorbs publish failures via its
/// own `TelemetryEmitter::dropped_count`; the mux's
/// [`dropped_count`](Self::dropped_count) sums across them.
///
/// Single-endpoint legacy deployments construct a one-element mux
/// and call `emit()` — same wire shape as the prior single-emitter
/// path.
#[derive(Clone)]
pub struct TelemetryEmitterMux {
    endpoints: Vec<(String, TelemetryEmitter)>,
}

impl std::fmt::Debug for TelemetryEmitterMux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.endpoints.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("TelemetryEmitterMux")
            .field("endpoints", &names)
            .field("dropped", &self.dropped_count())
            .finish()
    }
}

/// Construction-time errors for [`TelemetryEmitterMux`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryMuxError {
    /// Two or more endpoints share a name. `emit_for(name)` would
    /// silently target only the first match — fail at construction
    /// instead so misconfigured registries surface immediately.
    /// The payload is the list of names that appeared more than
    /// once (deduplicated, in first-collision order).
    DuplicateNames(Vec<String>),
}

impl std::fmt::Display for TelemetryMuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateNames(names) => write!(
                f,
                "telemetry endpoint names must be unique; duplicates: {names:?}"
            ),
        }
    }
}

impl std::error::Error for TelemetryMuxError {}

/// Validate endpoint-name uniqueness without touching any emitters.
/// Extracted as a free function so unit tests can exercise the
/// duplicate-detection contract synchronously without a NATS client.
fn validate_endpoint_names(names: &[String]) -> Result<(), TelemetryMuxError> {
    let mut seen = std::collections::HashSet::with_capacity(names.len());
    let mut dups: Vec<String> = Vec::new();
    for n in names {
        if !seen.insert(n.as_str()) && !dups.iter().any(|d| d == n) {
            dups.push(n.clone());
        }
    }
    if dups.is_empty() {
        Ok(())
    } else {
        Err(TelemetryMuxError::DuplicateNames(dups))
    }
}

impl TelemetryEmitterMux {
    /// Construct a mux from `(name, emitter)` pairs. Returns
    /// [`TelemetryMuxError::DuplicateNames`] if any name appears more
    /// than once — `emit_for(name)` would otherwise silently resolve
    /// to the first match and drop fan-outs to the rest, which is a
    /// hard-to-debug misconfiguration.
    pub fn new(endpoints: Vec<(String, TelemetryEmitter)>) -> Result<Self, TelemetryMuxError> {
        let names: Vec<String> = endpoints.iter().map(|(n, _)| n.clone()).collect();
        validate_endpoint_names(&names)?;
        Ok(Self { endpoints })
    }

    /// Wrap a single emitter as a one-element mux. Convenience for
    /// callers that built one emitter and need the mux-shaped
    /// interface — name uniqueness is trivially satisfied so this
    /// stays infallible.
    pub fn single(name: impl Into<String>, emitter: TelemetryEmitter) -> Self {
        Self {
            endpoints: vec![(name.into(), emitter)],
        }
    }

    /// Number of configured endpoints.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// `true` when no endpoints are configured. The mux still
    /// implements [`emit`](Self::emit) (no-op) so callers don't need
    /// an `Option<TelemetryEmitterMux>` wrapper at every site.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Endpoint names in declaration order. For dashboards that want
    /// to enumerate destinations.
    pub fn endpoint_names(&self) -> Vec<&str> {
        self.endpoints.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// Sum of dropped counts across endpoints.
    pub fn dropped_count(&self) -> u64 {
        self.endpoints.iter().map(|(_, e)| e.dropped_count()).sum()
    }

    /// Fire-and-forget fan-out: emit `event` to every configured
    /// endpoint. No-op when the mux is empty.
    pub fn emit(&self, event: &TelemetryEvent) {
        for (_, emitter) in &self.endpoints {
            emitter.emit(event);
        }
    }

    /// Fire-and-forget targeted emission. Drops with no counter
    /// increment when the named endpoint isn't configured —
    /// `emit_for` is a hint, not a contract; the schema can't tell
    /// at compile time which endpoints an operator deployed. The
    /// miss path logs at `debug` level with the requested name and
    /// the configured endpoint names so operators can spot
    /// targeting typos without paying for an error path.
    pub fn emit_for(&self, name: &str, event: &TelemetryEvent) {
        if let Some((_, emitter)) = self.endpoints.iter().find(|(n, _)| n == name) {
            emitter.emit(event);
        } else {
            tracing::debug!(
                requested = %name,
                available = ?self.endpoint_names(),
                "telemetry emit_for: endpoint name not configured, dropping event"
            );
        }
    }
}

/// Errors from [`connect_endpoints`].
#[derive(Debug, thiserror::Error)]
pub enum TelemetryConnectError {
    /// `agent_id` failed [`TelemetrySource::agent`] validation.
    #[error("invalid agent_id: {0}")]
    InvalidAgentId(String),
    /// Endpoint config has `nats_url: None`. The schema field is
    /// optional but every endpoint must declare a URL — `None` would
    /// otherwise mean "reuse some other connection" which we don't
    /// model.
    #[error("telemetry endpoint `{name}` missing nats_url")]
    MissingNatsUrl {
        /// Configured endpoint name.
        name: String,
    },
    /// NATS connect failed for the endpoint.
    #[error("connect to NATS for telemetry endpoint `{name}` failed: {source}")]
    NatsConnect {
        /// Configured endpoint name.
        name: String,
        /// Underlying connect error from [`connect_nats`].
        #[source]
        source: anyhow::Error,
    },
    /// Mux construction rejected the endpoint list (duplicate names).
    #[error("mux construction: {0}")]
    Mux(#[from] TelemetryMuxError),
}

/// Build a [`TelemetryEmitterMux`] from a [`TelemetryConfig`].
///
/// Returns `Ok(None)` when telemetry is disabled or `endpoints` is
/// empty — the worker treats that as "no emission" and skips the
/// emit sites entirely. Returns `Ok(Some(mux))` with one connected
/// emitter per endpoint when both are populated.
///
/// Each endpoint gets its own NATS client + credentials. Endpoints
/// that point at the same URL still get independent connections —
/// `async-nats` doesn't pool by address, and the simplification of
/// keeping each endpoint self-contained at this layer outweighs the
/// extra socket. Operators who want connection sharing collapse the
/// duplicate endpoints into one entry in the YAML.
pub async fn connect_endpoints(
    config: &TelemetryConfig,
    agent_id: &str,
) -> Result<Option<TelemetryEmitterMux>, TelemetryConnectError> {
    if !config.enabled || config.endpoints.is_empty() {
        return Ok(None);
    }
    let source = TelemetrySource::agent(agent_id)
        .map_err(|_| TelemetryConnectError::InvalidAgentId(agent_id.to_string()))?;

    let mut emitters = Vec::with_capacity(config.endpoints.len());
    for ep in &config.endpoints {
        let url = ep
            .nats_url
            .as_deref()
            .ok_or_else(|| TelemetryConnectError::MissingNatsUrl {
                name: ep.name.clone(),
            })?;
        let auth = ep.creds.as_ref().map(|path| crate::nats_utils::NatsAuth {
            creds_file: Some(path.clone()),
            ..Default::default()
        });
        let client = crate::nats_utils::connect_nats(url, auth.as_ref())
            .await
            .map_err(|e| TelemetryConnectError::NatsConnect {
                name: ep.name.clone(),
                source: e,
            })?;
        let mut emitter = TelemetryEmitter::new(client, source.clone());
        if let Some(prefix) = &ep.subject_prefix {
            emitter = emitter.with_prefix(prefix);
        }
        emitters.push((ep.name.clone(), emitter));
    }
    Ok(Some(TelemetryEmitterMux::new(emitters)?))
}

// ---------------------------------------------------------------------------
// Privacy redaction helpers
// ---------------------------------------------------------------------------

/// Redact sensitive content for telemetry.
///
/// Returns a redacted version of the input string suitable for telemetry.
/// For privacy reasons, we never emit full error messages, prompts, or content.
///
/// # Arguments
///
/// * `input` - The input string to redact
/// * `max_length` - Maximum length to keep
///
/// # Returns
///
/// A redacted string, or `"REDACTED_SENSITIVE"` if the input contains
/// sensitive patterns.
pub fn redact_content(input: &str, max_length: usize) -> String {
    if input.is_empty() {
        return "<empty>".to_string();
    }

    // Check for obviously sensitive patterns using word-boundary
    // matching to avoid false positives on "keyword", "author",
    // "monkey", "public_authority", etc.
    let lower = input.to_lowercase();
    let sensitive_words = [
        "password",
        "api_key",
        "secret_key",
        "access_key",
        "credential",
        "bearer",
    ];
    for word in &sensitive_words {
        // Simple word-boundary: look for the word preceded/followed by
        // a non-alphanumeric character (or start/end of string).
        let mut search = &lower[..];
        while let Some(pos) = search.find(word) {
            let before_ok = pos == 0 || !search.as_bytes()[pos - 1].is_ascii_alphanumeric();
            let after_pos = pos + word.len();
            let after_ok =
                after_pos >= search.len() || !search.as_bytes()[after_pos].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return "REDACTED_SENSITIVE".to_string();
            }
            search = &search[pos + 1..];
        }
    }

    // Truncate to max_length chars (not bytes) to avoid panicking on multi-byte UTF-8
    if input.chars().count() <= max_length {
        input.to_string()
    } else {
        let truncate_at = input
            .char_indices()
            .nth(max_length)
            .map(|(i, _)| i)
            .unwrap_or(input.len());
        format!("{}...", &input[..truncate_at])
    }
}

/// Redact error message for telemetry (max 100 chars).
pub fn redact_error_message(error_msg: &str) -> String {
    redact_content(error_msg, 100)
}

/// Redact URL for telemetry — strips query params, fragments, and
/// embedded credentials (`user:pass@host`) from the authority section
/// only. Paths containing `@` (e.g. `users/foo@bar/profile`) are
/// preserved intact.
pub fn redact_url(url: &str) -> String {
    // Remove query parameters and fragments; split always yields >= 1 element.
    let base = url.split(['?', '#']).next().unwrap();
    // Only strip credentials from the authority section (between :// and
    // the next /). This avoids mangling paths like /users/foo@bar.
    if let Some(scheme_end) = base.find("://") {
        let scheme = &base[..scheme_end + 3];
        let after_scheme = &base[scheme_end + 3..];
        if let Some(at_pos) = after_scheme.find('@') {
            // Only treat @ as credential separator if there's no / before it
            // in the authority section.
            let slash_pos = after_scheme.find('/');
            if slash_pos.is_none_or(|s| s > at_pos) {
                return format!("{scheme}{}", &after_scheme[at_pos + 1..]);
            }
        }
    }
    base.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_agent_common() -> AgentEventCommon {
        AgentEventCommon {
            agent_id: "CortexB".to_string(),
            job_id: Some("job-123".to_string()),
            round: Some(3),
            phase: Some(DeliberationPhase::Proposing),
            ts: 1_776_790_692_747,
            trace_id: derive_trace_id("job-123", 3, DeliberationPhase::Proposing, "CortexB"),
        }
    }

    /// Guard the bit-width of `trace_id` against future narrowing.
    /// 64 bits makes birthday collisions plausible at telemetry scale.
    /// Compile-time assertion fires independent of whether this test
    /// runs; the runtime check observes the produced hex length so a
    /// byte-loop bug that silently truncates would still surface.
    #[test]
    fn trace_id_width_is_at_least_128_bits() {
        const _: () = assert!(
            TRACE_ID_LEN >= 32,
            "trace_id must carry at least 128 bits (32 hex chars)"
        );
        let out = derive_trace_id("job-x", 1, DeliberationPhase::Proposing, "a");
        assert_eq!(out.len(), TRACE_ID_LEN);
    }

    /// `TelemetryContext::new` produces a `trace_id` of the same
    /// `TRACE_ID_LEN` lowercase-hex shape regardless of whether the
    /// task tuple is present or the session-less branch fires. Locks
    /// the cross-catalog uniformity that consumers rely on when
    /// parsing `trace_id` without first inspecting the variant.
    #[test]
    fn telemetry_context_trace_id_shape_is_uniform() {
        let task_ctx = TelemetryContext::new(
            "alice",
            Some("job-1"),
            Some(2),
            Some(DeliberationPhase::Proposing),
        );
        let task_trace = task_ctx.common().trace_id;
        assert_eq!(task_trace.len(), TRACE_ID_LEN);
        assert!(task_trace.chars().all(|c| c.is_ascii_hexdigit()));

        let sessionless = TelemetryContext::new("alice", None, None, None);
        let sl_trace = sessionless.common().trace_id;
        assert_eq!(sl_trace.len(), TRACE_ID_LEN);
        assert!(sl_trace.chars().all(|c| c.is_ascii_hexdigit()));

        // Two session-less constructions must not alias on the same
        // trace_id (UUIDv4 entropy preserved through the digest).
        let sl2 = TelemetryContext::new("alice", None, None, None);
        assert_ne!(sl_trace, sl2.common().trace_id);
    }

    // -----------------------------------------------------------------
    // TelemetryEmitterMux — multi-endpoint fan-out
    // -----------------------------------------------------------------

    #[test]
    fn empty_mux_is_empty_and_no_op_safe() {
        let mux = TelemetryEmitterMux::new(vec![]).expect("empty mux is valid");
        assert!(mux.is_empty());
        assert_eq!(mux.len(), 0);
        assert_eq!(mux.dropped_count(), 0);
        assert!(mux.endpoint_names().is_empty());
        // emit on an empty mux must not panic — operators may
        // construct one transiently before adding endpoints.
        let evt = TelemetryEvent::TaskAccepted(TaskAccepted {
            common: sample_agent_common(),
            dispatch_delay_ms: 0,
            task_publish_ts: None,
            job_age_at_accept_ms: None,
        });
        mux.emit(&evt);
        mux.emit_for("does-not-exist", &evt);
    }

    #[test]
    fn telemetry_endpoint_config_serde_roundtrip() {
        let cfg = TelemetryConfig {
            enabled: true,
            endpoints: vec![
                TelemetryEndpointConfig {
                    name: "service".into(),
                    nats_url: Some("nats://orch.example.com:4222".into()),
                    creds: Some("/etc/nsed/agent-service.creds".into()),
                    subject_prefix: None,
                },
                TelemetryEndpointConfig {
                    name: "own".into(),
                    nats_url: Some("nats://my-grafana.local:4222".into()),
                    creds: Some("/etc/nsed/agent-own.creds".into()),
                    subject_prefix: Some("telemetry.agent".into()),
                },
            ],
        };
        let json = serde_json::to_string(&cfg).expect("serialise");
        let back: TelemetryConfig = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, cfg);
    }

    #[test]
    fn telemetry_config_endpoints_omitted_defaults_empty() {
        // YAML without an `endpoints:` block deserialises with an
        // empty endpoints list. Construction of the runtime mux
        // happens at config-load and validates non-empty when
        // `enabled` is true.
        let yaml = "enabled: true\n";
        let cfg: TelemetryConfig = serde_yaml::from_str(yaml).expect("yaml parse");
        assert!(cfg.enabled);
        assert!(cfg.endpoints.is_empty());
    }

    #[test]
    fn validate_endpoint_names_rejects_duplicates() {
        // Validator runs before any emitter touch — exercising it
        // directly keeps the unit test sync (no NATS) while still
        // covering the duplicate-detection contract that
        // `TelemetryEmitterMux::new` enforces. Real-emitter coverage
        // lives in `tests/nats_integration/multi_endpoint_emit.rs`.
        let err = validate_endpoint_names(&[
            "dup".to_string(),
            "solo".to_string(),
            "dup".to_string(),
            "another".to_string(),
            "solo".to_string(),
        ])
        .expect_err("must reject duplicates");
        match err {
            TelemetryMuxError::DuplicateNames(mut names) => {
                names.sort();
                assert_eq!(names, vec!["dup".to_string(), "solo".to_string()]);
            }
        }
    }

    #[test]
    fn validate_endpoint_names_accepts_unique() {
        validate_endpoint_names(&["a".to_string(), "b".to_string(), "c".to_string()])
            .expect("unique names accepted");
    }

    /// `connect_endpoints` returns `Ok(None)` when telemetry is
    /// disabled — no NATS round-trip, no error.
    #[tokio::test]
    async fn connect_endpoints_disabled_yields_none() {
        let cfg = TelemetryConfig {
            enabled: false,
            endpoints: vec![TelemetryEndpointConfig {
                name: "wont-connect".into(),
                nats_url: Some("nats://does-not-resolve.invalid:4222".into()),
                creds: None,
                subject_prefix: None,
            }],
        };
        let mux = connect_endpoints(&cfg, "agent-x")
            .await
            .expect("disabled is not an error");
        assert!(mux.is_none());
    }

    /// `connect_endpoints` returns `Ok(None)` when the endpoint list
    /// is empty — same fall-through semantics as disabled.
    #[tokio::test]
    async fn connect_endpoints_empty_endpoints_yields_none() {
        let cfg = TelemetryConfig {
            enabled: true,
            endpoints: vec![],
        };
        let mux = connect_endpoints(&cfg, "agent-x")
            .await
            .expect("empty endpoints is not an error");
        assert!(mux.is_none());
    }

    /// Endpoint missing `nats_url` fails fast at the factory before
    /// attempting any connect — catches typos in YAML where the field
    /// was omitted.
    #[tokio::test]
    async fn connect_endpoints_missing_nats_url_errors() {
        let cfg = TelemetryConfig {
            enabled: true,
            endpoints: vec![TelemetryEndpointConfig {
                name: "no-url".into(),
                nats_url: None,
                creds: None,
                subject_prefix: None,
            }],
        };
        let err = connect_endpoints(&cfg, "agent-x")
            .await
            .expect_err("missing url must error");
        match err {
            TelemetryConnectError::MissingNatsUrl { name } => assert_eq!(name, "no-url"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Invalid `agent_id` (e.g. contains forbidden NATS subject
    /// characters) fails before any NATS call.
    #[tokio::test]
    async fn connect_endpoints_invalid_agent_id_errors() {
        let cfg = TelemetryConfig {
            enabled: true,
            endpoints: vec![TelemetryEndpointConfig {
                name: "real".into(),
                nats_url: Some("nats://ignored.invalid:4222".into()),
                creds: None,
                subject_prefix: None,
            }],
        };
        let err = connect_endpoints(&cfg, "bad agent_id with space")
            .await
            .expect_err("invalid agent_id must error");
        assert!(matches!(err, TelemetryConnectError::InvalidAgentId(_)));
    }

    // Happy-path connect (real NATS, asserts emitters publish to
    // both endpoints) lives in
    // `tests/nats_integration/multi_endpoint_emit.rs` alongside the
    // existing fan-out coverage.

    // Fan-out behaviour against a real NATS connection
    // (`mux.emit` reaches every endpoint, `mux.emit_for` only one,
    // and `dropped_count` aggregates) lives in
    // `tests/nats_integration/multi_endpoint_emit.rs` — that's where
    // we have the runtime + mock subscribers to verify routing.

    #[test]
    fn trace_id_is_deterministic_and_correct_length() {
        let a = derive_trace_id("job-x", 1, DeliberationPhase::Evaluating, "alpha");
        let b = derive_trace_id("job-x", 1, DeliberationPhase::Evaluating, "alpha");
        assert_eq!(a, b, "same inputs must produce the same trace_id");
        assert_eq!(a.len(), TRACE_ID_LEN);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn trace_id_differs_across_any_input() {
        let base = derive_trace_id("j", 1, DeliberationPhase::Proposing, "a");
        assert_ne!(
            base,
            derive_trace_id("j2", 1, DeliberationPhase::Proposing, "a")
        );
        assert_ne!(
            base,
            derive_trace_id("j", 2, DeliberationPhase::Proposing, "a")
        );
        assert_ne!(
            base,
            derive_trace_id("j", 1, DeliberationPhase::Evaluating, "a")
        );
        assert_ne!(
            base,
            derive_trace_id("j", 1, DeliberationPhase::Proposing, "b")
        );
    }

    /// Length-prefixed encoding must keep the (job_id, agent_id)
    /// boundary unambiguous even when the identifiers contain the
    /// internal delimiter characters or each other's content.
    /// Without length prefixes, naive concatenation
    /// `{job}|{round}|{phase}|{agent}` could collide e.g. for
    /// `job_id="ab", agent_id="c"` vs `job_id="a", agent_id="bc"`
    /// (both yield `"ab|...|c"` if the delimiter ever leaks).
    #[test]
    fn trace_id_resists_delimiter_collision_attacks() {
        // Pair 1: same total bytes spanning the (job_id, agent_id)
        // boundary, partitioned differently.
        let a = derive_trace_id("ab", 1, DeliberationPhase::Proposing, "c");
        let b = derive_trace_id("a", 1, DeliberationPhase::Proposing, "bc");
        assert_ne!(a, b, "boundary must be unambiguous");

        // Pair 2: an id that embeds the delimiter character used by
        // the encoder (`':'` and `'|'`). Length-prefixed encoding
        // keeps these distinct from any unprefixed form.
        let c = derive_trace_id("job:1|x", 1, DeliberationPhase::Proposing, "agent");
        let d = derive_trace_id("job", 1, DeliberationPhase::Proposing, "1|x:agent");
        assert_ne!(c, d, "embedded delimiter must not produce collision");
    }

    #[test]
    fn agent_subject_binds_agent_id_position() {
        let evt = TelemetryEvent::TaskAccepted(TaskAccepted {
            common: sample_agent_common(),
            dispatch_delay_ms: 42,
            task_publish_ts: None,
            job_age_at_accept_ms: None,
        });
        let src = TelemetrySource::agent("CortexB").unwrap();
        assert_eq!(
            src.subject(evt.kind(), None).unwrap(),
            "telemetry.agent.CortexB.task_accepted"
        );
    }

    /// Defence-in-depth: invalid agent ids must never produce a NATS
    /// subject. NATS forbidden chars (`.` `*` `>`), whitespace, and
    /// empty strings would silently reshape the subject hierarchy
    /// and break the JWT-bound `agent_id` position contract.
    #[test]
    fn agent_constructor_rejects_invalid_agent_ids() {
        for bad in [
            "evil.injection",
            "with*wildcard",
            "with>wildcard",
            "with whitespace",
            "with\nnewline",
            "",
        ] {
            assert!(
                TelemetrySource::agent(bad).is_err(),
                "agent({bad:?}) should be rejected"
            );
        }
    }

    #[test]
    fn agent_subject_rejects_invalid_agent_id_at_subject_time() {
        let evt = TelemetryEvent::TaskAccepted(TaskAccepted {
            common: sample_agent_common(),
            dispatch_delay_ms: 0,
            task_publish_ts: None,
            job_age_at_accept_ms: None,
        });
        let bad = TelemetrySource::Agent {
            agent_id: "evil.injection".into(),
        };
        assert!(bad.subject(evt.kind(), None).is_err());
    }

    /// Documents the contract that `TelemetryEmitter::emit` relies
    /// on: `tokio::runtime::Handle::try_current()` returns `Err`
    /// from a thread that has no active runtime, which lets the
    /// emitter degrade to a counted drop instead of panicking.
    #[test]
    fn tokio_runtime_handle_try_current_is_err_off_runtime() {
        let handle = std::thread::spawn(|| tokio::runtime::Handle::try_current().is_err())
            .join()
            .unwrap();
        assert!(
            handle,
            "off-runtime threads must report Err so emit() can degrade to a drop"
        );
    }

    #[test]
    fn custom_prefix_validates_each_dot_segment() {
        let src = TelemetrySource::agent("CortexB").unwrap();
        assert!(
            src.subject("task_accepted", Some("tenant.op42.agent"))
                .is_ok()
        );
        assert!(
            src.subject("task_accepted", Some("tenant.op*42.agent"))
                .is_err()
        );
        assert!(
            src.subject("task_accepted", Some("tenant. .agent"))
                .is_err()
        );
    }

    // -----------------------------------------------------------------
    // PromptExposureDetected
    // -----------------------------------------------------------------

    fn sample_prompt_exposure() -> PromptExposureDetected {
        PromptExposureDetected {
            common: sample_agent_common(),
            terminal_tool: "submit_proposal".into(),
            blocked: true,
            hit_count: 3,
            response_length_chars: 1_482,
            suspicion_score: 4.76,
            xml_tag_hits: 2,
            tool_name_hits: 1,
            instruction_hits: 0,
            wrong_acronym_hits: 0,
            sample_hits: vec![
                "xml-tag <working_memory>".into(),
                "xml-tag <key_findings>".into(),
                "tool-name submit_proposal".into(),
            ],
        }
    }

    #[test]
    fn roundtrip_prompt_exposure_detected() {
        let evt = TelemetryEvent::PromptExposureDetected(sample_prompt_exposure());
        let json = serde_json::to_string(&evt).unwrap();
        let back: TelemetryEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
        assert!(json.contains("\"type\":\"prompt_exposure_detected\""));
    }

    /// The variant must be reachable from `TelemetryEvent::kind()` with the
    /// stable `prompt_exposure_detected` tag. The existing
    /// `event_kind_covers_every_variant` test enforces the exhaustive
    /// invariant; this one pins the exact string the operator-facing
    /// subject hierarchy relies on.
    #[test]
    fn prompt_exposure_kind_is_stable() {
        let evt = TelemetryEvent::PromptExposureDetected(sample_prompt_exposure());
        assert_eq!(evt.kind(), "prompt_exposure_detected");
        let src = TelemetrySource::agent("CortexB").unwrap();
        assert_eq!(
            src.subject(evt.kind(), None).unwrap(),
            "telemetry.agent.CortexB.prompt_exposure_detected"
        );
    }

    /// Category counts must sum to `hit_count`. Operators reason about the
    /// total across the breakdown — an invariant broken by an off-by-one in
    /// the detector would silently skew dashboards. Assert at the type
    /// layer so a future builder that drifts fails a test instead of
    /// production.
    #[test]
    fn prompt_exposure_category_counts_sum_to_hit_count() {
        let evt = sample_prompt_exposure();
        let sum =
            evt.xml_tag_hits + evt.tool_name_hits + evt.instruction_hits + evt.wrong_acronym_hits;
        assert_eq!(sum, evt.hit_count);
    }

    /// `sample_hits` only carries dictionary-sourced labels (tag names,
    /// tool names, instruction phrases, acronyms the guardrail ships with).
    /// None of these are free-form user content. This test locks the
    /// expected prefix alphabet so a future detector that accidentally
    /// leaks proposal content into the sample array fails CI.
    #[test]
    fn prompt_exposure_sample_hits_only_dictionary_prefixes() {
        let evt = sample_prompt_exposure();
        for hit in &evt.sample_hits {
            let ok = hit.starts_with("xml-tag ")
                || hit.starts_with("tool-name ")
                || hit.starts_with("instruction ")
                || hit.starts_with("wrong-acronym ");
            assert!(
                ok,
                "sample_hits entry {hit:?} does not start with a known dictionary prefix"
            );
        }
    }

    /// `blocked=false` is a valid state: a detection can be *observed*
    /// (hits > 0) but fall under the `min_suspicion_score` threshold, so
    /// the guardrail lets the response through. Dashboards rely on this
    /// to compute false-positive rates, so the telemetry event must
    /// round-trip both states.
    #[test]
    fn prompt_exposure_below_threshold_still_roundtrips() {
        let evt = TelemetryEvent::PromptExposureDetected(PromptExposureDetected {
            blocked: false,
            hit_count: 1,
            xml_tag_hits: 1,
            tool_name_hits: 0,
            instruction_hits: 0,
            wrong_acronym_hits: 0,
            suspicion_score: 0.12,
            sample_hits: vec!["xml-tag <strategy>".into()],
            ..sample_prompt_exposure()
        });
        let json = serde_json::to_string(&evt).unwrap();
        let back: TelemetryEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn roundtrip_task_completed_with_g1_g6_fields() {
        let evt = TelemetryEvent::TaskCompleted(TaskCompleted {
            common: sample_agent_common(),
            duration_ms: 12_000,
            dispatch_delay_ms: 40,
            queue_wait_ms: Some(5),
            phase_budget_remaining_ms: 3_000,
            llm_attempts: Some(2),
            tool_call_count: Some(1),
            pending_publish_depth: Some(0),
        });
        let json = serde_json::to_string(&evt).unwrap();
        let back: TelemetryEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
        // Type tag present + snake_case kind.
        assert!(json.contains("\"type\":\"task_completed\""));
    }

    #[test]
    fn roundtrip_llm_request_complete_with_ttft_and_evaluator_counters() {
        let evt = TelemetryEvent::LlmRequestComplete(LlmRequestComplete {
            common: AgentEventCommon {
                phase: Some(DeliberationPhase::Evaluating),
                ..sample_agent_common()
            },
            request_id: "req-1".into(),
            latency_ms: 4_200,
            ttft_ms: Some(180),
            generation_ms: Some(4_020),
            input_tokens: 1_200,
            output_tokens: 350,
            reasoning_tokens: 120,
            cached_tokens: 0,
            cost_usd: 0.0041,
            finish_reason: FinishReason::Stop,
            provider_backend: Some("openrouter/deepinfra".into()),
            claim_assessments_emitted: Some(12),
            disagreements_emitted: Some(2),
            messages_chars: 4_800,
            max_tokens_requested: Some(2_000),
            response_chars: 1_400,
            tool_calls_emitted: 0,
            max_tokens_shrunk_to_floor: false,
            available_space_at_dispatch: None,
        });
        let json = serde_json::to_string(&evt).unwrap();
        let back: TelemetryEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn roundtrip_retry_loop_attempt_cost_fields() {
        let evt = TelemetryEvent::RetryLoopAttempt(RetryLoopAttempt {
            common: sample_agent_common(),
            attempt: 3,
            reason: RetryReason::SchemaError,
            cumulative_latency_ms: 18_400,
            cumulative_cost_usd: 0.0127,
            cumulative_input_tokens: 3_200,
            cumulative_output_tokens: 900,
        });
        let json = serde_json::to_string(&evt).unwrap();
        let back: TelemetryEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn event_kind_covers_every_variant() {
        // The `kind()` method is the ground truth for subject
        // derivation. Every variant must return a non-empty, stable,
        // lower-snake-case identifier.
        let samples: Vec<TelemetryEvent> = vec![
            TelemetryEvent::LlmRequestStart(LlmRequestStart {
                common: sample_agent_common(),
                request_id: "r".into(),
                model: "m".into(),
                provider_id: "p".into(),
                attempt: 1,
                estimated_input_tokens: 0,
                context_utilization_pct: 0.0,
                recent_tool_output_bytes: 0,
            }),
            TelemetryEvent::LlmRequestComplete(LlmRequestComplete {
                common: sample_agent_common(),
                request_id: "r".into(),
                latency_ms: 0,
                ttft_ms: None,
                generation_ms: None,
                input_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                cached_tokens: 0,
                cost_usd: 0.0,
                finish_reason: FinishReason::Stop,
                provider_backend: None,
                claim_assessments_emitted: None,
                disagreements_emitted: None,
                messages_chars: 0,
                max_tokens_requested: None,
                response_chars: 0,
                tool_calls_emitted: 0,
                max_tokens_shrunk_to_floor: false,
                available_space_at_dispatch: None,
            }),
            TelemetryEvent::LlmRequestFailed(LlmRequestFailed {
                common: sample_agent_common(),
                request_id: "r".into(),
                error_class: LlmErrorClass::Transport,
                http_status: None,
                retry_after_ms: None,
                latency_ms: 0,
                provider_id: "p".into(),
                provider_backend: None,
            }),
            TelemetryEvent::LlmRequestStalled(LlmRequestStalled {
                common: sample_agent_common(),
                request_id: "r".into(),
                elapsed_ms: 0,
                ttft_received: false,
                last_token_ms: None,
            }),
            TelemetryEvent::ToolCallExecuted(ToolCallExecuted {
                common: sample_agent_common(),
                tool_name: "scratchpad".into(),
                latency_ms: 0,
                success: true,
                output_bytes: 0,
                output_tokens_estimated: None,
                truncated: false,
                paginated: false,
            }),
            TelemetryEvent::RetryLoopAttempt(RetryLoopAttempt {
                common: sample_agent_common(),
                attempt: 1,
                reason: RetryReason::EmptyContent,
                cumulative_latency_ms: 0,
                cumulative_cost_usd: 0.0,
                cumulative_input_tokens: 0,
                cumulative_output_tokens: 0,
            }),
            TelemetryEvent::TaskAccepted(TaskAccepted {
                common: sample_agent_common(),
                dispatch_delay_ms: 0,
                task_publish_ts: None,
                job_age_at_accept_ms: None,
            }),
            TelemetryEvent::TaskCompleted(TaskCompleted {
                common: sample_agent_common(),
                duration_ms: 0,
                dispatch_delay_ms: 0,
                queue_wait_ms: Some(0),
                phase_budget_remaining_ms: 0,
                llm_attempts: Some(0),
                tool_call_count: Some(0),
                pending_publish_depth: Some(0),
            }),
            TelemetryEvent::TaskFailed(TaskFailed {
                common: sample_agent_common(),
                duration_ms: 0,
                dispatch_delay_ms: 0,
                queue_wait_ms: Some(0),
                phase_budget_remaining_ms: 0,
                llm_attempts: Some(0),
                tool_call_count: Some(0),
                failure_class: TaskFailureClass::Timeout,
                pending_publish_depth: Some(0),
            }),
            TelemetryEvent::NatsConnectionStateChanged(NatsConnectionStateChanged {
                common: sample_agent_common(),
                state: NatsConnectionState::Connected,
                reconnects_so_far: 0,
                pending_publish_depth: Some(0),
                buffer_bytes: Some(0),
            }),
            TelemetryEvent::PromptExposureDetected(PromptExposureDetected {
                common: sample_agent_common(),
                terminal_tool: "submit_proposal".into(),
                blocked: true,
                hit_count: 0,
                response_length_chars: 0,
                suspicion_score: 0.0,
                xml_tag_hits: 0,
                tool_name_hits: 0,
                instruction_hits: 0,
                wrong_acronym_hits: 0,
                sample_hits: vec![],
            }),
            TelemetryEvent::ApiError(ApiError {
                common: sample_agent_common(),
                http_status: 404,
                error_code: Some("not_found".into()),
                endpoint: "/health/{name}".into(),
                method: "GET".into(),
                duration_ms: 5,
            }),
            TelemetryEvent::ContextEmergencyShrink(ContextEmergencyShrink {
                common: sample_agent_common(),
                available_space: 100,
                requested_max: 4_000,
                floor_used: 200,
                estimated_input: 130_000,
                context_window: 131_072,
                recent_tool_outputs: vec![RecentToolOutput {
                    tool: "read_file".into(),
                    bytes: 240_000,
                }],
            }),
            TelemetryEvent::ClaudeSubprocessSpawn(ClaudeSubprocessSpawn {
                common: sample_agent_common(),
                session_id: "8ce6aa3f-d7c2-0000-0000-000000000000".into(),
                lock_present_at_spawn: false,
            }),
            TelemetryEvent::ClaudeSubprocessExit(ClaudeSubprocessExit {
                common: sample_agent_common(),
                session_id: "8ce6aa3f-d7c2-0000-0000-000000000000".into(),
                exit_code: 0,
                wallclock_ms: 12_345,
                session_lock_released: true,
            }),
            TelemetryEvent::ClaudeSessionLockCollision(ClaudeSessionLockCollision {
                common: sample_agent_common(),
                session_id: "8ce6aa3f-d7c2-0000-0000-000000000000".into(),
                prior_lock_age_secs: 42,
                prior_pid: Some(31415),
            }),
        ];
        // Sanity: every sample produces the snake_case the serde tag
        // uses. We verify by round-tripping JSON and reading the
        // `type` field.
        for evt in &samples {
            let kind = evt.kind();
            assert!(!kind.is_empty());
            assert!(kind.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
            let v: serde_json::Value = serde_json::to_value(evt).unwrap();
            assert_eq!(
                v["type"].as_str(),
                Some(kind),
                "kind() must match serde tag"
            );
        }
        // Every variant is distinct by discriminant.
        let mut kinds: Vec<&'static str> = samples.iter().map(|e| e.kind()).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), samples.len(), "duplicate kind() values");
    }

    #[test]
    fn telemetry_config_defaults_enabled_true() {
        let cfg: TelemetryConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.enabled);
        assert!(cfg.endpoints.is_empty());
    }

    #[test]
    fn telemetry_config_opt_out() {
        let cfg: TelemetryConfig = serde_yaml::from_str("enabled: false\n").unwrap();
        assert!(!cfg.enabled);
    }

    // -----------------------------------------------------------------------
    // PromptExposureDetected validation
    // -----------------------------------------------------------------------

    #[test]
    fn prompt_exposure_validate_ok() {
        let det = PromptExposureDetected {
            common: sample_agent_common(),
            terminal_tool: "submit_proposal".into(),
            blocked: true,
            hit_count: 5,
            response_length_chars: 1200,
            suspicion_score: 3.45,
            xml_tag_hits: 2,
            tool_name_hits: 1,
            instruction_hits: 1,
            wrong_acronym_hits: 1,
            sample_hits: vec!["xml-tag <working_memory>".into()],
        };
        assert!(det.validate().is_ok());
    }

    #[test]
    fn prompt_exposure_validate_hit_count_mismatch() {
        let det = PromptExposureDetected {
            common: sample_agent_common(),
            terminal_tool: "submit_proposal".into(),
            blocked: false,
            hit_count: 99, // mismatch — sum is 4
            response_length_chars: 500,
            suspicion_score: 2.0,
            xml_tag_hits: 1,
            tool_name_hits: 1,
            instruction_hits: 1,
            wrong_acronym_hits: 1,
            sample_hits: vec![],
        };
        let err = det.validate().unwrap_err();
        assert!(err.contains("hit_count 99 != sum"));
    }

    #[test]
    fn prompt_exposure_validate_sample_hit_too_long() {
        let det = PromptExposureDetected {
            common: sample_agent_common(),
            terminal_tool: "submit_proposal".into(),
            blocked: false,
            hit_count: 1,
            response_length_chars: 100,
            suspicion_score: 1.0,
            xml_tag_hits: 1,
            tool_name_hits: 0,
            instruction_hits: 0,
            wrong_acronym_hits: 0,
            sample_hits: vec!["a".repeat(65)],
        };
        let err = det.validate().unwrap_err();
        assert!(err.contains("exceeds 64 chars"));
    }

    #[test]
    fn prompt_exposure_validate_sample_hit_unknown_prefix() {
        let det = PromptExposureDetected {
            common: sample_agent_common(),
            terminal_tool: "submit_proposal".into(),
            blocked: false,
            hit_count: 1,
            response_length_chars: 100,
            suspicion_score: 1.0,
            xml_tag_hits: 1,
            tool_name_hits: 0,
            instruction_hits: 0,
            wrong_acronym_hits: 0,
            // Looks like raw content, not a dictionary label
            sample_hits: vec!["the quick brown fox".into()],
        };
        let err = det.validate().unwrap_err();
        assert!(err.contains("does not start with a known dictionary prefix"));
        assert!(err.contains("the quick brown fox"));
    }

    // -----------------------------------------------------------------------
    // Agent identity validation
    // -----------------------------------------------------------------------

    #[test]
    fn event_agent_id_accessor() {
        // Agent events return their agent_id
        let ev = TelemetryEvent::TaskAccepted(TaskAccepted {
            common: sample_agent_common(),
            dispatch_delay_ms: 42,
            task_publish_ts: None,
            job_age_at_accept_ms: None,
        });
        assert_eq!(ev.agent_id(), "CortexB");
    }

    #[test]
    fn emit_drops_mismatched_agent_id() {
        // Unit test the check that emit() delegates to.
        let src = TelemetrySource::Agent {
            agent_id: "CortexA".into(),
        };

        // Mismatched event
        let ev_mismatch = TelemetryEvent::TaskAccepted(TaskAccepted {
            common: AgentEventCommon {
                agent_id: "CortexB".into(),
                job_id: Some("job-x".into()),
                round: Some(1),
                phase: Some(DeliberationPhase::Proposing),
                ts: 0,
                trace_id: derive_trace_id("job-x", 1, DeliberationPhase::Proposing, "CortexB"),
            },
            dispatch_delay_ms: 0,
            task_publish_ts: None,
            job_age_at_accept_ms: None,
        });
        assert!(
            !source_agent_matches(&src, &ev_mismatch),
            "CortexB event should not match CortexA source"
        );

        // Matching event
        let ev_match = TelemetryEvent::TaskAccepted(TaskAccepted {
            common: AgentEventCommon {
                agent_id: "CortexA".into(),
                job_id: Some("job-x".into()),
                round: Some(1),
                phase: Some(DeliberationPhase::Proposing),
                ts: 0,
                trace_id: derive_trace_id("job-x", 1, DeliberationPhase::Proposing, "CortexA"),
            },
            dispatch_delay_ms: 0,
            task_publish_ts: None,
            job_age_at_accept_ms: None,
        });
        assert!(
            source_agent_matches(&src, &ev_match),
            "CortexA event should match CortexA source"
        );
    }

    // -----------------------------------------------------------------------
    // Redaction tests
    // -----------------------------------------------------------------------

    #[test]
    fn redact_content_detects_sensitive_words() {
        for input in [
            "password=abc",
            "my_api_key here",
            "secret_key: xyz",
            "access_key=123",
            "credential leaked",
            "Bearer tok_abc",
        ] {
            let out = redact_content(input, 100);
            assert_eq!(out, "REDACTED_SENSITIVE", "expected redaction for: {input}");
        }
    }

    #[test]
    fn redact_content_no_false_positives() {
        // These should NOT trigger redaction — word-boundary matching
        // prevents substring false positives.
        for input in [
            "keyword research",
            "the author of this",
            "a monkey in the tree",
            "public_authority report",
            "the secret of his success",
            "authenticate the user",
            "tokenized assets",
            "keyboard warrior",
        ] {
            let out = redact_content(input, 100);
            assert_eq!(out, input, "expected no redaction for: {input}");
        }
    }

    #[test]
    fn redact_content_truncates_long_input() {
        let input = "a".repeat(200);
        let out = redact_content(&input, 50);
        assert!(out.ends_with("..."));
        assert_eq!(out.chars().count(), 53); // 50 + 3 for "..."
    }

    #[test]
    fn redact_content_empty_input() {
        assert_eq!(redact_content("", 100), "<empty>");
    }

    #[test]
    fn redact_url_strips_credentials_in_authority() {
        assert_eq!(
            redact_url("https://user:pass@api.example.com/path"),
            "https://api.example.com/path"
        );
        assert_eq!(
            redact_url("http://admin:secret@host.com"),
            "http://host.com"
        );
    }

    #[test]
    fn redact_url_preserves_at_in_path() {
        assert_eq!(
            redact_url("https://api.example.com/users/foo@bar/profile"),
            "https://api.example.com/users/foo@bar/profile"
        );
    }

    #[test]
    fn redact_url_removes_query_and_fragment() {
        assert_eq!(
            redact_url("https://example.com/path?query=1#frag"),
            "https://example.com/path"
        );
    }

    #[test]
    fn redact_url_no_credentials() {
        assert_eq!(
            redact_url("https://example.com/path"),
            "https://example.com/path"
        );
    }

    #[test]
    fn redact_error_message_delegates_to_redact_content() {
        let msg = "password leaked in log";
        assert_eq!(redact_error_message(msg), "REDACTED_SENSITIVE");
        let long = "a".repeat(200);
        let out = redact_error_message(&long);
        assert!(out.ends_with("..."));
    }
}
