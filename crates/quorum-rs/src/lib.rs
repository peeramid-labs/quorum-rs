//! # NSED Agent SDK
//!
//! Trait definitions, data types, and utilities for building custom NSED agents.
//!
//! This crate provides the **interface layer** that third-party developers code against
//! to build custom agents for the NSED deliberation protocol. It contains:
//!
//! - **Agent traits**: [`NsedAgent`], [`PersistenceStore`], [`TokenEstimator`]
//! - **LLM traits**: [`AiModel`], [`ChatStrategy`]
//! - **Tool trait**: [`Tool`]
//! - **Prompt trait**: [`PromptSet`]
//! - **Data types**: [`AgentContext`], [`Proposal`], [`Evaluation`], [`AgentConfig`], etc.
//! - **NATS utilities**: [`nats_utils::sanitize_subject_component`], [`nats_utils::ensure_kv_bucket`]
//!
//! - **Agent implementations**: [`ExecAgent`], [`McpAgent`], [`ClaudeAgent`]
//!
//! The reference `ProposerEvaluatorAgent` (native LLM) lives in the `nsed-agent` crate (BSL 1.1).
//! This SDK is MIT-licensed — you can implement these traits without any licensing obligation.

pub mod agents;
pub mod api_error_middleware;
pub mod config;
pub mod control_plane;
#[cfg(feature = "audit")]
pub mod crypto;
pub mod events;
pub mod llms;
pub mod middleware;
pub mod nats_utils;
pub mod orchestrator_registry;
pub mod prompts;
pub mod scheduling;
pub mod status;
pub mod telemetry;
pub mod tools;
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
    CategoryScoreBreakdown, ProposalControversyEntry, ProposalScoreEntry, RoundSummaryEvent,
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
pub use nsed_crypto_core;
