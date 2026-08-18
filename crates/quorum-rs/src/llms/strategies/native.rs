use super::{ChatStrategy, RequestOverrides};
use crate::agents::config::AgentConfig;
use crate::llms::RequestConfig;
use crate::telemetry::LlmError;
use async_openai::types::{CreateChatCompletionRequest, CreateChatCompletionResponse};
use async_trait::async_trait;
use tracing::{debug, warn};

#[derive(Debug, Default)]
pub struct NativeStrategy {
    engine: Option<String>,
}

impl NativeStrategy {
    pub fn new(engine: &str) -> Self {
        Self {
            engine: Some(engine.to_string()),
        }
    }

    fn extract_from_result_field(&self, value: &serde_json::Value) -> Option<String> {
        if let Some(result) = value.get("result") {
            if let Some(response_str) = result.get("response").and_then(|v| v.as_str()) {
                Some(response_str.to_string())
            } else {
                result.as_str().map(|s| s.to_string())
            }
        } else {
            None
        }
    }

    fn extract_from_output_array(&self, value: &serde_json::Value) -> Option<String> {
        if let Some(outputs) = value.get("output").and_then(|v| v.as_array()) {
            let mut full_text = String::new();
            let mut reasoning_text = String::new();

            for item in outputs {
                let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if item_type == "message" {
                    if let Some(content_arr) = item.get("content").and_then(|v| v.as_array()) {
                        for part in content_arr {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                full_text.push_str(text);
                            }
                        }
                    }
                } else if item_type == "reasoning"
                    && let Some(content_arr) = item.get("content").and_then(|v| v.as_array())
                {
                    for part in content_arr {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            reasoning_text.push_str(text);
                        }
                    }
                }
            }

            if !full_text.is_empty() || !reasoning_text.is_empty() {
                if !reasoning_text.is_empty() {
                    return Some(if !full_text.is_empty() {
                        format!("<think>{}</think>\n{}", reasoning_text, full_text)
                    } else {
                        format!("<think>{}</think>", reasoning_text)
                    });
                } else {
                    return Some(full_text);
                }
            }
        }
        None
    }
}

#[async_trait]
impl ChatStrategy for NativeStrategy {
    async fn prepare_request(
        &self,
        agent: &AgentConfig,
        request_config: &RequestConfig,
        overrides: &RequestOverrides,
    ) -> Result<serde_json::Value, LlmError> {
        let max_tokens = overrides.max_tokens.unwrap_or(agent.max_tokens as u32);
        let presence_penalty = request_config.presence_penalty.or(agent.presence_penalty);

        // Last-line-of-defense history hygiene: ensure every assistant
        // `tool_calls` id has a matching `role: "tool"` follow-up.
        // Strict providers (Cerebras) reject with HTTP 422 otherwise.
        // Runs on every send — idempotent and a no-op when history is
        // already clean, so cost is negligible.
        let mut messages = request_config.messages.clone();
        llm_repair::pair_orphan_tool_calls(&mut messages);

        #[allow(deprecated)]
        let request = CreateChatCompletionRequest {
            model: agent.model_name.clone(),
            temperature: Some(agent.temperature),
            max_tokens: Some(max_tokens),
            frequency_penalty: agent.frequency_penalty,
            presence_penalty,
            messages,
            tools: request_config.tools.clone(),
            tool_choice: request_config.tool_choice.clone(),
            stream: Some(agent.use_streaming),
            ..Default::default()
        };

        let mut v = serde_json::to_value(&request).map_err(|e| LlmError::Parse(e.into()))?;

        // Inject `response_format: {type: "json_object"}` when the agent
        // has `json_mode: true`. Tells providers that support JSON mode
        // (OpenAI, OpenRouter w/ provider-routed backends, Together, etc.)
        // to constrain the model's `content` output to valid JSON.
        //
        // NOTE: json_mode does not affect tool_call arguments — those are
        // already bound to the tool's JSON schema. It's a content-only
        // constraint. Still useful as an additional signal for models
        // (e.g. Gemma-4 on OR) that emit Python-kwargs syntax inside
        // tool-call arguments because they're "thinking in code" mode.
        if agent.json_mode {
            v["response_format"] = serde_json::json!({"type": "json_object"});
        }

        // Ensure all tool definitions have a `parameters` field.
        // OpenAI allows omitting it, but providers like Together AI reject
        // requests where `parameters` is missing from a function definition.
        if let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) {
            for tool in tools {
                if let Some(func) = tool.get_mut("function")
                    && func.get("parameters").is_none()
                {
                    func["parameters"] = serde_json::json!({"type": "object", "properties": {}});
                }
            }
        }

        // Apply engine-specific quirks
        if self.engine.as_deref() == Some("vllm_responses") {
            // vLLM Native Responses API: Remap 'messages' -> 'input'
            if let Some(msgs) = v.get("messages") {
                let mut sanitized_msgs = msgs.clone();

                // Sanitize history to prevent "Unknown recipient" errors on Cloudflare/vLLM
                // when using manual tools (gpt-oss hallucinates native tool syntax which backend rejects)
                if let Some(arr) = sanitized_msgs.as_array_mut() {
                    for msg in arr {
                        if let Some(role) = msg.get("role").and_then(|r| r.as_str())
                            && role == "assistant"
                            && let Some(content_val) = msg.get_mut("content")
                            && let Some(content_str) = content_val.as_str()
                            && content_str.contains("[tool=")
                        {
                            warn!(
                                "Sanitizing hallucinated tool call syntax in history: replacing '[tool=' with '[call=' to prevent backend rejection."
                            );
                            // Break the pattern so backend treats it as plain text
                            *content_val =
                                serde_json::Value::String(content_str.replace("[tool=", "[call="));
                        }
                    }
                }

                v["input"] = sanitized_msgs;
                if let Some(obj) = v.as_object_mut() {
                    obj.remove("messages");
                }
            }
            // Responses API often prefers non-streaming for reasoning models
            if v.get("stream").is_some()
                && let Some(obj) = v.as_object_mut()
            {
                obj.remove("stream");
            }

            // Cloudflare @cf/ models are strict about input schema
            if agent.model_name.starts_with("@cf/") {
                if let Some(obj) = v.as_object_mut() {
                    // Cloudflare Responses API schema does not support these OpenAI standard fields
                    obj.remove("temperature");
                    obj.remove("max_tokens");
                    obj.remove("frequency_penalty");
                    obj.remove("presence_penalty");
                    obj.remove("tools");
                    obj.remove("tool_choice");
                    obj.remove("top_p");
                    obj.remove("n");
                    obj.remove("stop");
                    obj.remove("logit_bias");
                    obj.remove("user");
                    obj.remove("seed");
                }

                // Map reasoning effort to Cloudflare structure
                if let Some(effort) = &agent.reasoning_effort {
                    v["reasoning"] = serde_json::json!({ "effort": effort });
                }
            } else if let Some(effort) = &agent.reasoning_effort {
                v["reasoning_effort"] = serde_json::json!(effort);
            }
        } else {
            // Inject stream_options
            if agent.use_streaming {
                v["stream_options"] = serde_json::json!({"include_usage": true});
            }

            // Inject reasoning_effort if specified (TogetherAI / Cloudflare).
            //
            // When the agent's OpenRouter block requests
            // `exclude_reasoning: true`, switch to the unified
            // `reasoning: { effort, exclude: true }` object so OpenRouter
            // keeps internal chain-of-thought at the requested effort
            // level but strips it from the visible `content` stream.
            // Required for models (e.g. gpt-oss-120b) that otherwise
            // dump reasoning into `content` and leave zero budget for
            // the actual structured-output tool call.
            //
            // Gate OR-specific semantics on `engine == "openrouter"`.
            // The `openrouter` config block may be populated for an
            // agent whose engine is later flipped to TogetherAI /
            // Cloudflare; we must not emit OpenRouter-flavoured
            // `reasoning.exclude` or a `provider` block on a non-OR
            // endpoint that would reject or silently misinterpret it.
            let is_openrouter_engine = self.engine.as_deref() == Some("openrouter");
            let exclude_reasoning = is_openrouter_engine
                && agent
                    .openrouter
                    .as_ref()
                    .and_then(|or| or.exclude_reasoning)
                    .unwrap_or(false);
            if let Some(effort) = &agent.reasoning_effort {
                if exclude_reasoning {
                    v["reasoning"] = serde_json::json!({ "effort": effort, "exclude": true });
                } else if self.engine.as_deref() == Some("cloudflare") {
                    // Cloudflare: "reasoning": { "effort": "medium" }
                    v["reasoning"] = serde_json::json!({ "effort": effort });
                } else {
                    // TogetherAI (Default): "reasoning_effort": "medium"
                    v["reasoning_effort"] = serde_json::json!(effort);
                }
            } else if exclude_reasoning {
                // No effort set but still want reasoning excluded from
                // content (e.g. model reasons by default on OR).
                v["reasoning"] = serde_json::json!({ "exclude": true });
            }

            // Inject OpenRouter provider routing block when configured
            // AND the engine is openrouter. Non-OR providers generally
            // ignore an unknown `provider` key but some reject it; keep
            // the emission scope tight to avoid surprise rejections.
            // Docs: https://openrouter.ai/docs/guides/routing/provider-selection
            if is_openrouter_engine && let Some(or) = &agent.openrouter {
                let mut provider_obj = serde_json::Map::new();
                if let Some(sort) = &or.provider_sort {
                    provider_obj.insert("sort".to_string(), serde_json::json!(sort));
                }
                if let Some(zdr) = or.zdr {
                    provider_obj.insert("zdr".to_string(), serde_json::json!(zdr));
                }
                if let Some(af) = or.allow_fallbacks {
                    provider_obj.insert("allow_fallbacks".to_string(), serde_json::json!(af));
                }
                if !or.ignore.is_empty() {
                    provider_obj.insert("ignore".to_string(), serde_json::json!(or.ignore));
                }
                if !or.only.is_empty() {
                    provider_obj.insert("only".to_string(), serde_json::json!(or.only));
                }
                if !provider_obj.is_empty() {
                    v["provider"] = serde_json::Value::Object(provider_obj);
                }

                // Web-search plugin is opt-in per agent. When configured, emit
                // `plugins: [{ "id": "web", ... }]`; otherwise pin `plugins: []`
                // so the request is explicit about the no-plugin stance — no
                // provider default or model-inherent tool access can silently
                // enable outbound network calls on an agent that didn't ask.
                if let Some(ws) = &or.web_search {
                    let mut web = serde_json::Map::new();
                    web.insert("id".to_string(), serde_json::json!("web"));
                    if let Some(engine) = &ws.engine {
                        web.insert("engine".to_string(), serde_json::json!(engine));
                    }
                    if let Some(n) = ws.max_results {
                        web.insert("max_results".to_string(), serde_json::json!(n));
                    }
                    if let Some(p) = &ws.search_prompt {
                        web.insert("search_prompt".to_string(), serde_json::json!(p));
                    }
                    v["plugins"] = serde_json::json!([serde_json::Value::Object(web)]);
                } else {
                    v["plugins"] = serde_json::json!([]);
                }
            }
        }

        if tracing::enabled!(tracing::Level::DEBUG) {
            debug!("NativeStrategy Prepared Body: {}", v);
        }

        // Opt-in request dump (NSED_DUMP_PROMPTS_DIR) — the final Chat Completions
        // request JSON as sent, for token-efficiency analysis. Best-effort.
        crate::dump_prompts::dump(
            &crate::dump_prompts::Meta {
                agent: &agent.name,
                round: None,
                phase: None,
            },
            self.engine.as_deref().unwrap_or("openai"),
            "json",
            &serde_json::to_string_pretty(&v).unwrap_or_default(),
        );

        Ok(v)
    }

    async fn parse_response(
        &self,
        response_body: &str,
    ) -> Result<CreateChatCompletionResponse, LlmError> {
        // Parse into generic Value first to handle provider quirks
        let mut value: serde_json::Value =
            serde_json::from_str(response_body).map_err(|e| LlmError::Parse(e.into()))?;

        // Cloudflare returns "service_tier": "auto" which fails strict enum parsing in async-openai
        if let Some(tier) = value.get("service_tier").and_then(|v| v.as_str())
            && tier == "auto"
            && let Some(obj) = value.as_object_mut()
        {
            obj.remove("service_tier");
        }

        // Patch missing usage fields (Cloudflare sometimes returns incomplete usage objects)
        if let Some(usage) = value.get_mut("usage").and_then(|u| u.as_object_mut()) {
            if !usage.contains_key("prompt_tokens") {
                // Map Cloudflare's "input_tokens" if available
                if let Some(input) = usage.get("input_tokens").cloned() {
                    usage.insert("prompt_tokens".to_string(), input);
                } else {
                    usage.insert("prompt_tokens".to_string(), serde_json::json!(0));
                }
            }
            if !usage.contains_key("completion_tokens") {
                // Map Cloudflare's "output_tokens" if available
                if let Some(output) = usage.get("output_tokens").cloned() {
                    usage.insert("completion_tokens".to_string(), output);
                } else {
                    usage.insert("completion_tokens".to_string(), serde_json::json!(0));
                }
            }
            if !usage.contains_key("total_tokens") {
                let prompt = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let completion = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                usage.insert(
                    "total_tokens".to_string(),
                    serde_json::json!(prompt + completion),
                );
            }
        }

        // Handle Cloudflare's non-standard response format where 'choices' is missing or null
        if value.get("choices").is_none() || value.get("choices").is_some_and(|v| v.is_null()) {
            let mut extracted_content = self.extract_from_result_field(&value);

            if extracted_content.is_none() {
                extracted_content = self.extract_from_output_array(&value);
            }

            if let Some(text) = extracted_content {
                let choice = serde_json::json!({
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": text
                    },
                    "finish_reason": "stop"
                });

                if let Some(obj) = value.as_object_mut() {
                    obj.insert("choices".to_string(), serde_json::json!([choice]));
                    // Ensure minimal required fields
                    if !obj.contains_key("id") {
                        obj.insert("id".to_string(), serde_json::json!("cf-response"));
                    }
                    if !obj.contains_key("object") {
                        obj.insert("object".to_string(), serde_json::json!("chat.completion"));
                    }
                    if !obj.contains_key("created") {
                        obj.insert("created".to_string(), serde_json::json!(0));
                    }
                    if !obj.contains_key("model") {
                        obj.insert("model".to_string(), serde_json::json!("unknown"));
                    }
                }
            }
        }

        // Fallback: If choices is still missing or null, synthesize a minimal response.
        // In debug builds, include truncated raw JSON for diagnostics.
        // In release builds, use a generic message to avoid leaking provider internals.
        if value.get("choices").is_none() || value.get("choices").is_some_and(|v| v.is_null()) {
            let content = if cfg!(debug_assertions) {
                let dump = serde_json::to_string_pretty(&value).unwrap_or_default();
                let truncated_dump = if dump.len() > 500 {
                    format!("{}... (truncated)", &dump[..500])
                } else {
                    dump
                };
                format!(
                    "DEBUG: Failed to parse response structure. Raw body:\n{}",
                    truncated_dump
                )
            } else {
                "Unable to parse response from provider.".to_string()
            };
            let choice = serde_json::json!({
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content
                },
                "finish_reason": "stop"
            });

            if let Some(obj) = value.as_object_mut() {
                obj.insert("choices".to_string(), serde_json::json!([choice]));
                // Ensure minimal required fields
                if !obj.contains_key("id") {
                    obj.insert("id".to_string(), serde_json::json!("fallback-response"));
                }
                if !obj.contains_key("object") {
                    obj.insert("object".to_string(), serde_json::json!("chat.completion"));
                }
                if !obj.contains_key("created") {
                    obj.insert("created".to_string(), serde_json::json!(0));
                }
                if !obj.contains_key("model") {
                    obj.insert("model".to_string(), serde_json::json!("unknown"));
                }
            }
        }

        let response: CreateChatCompletionResponse =
            serde_json::from_value(value).map_err(|e| LlmError::Parse(e.into()))?;
        Ok(response)
    }

    fn endpoint_suffix(&self) -> &str {
        if self.engine.as_deref() == Some("vllm_responses") {
            "/responses"
        } else {
            "/chat/completions"
        }
    }

    fn supports_streaming(&self) -> bool {
        self.engine.as_deref() != Some("vllm_responses")
    }

    /// Gateways fronting the OpenAI-compatible API report the true cost beside
    /// `usage`; first-party OpenAI and self-hosted backends report none, and
    /// this yields all-`None` for them without special-casing either.
    fn provider_usage(&self, response: &serde_json::Value) -> crate::llms::ProviderUsage {
        response
            .get("usage")
            .map(crate::llms::ProviderUsage::from_usage_value)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::{
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent,
    };

    #[tokio::test]
    async fn test_prepare_request_vllm_quirks() {
        let strategy = NativeStrategy::new("vllm_responses");
        let agent = AgentConfig {
            name: "test".to_string(),
            provider_id: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.0,
            max_tokens: 100,
            system_prompt_override: None,
            persona: None,
            max_react_iterations: None,
            max_scratchpad_size: None,
            tool_format: None,
            max_retries: None,
            supports_native_thinking: false,
            frequency_penalty: None,
            presence_penalty: None,
            textual_feedback: false,
            use_streaming: true, // Should be disabled by quirk logic
            merge_system_prompt: false,
            unwrap_hallucinated_tool_calls: false,
            repair_invalid_escapes: false,
            scratchpad_limit: 0,
            json_mode: false,
            disable_native_tools: false,
            context_window: 0,
            reasoning_effort: None,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            chars_per_token: None,
            task_precision: None,
            orchestrators: vec![],
            failure_dumps: None,
            response_sla_secs: 300,
            propagate_payment_error: true,
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hello".to_string()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let overrides = RequestOverrides::default();

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();

        // Verify "messages" -> "input" mapping
        assert!(body.get("messages").is_none());
        assert!(body.get("input").is_some());

        // Verify "stream" is removed
        assert!(body.get("stream").is_none());
    }

    #[tokio::test]
    async fn test_parse_response_cloudflare_result_string() {
        let strategy = NativeStrategy::new("test");
        // Simulate Cloudflare returning {"result": "The answer is 42"}
        let body = r#"{
            "result": "The answer is 42",
            "success": true,
            "errors": [],
            "messages": []
        }"#;

        let response = strategy.parse_response(body).await.expect("Should parse");
        let content = response.choices[0].message.content.as_ref().unwrap();
        assert_eq!(content, "The answer is 42");
    }

    #[tokio::test]
    async fn test_parse_response_vllm_output_array() {
        let strategy = NativeStrategy::new("test");
        // vLLM / GPT-OSS format with reasoning
        let body = r#"{
            "output": [
                { "type": "reasoning", "content": [{"text": "Thinking..."}] },
                { "type": "message", "content": [{"text": "42"}] }
            ],
            "id": "test",
            "created": 123,
            "model": "gpt-oss",
            "object": "chat.completion"
        }"#;

        let response = strategy.parse_response(body).await.expect("Should parse");
        let content = response.choices[0].message.content.as_ref().unwrap();
        assert_eq!(content, "<think>Thinking...</think>\n42");
    }

    #[tokio::test]
    async fn test_parse_response_debug_fallback() {
        let strategy = NativeStrategy::new("test");
        // Malformed structure
        let body = r#"{ "foo": "bar" }"#;

        let response = strategy
            .parse_response(body)
            .await
            .expect("Should parse via fallback");
        let content = response.choices[0].message.content.as_ref().unwrap();
        assert!(content.contains("DEBUG: Failed to parse response structure"));
        assert!(content.contains("foo"));
        assert_eq!(response.id, "fallback-response");
    }

    #[tokio::test]
    async fn test_parse_response_usage_patching() {
        let strategy = NativeStrategy::new("test");
        // Cloudflare input_tokens -> prompt_tokens mapping
        let body = r#"{
            "id": "test-id",
            "created": 123,
            "model": "test-model",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi"}, "finish_reason": "stop"}],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20
            }
        }"#;

        let response = strategy.parse_response(body).await.expect("Should parse");
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        // total_tokens is computed from prompt + completion when missing
        assert_eq!(usage.total_tokens, 30);
    }

    #[test]
    fn the_openai_compatible_dialect_reads_a_gateway_reported_cost() {
        let body = serde_json::json!({
            "usage": { "prompt_tokens": 6, "completion_tokens": 2, "cost": 7.434e-07,
                       "prompt_tokens_details": { "cached_tokens": 4, "cache_write_tokens": 2 } }
        });

        let usage = NativeStrategy::default().provider_usage(&body);

        assert_eq!(usage.cost_usd, Some(7.434e-07));
        assert_eq!(usage.cached_tokens, Some(4));
        assert_eq!(usage.cache_write_tokens, Some(2));
    }

    #[test]
    fn a_provider_that_reports_no_cost_yields_nothing() {
        // First-party OpenAI and self-hosted backends: tokens, no cost. The
        // caller falls back to its price list rather than inventing a figure.
        let body = serde_json::json!({
            "usage": { "prompt_tokens": 6, "completion_tokens": 2 }
        });

        let usage = NativeStrategy::default().provider_usage(&body);

        assert_eq!(usage.cost_usd, None);
    }

    #[test]
    fn a_response_with_no_usage_at_all_yields_nothing() {
        let usage = NativeStrategy::default().provider_usage(&serde_json::json!({"id": "x"}));

        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.cached_tokens, None);
    }

    #[tokio::test]
    async fn test_prepare_request_injects_missing_parameters() {
        // Together AI (and other providers) require `parameters` on all function tools.
        // Tools without parameters (like user_dm_user) should get an empty object schema.
        use async_openai::types::{ChatCompletionTool, ChatCompletionToolType, FunctionObject};

        let strategy = NativeStrategy::default();
        let agent = AgentConfig {
            name: "test".to_string(),
            provider_id: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.0,
            max_tokens: 100,
            system_prompt_override: None,
            persona: None,
            max_react_iterations: None,
            max_scratchpad_size: None,
            tool_format: None,
            max_retries: None,
            supports_native_thinking: false,
            frequency_penalty: None,
            presence_penalty: None,
            textual_feedback: false,
            use_streaming: false,
            merge_system_prompt: false,
            unwrap_hallucinated_tool_calls: false,
            repair_invalid_escapes: false,
            scratchpad_limit: 0,
            json_mode: false,
            disable_native_tools: false,
            context_window: 0,
            reasoning_effort: None,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            chars_per_token: None,
            task_precision: None,
            orchestrators: vec![],
            failure_dumps: None,
            response_sla_secs: 300,
            propagate_payment_error: true,
            ..Default::default()
        };

        // One tool WITH parameters, one WITHOUT
        let tools = vec![
            ChatCompletionTool {
                r#type: ChatCompletionToolType::Function,
                function: FunctionObject {
                    name: "submit_proposal".to_string(),
                    description: Some("Submit a proposal".to_string()),
                    parameters: Some(serde_json::json!({
                        "type": "object",
                        "properties": { "content": { "type": "string" } },
                        "required": ["content"]
                    })),
                    strict: Some(true),
                },
            },
            ChatCompletionTool {
                r#type: ChatCompletionToolType::Function,
                function: FunctionObject {
                    name: "user_dm_user".to_string(),
                    description: Some("Send a DM to the user".to_string()),
                    parameters: None, // <-- This is the problematic case
                    strict: None,
                },
            },
        ];

        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hello".to_string()),
                    name: None,
                },
            )],
            tools: Some(tools),
            tool_choice: None,
            presence_penalty: None,
        };
        let overrides = RequestOverrides::default();

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();

        let tools_json = body.get("tools").unwrap().as_array().unwrap();

        // Tool 0 (submit_proposal) should keep its original parameters
        let params_0 = tools_json[0]["function"]["parameters"].as_object().unwrap();
        assert!(params_0.contains_key("properties"));
        assert!(params_0.contains_key("required"));

        // Tool 1 (user_dm_user) should now have a default empty parameters object
        let params_1 = &tools_json[1]["function"]["parameters"];
        assert!(!params_1.is_null(), "parameters must not be null");
        assert!(params_1.is_object(), "parameters must be an object");
        assert_eq!(params_1["type"], "object");
        assert!(params_1["properties"].is_object());
    }

    #[tokio::test]
    async fn test_prepare_request_injects_openrouter_provider_block() {
        use crate::agents::config::OpenRouterConfig;

        // Provider-block emission is gated on engine=="openrouter";
        // the default NativeStrategy has engine=None so it wouldn't
        // emit the block even with an OR config present.
        let strategy = NativeStrategy::new("openrouter");
        let agent = AgentConfig {
            name: "cortex_a".to_string(),
            provider_id: "openrouter".to_string(),
            model_name: "google/gemma-4-26b-a4b-it".to_string(),
            temperature: 0.7,
            max_tokens: 16384,
            use_streaming: false,
            openrouter: Some(OpenRouterConfig {
                provider_sort: Some("throughput".to_string()),
                zdr: Some(true),
                allow_fallbacks: Some(false),
                ignore: vec!["nextbit".to_string()],
                only: vec!["akashml/fp8".to_string(), "parasail/fp8".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hi".to_string()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let body = strategy
            .prepare_request(&agent, &request_config, &RequestOverrides::default())
            .await
            .unwrap();

        let provider = body.get("provider").expect("provider block must exist");
        assert_eq!(provider["sort"], "throughput");
        assert_eq!(provider["zdr"], true);
        assert_eq!(provider["allow_fallbacks"], false);
        let ignore = provider["ignore"].as_array().expect("ignore is array");
        assert_eq!(ignore.len(), 1);
        assert_eq!(ignore[0], "nextbit");
        let only = provider["only"].as_array().expect("only is array");
        assert_eq!(only.len(), 2);
        assert_eq!(only[0], "akashml/fp8");
        assert_eq!(only[1], "parasail/fp8");

        // Belt-and-suspenders: an explicit empty plugins array must
        // accompany every OR request so no amount of future default
        // plugin activation can silently enable web search or other
        // outbound-network tools.
        let plugins = body.get("plugins").expect("plugins array must exist");
        assert!(plugins.is_array(), "plugins must be a JSON array");
        assert_eq!(
            plugins.as_array().unwrap().len(),
            0,
            "plugins array must be empty"
        );
    }

    #[tokio::test]
    async fn test_prepare_request_injects_web_search_plugin() {
        use crate::agents::config::{OpenRouterConfig, WebSearchConfig};

        let strategy = NativeStrategy::new("openrouter");
        let agent = AgentConfig {
            name: "searcher".to_string(),
            provider_id: "openrouter".to_string(),
            model_name: "openai/gpt-5.6-luna".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            use_streaming: false,
            openrouter: Some(OpenRouterConfig {
                zdr: Some(true),
                allow_fallbacks: Some(true),
                web_search: Some(WebSearchConfig {
                    engine: Some("exa".to_string()),
                    max_results: Some(3),
                    search_prompt: Some("Prefer peer-reviewed sources.".to_string()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hi".to_string()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let body = strategy
            .prepare_request(&agent, &request_config, &RequestOverrides::default())
            .await
            .unwrap();

        let plugins = body.get("plugins").expect("plugins array must exist");
        let arr = plugins.as_array().expect("plugins is array");
        assert_eq!(arr.len(), 1, "web-search plugin must be present");
        assert_eq!(arr[0]["id"], "web");
        assert_eq!(arr[0]["engine"], "exa");
        assert_eq!(arr[0]["max_results"], 3);
        assert_eq!(arr[0]["search_prompt"], "Prefer peer-reviewed sources.");
        // ZDR + fallback still emitted alongside the plugin.
        assert_eq!(body["provider"]["zdr"], true);
        assert_eq!(body["provider"]["allow_fallbacks"], true);
    }

    #[tokio::test]
    async fn test_prepare_request_no_plugins_field_without_openrouter_block() {
        // Agents without an `openrouter` config block (Together, Cloudflare,
        // Ollama, etc.) must NOT get a `plugins` field — it's an OR-only
        // extension and unrelated providers would either error or misroute.
        let strategy = NativeStrategy::default();
        let agent = AgentConfig {
            name: "together_agent".to_string(),
            provider_id: "together_ai".to_string(),
            model_name: "some/model".to_string(),
            temperature: 0.5,
            max_tokens: 512,
            use_streaming: false,
            openrouter: None,
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hi".to_string()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let body = strategy
            .prepare_request(&agent, &request_config, &RequestOverrides::default())
            .await
            .unwrap();

        assert!(
            body.get("plugins").is_none(),
            "plugins must not leak into non-OpenRouter requests"
        );
    }

    #[tokio::test]
    async fn test_prepare_request_exclude_reasoning_switches_to_unified_field() {
        // With `exclude_reasoning: true`, reasoning stays at configured
        // effort level (quality preserved) but is stripped from visible
        // content. Required for gpt-oss-120b on OpenRouter — otherwise the
        // model spends its entire output budget on reasoning tokens and
        // never emits the submit_proposal tool call.
        use crate::agents::config::OpenRouterConfig;

        // reasoning.exclude unified field is OR-specific; needs engine.
        let strategy = NativeStrategy::new("openrouter");
        let agent = AgentConfig {
            name: "gptoss".to_string(),
            provider_id: "openrouter".to_string(),
            model_name: "openai/gpt-oss-120b".to_string(),
            temperature: 0.5,
            max_tokens: 4096,
            use_streaming: false,
            reasoning_effort: Some("medium".to_string()),
            openrouter: Some(OpenRouterConfig {
                exclude_reasoning: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hi".to_string()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let body = strategy
            .prepare_request(&agent, &request_config, &RequestOverrides::default())
            .await
            .unwrap();

        // Legacy field must NOT be emitted.
        assert!(body.get("reasoning_effort").is_none());
        // Unified reasoning.effort + reasoning.exclude must both be set.
        let reasoning = body.get("reasoning").expect("reasoning must exist");
        assert_eq!(reasoning["effort"], "medium");
        assert_eq!(reasoning["exclude"], true);
    }

    #[tokio::test]
    async fn test_prepare_request_exclude_reasoning_without_effort() {
        // Without any reasoning_effort but exclude_reasoning=true — emit
        // just `reasoning: {exclude: true}` so models that reason by
        // default (OpenRouter-hosted gpt-oss with no explicit effort)
        // strip their reasoning from the response.
        use crate::agents::config::OpenRouterConfig;

        // reasoning.exclude unified field is OR-specific; needs engine.
        let strategy = NativeStrategy::new("openrouter");
        let agent = AgentConfig {
            name: "gptoss".to_string(),
            provider_id: "openrouter".to_string(),
            model_name: "openai/gpt-oss-120b".to_string(),
            temperature: 0.5,
            max_tokens: 4096,
            use_streaming: false,
            reasoning_effort: None,
            openrouter: Some(OpenRouterConfig {
                exclude_reasoning: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hi".to_string()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let body = strategy
            .prepare_request(&agent, &request_config, &RequestOverrides::default())
            .await
            .unwrap();

        assert!(body.get("reasoning_effort").is_none());
        let reasoning = body.get("reasoning").expect("reasoning must exist");
        assert_eq!(reasoning["exclude"], true);
        assert!(reasoning.get("effort").is_none());
    }

    #[tokio::test]
    async fn test_prepare_request_reasoning_medium_still_uses_legacy_field() {
        // Regression guard: non-sentinel values must keep emitting the legacy
        // reasoning_effort field so existing OpenRouter/TogetherAI routing is
        // unchanged.
        let strategy = NativeStrategy::default();
        let agent = AgentConfig {
            name: "gptoss".to_string(),
            provider_id: "openrouter".to_string(),
            model_name: "openai/gpt-oss-120b".to_string(),
            temperature: 0.5,
            max_tokens: 4096,
            use_streaming: false,
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hi".to_string()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let body = strategy
            .prepare_request(&agent, &request_config, &RequestOverrides::default())
            .await
            .unwrap();

        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("reasoning").is_none());
    }

    #[tokio::test]
    async fn test_prepare_request_omits_provider_block_when_unset() {
        let strategy = NativeStrategy::default();
        let agent = AgentConfig {
            name: "no_or".to_string(),
            provider_id: "openrouter".to_string(),
            model_name: "x".to_string(),
            temperature: 0.5,
            max_tokens: 100,
            use_streaming: false,
            openrouter: None,
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hi".to_string()),
                    name: None,
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
        };
        let body = strategy
            .prepare_request(&agent, &request_config, &RequestOverrides::default())
            .await
            .unwrap();

        assert!(body.get("provider").is_none());
    }

    #[tokio::test]
    async fn test_parse_response_service_tier_removal() {
        let strategy = NativeStrategy::new("test");
        // "service_tier": "auto" cleanup
        let body = r#"{
            "id": "test-id",
            "created": 123,
            "model": "test-model",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi"}, "finish_reason": "stop"}],
            "service_tier": "auto"
        }"#;

        let response = strategy.parse_response(body).await.expect("Should parse");
        assert!(response.service_tier.is_none());
    }

    // --- extract_from_result_field tests ---

    #[test]
    fn test_extract_from_result_field_string() {
        let strategy = NativeStrategy::new("test");
        let value: serde_json::Value = serde_json::json!({"result": "hello"});
        let result = strategy.extract_from_result_field(&value);
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn test_extract_from_result_field_nested() {
        let strategy = NativeStrategy::new("test");
        let value: serde_json::Value = serde_json::json!({"result": {"response": "nested text"}});
        let result = strategy.extract_from_result_field(&value);
        assert_eq!(result, Some("nested text".to_string()));
    }

    #[test]
    fn test_extract_from_result_field_missing() {
        let strategy = NativeStrategy::new("test");
        let value: serde_json::Value = serde_json::json!({"foo": "bar"});
        let result = strategy.extract_from_result_field(&value);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_from_result_field_null() {
        let strategy = NativeStrategy::new("test");
        let value: serde_json::Value = serde_json::json!({"result": null});
        let result = strategy.extract_from_result_field(&value);
        assert_eq!(result, None);
    }

    // --- extract_from_output_array tests ---

    #[test]
    fn test_extract_from_output_array_message_only() {
        let strategy = NativeStrategy::new("test");
        let value: serde_json::Value = serde_json::json!({
            "output": [
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "hello"}]
                }
            ]
        });
        let result = strategy.extract_from_output_array(&value);
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn test_extract_from_output_array_with_reasoning() {
        let strategy = NativeStrategy::new("test");
        let value: serde_json::Value = serde_json::json!({
            "output": [
                {
                    "type": "reasoning",
                    "content": [{"text": "Let me think about this..."}]
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "The answer is 42"}]
                }
            ]
        });
        let result = strategy.extract_from_output_array(&value);
        let text = result.expect("Should return Some");
        assert!(
            text.contains("The answer is 42"),
            "Should contain the message text"
        );
        assert!(
            text.contains("<think>Let me think about this...</think>"),
            "Should contain reasoning wrapped in think tags"
        );
    }

    #[test]
    fn test_extract_from_output_array_empty() {
        let strategy = NativeStrategy::new("test");
        let value: serde_json::Value = serde_json::json!({"output": []});
        let result = strategy.extract_from_output_array(&value);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_from_output_array_missing() {
        let strategy = NativeStrategy::new("test");
        let value: serde_json::Value = serde_json::json!({"foo": "bar"});
        let result = strategy.extract_from_output_array(&value);
        assert_eq!(result, None);
    }

    // --- endpoint_suffix tests ---

    #[test]
    fn test_endpoint_suffix_default() {
        let strategy = NativeStrategy::new("");
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");
    }

    #[test]
    fn test_endpoint_suffix_vllm() {
        let strategy = NativeStrategy::new("vllm_responses");
        assert_eq!(strategy.endpoint_suffix(), "/responses");
    }

    // --- supports_streaming tests ---

    #[test]
    fn test_supports_streaming_default() {
        let strategy = NativeStrategy::new("");
        assert!(strategy.supports_streaming());
    }

    #[test]
    fn test_supports_streaming_vllm() {
        let strategy = NativeStrategy::new("vllm_responses");
        assert!(!strategy.supports_streaming());
    }
}
