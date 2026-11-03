//! Defines the low-level traits for interacting with language models.
//!
//! This module provides a generic `AiModel` trait that abstracts the specific
//! details of communicating with different LLM providers. The goal is to have a
//! single, consistent interface that the high-level agent logic can use.
//!
//! For a quick-start model implementation, see [`SimpleOpenAIModel`].

pub mod error;
pub mod openai_codex;
pub mod openai_compatible;
pub mod provider_usage;
pub mod rate_limiter;
pub mod simple_model;
pub mod simulated;
pub mod span;
pub mod strategies;
pub mod stub;
pub use error::LlmError;
pub use openai_codex::{OpenAICodexAuthStore, OpenAICodexModel};
pub use openai_compatible::OpenAICompatibleModel;
pub use provider_usage::ProviderUsage;
pub use rate_limiter::RateLimiter;
pub use simple_model::SimpleOpenAIModel;
pub use span::LlmRequestSpan;
pub use stub::StubModel;

use crate::agents::AgentConfig;
use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionTool, ChatCompletionToolChoiceOption,
    CreateChatCompletionResponse,
};
use async_trait::async_trait;
use dyn_clone::DynClone;
use std::fmt::Debug;

/// A "partial" request struct that contains the universal parts of a request
/// (messages and tools) that are assembled by the agent's ReAct loop.
#[derive(Debug, Clone)]
pub struct RequestConfig {
    pub messages: Vec<ChatCompletionRequestMessage>,
    pub tools: Option<Vec<ChatCompletionTool>>,
    pub tool_choice: Option<ChatCompletionToolChoiceOption>,
    pub presence_penalty: Option<f32>,
    /// Provider service tier for this call, e.g. `flex` for a cheaper
    /// best-effort queue.
    ///
    /// Belongs to the call, not the agent: it is the caller's cost-versus-
    /// latency choice about one job, and the same council may be run cheaply
    /// for a background question and at the default tier when someone is
    /// waiting. Riding the request also means one job's choice cannot outlive
    /// it and settle onto the agent's later work.
    ///
    /// Carried verbatim and read per provider — a tier is not a universal
    /// concept, so each [`ChatStrategy`] decides in its own `prepare_request`
    /// whether it means anything at all.
    pub service_tier: Option<String>,
}

impl RequestConfig {
    /// Ask the provider for a specific service tier on this call.
    ///
    /// Blank is treated as absent: an empty string survives a round trip
    /// through YAML or JSON easily, and providers reject it.
    pub fn with_service_tier_flag(mut self, tier: Option<&str>) -> Self {
        self.service_tier = tier
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        self
    }
}

/// Result of a chat completion including the response, raw request, and timing.
pub struct ChatCompletionResult {
    pub response: CreateChatCompletionResponse,
    pub raw_request: String,
    pub timing: TimingMetadata,
    pub provider_backend: Option<String>,
    /// `Some` when the SDK shrink-guard rewrote `max_tokens`.
    pub shrink_info: Option<ShrinkInfo>,
    /// Cost and cache figures the provider reported outside the OpenAI schema.
    /// All-`None` for backends that report none of it.
    pub provider_usage: ProviderUsage,
}

/// Timing metadata for an LLM call.
pub struct TimingMetadata {
    pub ttft_ms: Option<u64>,
    pub generation_ms: Option<u64>,
}

/// Shrink-guard state for one LLM call. Consumed by
/// `LlmRequestSpan::complete` to populate
/// [`LlmRequestComplete`] and emit [`ContextEmergencyShrink`].
#[derive(Debug, Clone)]
pub struct ShrinkInfo {
    /// `true` when `available < floor` — the floor-clamp case,
    /// distinct from healthy `available > floor` adaptive shrinks.
    pub floor_used: bool,
    /// Raw headroom (saturating non-negative). Pre-clamp, so
    /// `available = 0` and `available = 199` are reported distinctly.
    pub available_space: u32,
    pub requested_max: u32,
    pub floor: u32,
    pub estimated_input: u32,
    pub context_window: u32,
}

/// A trait for a client that can interact with a language model.
/// This is the lowest level of abstraction for model interaction.
#[async_trait]
pub trait AiModel: Send + Sync + DynClone + Debug {
    /// The core function for interacting with a model.
    /// It takes an agent's configuration and a partial request config,
    /// combines them into a full request, and returns the model's response.
    async fn chat_completion(
        &self,
        agent: &AgentConfig,
        request_config: RequestConfig,
    ) -> Result<ChatCompletionResult, LlmError>;
}

// Make the trait cloneable for dependency injection.
dyn_clone::clone_trait_object!(AiModel);

/// Overrides that strategies can apply to the request.
#[derive(Debug, Clone, Default)]
pub struct RequestOverrides {
    pub max_tokens: Option<u32>,
}

/// A trait for transforming LLM requests/responses for specific providers.
///
/// Different providers (OpenAI, vLLM, Harmony) require different request
/// formats and return responses in different structures. The `ChatStrategy`
/// trait allows the `OpenAICompatibleModel` to delegate provider-specific
/// transformations to pluggable strategy implementations.
#[async_trait]
pub trait ChatStrategy: Send + Sync {
    /// Prepares the JSON body for the HTTP request.
    async fn prepare_request(
        &self,
        agent: &AgentConfig,
        request: &RequestConfig,
        overrides: &RequestOverrides,
    ) -> Result<serde_json::Value, LlmError>;

    /// Parses the raw response body into a standardized OpenAI response.
    async fn parse_response(
        &self,
        response_body: &str,
    ) -> Result<CreateChatCompletionResponse, LlmError>;

    /// Returns the API endpoint suffix (e.g., "/chat/completions").
    fn endpoint_suffix(&self) -> &str {
        "/chat/completions"
    }

    /// Returns whether this strategy supports streaming responses.
    fn supports_streaming(&self) -> bool {
        true
    }

    /// What the provider said this call cost, and what it served from cache.
    fn provider_usage(&self, _response: &serde_json::Value) -> ProviderUsage {
        ProviderUsage::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal strategy that only implements the required methods,
    /// leaving `endpoint_suffix` and `supports_streaming` at their defaults.
    #[derive(Debug)]
    struct DefaultStrategy;

    #[async_trait]
    impl ChatStrategy for DefaultStrategy {
        async fn prepare_request(
            &self,
            _agent: &AgentConfig,
            _request: &RequestConfig,
            _overrides: &RequestOverrides,
        ) -> Result<serde_json::Value, LlmError> {
            Ok(serde_json::json!({}))
        }

        async fn parse_response(
            &self,
            _response_body: &str,
        ) -> Result<CreateChatCompletionResponse, LlmError> {
            Err(LlmError::Other("not implemented".into()))
        }
    }

    #[test]
    fn chat_strategy_default_endpoint_suffix() {
        let strategy = DefaultStrategy;
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");
    }

    #[test]
    fn chat_strategy_default_supports_streaming() {
        let strategy = DefaultStrategy;
        assert!(strategy.supports_streaming());
    }

    #[test]
    fn request_overrides_default() {
        let overrides = RequestOverrides::default();
        assert!(overrides.max_tokens.is_none());
    }
}
