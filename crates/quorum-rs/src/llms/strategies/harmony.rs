use super::{ChatStrategy, RequestOverrides};
use crate::agents::config::AgentConfig;
use crate::llms::RequestConfig;
use crate::telemetry::LlmError;
use anyhow::Result;
use async_openai::types::{
    ChatChoice, ChatCompletionMessageToolCall, ChatCompletionRequestAssistantMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessageContent, ChatCompletionResponseMessage, ChatCompletionToolType,
    CompletionUsage, CreateChatCompletionResponse, FinishReason, FunctionCall, Role as OpenAIRole,
};
use async_trait::async_trait;
use openai_harmony::chat::{
    Author, Conversation, DeveloperContent, Message, Role, SystemContent, ToolDescription,
};
use openai_harmony::{HarmonyEncodingName, load_harmony_encoding};
use serde_json::{Value, json};
use tracing::{debug, warn};

pub struct HarmonyStrategy;

impl Default for HarmonyStrategy {
    fn default() -> Self {
        Self
    }
}

impl HarmonyStrategy {
    /// Convert generic OpenAI tools to Harmony ToolDescriptions
    fn convert_tools(
        &self,
        tools: &[async_openai::types::ChatCompletionTool],
    ) -> Vec<ToolDescription> {
        tools
            .iter()
            .map(|t| {
                let params = t.function.parameters.clone().unwrap_or(json!({}));
                ToolDescription::new(
                    t.function.name.clone(),
                    t.function.description.clone().unwrap_or_default(),
                    Some(params),
                )
            })
            .collect()
    }
}

#[async_trait]
impl ChatStrategy for HarmonyStrategy {
    async fn prepare_request(
        &self,
        agent: &AgentConfig,
        request: &RequestConfig,
        overrides: &RequestOverrides,
    ) -> Result<serde_json::Value, LlmError> {
        let encoding = tokio::task::spawn_blocking(move || {
            load_harmony_encoding(HarmonyEncodingName::HarmonyGptOss)
        })
        .await
        .map_err(|e| LlmError::Other(e.into()))?
        .map_err(|e| LlmError::Other(e.into()))?;

        // 1. Build Harmony Conversation
        // Reasoning Effort Mapping
        let effort = match agent.reasoning_effort.as_deref() {
            Some("high") => openai_harmony::chat::ReasoningEffort::High,
            Some("low") => openai_harmony::chat::ReasoningEffort::Low,
            _ => openai_harmony::chat::ReasoningEffort::Medium,
        };

        let system_content = SystemContent::new().with_reasoning_effort(effort);

        let mut harmony_messages =
            vec![Message::from_role_and_content(Role::System, system_content)];

        // 2. Handle Developer Message (Instructions + Tools)
        let mut developer_content = DeveloperContent::new();

        // Extract instructions from NSED System messages
        let mut instructions = String::new();
        for msg in &request.messages {
            if let ChatCompletionRequestMessage::System(sys) = msg
                && let async_openai::types::ChatCompletionRequestSystemMessageContent::Text(t) =
                    &sys.content
            {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(t);
            }
        }
        if !instructions.is_empty() {
            developer_content = developer_content.with_instructions(instructions);
        }

        // Add Tools
        if let Some(tools) = &request.tools {
            let harmony_tools = self.convert_tools(tools);
            developer_content = developer_content.with_function_tools(harmony_tools);
        }

        harmony_messages.push(Message::from_role_and_content(
            Role::Developer,
            developer_content,
        ));

        // 3. Map remaining messages
        for msg in &request.messages {
            match msg {
                ChatCompletionRequestMessage::User(u) => {
                    let content = &u.content;
                    let text = match content {
                        ChatCompletionRequestUserMessageContent::Text(t) => t.clone(),
                        ChatCompletionRequestUserMessageContent::Array(parts) => parts
                            .iter()
                            .map(|p| {
                                let v = serde_json::to_value(p).unwrap_or_default();
                                if v.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    v.get("text")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string()
                                } else {
                                    warn!("HarmonyStrategy: Non-text content in User message partially supported (placeholder inserted)");
                                    "[NON_TEXT_CONTENT]".to_string()
                                }
                            })
                            .collect::<Vec<String>>()
                            .join(" "),
                    };
                    harmony_messages.push(Message::from_role_and_content(Role::User, text));
                }
                ChatCompletionRequestMessage::Assistant(a) => {
                    if let Some(content) = &a.content {
                        let text = match content {
                            ChatCompletionRequestAssistantMessageContent::Text(t) => t.clone(),
                            _ => "".to_string(),
                        };

                        if !text.is_empty() {
                            harmony_messages
                                .push(Message::from_role_and_content(Role::Assistant, text));
                        }
                    }
                    // If there are tool calls in history
                    if let Some(tool_calls) = &a.tool_calls {
                        for tc in tool_calls {
                            let json_args = &tc.function.arguments;
                            let msg =
                                Message::from_role_and_content(Role::Assistant, json_args.clone())
                                    .with_channel("commentary")
                                    .with_recipient(format!("functions.{}", tc.function.name))
                                    .with_content_type("<|constrain|> json");
                            harmony_messages.push(msg);
                        }
                    }
                }
                ChatCompletionRequestMessage::Tool(t) => {
                    let mut func_name = "unknown".to_string();
                    // Heuristic to find function name from ID
                    for prev in request.messages.iter().rev() {
                        if let ChatCompletionRequestMessage::Assistant(a) = prev
                            && let Some(calls) = &a.tool_calls
                            && let Some(found) = calls.iter().find(|c| c.id == t.tool_call_id)
                        {
                            func_name = found.function.name.clone();
                            break;
                        }
                    }

                    if func_name == "unknown" {
                        warn!(
                            "HarmonyStrategy: Could not find function name for tool call ID {}",
                            t.tool_call_id
                        );
                    }

                    let author = Author::new(Role::Tool, format!("functions.{}", func_name));

                    let content_str = match &t.content {
                        ChatCompletionRequestToolMessageContent::Text(s) => s.clone(),
                        ChatCompletionRequestToolMessageContent::Array(parts) => parts
                            .iter()
                            .map(|p| {
                                let v = serde_json::to_value(p).unwrap_or_default();
                                if v.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    v.get("text")
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("")
                                        .to_string()
                                } else {
                                    warn!("HarmonyStrategy: Non-text content in Tool message partially supported (placeholder inserted)");
                                    "[NON_TEXT_CONTENT]".to_string()
                                }
                            })
                            .collect::<Vec<String>>()
                            .join(" "),
                    };

                    let msg = Message::from_author_and_content(author, content_str)
                        .with_channel("commentary");
                    harmony_messages.push(msg);
                }
                _ => {}
            }
        }

        let conversation = Conversation::from_messages(harmony_messages);

        // 4. Render to tokens
        let tokens = encoding
            .render_conversation_for_completion(&conversation, Role::Assistant, None)
            .map_err(|e| LlmError::Other(e.into()))?;

        // 5. Build Request Body for /v1/completions
        let max_tokens = overrides.max_tokens.unwrap_or(agent.max_tokens as u32);
        let presence_penalty = request.presence_penalty.or(agent.presence_penalty);

        let body = json!({
            "model": agent.model_name,
            "prompt": tokens,
            "max_tokens": max_tokens,
            "temperature": agent.temperature,
            "stream": false,
            "skip_special_tokens": false,
            "presence_penalty": presence_penalty,
        });

        debug!(
            "HarmonyStrategy: Prepared request with {} tokens",
            tokens.len()
        );
        Ok(body)
    }

    async fn parse_response(
        &self,
        response_body: &str,
    ) -> Result<CreateChatCompletionResponse, LlmError> {
        // Parse JSON response from /v1/completions
        let json_resp: Value =
            serde_json::from_str(response_body).map_err(|e| LlmError::Parse(e.into()))?;

        let text_output = json_resp["choices"][0]["text"].as_str().unwrap_or_default();
        let finish_reason_str = json_resp["choices"][0]["finish_reason"]
            .as_str()
            .unwrap_or("stop");

        // REGEX FALLBACK parsing for Harmony Output
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        // Track whether any channel segment was parsed so we can distinguish
        // "no channels at all" (raw text) from "channels found but produced
        // no output" (e.g., commentary without to=).
        let mut had_channel_segments = false;

        // Simple heuristic parser
        let parts: Vec<&str> = text_output.split("<|channel|>").collect();
        for part in parts {
            if part.trim().is_empty() {
                continue;
            }

            if let Some((channel, rest)) = part.split_once("<|message|>") {
                had_channel_segments = true;
                let channel_section = channel.trim();
                let actual_channel =
                    if channel_section == "analysis" || channel_section.starts_with("analysis ") {
                        "analysis"
                    } else if channel_section == "commentary"
                        || channel_section.starts_with("commentary ")
                    {
                        "commentary"
                    } else {
                        "final"
                    };

                let msg_content = rest
                    .split("<|end|>")
                    .next()
                    .unwrap_or(rest)
                    .split("<|call|>")
                    .next()
                    .unwrap_or(rest)
                    .split("<|return|>")
                    .next()
                    .unwrap_or(rest);

                match actual_channel {
                    "analysis" => {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str("<thought_process>\n");
                        content.push_str(msg_content);
                        content.push_str("\n</thought_process>\n");
                    }
                    "final" => {
                        content.push_str(msg_content);
                    }
                    "commentary" => {
                        if channel_section.contains("to=")
                            && let Some(to_pos) = channel_section.find("to=")
                        {
                            let recipient_str = &channel_section[to_pos + 3..];
                            let recipient_clean =
                                recipient_str.split_whitespace().next().unwrap_or("unknown");
                            let func_name = recipient_clean.replace("functions.", "");

                            tool_calls.push(ChatCompletionMessageToolCall {
                                id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                                r#type: ChatCompletionToolType::Function,
                                function: FunctionCall {
                                    name: func_name,
                                    arguments: msg_content.to_string(),
                                },
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        // Only fall back to raw text_output when no channel segments were
        // detected at all (i.e., the response is plain text without Harmony
        // protocol markers).  If channels were parsed but produced no content
        // (e.g., commentary without to=), leave content empty rather than
        // leaking protocol markers into the consumer.
        if content.is_empty()
            && tool_calls.is_empty()
            && !text_output.is_empty()
            && !had_channel_segments
        {
            content = text_output.to_string();
        }

        let has_tool_calls = !tool_calls.is_empty();

        let choice = ChatChoice {
            index: 0,
            message: ChatCompletionResponseMessage {
                content: if content.is_empty() {
                    None
                } else {
                    Some(content)
                },
                tool_calls: if has_tool_calls {
                    Some(tool_calls)
                } else {
                    None
                },
                role: OpenAIRole::Assistant,
                #[allow(deprecated)]
                function_call: None,
                refusal: None,
                audio: None,
            },
            finish_reason: Some(if has_tool_calls {
                FinishReason::ToolCalls
            } else {
                match finish_reason_str {
                    "stop" => FinishReason::Stop,
                    "length" => FinishReason::Length,
                    // Normalize: provider said "tool_calls" but we parsed none →
                    // treat as Stop to avoid a contradictory state.
                    "tool_calls" => FinishReason::Stop,
                    _ => FinishReason::Stop,
                }
            }),
            logprobs: None,
        };

        Ok(CreateChatCompletionResponse {
            id: json_resp["id"].as_str().unwrap_or("harmony-id").to_string(),
            object: "chat.completion".to_string(),
            created: json_resp["created"].as_u64().unwrap_or(0) as u32,
            model: json_resp["model"].as_str().unwrap_or("unknown").to_string(),
            system_fingerprint: None,
            choices: vec![choice],
            usage: if let Some(usage) = json_resp.get("usage") {
                serde_json::from_value(usage.clone()).ok()
            } else {
                debug!("Harmony response missing usage stats, defaulting to zero.");
                Some(CompletionUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    completion_tokens_details: None,
                    prompt_tokens_details: None,
                })
            },
            service_tier: None,
        })
    }

    fn endpoint_suffix(&self) -> &str {
        "/completions"
    }

    fn supports_streaming(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_response_harmony_text() {
        let strategy = HarmonyStrategy;
        // Emulate output from vLLM /completions (text field)
        // Harmony format: <|channel|>analysis<|message|>Thinking process...<|end|><|channel|>final<|message|>Final Answer<|end|>
        // Note: The simple parser splits by <|channel|>, looks for <|message|>, and splits by <|end|>
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 123,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>analysis<|message|>Thinking process...<|end|><|channel|>final<|message|>Final Answer<|end|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];
        let content = choice.message.content.as_ref().unwrap();

        assert!(content.contains("<thought_process>\nThinking process..."));
        assert!(content.contains("Final Answer"));
    }

    #[tokio::test]
    async fn test_parse_response_harmony_tool_call() {
        let strategy = HarmonyStrategy;
        // Harmony tool call format
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 123,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>commentary to=functions.get_weather<|message|>{\"location\": \"SF\"}<|call|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];

        assert!(choice.message.tool_calls.is_some());
        let tools = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(tools[0].function.name, "get_weather");
        assert_eq!(tools[0].function.arguments, "{\"location\": \"SF\"}");
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
    }

    #[test]
    fn test_convert_tools_empty() {
        let strategy = HarmonyStrategy;
        let tools: Vec<async_openai::types::ChatCompletionTool> = vec![];
        let result = strategy.convert_tools(&tools);
        assert!(result.is_empty());
    }

    #[test]
    fn test_convert_tools_single() {
        let strategy = HarmonyStrategy;
        let tools = vec![async_openai::types::ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: async_openai::types::FunctionObject {
                name: "get_weather".to_string(),
                description: Some("Get the current weather".to_string()),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "location": { "type": "string" }
                    }
                })),
                strict: None,
            },
        }];

        let result = strategy.convert_tools(&tools);
        assert_eq!(result.len(), 1);

        // Verify the converted tool has the correct name and description
        // ToolDescription is opaque, so we serialize to check
        let serialized = serde_json::to_value(&result[0]).unwrap();
        assert_eq!(serialized["name"], "get_weather");
        assert_eq!(serialized["description"], "Get the current weather");
        assert!(serialized["parameters"].is_object());
        assert_eq!(serialized["parameters"]["type"], "object");
    }

    #[test]
    fn test_convert_tools_no_description() {
        let strategy = HarmonyStrategy;
        let tools = vec![async_openai::types::ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: async_openai::types::FunctionObject {
                name: "do_something".to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        }];

        let result = strategy.convert_tools(&tools);
        assert_eq!(result.len(), 1);

        let serialized = serde_json::to_value(&result[0]).unwrap();
        assert_eq!(serialized["name"], "do_something");
        // None description should become empty string via unwrap_or_default()
        assert_eq!(serialized["description"], "");
        // None parameters should become empty object via json!({})
        assert!(serialized["parameters"].is_object());
    }

    #[test]
    fn test_convert_tools_multiple() {
        let strategy = HarmonyStrategy;
        let tools = vec![
            async_openai::types::ChatCompletionTool {
                r#type: ChatCompletionToolType::Function,
                function: async_openai::types::FunctionObject {
                    name: "tool_alpha".to_string(),
                    description: Some("First tool".to_string()),
                    parameters: Some(json!({"type": "object"})),
                    strict: None,
                },
            },
            async_openai::types::ChatCompletionTool {
                r#type: ChatCompletionToolType::Function,
                function: async_openai::types::FunctionObject {
                    name: "tool_beta".to_string(),
                    description: Some("Second tool".to_string()),
                    parameters: Some(
                        json!({"type": "object", "properties": {"x": {"type": "number"}}}),
                    ),
                    strict: None,
                },
            },
            async_openai::types::ChatCompletionTool {
                r#type: ChatCompletionToolType::Function,
                function: async_openai::types::FunctionObject {
                    name: "tool_gamma".to_string(),
                    description: None,
                    parameters: None,
                    strict: None,
                },
            },
        ];

        let result = strategy.convert_tools(&tools);
        assert_eq!(result.len(), 3);

        let names: Vec<String> = result
            .iter()
            .map(|t| {
                serde_json::to_value(t).unwrap()["name"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["tool_alpha", "tool_beta", "tool_gamma"]);
    }

    #[tokio::test]
    async fn test_parse_response_multiple_tool_calls() {
        let strategy = HarmonyStrategy;
        // Harmony output with multiple tool calls
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 123,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>commentary to=functions.tool1<|message|>{\"arg\": 1}<|call|>\n<|channel|>commentary to=functions.tool2<|message|>{\"arg\": 2}<|call|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];

        assert!(choice.message.tool_calls.is_some());
        let tools = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].function.name, "tool1");
        assert_eq!(tools[0].function.arguments, "{\"arg\": 1}");
        assert_eq!(tools[1].function.name, "tool2");
        assert_eq!(tools[1].function.arguments, "{\"arg\": 2}");
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
    }

    // ---------------------------------------------------------------
    // HarmonyStrategy::default() construction
    // ---------------------------------------------------------------

    #[test]
    fn test_harmony_strategy_default() {
        let _strategy = HarmonyStrategy;
        // Should construct successfully — no panic
    }

    // ---------------------------------------------------------------
    // endpoint_suffix and supports_streaming
    // ---------------------------------------------------------------

    #[test]
    fn test_harmony_endpoint_suffix() {
        let strategy = HarmonyStrategy;
        assert_eq!(strategy.endpoint_suffix(), "/completions");
    }

    #[test]
    fn test_harmony_supports_streaming() {
        let strategy = HarmonyStrategy;
        assert!(!strategy.supports_streaming());
    }

    // ---------------------------------------------------------------
    // parse_response with analysis-only and missing usage
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_analysis_only_no_usage() {
        let strategy = HarmonyStrategy;
        // Response with only analysis channel, no usage field
        let response_body = json!({
            "id": "test-no-usage",
            "object": "text_completion",
            "created": 456,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>analysis<|message|>Internal reasoning only<|end|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];

        // Should have analysis content wrapped in thought_process tags
        let content = choice.message.content.as_ref().unwrap();
        assert!(content.contains("<thought_process>"));
        assert!(content.contains("Internal reasoning only"));

        // No tool calls
        assert!(choice.message.tool_calls.is_none());

        // Missing usage should default to zeros
        assert!(result.usage.is_some());
        let usage = result.usage.unwrap();
        assert_eq!(usage.total_tokens, 0);
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
    }

    #[tokio::test]
    async fn test_parse_response_with_usage() {
        let strategy = HarmonyStrategy;
        let response_body = json!({
            "id": "test-with-usage",
            "object": "text_completion",
            "created": 789,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>final<|message|>The answer is 42<|end|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
    }

    #[tokio::test]
    async fn test_parse_response_empty_text_no_channels() {
        let strategy = HarmonyStrategy;
        // Raw text output with no channel delimiters — should fall through to text_output fallback
        let response_body = json!({
            "id": "raw",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "Just raw text without any channel markers",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert_eq!(content, "Just raw text without any channel markers");
    }

    // ---------------------------------------------------------------
    // parse_response: commentary channel without to= (no tool call)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_commentary_without_recipient() {
        let strategy = HarmonyStrategy;
        // commentary channel without "to=" should NOT generate a tool call
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 123,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>commentary<|message|>Some internal commentary<|end|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];
        // No tool calls because commentary has no "to=" prefix
        assert!(choice.message.tool_calls.is_none());
        // Commentary without to= is treated as a protocol-internal segment
        // and dropped — raw <|channel|> markers must not leak into content.
        assert!(
            choice.message.content.is_none(),
            "Commentary without to= should not leak protocol markers into content, got: {:?}",
            choice.message.content
        );
    }

    // ---------------------------------------------------------------
    // parse_response: empty text
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_empty_text() {
        let strategy = HarmonyStrategy;
        let response_body = json!({
            "id": "empty",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];
        // Empty text => content is None (empty and no tool calls)
        assert!(choice.message.content.is_none());
        assert!(choice.message.tool_calls.is_none());
    }

    // ---------------------------------------------------------------
    // parse_response: analysis + final combined
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_analysis_then_final() {
        let strategy = HarmonyStrategy;
        let response_body = json!({
            "id": "combined",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>analysis<|message|>Step 1: think<|end|><|channel|>final<|message|>The answer<|end|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(content.contains("<thought_process>"));
        assert!(content.contains("Step 1: think"));
        assert!(content.contains("The answer"));
    }

    // ---------------------------------------------------------------
    // parse_response: analysis channel with space suffix
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_analysis_channel_with_space_suffix() {
        let strategy = HarmonyStrategy;
        // "analysis " (with trailing space) should still be matched as "analysis"
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>analysis extra-info<|message|>Deep thinking<|end|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(content.contains("<thought_process>"));
        assert!(content.contains("Deep thinking"));
    }

    // ---------------------------------------------------------------
    // parse_response: commentary channel with space-prefixed "to="
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_commentary_with_space_prefixed_to() {
        let strategy = HarmonyStrategy;
        // "commentary to=functions.search" — with space before to=
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>commentary to=functions.search<|message|>{\"query\": \"test\"}<|call|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];
        assert!(choice.message.tool_calls.is_some());
        let tools = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(tools[0].function.name, "search");
    }

    // ---------------------------------------------------------------
    // parse_response: finish_reason "tool_calls" from provider
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_finish_reason_tool_calls_from_provider() {
        let strategy = HarmonyStrategy;
        // Provider says "tool_calls" but no actual tool calls parsed from content
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "Just plain text",
                    "index": 0,
                    "finish_reason": "tool_calls"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];
        // Provider claimed "tool_calls" but content has no channel markers →
        // finish_reason is normalized to Stop to avoid a contradictory state.
        assert_eq!(
            choice.finish_reason,
            Some(FinishReason::Stop),
            "Should normalize 'tool_calls' finish_reason to Stop when no tool calls were parsed"
        );
        assert!(
            choice.message.tool_calls.is_none(),
            "No actual tool calls should be parsed from plain text"
        );
    }

    // ---------------------------------------------------------------
    // parse_response: unknown finish_reason defaults to Stop
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_finish_reason_unknown_defaults_to_stop() {
        let strategy = HarmonyStrategy;
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "Some text",
                    "index": 0,
                    "finish_reason": "some_unknown_reason"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        assert_eq!(result.choices[0].finish_reason, Some(FinishReason::Stop));
    }

    // ---------------------------------------------------------------
    // parse_response: message content split by <|return|>
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_return_delimiter() {
        let strategy = HarmonyStrategy;
        // Content with <|return|> delimiter — should only take content before it
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>final<|message|>Before return<|return|>After return<|end|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(content.contains("Before return"));
        assert!(!content.contains("After return"));
    }

    // ---------------------------------------------------------------
    // parse_response: mixed analysis + tool call
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_analysis_and_tool_call_combined() {
        let strategy = HarmonyStrategy;
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>analysis<|message|>Let me think<|end|><|channel|>commentary to=functions.submit_proposal<|message|>{\"content\": \"result\"}<|call|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];
        // Should have both content (from analysis) and tool calls (from commentary)
        let content = choice.message.content.as_ref().unwrap();
        assert!(content.contains("<thought_process>"));
        assert!(content.contains("Let me think"));
        assert!(choice.message.tool_calls.is_some());
        let tools = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(tools[0].function.name, "submit_proposal");
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
    }

    #[tokio::test]
    async fn test_parse_response_finish_reason_length() {
        let strategy = HarmonyStrategy;
        let response_body = json!({
            "id": "len",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>final<|message|>Truncated output<|end|>",
                    "index": 0,
                    "finish_reason": "length"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        assert_eq!(result.choices[0].finish_reason, Some(FinishReason::Length));
    }

    // ---------------------------------------------------------------
    // parse_response: invalid JSON body => error
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_invalid_json() {
        let strategy = HarmonyStrategy;
        let response_body = "not valid json at all!!";
        let result = strategy.parse_response(response_body).await;
        assert!(result.is_err(), "Invalid JSON should return an error");
    }

    // ---------------------------------------------------------------
    // parse_response: missing choices field defaults
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_missing_choices_text() {
        let strategy = HarmonyStrategy;
        // choices[0].text is missing — unwrap_or_default should return ""
        let response_body = json!({
            "id": "test",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        // Empty text and no tool calls => content should be None
        assert!(result.choices[0].message.content.is_none());
    }

    // ---------------------------------------------------------------
    // parse_response: missing id/created/model defaults
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_missing_metadata_fields() {
        let strategy = HarmonyStrategy;
        let response_body = json!({
            "choices": [
                {
                    "text": "<|channel|>final<|message|>Answer<|end|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        // Missing id defaults to "harmony-id"
        assert_eq!(result.id, "harmony-id");
        // Missing created defaults to 0
        assert_eq!(result.created, 0);
        // Missing model defaults to "unknown"
        assert_eq!(result.model, "unknown");
    }

    // ---------------------------------------------------------------
    // parse_response: multiple channels mixed
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_analysis_final_and_tool_call() {
        let strategy = HarmonyStrategy;
        // All three channel types in a single response
        let response_body = json!({
            "id": "multi",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>analysis<|message|>Step 1: analyze<|end|><|channel|>final<|message|>My conclusion<|end|><|channel|>commentary to=functions.submit_proposal<|message|>{\"content\": \"result\"}<|call|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];

        // Content should have both analysis and final parts
        let content = choice.message.content.as_ref().unwrap();
        assert!(content.contains("<thought_process>"));
        assert!(content.contains("Step 1: analyze"));
        assert!(content.contains("My conclusion"));

        // Tool call should also be present
        assert!(choice.message.tool_calls.is_some());
        let tools = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(tools[0].function.name, "submit_proposal");

        // Finish reason overridden to ToolCalls when tool calls present
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
    }

    // ---------------------------------------------------------------
    // HarmonyStrategy::default() via Default trait explicitly
    // ---------------------------------------------------------------

    #[test]
    fn test_harmony_strategy_default_trait() {
        let strategy = <HarmonyStrategy as Default>::default();
        // Verify the strategy methods work on Default-constructed instance
        assert_eq!(strategy.endpoint_suffix(), "/completions");
        assert!(!strategy.supports_streaming());
    }

    // ---------------------------------------------------------------
    // parse_response: content with only tool call, no text content
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_tool_call_only_no_text_content() {
        let strategy = HarmonyStrategy;
        let response_body = json!({
            "id": "tc-only",
            "object": "text_completion",
            "created": 0,
            "model": "gpt-oss",
            "choices": [
                {
                    "text": "<|channel|>commentary to=functions.read_proposal<|message|>{\"agent_id\": \"Alice\", \"round\": 1}<|call|>",
                    "index": 0,
                    "finish_reason": "stop"
                }
            ]
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];

        // Tool call should be present
        assert!(choice.message.tool_calls.is_some());
        let tools = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(tools[0].function.name, "read_proposal");

        // Tool-call-only response: no text content was parsed, so content
        // should be None (not Some("")) to avoid downstream confusion.
        assert!(
            choice.message.content.is_none(),
            "Tool-call-only response should have content=None, got {:?}",
            choice.message.content
        );
    }

    // ---------------------------------------------------------------
    // prepare_request: basic request with system + user messages
    // ---------------------------------------------------------------

    /// Try to load the Harmony encoding; returns false if the tiktoken vocab
    /// file is missing (e.g. on CI runners). Tests that need the encoding
    /// should call this and `return` early when it returns false.
    ///
    /// Uses `spawn_blocking` just like the production code, because
    /// `load_harmony_encoding` performs blocking I/O (file read / HTTP
    /// download) that cannot run inside an async tokio context directly.
    async fn harmony_encoding_available() -> bool {
        match tokio::task::spawn_blocking(|| {
            load_harmony_encoding(HarmonyEncodingName::HarmonyGptOss)
        })
        .await
        {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                eprintln!("Skipping Harmony prepare_request test: encoding unavailable ({e})");
                false
            }
            Err(e) => {
                eprintln!("Skipping Harmony prepare_request test: spawn_blocking failed ({e})");
                false
            }
        }
    }

    fn make_test_agent() -> AgentConfig {
        AgentConfig {
            name: "test-agent".to_string(),
            provider_id: "test".to_string(),
            model_name: "gpt-oss".to_string(),
            temperature: 0.7,
            max_tokens: 1000,
            reasoning_effort: Some("high".to_string()),
            presence_penalty: Some(1.0),
            ..AgentConfig::default()
        }
    }

    #[tokio::test]
    async fn test_prepare_request_basic_system_and_user() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let agent = make_test_agent();
        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "You are a helpful agent.".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::User(
                    async_openai::types::ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text(
                            "Hello, world!".to_string(),
                        ),
                        name: None,
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(500),
        };

        let result = strategy.prepare_request(&agent, &request, &overrides).await;
        assert!(
            result.is_ok(),
            "prepare_request should succeed: {:?}",
            result.err()
        );
        let body = result.unwrap();
        assert_eq!(body["model"], "gpt-oss");
        assert_eq!(body["max_tokens"], 500);
        assert_eq!(body["stream"], false);
        // prompt should be a token array (list of ints)
        assert!(body["prompt"].is_array(), "prompt should be a token array");
    }

    #[tokio::test]
    async fn test_prepare_request_with_tools() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let agent = make_test_agent();
        let tools = vec![async_openai::types::ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: async_openai::types::FunctionObject {
                name: "submit_proposal".to_string(),
                description: Some("Submit a solution".to_string()),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" }
                    }
                })),
                strict: None,
            },
        }];
        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "System instructions.".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::User(
                    async_openai::types::ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text(
                            "Solve this problem".to_string(),
                        ),
                        name: None,
                    },
                ),
            ],
            tools: Some(tools),
            tool_choice: None,
            presence_penalty: Some(1.5),
            service_tier: None,
        };
        let overrides = RequestOverrides { max_tokens: None };

        let result = strategy.prepare_request(&agent, &request, &overrides).await;
        assert!(
            result.is_ok(),
            "prepare_request with tools should succeed: {:?}",
            result.err()
        );
        let body = result.unwrap();
        assert_eq!(body["max_tokens"], 1000); // from agent config
        assert_eq!(body["presence_penalty"], 1.5); // from request

        // Harmony encodes tools into the token array via convert_tools() /
        // with_function_tools(). Verify tools are actually serialized by
        // comparing prompt length with and without tools — tools add
        // developer-message tokens that expand the prompt array.
        let prompt_with_tools = body["prompt"]
            .as_array()
            .expect("prompt should be token array");

        // Build an identical request WITHOUT tools for comparison
        let request_no_tools = RequestConfig {
            messages: request.messages.clone(),
            tools: None,
            tool_choice: None,
            presence_penalty: Some(1.5),
            service_tier: None,
        };
        let body_no_tools = strategy
            .prepare_request(&agent, &request_no_tools, &overrides)
            .await
            .unwrap();
        let prompt_no_tools = body_no_tools["prompt"]
            .as_array()
            .expect("prompt should be token array");

        assert!(
            prompt_with_tools.len() > prompt_no_tools.len(),
            "Prompt with tools ({} tokens) should be longer than without ({} tokens) — \
             tools were not encoded into the Harmony token array",
            prompt_with_tools.len(),
            prompt_no_tools.len(),
        );
    }

    #[tokio::test]
    async fn test_prepare_request_user_array_content() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let agent = make_test_agent();

        // User message with Array content (multi-part with text and non-text)
        let text_part = json!({"type": "text", "text": "Hello from array"});
        let image_part =
            json!({"type": "image_url", "image_url": {"url": "http://example.com/img.png"}});

        let parts: Vec<async_openai::types::ChatCompletionRequestUserMessageContentPart> = vec![
            serde_json::from_value(text_part).unwrap(),
            serde_json::from_value(image_part).unwrap(),
        ];

        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "System".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::User(
                    async_openai::types::ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Array(parts),
                        name: None,
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(100),
        };

        let result = strategy.prepare_request(&agent, &request, &overrides).await;
        assert!(
            result.is_ok(),
            "prepare_request with array content should succeed: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_prepare_request_assistant_with_tool_calls_and_tool_response() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let agent = make_test_agent();

        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "Instructions".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::User(
                    async_openai::types::ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text(
                            "Call a tool".to_string(),
                        ),
                        name: None,
                    },
                ),
                // Assistant message with tool call
                ChatCompletionRequestMessage::Assistant(
                    async_openai::types::ChatCompletionRequestAssistantMessage {
                        content: Some(ChatCompletionRequestAssistantMessageContent::Text(
                            "I will call the tool.".to_string(),
                        )),
                        tool_calls: Some(vec![ChatCompletionMessageToolCall {
                            id: "call_123".to_string(),
                            r#type: ChatCompletionToolType::Function,
                            function: FunctionCall {
                                name: "get_weather".to_string(),
                                arguments: r#"{"location": "SF"}"#.to_string(),
                            },
                        }]),
                        name: None,
                        refusal: None,
                        audio: None,
                        #[allow(deprecated)]
                        function_call: None,
                    },
                ),
                // Tool response
                ChatCompletionRequestMessage::Tool(
                    async_openai::types::ChatCompletionRequestToolMessage {
                        content: ChatCompletionRequestToolMessageContent::Text(
                            r#"{"temperature": 72}"#.to_string(),
                        ),
                        tool_call_id: "call_123".to_string(),
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(200),
        };

        let result = strategy.prepare_request(&agent, &request, &overrides).await;
        assert!(
            result.is_ok(),
            "prepare_request with tool history should succeed: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_prepare_request_tool_response_unknown_function() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let agent = make_test_agent();

        // Tool response with no matching assistant tool call => func_name stays "unknown"
        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "System".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::Tool(
                    async_openai::types::ChatCompletionRequestToolMessage {
                        content: ChatCompletionRequestToolMessageContent::Text(
                            "result".to_string(),
                        ),
                        tool_call_id: "nonexistent_call_id".to_string(),
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(200),
        };

        let result = strategy.prepare_request(&agent, &request, &overrides).await;
        // Should still succeed, just with "unknown" function name
        assert!(
            result.is_ok(),
            "prepare_request with unknown tool call ID should succeed: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_prepare_request_reasoning_effort_low() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let mut agent = make_test_agent(); // default: high reasoning effort
        agent.reasoning_effort = Some("low".to_string());

        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "System prompt".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::User(
                    async_openai::types::ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text(
                            "Quick question".to_string(),
                        ),
                        name: None,
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(100),
        };

        let low_body = strategy
            .prepare_request(&agent, &request, &overrides)
            .await
            .expect("Low reasoning effort should succeed");
        assert_eq!(low_body["max_tokens"], 100);
        let low_prompt = low_body["prompt"]
            .as_array()
            .expect("prompt should be a token array");

        // Build the same request with high reasoning effort for comparison
        agent.reasoning_effort = Some("high".to_string());
        let high_body = strategy
            .prepare_request(&agent, &request, &overrides)
            .await
            .expect("High reasoning effort should succeed");
        let high_prompt = high_body["prompt"]
            .as_array()
            .expect("prompt should be a token array");

        // The Harmony encoding embeds reasoning effort into system tokens,
        // so low and high should produce different prompt token arrays.
        assert_ne!(
            low_prompt, high_prompt,
            "Low and high reasoning effort should produce different prompt token arrays"
        );
    }

    #[tokio::test]
    async fn test_prepare_request_multiple_system_messages() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let agent = make_test_agent();

        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "First system instruction.".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "Second system instruction.".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::User(
                    async_openai::types::ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text("Hello".to_string()),
                        name: None,
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(100),
        };

        let result = strategy.prepare_request(&agent, &request, &overrides).await;
        assert!(
            result.is_ok(),
            "Multiple system messages should be concatenated: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_prepare_request_tool_response_with_array_content() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let agent = make_test_agent();

        // Tool message with Array content (not Text)
        // This triggers the Array(parts) branch on lines 181-196
        let text_part = async_openai::types::ChatCompletionRequestToolMessageContentPart::Text(
            async_openai::types::ChatCompletionRequestMessageContentPartText {
                text: "Result data here".to_string(),
            },
        );

        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "System".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::User(
                    async_openai::types::ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text(
                            "Call a tool".to_string(),
                        ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::Assistant(
                    async_openai::types::ChatCompletionRequestAssistantMessage {
                        content: None,
                        tool_calls: Some(vec![ChatCompletionMessageToolCall {
                            id: "call_789".to_string(),
                            r#type: ChatCompletionToolType::Function,
                            function: FunctionCall {
                                name: "get_data".to_string(),
                                arguments: "{}".to_string(),
                            },
                        }]),
                        name: None,
                        refusal: None,
                        audio: None,
                        #[allow(deprecated)]
                        function_call: None,
                    },
                ),
                // Tool response with Array content
                ChatCompletionRequestMessage::Tool(
                    async_openai::types::ChatCompletionRequestToolMessage {
                        content: ChatCompletionRequestToolMessageContent::Array(vec![text_part]),
                        tool_call_id: "call_789".to_string(),
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(200),
        };

        let result = strategy.prepare_request(&agent, &request, &overrides).await;
        assert!(
            result.is_ok(),
            "Tool response with Array content should succeed: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_prepare_request_assistant_no_content() {
        if !harmony_encoding_available().await {
            return;
        }
        let strategy = HarmonyStrategy;
        let agent = make_test_agent();

        // Assistant message with no content but with tool calls
        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(
                    async_openai::types::ChatCompletionRequestSystemMessage {
                        content:
                            async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                "System".to_string(),
                            ),
                        name: None,
                    },
                ),
                ChatCompletionRequestMessage::Assistant(
                    async_openai::types::ChatCompletionRequestAssistantMessage {
                        content: None,
                        tool_calls: Some(vec![ChatCompletionMessageToolCall {
                            id: "call_456".to_string(),
                            r#type: ChatCompletionToolType::Function,
                            function: FunctionCall {
                                name: "search".to_string(),
                                arguments: r#"{"q": "test"}"#.to_string(),
                            },
                        }]),
                        name: None,
                        refusal: None,
                        audio: None,
                        #[allow(deprecated)]
                        function_call: None,
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(100),
        };

        let result = strategy.prepare_request(&agent, &request, &overrides).await;
        assert!(
            result.is_ok(),
            "Assistant with no content should succeed: {:?}",
            result.err()
        );
    }
}
