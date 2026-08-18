//! Minimal OpenAI-compatible model for getting started quickly.
//!
//! [`SimpleOpenAIModel`] implements [`AiModel`] with a direct HTTP call to
//! any OpenAI-compatible `/chat/completions` endpoint.  No streaming, no rate
//! limiting, no retry logic, no provider-specific strategies — just the
//! bare minimum to build and test a custom agent.
//!
//! For production use (streaming, rate limiting, provider strategies), see
//! [`OpenAICompatibleModel`](crate::llms::OpenAICompatibleModel) in this crate.

use crate::agents::AgentConfig;
use crate::llms::{AiModel, ChatCompletionResult, RequestConfig, TimingMetadata};
use crate::telemetry::LlmError;
use async_openai::types::CreateChatCompletionResponse;
use async_trait::async_trait;
use std::fmt::Debug;
use std::time::Duration;

/// Minimal OpenAI-compatible LLM client.
///
/// # Example
///
/// ```rust,no_run
/// use quorum_rs::llms::SimpleOpenAIModel;
///
/// let model = SimpleOpenAIModel::new(
///     "https://api.openai.com/v1".into(),
///     std::env::var("OPENAI_API_KEY").unwrap(),
/// );
/// ```
#[derive(Clone)]
pub struct SimpleOpenAIModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Debug for SimpleOpenAIModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimpleOpenAIModel")
            .field("base_url", &self.base_url)
            .field("api_key", &"***")
            .finish()
    }
}

/// Default request timeout for LLM calls (2 hours).
/// LLM inference can be very slow for large models or long contexts.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(7200);

impl SimpleOpenAIModel {
    /// Create a new model client pointing at an OpenAI-compatible endpoint.
    ///
    /// Uses a default timeout of 2 hours. For a custom timeout, use
    /// [`SimpleOpenAIModel::with_timeout`].
    pub fn new(base_url: String, api_key: String) -> Self {
        Self::with_timeout(base_url, api_key, DEFAULT_TIMEOUT)
    }

    /// Create a new model client with a custom request timeout.
    pub fn with_timeout(base_url: String, api_key: String, timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("failed to build reqwest client with timeout"),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

#[async_trait]
impl AiModel for SimpleOpenAIModel {
    async fn chat_completion(
        &self,
        agent: &AgentConfig,
        request_config: RequestConfig,
    ) -> Result<ChatCompletionResult, LlmError> {
        // Build the request body
        let mut body = serde_json::json!({
            "model": agent.model_name,
            "messages": request_config.messages,
        });

        // Always send temperature — 0.0 is a valid explicit value (deterministic output)
        body["temperature"] = serde_json::json!(agent.temperature);
        if agent.max_tokens > 0 {
            body["max_tokens"] = serde_json::json!(agent.max_tokens);
        }
        if let Some(tools) = &request_config.tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::json!(tools);
            }
        }
        if let Some(tool_choice) = &request_config.tool_choice {
            body["tool_choice"] = serde_json::json!(tool_choice);
        }
        if let Some(pp) = request_config.presence_penalty {
            body["presence_penalty"] = serde_json::json!(pp);
        }

        let request_body =
            serde_json::to_string_pretty(&body).map_err(|e| LlmError::Parse(e.into()))?;
        let url = format!("{}/chat/completions", self.base_url);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(request_body.clone())
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() || e.is_connect() {
                    LlmError::Transport(e.into())
                } else if let Some(status) = e.status() {
                    if status.as_u16() == 429 {
                        LlmError::RateLimit {
                            retry_after_ms: None,
                            status: status.as_u16(),
                        }
                    } else if status.as_u16() == 402 {
                        LlmError::PaymentRequired { status: 402 }
                    } else if status.is_server_error() {
                        LlmError::ServerError {
                            status: status.as_u16(),
                        }
                    } else {
                        LlmError::Transport(e.into())
                    }
                } else {
                    LlmError::Transport(e.into())
                }
            })?;

        let status = response.status();
        let response_text = response
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.into()))?;

        if !status.is_success() {
            let status_code = status.as_u16();
            if status_code == 402 {
                return Err(LlmError::PaymentRequired { status: 402 });
            } else if status_code == 429 {
                return Err(LlmError::RateLimit {
                    retry_after_ms: None,
                    status: status_code,
                });
            } else if status.is_server_error() {
                return Err(LlmError::ServerError {
                    status: status_code,
                });
            } else {
                let truncated: String = response_text.chars().take(500).collect();
                return Err(LlmError::Other(
                    format!("LLM API error ({}): {}", status, truncated).into(),
                ));
            }
        }

        let parsed: CreateChatCompletionResponse =
            serde_json::from_str(&response_text).map_err(|e| LlmError::Parse(e.into()))?;
        Ok(ChatCompletionResult {
            response: parsed,
            raw_request: request_body,
            timing: TimingMetadata {
                ttft_ms: None,
                generation_ms: None,
            },
            provider_backend: None,
            shrink_info: None,
            provider_usage: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_stores_base_url() {
        let model = SimpleOpenAIModel::new("https://api.openai.com/v1".into(), "sk-test".into());
        let debug = format!("{:?}", model);
        assert!(
            debug.contains("https://api.openai.com/v1"),
            "Debug output should contain the base URL, got: {debug}"
        );
    }

    #[test]
    fn test_new_trims_trailing_slash() {
        let model = SimpleOpenAIModel::new("https://api.openai.com/v1/".into(), "sk-test".into());
        let debug = format!("{:?}", model);
        assert!(
            debug.contains("https://api.openai.com/v1"),
            "Debug output should contain the base URL, got: {debug}"
        );
        assert!(
            !debug.contains("https://api.openai.com/v1/"),
            "Trailing slash should be trimmed, got: {debug}"
        );
    }

    #[test]
    fn test_debug_masks_api_key() {
        let model = SimpleOpenAIModel::new(
            "https://api.openai.com/v1".into(),
            "sk-super-secret-key-123".into(),
        );
        let debug = format!("{:?}", model);
        assert!(
            debug.contains("***"),
            "Debug output should contain masked key '***', got: {debug}"
        );
        assert!(
            !debug.contains("sk-super-secret-key-123"),
            "Debug output must NOT contain the raw API key, got: {debug}"
        );
    }

    #[test]
    fn test_with_timeout_custom() {
        let _model = SimpleOpenAIModel::with_timeout(
            "https://api.openai.com/v1".into(),
            "sk-test".into(),
            Duration::from_secs(30),
        );
        // Construction succeeded without panic — timeout was accepted.
    }

    #[test]
    fn test_clone() {
        let model = SimpleOpenAIModel::new("https://api.openai.com/v1".into(), "sk-test".into());
        let cloned = model.clone();
        let debug_original = format!("{:?}", model);
        let debug_cloned = format!("{:?}", cloned);
        assert_eq!(
            debug_original, debug_cloned,
            "Cloned model should have identical Debug output"
        );
    }
}
