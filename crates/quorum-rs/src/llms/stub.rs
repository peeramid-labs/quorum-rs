//! Stub provider — returns an empty placeholder response immediately.
//!
//! Used when the "agent" is externally operated (human or external system).
//! The placeholder enters the [`ResponseBuffer`] in stopped mode, and an
//! external system edits + releases it via the HITL buffer REST API.

use crate::agents::config::AgentConfig;
use crate::llms::{AiModel, ChatCompletionResult, RequestConfig, TimingMetadata};
use crate::telemetry::LlmError;
use async_openai::types::{
    ChatChoice, ChatCompletionResponseMessage, CompletionUsage, CreateChatCompletionResponse,
    FinishReason, Role,
};
use async_trait::async_trait;

/// Stub model that returns an empty response immediately.
///
/// When paired with `auto_stop: true` in [`AgentConfig`], the buffer entry
/// starts stopped — preventing auto-release until an external system edits
/// and explicitly releases it.
#[derive(Clone, Debug)]
pub struct StubModel {
    model_name: String,
}

impl StubModel {
    pub fn new(model_name: String) -> Self {
        Self { model_name }
    }
}

#[async_trait]
impl AiModel for StubModel {
    async fn chat_completion(
        &self,
        _agent: &AgentConfig,
        _request_config: RequestConfig,
    ) -> Result<ChatCompletionResult, LlmError> {
        // Return a minimal valid response — empty content that the external
        // system will replace via PUT /buffer/{id}
        let response = CreateChatCompletionResponse {
            id: format!("stub-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u32,
            model: self.model_name.clone(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: Role::Assistant,
                    content: Some(String::new()),
                    tool_calls: None,
                    #[allow(deprecated)]
                    function_call: None,
                    refusal: None,
                    audio: None,
                },
                finish_reason: Some(FinishReason::Stop),
                logprobs: None,
            }],
            usage: Some(CompletionUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            service_tier: None,
            system_fingerprint: None,
        };

        Ok(ChatCompletionResult {
            response,
            raw_request: self.model_name.clone(),
            timing: TimingMetadata {
                ttft_ms: None,
                generation_ms: None,
            },
            provider_backend: None,
            shrink_info: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_returns_empty_response() {
        let model = StubModel::new("stub".to_string());
        let config = AgentConfig::default();
        let request = RequestConfig {
            messages: vec![],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let result = model.chat_completion(&config, request).await.unwrap();
        let model_name = &result.raw_request;
        let response = &result.response;
        assert_eq!(model_name, "stub");
        assert_eq!(response.choices.len(), 1);
        assert_eq!(response.choices[0].message.content.as_deref(), Some(""));
        assert_eq!(response.usage.as_ref().unwrap().total_tokens, 0);
    }

    #[test]
    fn stub_model_is_clone() {
        let model = StubModel::new("test".to_string());
        let _cloned = model.clone();
    }

    #[test]
    fn stub_model_is_debug() {
        let model = StubModel::new("test".to_string());
        let debug = format!("{:?}", model);
        assert!(debug.contains("StubModel"));
    }
}
