//! An implementation of the `AiModel` trait for any service that provides
//! an OpenAI-compatible API endpoint.

use crate::agents::config::AgentConfig;
use crate::llms::strategies::{RequestOverrides, StrategyResolver};
use crate::llms::{AiModel, ChatCompletionResult, RequestConfig, ShrinkInfo, TimingMetadata};
use crate::telemetry::LlmError;
use async_openai::types::{
    ChatChoice, ChatCompletionMessageToolCall, ChatCompletionToolType,
    CreateChatCompletionResponse, CreateChatCompletionStreamResponse, FinishReason, FunctionCall,
    Role,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use super::RateLimiter;
use std::collections::HashMap;
use std::sync::Mutex;

/// A client for any LLM that exposes an OpenAI-compatible API.
#[derive(Debug, Clone)]
pub struct OpenAICompatibleModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    semaphore: Option<Arc<Semaphore>>,
    rate_limiter: Option<Arc<RateLimiter>>,
    engine: Option<String>,
}

impl OpenAICompatibleModel {
    pub fn new(base_url: String, api_key: String, engine: Option<String>) -> Self {
        info!(
            "Initializing OpenAICompatibleModel with base_url: {} engine: {:?}",
            base_url, engine
        );
        Self {
            // We keep a long timeout for the connection, but streaming ensures we don't hit it.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(7200))
                .build()
                .expect("Failed to create HTTP client"),
            base_url,
            api_key,
            semaphore: None,
            rate_limiter: None,
            engine,
        }
    }

    pub fn with_semaphore(mut self, semaphore: Arc<Semaphore>) -> Self {
        self.semaphore = Some(semaphore);
        self
    }

    pub fn with_rate_limiter(mut self, rate_limiter: Arc<RateLimiter>) -> Self {
        self.rate_limiter = Some(rate_limiter);
        self
    }
}

#[async_trait]
impl AiModel for OpenAICompatibleModel {
    async fn chat_completion(
        &self,
        agent: &AgentConfig,
        request_config: RequestConfig,
    ) -> Result<ChatCompletionResult, LlmError> {
        let _permit = if let Some(sem) = &self.semaphore {
            Some(
                sem.acquire()
                    .await
                    .map_err(|e| LlmError::Other(Box::new(e)))?,
            )
        } else {
            None
        };

        let estimated_input_tokens = estimate_input_tokens(&request_config)?;

        // Reactive retries (vLLM 400 below) overwrite the proactive
        // value: only the shrink that left the SDK is reported.
        let mut shrink_info: Option<ShrinkInfo> = None;
        const SHRINK_FLOOR: u32 = 200;
        let requested_max_tokens = agent.max_tokens as u32;
        let mut final_max_tokens = requested_max_tokens;
        if agent.context_window > 0 {
            let safety_buffer = 500;
            // True headroom is reserved for telemetry; the safety
            // buffer + floor only shape `final_max_tokens`.
            let raw_available = agent.context_window.saturating_sub(estimated_input_tokens);
            let available_headroom = raw_available.max(0) as u32;
            let post_buffer = raw_available.saturating_sub(safety_buffer);
            let final_after_clamp = post_buffer.max(SHRINK_FLOOR as i32) as u32;

            if final_max_tokens > final_after_clamp {
                warn!(
                    agent = %agent.name,
                    requested_max = final_max_tokens,
                    available_space = final_after_clamp,
                    estimated_input = estimated_input_tokens,
                    context_limit = agent.context_window,
                    "Dynamically shrinking max_tokens to prevent context overflow."
                );
                shrink_info = Some(ShrinkInfo {
                    floor_used: post_buffer < SHRINK_FLOOR as i32,
                    available_space: available_headroom,
                    requested_max: final_max_tokens,
                    floor: SHRINK_FLOOR,
                    estimated_input: estimated_input_tokens.max(0) as u32,
                    context_window: agent.context_window.max(0) as u32,
                });
                final_max_tokens = final_after_clamp;
            }
        }

        // Resolve Strategy
        let strategy = StrategyResolver::resolve(self.engine.as_deref());

        let base = self.base_url.trim_end_matches('/');
        let clean_base = base.strip_suffix("/v1").unwrap_or(base);
        let endpoint = format!("{}/v1{}", clean_base, strategy.endpoint_suffix());

        let mut attempts = 0;
        let loop_start = std::time::Instant::now();

        let (response, request_body_out, _request_json, provider_backend) = loop {
            attempts += 1;

            let overrides = RequestOverrides {
                max_tokens: Some(final_max_tokens),
            };

            // `prepare_request` already returns `LlmError`; propagate via
            // `?` so the caller observes the original variant (e.g.
            // `Parse` for serialisation failures inside the strategy)
            // rather than re-wrapping into `Other` and erasing it.
            let request_json = strategy
                .prepare_request(agent, &request_config, &overrides)
                .await?;

            let request_body =
                serde_json::to_string(&request_json).map_err(|e| LlmError::Parse(Box::new(e)))?;

            if tracing::enabled!(tracing::Level::DEBUG) {
                debug!("Strategy Prepared Request Body: {}", request_body);
            }

            debug!("Sending request to: {}", endpoint);
            if tracing::enabled!(tracing::Level::DEBUG) {
                debug!("Request Body: {}", request_body);
            }

            // Enforce QPS Limit
            if let Some(limiter) = &self.rate_limiter {
                limiter.acquire().await;
            }

            // Manually build request to include patched body
            let response = self
                .client
                .post(&endpoint)
                .bearer_auth(&self.api_key)
                .header("Content-Type", "application/json")
                .body(request_body.clone())
                .send()
                .await
                .map_err(|e| LlmError::Transport(Box::new(e)))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();

                // Reactive Context Window Management for vLLM
                // Error example: "This model's maximum context length is 21000 tokens and your request has 5395 input tokens"
                if status.as_u16() == 400
                    && attempts < 2
                    && let Some((limit, input)) = parse_vllm_context_error(&body)
                {
                    let safety_buffer = 100;
                    let raw_available = limit.saturating_sub(input);
                    let post_buffer = raw_available.saturating_sub(safety_buffer);
                    let clamped = post_buffer.max(SHRINK_FLOOR);

                    warn!(
                        agent = %agent.name,
                        error = "Context Window Exceeded",
                        server_limit = limit,
                        server_input = input,
                        new_max_tokens = clamped,
                        "Reactive Retry: Shrinking max_tokens based on server error."
                    );

                    shrink_info = Some(ShrinkInfo {
                        floor_used: post_buffer < SHRINK_FLOOR,
                        available_space: raw_available,
                        requested_max: requested_max_tokens,
                        floor: SHRINK_FLOOR,
                        estimated_input: input,
                        context_window: limit,
                    });
                    final_max_tokens = clamped; // Ensure minimal output
                    continue; // Retry with new token limit
                }

                // 402 Payment Required — provider billing / credits issue.
                // Retry with backoff for up to half the agent's SLA budget;
                // transient billing hiccups (pre-paid top-ups, quota resets)
                // often resolve within seconds.
                if status.as_u16() == 402 {
                    let sla_budget = std::time::Duration::from_secs(agent.response_sla_secs / 2);
                    let elapsed = loop_start.elapsed();
                    if elapsed < sla_budget {
                        let remaining = sla_budget.saturating_sub(elapsed);
                        let sleep_secs = 2u64.pow(attempts).min(30).min(remaining.as_secs());
                        if sleep_secs == 0 {
                            // No remaining budget — fall through to error path
                        } else {
                            warn!(
                                agent = %agent.name,
                                status = %status,
                                attempt = attempts,
                                retry_in_secs = sleep_secs,
                                remaining_budget = ?remaining,
                                "💳 402 Payment Required — retrying with backoff \
                                 ({remaining:?} of SLA budget remaining)."
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                            continue;
                        }
                    }
                    return Err(LlmError::PaymentRequired {
                        status: status.as_u16(),
                    });
                }

                // Retry logic for 429 Too Many Requests and 5xx Server Errors
                if status.as_u16() == 429 || status.is_server_error() {
                    // Update Error Metrics
                    {
                        static ERROR_COUNTS: std::sync::OnceLock<
                            Mutex<HashMap<(String, u16), u64>>,
                        > = std::sync::OnceLock::new();
                        let counts = ERROR_COUNTS.get_or_init(|| Mutex::new(HashMap::new()));
                        if let Ok(mut map) = counts.lock() {
                            let key = (agent.name.clone(), status.as_u16());
                            *map.entry(key).or_insert(0) += 1;

                            // Offload blocking file I/O to a separate thread to avoid stalling the async runtime
                            let snapshot = map.clone();
                            tokio::task::spawn_blocking(move || {
                                let path = std::env::temp_dir().join("nsed_error_metrics.prom");
                                if let Ok(file) = std::fs::File::create(&path) {
                                    use std::io::Write;
                                    let mut writer = std::io::BufWriter::new(file);
                                    let _ = writeln!(
                                        writer,
                                        "# HELP nsed_api_errors_total Total API errors by agent and status code"
                                    );
                                    let _ =
                                        writeln!(writer, "# TYPE nsed_api_errors_total counter");
                                    for ((a_name, code), count) in snapshot.iter() {
                                        let _ = writeln!(
                                            writer,
                                            "nsed_api_errors_total{{agent=\"{}\",status=\"{}\"}} {}",
                                            a_name, code, count
                                        );
                                    }
                                }
                            });
                        }
                    }

                    if attempts < 11 {
                        let sleep_duration = std::time::Duration::from_secs(2u64.pow(attempts));
                        warn!(
                            agent = %agent.name,
                            status = %status,
                            attempt = attempts,
                            retry_after = ?sleep_duration,
                            "API request failed (Rate Limit or Server Error). Retrying with backoff."
                        );
                        tokio::time::sleep(sleep_duration).await;
                        continue;
                    }
                }

                // All retry paths above either continued or returned;
                // we land here only when retries are exhausted (or never
                // applied for this status). Map to a typed variant so
                // `LlmRequestFailed.error_class` / `http_status` carry
                // structured info instead of a stringified Other.
                //
                // The provider's response `body` is intentionally NOT
                // included in the error message: for content-filter
                // errors it can echo a fragment of the agent's prompt,
                // for rate-limit errors it can include user-quota
                // metadata, and for auth errors it can leak the API
                // key prefix. The error then flows into `tracing` logs
                // and `write_failure_dump` files, both of which are
                // operator-readable surfaces. Status code +
                // `LlmRequestFailed.http_status` give ops everything
                // they need to triage; if the body is needed for a
                // bug repro, debug-mode `parse_vllm_context_error` and
                // similar structured extractors are the right path.
                let code = status.as_u16();
                // `body` is only read by the structured
                // `parse_vllm_context_error` extractor below — never
                // formatted into an error message.
                let err = if code == 429 {
                    // Retry-After header parsing is a follow-up; status
                    // alone is enough for the dashboard taxonomy.
                    LlmError::RateLimit {
                        retry_after_ms: None,
                        status: code,
                    }
                } else if status.is_server_error() {
                    LlmError::ServerError { status: code }
                } else if code == 400
                    && let Some((limit, tokens)) = parse_vllm_context_error(&body)
                {
                    LlmError::ContextOverflow { tokens, limit }
                } else {
                    LlmError::Other(Box::new(std::io::Error::other(format!(
                        "API request failed with status {status}"
                    ))))
                };
                return Err(err);
            }

            let provider_backend = response
                .headers()
                .get("x-or-backend")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            break (response, request_body, request_json, provider_backend);
        };

        let use_streaming = agent.use_streaming && strategy.supports_streaming();

        if !use_streaming {
            let body = response
                .text()
                .await
                .map_err(|e| LlmError::Transport(Box::new(e)))?;
            // `parse_response` already returns `LlmError`; propagate
            // via `?` so the caller sees the original variant.
            let chat_response = strategy.parse_response(&body).await?;
            // Non-streaming has no first-chunk visibility — TTFT and the
            // first-token-to-finish split are unobservable. The span emits
            // the total `latency_ms` separately so dashboards still get
            // wall-clock duration; ttft / generation_ms stay `None`.
            return Ok(ChatCompletionResult {
                response: chat_response,
                raw_request: request_body_out,
                timing: TimingMetadata {
                    ttft_ms: None,
                    generation_ms: None,
                },
                provider_backend: provider_backend.clone(),
                shrink_info: shrink_info.clone(),
            });
        }

        let mut response_stream = response.bytes_stream();

        // Accumulator state
        let mut full_content = String::new();
        let mut ttft_ms: Option<u64> = None;
        let mut first_chunk_at: Option<std::time::Instant> = None;
        let mut last_chunk_at: Option<std::time::Instant> = None;
        // Use BTreeMap to keep tool calls sorted by index automatically
        let mut tool_calls_map: BTreeMap<u32, ChatCompletionMessageToolCall> = BTreeMap::new();
        let mut finish_reason: Option<FinishReason> = None;
        let mut role = Role::Assistant; // Default
        let mut id = String::new();
        let mut created = 0;
        let mut model = agent.model_name.clone();
        let mut system_fingerprint = None;
        let mut usage = None;

        // Simple SSE parser loop
        let mut buffer = String::new();
        let mut done = false;
        'stream_loop: while let Some(chunk) = response_stream.next().await {
            let bytes = chunk.map_err(|e| LlmError::Transport(Box::new(e)))?;
            let text = String::from_utf8_lossy(&bytes);
            buffer.push_str(&text);

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim().to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    tracing::debug!("Stream Chunk: {}", data);

                    if data == "[DONE]" {
                        tracing::debug!("Stream finished with [DONE]");
                        done = true;
                        continue;
                    }

                    // Robustly extract usage from JSON value in case struct is missing it
                    if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(data)
                        && let Some(usage_val) = json_val.get("usage")
                        && !usage_val.is_null()
                    {
                        info!(
                            "CreateChatCompletionStreamResponse: Found usage in stream chunk: {:?}",
                            usage_val
                        );
                        if let Ok(parsed_usage) = serde_json::from_value(usage_val.clone()) {
                            usage = Some(parsed_usage);
                        } else {
                            warn!("Failed to deserialize usage object: {:?}", usage_val);
                        }
                    }

                    match serde_json::from_str::<CreateChatCompletionStreamResponse>(data) {
                        Err(e) => {
                            warn!("Failed to parse stream chunk: {}. Data: {}", e, data);
                        }
                        Ok(chat_response) => {
                            if !chat_response.id.is_empty() {
                                id = chat_response.id;
                            }
                            if chat_response.created > 0 {
                                created = chat_response.created;
                            }
                            if !chat_response.model.is_empty() {
                                model = chat_response.model;
                            }
                            if chat_response.system_fingerprint.is_some() {
                                system_fingerprint = chat_response.system_fingerprint;
                            }

                            if let Some(choice) = chat_response.choices.first() {
                                // Record TTFT on first meaningful chunk
                                if first_chunk_at.is_none() {
                                    let has_content = choice
                                        .delta
                                        .content
                                        .as_ref()
                                        .is_some_and(|c| !c.is_empty());
                                    let has_tool_calls = choice
                                        .delta
                                        .tool_calls
                                        .as_ref()
                                        .is_some_and(|t| !t.is_empty());
                                    if has_content || has_tool_calls {
                                        first_chunk_at = Some(std::time::Instant::now());
                                        ttft_ms = Some(loop_start.elapsed().as_millis() as u64);
                                    }
                                }
                                // Capture finish reason if present
                                if let Some(reason) = choice.finish_reason {
                                    finish_reason = Some(reason);
                                }

                                let delta = &choice.delta;

                                // Capture Role
                                if let Some(r) = delta.role {
                                    role = r;
                                }

                                // Accumulate Content
                                if let Some(content) = &delta.content {
                                    full_content.push_str(content);
                                }

                                last_chunk_at = Some(std::time::Instant::now());

                                // Accumulate Tool Calls
                                if let Some(tool_calls) = &delta.tool_calls {
                                    for tool_call in tool_calls {
                                        let index = tool_call.index;
                                        if let Some(existing) = tool_calls_map.get_mut(&index) {
                                            // Update existing tool call
                                            if let Some(id) = &tool_call.id
                                                && existing.id.is_empty()
                                            {
                                                existing.id = id.clone();
                                            }
                                            if let Some(func) = &tool_call.function {
                                                if let Some(name) = &func.name {
                                                    existing.function.name.push_str(name);
                                                }
                                                if let Some(args) = &func.arguments {
                                                    existing.function.arguments.push_str(args);
                                                }
                                            }
                                        } else {
                                            // New tool call
                                            // We need to convert StreamToolCall to MessageToolCall
                                            // The delta tool call has partial fields.
                                            let mut name = String::new();
                                            let mut arguments = String::new();

                                            if let Some(func) = &tool_call.function {
                                                if let Some(n) = &func.name {
                                                    name = n.clone();
                                                }
                                                if let Some(args) = &func.arguments {
                                                    arguments = args.clone();
                                                }
                                            }

                                            let function = FunctionCall { name, arguments };

                                            let message_tool_call = ChatCompletionMessageToolCall {
                                                id: tool_call.id.clone().unwrap_or_default(),
                                                r#type: ChatCompletionToolType::Function,
                                                function,
                                            };
                                            tool_calls_map.insert(index, message_tool_call);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if done {
                break 'stream_loop;
            }
        }

        // Reconstruct final response
        let tool_calls = if tool_calls_map.is_empty() {
            None
        } else {
            Some(tool_calls_map.into_values().collect())
        };

        // Construct the ChatCompletionResponseMessage
        #[allow(deprecated)]
        let message = async_openai::types::ChatCompletionResponseMessage {
            role,
            content: if full_content.is_empty() {
                None
            } else {
                Some(full_content)
            },
            tool_calls,
            function_call: None, // Deprecated
            refusal: None,
            audio: None,
        };

        let choice = ChatChoice {
            index: 0,
            message,
            finish_reason,
            logprobs: None,
        };

        let final_response = CreateChatCompletionResponse {
            id: if id.is_empty() {
                "streamed".to_string()
            } else {
                id
            },
            object: "chat.completion".to_string(),
            created,
            model,
            choices: vec![choice],
            usage,
            service_tier: None,
            system_fingerprint,
        };

        if tracing::enabled!(tracing::Level::DEBUG) {
            let body = serde_json::to_string(&final_response).unwrap_or_default();
            debug!("Final Accumulated Response: {}", body);
        }

        let generation_ms = match (first_chunk_at, last_chunk_at) {
            (Some(first), Some(last)) => Some(last.duration_since(first).as_millis() as u64),
            _ => None,
        };
        Ok(ChatCompletionResult {
            response: final_response,
            raw_request: request_body_out,
            timing: TimingMetadata {
                ttft_ms,
                generation_ms,
            },
            provider_backend,
            shrink_info,
        })
    }
}

/// Combined messages + tool-schema token estimate. Both surfaces
/// are tokenized by the provider; counting messages alone
/// under-reserves on agents with non-trivial tool grants and lets
/// the shrink-guard ship requests that exceed `context_window`.
fn estimate_input_tokens(request_config: &RequestConfig) -> Result<i32, LlmError> {
    let messages_json = serde_json::to_string(&request_config.messages)
        .map_err(|e| LlmError::Parse(Box::new(e)))?;
    let tools_len = match request_config.tools.as_ref() {
        Some(t) if !t.is_empty() => serde_json::to_string(t)
            .map_err(|e| LlmError::Parse(Box::new(e)))?
            .len(),
        _ => 0,
    };
    Ok(((messages_json.len() + tools_len) as f32 / 3.0) as i32)
}

fn parse_vllm_context_error(body: &str) -> Option<(u32, u32)> {
    let limit_marker = "maximum context length is ";
    let input_marker = " tokens and your request has ";

    if let (Some(idx_limit), Some(idx_input)) = (body.find(limit_marker), body.find(input_marker)) {
        // Extract numbers safely
        let limit_part = &body[idx_limit + limit_marker.len()..];
        let input_part = &body[idx_input + input_marker.len()..];

        let limit_str = limit_part
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .trim_matches(|c: char| !c.is_numeric());
        let input_str = input_part
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .trim_matches(|c: char| !c.is_numeric());

        if let (Ok(limit), Ok(input)) = (limit_str.parse::<u32>(), input_str.parse::<u32>()) {
            return Some((limit, input));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_vllm_context_error() {
        let error_msg = r#"{"error":{"message":"'max_tokens' or 'max_completion_tokens' is too large: 16000. This model's maximum context length is 21000 tokens and your request has 5395 input tokens (16000 > 21000 - 5395). None","type":"BadRequestError","param":null,"code":400}}"#;
        assert_eq!(parse_vllm_context_error(error_msg), Some((21000, 5395)));

        let error_msg_2 = "This model's maximum context length is 8192 tokens and your request has 10000 input tokens";
        assert_eq!(parse_vllm_context_error(error_msg_2), Some((8192, 10000)));

        let invalid_msg = "Some other error";
        assert_eq!(parse_vllm_context_error(invalid_msg), None);
    }

    // ---------------------------------------------------------------
    // Constructor and builder tests
    // ---------------------------------------------------------------

    #[test]
    fn test_new_constructor() {
        let model = OpenAICompatibleModel::new(
            "https://api.example.com".to_string(),
            "test-key".to_string(),
            None,
        );
        assert_eq!(model.base_url, "https://api.example.com");
        assert_eq!(model.api_key, "test-key");
        assert!(model.engine.is_none());
        assert!(model.semaphore.is_none());
        assert!(model.rate_limiter.is_none());
    }

    #[test]
    fn test_new_with_engine() {
        let model = OpenAICompatibleModel::new(
            "https://api.example.com".to_string(),
            "key".to_string(),
            Some("vllm_xml".to_string()),
        );
        assert_eq!(model.engine.as_deref(), Some("vllm_xml"));
    }

    #[test]
    fn test_with_semaphore_builder() {
        let model = OpenAICompatibleModel::new(
            "https://api.example.com".to_string(),
            "key".to_string(),
            None,
        )
        .with_semaphore(Arc::new(Semaphore::new(5)));

        assert!(model.semaphore.is_some());
    }

    #[tokio::test]
    async fn test_with_rate_limiter_builder() {
        let limiter = Arc::new(RateLimiter::new(10.0));
        let model = OpenAICompatibleModel::new(
            "https://api.example.com".to_string(),
            "key".to_string(),
            None,
        )
        .with_rate_limiter(limiter);

        assert!(model.rate_limiter.is_some());
    }

    #[tokio::test]
    async fn test_builder_chaining() {
        let model = OpenAICompatibleModel::new(
            "https://api.example.com".to_string(),
            "key".to_string(),
            Some("vllm".to_string()),
        )
        .with_semaphore(Arc::new(Semaphore::new(3)))
        .with_rate_limiter(Arc::new(RateLimiter::new(5.0)));

        assert!(model.semaphore.is_some());
        assert!(model.rate_limiter.is_some());
        assert_eq!(model.engine.as_deref(), Some("vllm"));
    }

    // ---------------------------------------------------------------
    // parse_vllm_context_error — additional edge cases
    // ---------------------------------------------------------------

    #[test]
    fn parse_vllm_context_error_only_limit_marker_no_input() {
        // Has the limit marker but not the input marker
        let body = "This model's maximum context length is 4096 tokens";
        assert_eq!(parse_vllm_context_error(body), None);
    }

    #[test]
    fn parse_vllm_context_error_only_input_marker_no_limit() {
        // Has the input marker but not the limit marker
        let body = "tokens and your request has 5000 input tokens";
        assert_eq!(parse_vllm_context_error(body), None);
    }

    #[test]
    fn parse_vllm_context_error_non_numeric_values() {
        // Markers present but values are not numbers
        let body = "maximum context length is abc tokens and your request has xyz input tokens";
        assert_eq!(parse_vllm_context_error(body), None);
    }

    #[test]
    fn parse_vllm_context_error_empty_body() {
        assert_eq!(parse_vllm_context_error(""), None);
    }

    #[test]
    fn parse_vllm_context_error_large_values() {
        let body =
            "maximum context length is 131072 tokens and your request has 120000 input tokens";
        assert_eq!(parse_vllm_context_error(body), Some((131072, 120000)));
    }

    #[test]
    fn parse_vllm_context_error_embedded_in_json_with_escapes() {
        // Realistic vLLM JSON error response with additional text
        let body = r#"{"object":"error","message":"This model's maximum context length is 32768 tokens and your request has 35000 input tokens. Please reduce the length of the messages or completion.","type":"BadRequestError","code":400}"#;
        assert_eq!(parse_vllm_context_error(body), Some((32768, 35000)));
    }

    // ---------------------------------------------------------------
    // URL endpoint construction verification
    // ---------------------------------------------------------------

    #[test]
    fn base_url_cleaning_removes_trailing_slash_and_v1() {
        // Verify the URL cleaning logic used in chat_completion
        let test_cases = vec![
            ("https://api.example.com/v1/", "https://api.example.com"),
            ("https://api.example.com/v1", "https://api.example.com"),
            ("https://api.example.com/", "https://api.example.com"),
            ("https://api.example.com", "https://api.example.com"),
            (
                "https://api.example.com/custom/v1",
                "https://api.example.com/custom",
            ),
        ];

        for (input, expected) in test_cases {
            let base = input.trim_end_matches('/');
            let clean_base = base.strip_suffix("/v1").unwrap_or(base);
            assert_eq!(
                clean_base, expected,
                "URL cleaning failed for input: {}",
                input
            );
        }
    }

    // ---------------------------------------------------------------
    // Strategy resolution from engine field
    // ---------------------------------------------------------------

    #[test]
    fn strategy_resolution_from_engine_field() {
        use crate::llms::strategies::StrategyResolver;

        // vllm engine → XmlRegex strategy (no streaming)
        let strategy = StrategyResolver::resolve(Some("vllm"));
        assert!(
            !strategy.supports_streaming(),
            "vllm strategy should not support streaming"
        );

        // gpt-oss engine → Harmony strategy
        let strategy = StrategyResolver::resolve(Some("gpt-oss"));
        assert_eq!(
            strategy.endpoint_suffix(),
            "/completions",
            "gpt-oss should use /completions endpoint"
        );

        // harmony engine → Harmony strategy
        let strategy = StrategyResolver::resolve(Some("harmony"));
        assert_eq!(strategy.endpoint_suffix(), "/completions");
        assert!(!strategy.supports_streaming());

        // None → Native strategy (streaming enabled)
        let strategy = StrategyResolver::resolve(None);
        assert!(
            strategy.supports_streaming(),
            "default strategy should support streaming"
        );
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");

        // Unknown engine → Native strategy fallback
        let strategy = StrategyResolver::resolve(Some("some_unknown_engine"));
        assert!(strategy.supports_streaming());
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");
    }

    // ---------------------------------------------------------------
    // Context window shrinking logic (pure logic verification)
    // ---------------------------------------------------------------

    #[test]
    fn context_window_shrinking_logic() {
        // Simulate the context window shrinking logic from chat_completion
        struct TestCase {
            context_window: i32,
            estimated_input_tokens: i32,
            requested_max_tokens: u32,
            expected_max_tokens: u32,
        }

        let cases = [
            // Case 1: Plenty of room, no shrinking needed
            TestCase {
                context_window: 32000,
                estimated_input_tokens: 5000,
                requested_max_tokens: 4096,
                expected_max_tokens: 4096,
            },
            // Case 2: Need to shrink — request exceeds available space
            TestCase {
                context_window: 8000,
                estimated_input_tokens: 5000,
                requested_max_tokens: 4096,
                expected_max_tokens: 2500, // 8000 - 5000 - 500(safety) = 2500
            },
            // Case 3: Very tight — would go below 200 minimum
            TestCase {
                context_window: 1000,
                estimated_input_tokens: 900,
                requested_max_tokens: 4096,
                expected_max_tokens: 200, // max(1000-900-500, 200) = 200
            },
            // Case 4: context_window is 0 — no shrinking applied
            TestCase {
                context_window: 0,
                estimated_input_tokens: 5000,
                requested_max_tokens: 4096,
                expected_max_tokens: 4096,
            },
        ];

        for (i, tc) in cases.iter().enumerate() {
            let mut final_max_tokens = tc.requested_max_tokens;

            if tc.context_window > 0 {
                let safety_buffer = 500;
                let available = tc
                    .context_window
                    .saturating_sub(tc.estimated_input_tokens)
                    .saturating_sub(safety_buffer);
                let available_u32 = available.max(200) as u32;

                if final_max_tokens > available_u32 {
                    final_max_tokens = available_u32;
                }
            }

            assert_eq!(
                final_max_tokens, tc.expected_max_tokens,
                "Test case {} failed: context_window={}, input={}, requested={}",
                i, tc.context_window, tc.estimated_input_tokens, tc.requested_max_tokens
            );
        }
    }

    /// Regression for #351 — AdiInternalReviewer scenario: ctx=131072,
    /// max_tokens=131072, modest text input but ~1.8K tokens of tool
    /// schemas. The pre-fix shrink under-reserved by exactly the tool
    /// budget, the request blew past `context_window`, and the
    /// provider returned 400 in a tight retry loop. Drive the
    /// production `estimate_input_tokens` helper directly so the test
    /// is bound to the same code path the SDK runs in `chat_completion`.
    #[test]
    fn estimate_input_tokens_includes_tool_schemas() {
        use crate::llms::RequestConfig;
        use async_openai::types::{
            ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
            ChatCompletionTool, ChatCompletionToolType, FunctionObject,
        };
        use serde_json::json;

        // Three tool schemas roughly the size of the real
        // read_file / grep_search / pdf_query bodies.
        let big_tool = |name: &str| ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: name.into(),
                description: Some(
                    "A non-trivial tool whose JSON-schema body contributes \
                     hundreds of tokens to the provider-side input."
                        .repeat(4),
                ),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Absolute path".repeat(8)},
                        "offset": {"type": "integer", "minimum": 0},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 4096},
                        "regex": {"type": "string", "description": "Anchored pattern".repeat(8)}
                    },
                    "required": ["path"]
                })),
                strict: Some(true),
            },
        };
        let tools = vec![
            big_tool("read_file"),
            big_tool("grep_search"),
            big_tool("pdf_query"),
        ];

        let messages = vec![
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    "Review this kernel patch.".repeat(50),
                ),
                ..Default::default()
            }
            .into(),
        ];

        let no_tools = RequestConfig {
            messages: messages.clone(),
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let with_tools = RequestConfig {
            messages,
            tools: Some(tools),
            tool_choice: None,
            presence_penalty: None,
        };

        let no_tools_estimate = estimate_input_tokens(&no_tools).unwrap();
        let with_tools_estimate = estimate_input_tokens(&with_tools).unwrap();
        let tool_overhead = with_tools_estimate - no_tools_estimate;

        assert!(
            tool_overhead >= 200,
            "tool schemas must contribute ≥200 tokens; got {tool_overhead}"
        );

        // Replay the AdiInternalReviewer ctx=131072 / max=131072
        // scenario through the same shrink math the production path
        // applies post-helper.
        let context_window: i32 = 131_072;
        let safety_buffer: i32 = 500;
        let raw_available = context_window.saturating_sub(with_tools_estimate);
        let final_max_tokens = raw_available
            .saturating_sub(safety_buffer)
            .max(SHRINK_FLOOR_FOR_TEST as i32) as u32;

        let sent = (final_max_tokens as i32) + with_tools_estimate + safety_buffer;
        assert!(
            sent <= context_window,
            "post-fix budget must respect ctx: \
             final_max={final_max_tokens} + input+tools={with_tools_estimate} \
             + safety={safety_buffer} = {sent} > ctx={context_window}"
        );

        // Replay pre-fix behavior: shrink against messages only, then
        // measure what the provider actually sees (messages + tools).
        // The pre-fix path overflows by exactly the tool-token budget,
        // which is what bit AdiInternalReviewer in the kernel-review run.
        let pre_fix_final_max = context_window
            .saturating_sub(no_tools_estimate)
            .saturating_sub(safety_buffer)
            .max(SHRINK_FLOOR_FOR_TEST as i32) as u32;
        let pre_fix_sent = (pre_fix_final_max as i32) + with_tools_estimate + safety_buffer;
        assert!(
            pre_fix_sent > context_window,
            "pre-fix shrink (messages-only) must overflow ctx by ~tool_overhead; \
             pre_fix_final={pre_fix_final_max} + input+tools={with_tools_estimate} \
             + safety={safety_buffer} = {pre_fix_sent} <= ctx={context_window}"
        );
        assert_eq!(
            pre_fix_sent - context_window,
            tool_overhead,
            "overflow must equal the tool-overhead budget; \
             overflow={} tool_overhead={}",
            pre_fix_sent - context_window,
            tool_overhead
        );
    }
    // Mirrors the inline `const SHRINK_FLOOR` in `chat_completion`.
    const SHRINK_FLOOR_FOR_TEST: u32 = 200;

    // ---------------------------------------------------------------
    // Reactive context window shrinking (vLLM 400 error handling)
    // ---------------------------------------------------------------

    #[test]
    fn reactive_shrink_from_vllm_error() {
        // Simulate the reactive context window shrinking from a vLLM 400 error
        let body = r#"{"error":{"message":"This model's maximum context length is 21000 tokens and your request has 18000 input tokens"}}"#;

        if let Some((limit, input)) = parse_vllm_context_error(body) {
            let safety_buffer = 100;
            let available = limit.saturating_sub(input).saturating_sub(safety_buffer);
            let new_max_tokens = available.max(200);

            assert_eq!(limit, 21000);
            assert_eq!(input, 18000);
            // 21000 - 18000 - 100 = 2900
            assert_eq!(new_max_tokens, 2900);
        } else {
            panic!("Should have parsed the vLLM context error");
        }
    }

    #[test]
    fn reactive_shrink_clamps_to_minimum() {
        // When the available space is tiny, it should clamp to 200
        let body = "maximum context length is 5000 tokens and your request has 4950 input tokens";

        if let Some((limit, input)) = parse_vllm_context_error(body) {
            let safety_buffer = 100;
            let available = limit.saturating_sub(input).saturating_sub(safety_buffer);
            let new_max_tokens = available.max(200);

            // 5000 - 4950 - 100 = -50 → saturating_sub gives 0 → max(0, 200) = 200
            assert_eq!(new_max_tokens, 200);
        } else {
            panic!("Should have parsed the vLLM context error");
        }
    }

    // ---------------------------------------------------------------
    // Error classification tests (402, 429, 5xx)
    // ---------------------------------------------------------------

    #[test]
    fn error_status_classification_402() {
        let status = reqwest::StatusCode::from_u16(402).unwrap();
        assert_eq!(status.as_u16(), 402);
        assert!(!status.is_success());
        assert!(!status.is_server_error());
        // 402 is handled separately from 429/5xx — it has its own retry loop
    }

    #[test]
    fn error_status_classification_429() {
        let status = reqwest::StatusCode::from_u16(429).unwrap();
        assert_eq!(status.as_u16(), 429);
        assert!(!status.is_success());
        assert!(!status.is_server_error());
        // 429 triggers the retry with exponential backoff
    }

    #[test]
    fn error_status_classification_5xx() {
        for code in [500, 502, 503, 504] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert!(
                status.is_server_error(),
                "Status {} should be classified as server error",
                code
            );
        }
    }

    #[test]
    fn error_status_classification_non_retryable() {
        // 400, 401, 403, 404 are NOT retryable (except 400 for vLLM context errors)
        for code in [400, 401, 403, 404] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert!(!status.is_server_error());
            assert_ne!(status.as_u16(), 429);
            assert_ne!(status.as_u16(), 402);
        }
    }

    // ---------------------------------------------------------------
    // Backoff calculation tests
    // ---------------------------------------------------------------

    #[test]
    fn backoff_calculation_for_429_retries() {
        // Verify exponential backoff: 2^attempts, max 11 attempts
        let expected = vec![2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048];
        for (i, &exp) in expected.iter().enumerate() {
            let attempts = (i + 1) as u32;
            let sleep_secs = 2u64.pow(attempts);
            assert_eq!(
                sleep_secs, exp,
                "Backoff for attempt {} should be {}s",
                attempts, exp
            );
        }
    }

    #[test]
    fn backoff_calculation_for_402_retries() {
        // 402 uses 2^attempts capped at 30 seconds
        for attempts in 1..=10u32 {
            let sleep_secs = 2u64.pow(attempts).min(30);
            assert!(
                sleep_secs <= 30,
                "402 backoff should be capped at 30s, got {} for attempt {}",
                sleep_secs,
                attempts
            );
        }
        // Verify specific values
        assert_eq!(2u64.pow(1).min(30), 2);
        assert_eq!(2u64.pow(2).min(30), 4);
        assert_eq!(2u64.pow(3).min(30), 8);
        assert_eq!(2u64.pow(4).min(30), 16);
        assert_eq!(2u64.pow(5).min(30), 30); // capped
    }
}
