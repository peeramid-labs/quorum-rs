//! # quorum-rs
//!
//! Trait definitions, data types, and runtime for building deliberation agents.
//!
//! This crate provides the agent SDK plus a reference runtime: traits to code
//! against, batteries-included implementations to compose with, and the NATS
//! JetStream worker that ties them together.
//!
//! - **Agent traits**: [`NsedAgent`], [`PersistenceStore`], [`TokenEstimator`]
//! - **LLM traits**: [`AiModel`], [`ChatStrategy`]
//! - **Tool trait**: [`Tool`]
//! - **Prompt trait**: [`PromptSet`]
//! - **Data types**: [`AgentContext`], [`Proposal`], [`Evaluation`], [`AgentConfig`], etc.
//! - **NATS utilities**: [`nats_utils::sanitize_subject_component`], [`nats_utils::ensure_kv_bucket`]
//! - **Agent implementations**: [`ExecAgent`], [`McpAgent`], [`ClaudeAgent`], and the
//!   reference `ProposerEvaluatorAgent` (native LLM via `OpenAICompatibleModel`).
//!
//! Dual-licensed Apache-2.0 OR MIT.

pub mod agent_manager;
pub mod agents;
pub mod api_error_middleware;
/// Brand strings — banner ASCII, copy snippets used by `quorum
/// init` and `quorum validate`. Kept in one module so a future
/// rebrand changes one file, not 12 string-literal sites.
pub mod brand;
#[cfg(feature = "cli")]
pub mod cli;
pub mod config;
pub mod control_plane;
pub mod conversation;
#[cfg(feature = "audit")]
pub mod crypto;
pub mod dump_prompts;
pub mod epic_read;
pub mod events;
/// Workspace + agent-fleet config primitives that `quorum init`
/// composes: provider catalog, persona presets, yaml renderer,
/// env-file merger, generic wizard helpers. No CLI surface —
/// the binary-facing wizard lives at `cli::commands::init_wizard`.
pub mod init;
pub mod llms;
pub mod middleware;
pub mod multi_agent;
pub mod nats_utils;
pub mod orchestrator_registry;
pub mod project_registry;
pub mod project_sync;
pub mod prompts;
pub mod providers;
pub mod scheduling;
pub mod serve;
pub mod status;
pub mod telemetry;
pub mod tools;
pub mod version_check;
pub mod workers;

// Re-export commonly used items at crate root for convenience
pub use agents::DeliberationPhase;
pub use agents::ProposalRecord;
pub use agents::config::{AgentConfig, TaskPrecision};
pub use agents::exec_agent::ExecAgent;
pub use agents::mcp_agent::{ClaudeAgent, McpAgent};
pub use agents::{
    AgentContext, AgentHeartbeat, AgentLiveStatus, AgentPricingInfo, AnnotationType,
    CandidateProposal, ChatCapable, Evaluation, EvaluationRecord, HeuristicTokenEstimator,
    InjectionPriority, NsedAgent, OperatorAnnotation, OrchestratorPing, PendingToolCall,
    PersistenceStore, Proposal, TokenEstimator, TokenUsage, ToolCallStatus, ToolChanges,
    UserInjection, UserToolDefinition,
};
pub use control_plane::{AgentControlPlane, ConfigPatch};
#[cfg(feature = "audit")]
pub use crypto::{AgentKeyPair, SigningHook};
pub use events::{
    CategoryScoreBreakdown, JobCompleteEvent, ProposalControversyEntry, ProposalScoreEntry,
    RoundSummaryEvent,
};
pub use llms::{
    AiModel, ChatCompletionResult, LlmRequestSpan, RequestConfig, SimpleOpenAIModel, TimingMetadata,
};
pub use nats_utils::{NatsAuth, OrchestratorEntry, connect_nats, sha256_hex};
pub use prompts::PromptSet;
pub use scheduling::PolicySla;
pub use status::{AgentStatusSnapshot, EventLogEntry, ScoreEntry, SharedAgentStatus, TaskLogEntry};
pub use telemetry::{
    AgentEventCommon, FinishReason, LlmError, LlmErrorClass, NatsConnectionState,
    PromptExposureDetected, RetryReason, TaskFailureClass, TelemetryConfig, TelemetryContext,
    TelemetryEmitter, TelemetryEvent, TelemetrySource, derive_trace_id, trace_id_for,
};
pub use tools::Tool;
pub use workers::buffer::{AckHandle, BufferEntryDetail, BufferEntrySummary, ResponseBuffer};
pub use workers::{
    JobManifest, NatsNsedWorker, NatsScratchpadStore, UserToolHandlerFactory, WorkerConfig,
    WorkerHook,
};
// Re-export crypto-core for downstream consumers
#[cfg(feature = "audit")]
pub use quorum_crypto_core;
