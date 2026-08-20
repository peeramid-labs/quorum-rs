use super::apply_sampling_params;
use super::{ChatStrategy, RequestOverrides};
use crate::agents::config::AgentConfig;
use crate::llms::RequestConfig;
use crate::telemetry::LlmError;
use async_openai::types::{
    ChatCompletionMessageToolCall, ChatCompletionRequestMessage, ChatCompletionToolType,
    CreateChatCompletionResponse, FinishReason, FunctionCall,
};
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use tracing::{debug, warn};

/// Cap on the compiled-regex cache. `tool_name` is derived from the model's own
/// closing tag, so an adversarial stream of fresh tag names would grow the cache
/// without bound over the process lifetime. The real tool set is tiny (~10), so a
/// generous cap that clears on overflow (a regex recompile is cheap) bounds memory
/// without evicting live tools in normal operation.
const MAX_TOOL_RE_CACHE: usize = 256;

/// Get or compile the `<tool>(.*?)</tool>` capture regex for `tool_name`, caching by
/// name with a hard size cap (see [`MAX_TOOL_RE_CACHE`]). `None` if the name fails to
/// compile (regex-escaped, so effectively never for well-formed names).
fn bounded_tool_regex(cache: &RwLock<HashMap<String, Regex>>, tool_name: &str) -> Option<Regex> {
    if let Ok(guard) = cache.read()
        && let Some(re) = guard.get(tool_name)
    {
        return Some(re.clone());
    }
    let pattern = format!(r"(?s)<{0}>(.*?)</{0}>", regex::escape(tool_name));
    match Regex::new(&pattern) {
        Ok(new_re) => {
            if let Ok(mut guard) = cache.write() {
                // Bound memory: clear past the cap rather than growing unboundedly.
                if guard.len() >= MAX_TOOL_RE_CACHE {
                    guard.clear();
                }
                guard.insert(tool_name.to_string(), new_re.clone());
            }
            Some(new_re)
        }
        Err(e) => {
            warn!("Failed to compile regex for tool '{}': {}", tool_name, e);
            None
        }
    }
}

#[derive(Debug, Default)]
pub struct XmlRegexStrategy {
    pub engine: Option<String>,
}

impl XmlRegexStrategy {
    pub fn new(engine: Option<String>) -> Self {
        Self { engine }
    }

    /// Generates a regex pattern to enforce XML structure for a given tool.
    /// Currently optimized for `submit_proposal` style tools where we want to capture
    /// a thought process and the tool content as raw text (to avoid JSON escaping issues).
    fn to_xml_regex(&self, tool: &async_openai::types::ChatCompletionTool) -> String {
        let name = &tool.function.name;
        let escaped = regex::escape(name);
        // Pattern matches:
        // 1. <thought_process> ... </thought_process> (non-greedy)
        // 2. <tool_name> ... </tool_name> (non-greedy)
        // [\s\S] matches any character including newlines.
        format!(r"<thought_process>[\s\S]*?</thought_process>\s*<{escaped}>[\s\S]*?</{escaped}>")
    }

    /// Extracts content from XML tags.
    /// Returns (thought_process, tool_args_as_json_string)
    fn parse_xml(&self, content: &str, tool_name: &str) -> (Option<String>, Option<String>) {
        // 1. Extract thought process
        static THOUGHT_RE: OnceLock<Regex> = OnceLock::new();
        let thought_re = THOUGHT_RE
            .get_or_init(|| Regex::new(r"(?s)<thought_process>(.*?)</thought_process>").unwrap());
        let thought = thought_re
            .captures(content)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string());

        // 2. Extract tool body
        static TOOL_RE_CACHE: OnceLock<RwLock<HashMap<String, Regex>>> = OnceLock::new();
        let cache = TOOL_RE_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
        let re = bounded_tool_regex(cache, tool_name);

        let tool_content = re
            .as_ref()
            .and_then(|r| r.captures(content))
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string());

        // Wrap the content in a JSON object using the tool name to infer the
        // expected parameter key. The model outputs raw text but the Agent
        // framework expects structured arguments.
        let json_args = tool_content.map(|text| {
            match tool_name {
                "submit_proposal" => {
                    // submit_proposal expects { solution_content, thought_process }.
                    // thought_process is already extracted separately and will be
                    // set as the message content; include it in args too so the
                    // downstream parser can find it.
                    let mut args = serde_json::Map::new();
                    args.insert("solution_content".to_string(), json!(text));
                    if let Some(ref tp) = thought {
                        args.insert("thought_process".to_string(), json!(tp));
                    }
                    serde_json::Value::Object(args).to_string()
                }
                "submit_batch_evaluation" => {
                    // Try to parse as JSON first (evaluations are usually structured)
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(parsed) => {
                            // If it's already an object with "evaluations", use as-is
                            if parsed.get("evaluations").is_some() {
                                parsed.to_string()
                            } else {
                                json!({ "evaluations": parsed }).to_string()
                            }
                        }
                        Err(e) => {
                            debug!(
                                "submit_batch_evaluation: JSON parse failed ({}), wrapping raw text ({} chars)",
                                e, text.len()
                            );
                            json!({ "evaluations": text }).to_string()
                        }
                    }
                }
                _ => json!({ "content": text }).to_string(),
            }
        });

        (thought, json_args)
    }
}

#[async_trait]
impl ChatStrategy for XmlRegexStrategy {
    async fn prepare_request(
        &self,
        agent: &AgentConfig,
        request: &RequestConfig,
        overrides: &RequestOverrides,
    ) -> Result<serde_json::Value, LlmError> {
        let max_tokens = overrides.max_tokens.unwrap_or(agent.max_tokens as u32);
        let presence_penalty = (!agent.omit_sampling_params)
            .then(|| request.presence_penalty.or(agent.presence_penalty))
            .flatten();

        // Clone messages to modify them with XML instructions
        let mut messages = request.messages.clone();

        // 1. Identify if we have a tool to enforce
        // We only support the first tool for guided_regex currently
        let target_tool = request.tools.as_ref().and_then(|t| {
            if t.len() > 1 {
                warn!(
                    "XmlRegexStrategy only supports one tool. Ignoring {} additional tools.",
                    t.len() - 1
                );
            }
            t.first()
        });

        let regex_pattern = target_tool.map(|tool| self.to_xml_regex(tool));

        // 2. Inject System Prompt instructions
        if let Some(tool) = target_tool {
            let name = &tool.function.name;
            let instructions = format!(
                "\nIMPORTANT: Output format required.\n\
                  1. Think step-by-step inside <thought_process>...</thought_process>\n\
                  2. Output your final answer inside <{name}>...</{name}>\n\
                  Do NOT output JSON. Do NOT escape LaTeX. Write raw text inside the tags."
            );

            // Append instructions to the first system message
            let mut found = false;
            for msg in messages.iter_mut() {
                if let ChatCompletionRequestMessage::System(sys) = msg
                    && let async_openai::types::ChatCompletionRequestSystemMessageContent::Text(t) =
                        &mut sys.content
                {
                    t.push_str(&instructions);
                    found = true;
                    break;
                }
            }
            // If no system message exists, create one
            if !found {
                messages.insert(
                    0,
                    ChatCompletionRequestMessage::System(
                        async_openai::types::ChatCompletionRequestSystemMessage {
                            content: async_openai::types::ChatCompletionRequestSystemMessageContent::Text(instructions),
                            name: None,
                        },
                    ),
                );
            }
        }

        // 3. Build Request Body
        // We do NOT send standard `tools` to vLLM when using `guided_regex`
        // to avoid conflicting with the regex constraint.
        let mut body = json!({
            "model": agent.model_name,
            "messages": messages,
            "stream": false,
        });
        apply_sampling_params(&mut body, agent, max_tokens, presence_penalty);

        if let Some(regex) = &regex_pattern {
            body["guided_regex"] = json!(regex);
        }

        // Handle vllm_responses quirk (remap messages -> input)
        if (self.engine.as_deref() == Some("vllm_responses")
            || self.engine.as_deref() == Some("vllm_xml_responses"))
            && let Some(msgs) = body.get("messages")
        {
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
                        // Break the pattern so backend treats it as plain text
                        *content_val =
                            serde_json::Value::String(content_str.replace("[tool=", "[call="));
                    }
                }
            }

            body["input"] = sanitized_msgs;
            if let Some(obj) = body.as_object_mut() {
                obj.remove("messages");
                // Responses API often prefers non-streaming
                obj.remove("stream");
            }
        }

        debug!(
            "XmlRegexStrategy: Prepared request with guided_regex: {:?}",
            regex_pattern.is_some()
        );
        Ok(body)
    }

    async fn parse_response(
        &self,
        response_body: &str,
    ) -> Result<CreateChatCompletionResponse, LlmError> {
        // Parse into generic Value first to handle quirks
        let mut value: serde_json::Value =
            serde_json::from_str(response_body).map_err(|e| LlmError::Parse(e.into()))?;

        // vLLM Responses API quirk: sometimes returns "service_tier": "auto" which async-openai rejects
        if let Some(tier) = value.get("service_tier").and_then(|v| v.as_str())
            && tier == "auto"
            && let Some(obj) = value.as_object_mut()
        {
            obj.remove("service_tier");
        }

        // Patch missing usage fields (vLLM/Cloudflare sometimes returns incomplete usage objects)
        if let Some(usage) = value.get_mut("usage").and_then(|u| u.as_object_mut()) {
            if !usage.contains_key("prompt_tokens") {
                usage.insert("prompt_tokens".to_string(), serde_json::json!(0));
            }
            if !usage.contains_key("completion_tokens") {
                usage.insert("completion_tokens".to_string(), serde_json::json!(0));
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

        // Handle vLLM/Cloudflare non-standard response format where 'choices' is missing
        if value.get("choices").is_none() {
            let mut extracted_content = None;

            // Case 1: "result": { "response": "..." } or "result": "..."
            if let Some(result) = value.get("result") {
                extracted_content =
                    if let Some(response_str) = result.get("response").and_then(|v| v.as_str()) {
                        Some(response_str.to_string())
                    } else {
                        result.as_str().map(|s| s.to_string())
                    };
            }

            // Case 2: "output": [ { "type": "message", "content": [ { "text": "..." } ] }, ... ]
            // This matches the gpt-oss-120b schema observed in logs.
            if extracted_content.is_none()
                && let Some(outputs) = value.get("output").and_then(|v| v.as_array())
            {
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

                if !full_text.is_empty() {
                    if !reasoning_text.is_empty() {
                        extracted_content = Some(format!(
                            "<thought_process>{}</thought_process>\n{}",
                            reasoning_text, full_text
                        ));
                    } else {
                        extracted_content = Some(full_text);
                    }
                }
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

        // Fallback: If choices is still missing, synthesize a minimal response.
        // In debug builds, include truncated raw JSON for diagnostics.
        // In release builds, use a generic message to avoid leaking provider internals.
        if value.get("choices").is_none() || value.get("choices").is_some_and(|v| v.is_null()) {
            let content = if cfg!(debug_assertions) {
                // Redact fields that may contain sensitive data before dumping
                if let Some(obj) = value.as_object_mut() {
                    for key in ["api_key", "authorization", "token", "secret"] {
                        if obj.contains_key(key) {
                            obj.insert(key.to_string(), serde_json::json!("[REDACTED]"));
                        }
                    }
                }
                let dump = serde_json::to_string_pretty(&value).unwrap_or_default();
                let char_count = dump.chars().count();
                let truncated_dump = if char_count > 1000 {
                    let truncated: String = dump.chars().take(1000).collect();
                    format!(
                        "{}... (truncated from {} characters)",
                        truncated, char_count
                    )
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

        // Standard OpenAI JSON Parse
        let mut response: CreateChatCompletionResponse =
            serde_json::from_value(value).map_err(|e| LlmError::Parse(e.into()))?;

        // Post-process content to extract XML if present
        if let Some(choice) = response.choices.first_mut()
            && let Some(content) = &choice.message.content
            && content.contains("<thought_process>")
        {
            // Heuristic: Identify tool name by looking for the last closing tag
            static TAG_RE: OnceLock<Regex> = OnceLock::new();
            let tag_re = TAG_RE.get_or_init(|| Regex::new(r"</([a-zA-Z0-9_-]+)>\s*$").unwrap());
            if let Some(tool_name) = tag_re
                .captures(content)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
            {
                if tool_name != "thought_process" {
                    let (thought, json_args) = self.parse_xml(content, &tool_name);

                    // Update message content (keep only thought process or full content?)
                    // NSED usually likes to see the reasoning in the content.
                    if let Some(t) = thought {
                        choice.message.content = Some(t);
                    }

                    // Construct Tool Call
                    if let Some(args) = json_args {
                        choice.message.tool_calls = Some(vec![ChatCompletionMessageToolCall {
                            id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                            r#type: ChatCompletionToolType::Function,
                            function: FunctionCall {
                                name: tool_name,
                                arguments: args,
                            },
                        }]);
                        choice.finish_reason = Some(FinishReason::ToolCalls);
                    }
                }
            } else {
                warn!(
                    "XmlRegexStrategy: Failed to extract tool name from content. Skipping tool call construction."
                );
            }
        }

        Ok(response)
    }

    fn endpoint_suffix(&self) -> &str {
        if self.engine.as_deref() == Some("vllm_responses")
            || self.engine.as_deref() == Some("vllm_xml_responses")
        {
            "/responses"
        } else {
            "/chat/completions"
        }
    }

    fn supports_streaming(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::{ChatCompletionTool, FunctionObject};

    #[test]
    fn bounded_tool_regex_caps_cache_and_still_matches() {
        let cache = RwLock::new(HashMap::new());
        // A flood of distinct model-controlled tag names must not grow the cache
        // without bound — it stays at/under the cap.
        for i in 0..(MAX_TOOL_RE_CACHE + 50) {
            let re = bounded_tool_regex(&cache, &format!("tag_{i}")).expect("compiles");
            // The compiled regex actually matches its tag.
            assert!(re.is_match(&format!("<tag_{i}>body</tag_{i}>")));
        }
        assert!(
            cache.read().unwrap().len() <= MAX_TOOL_RE_CACHE,
            "cache must stay within the cap, got {}",
            cache.read().unwrap().len()
        );
        // A cache hit returns a working regex for a name inserted after the last clear.
        let re = bounded_tool_regex(&cache, "submit_proposal").unwrap();
        assert!(
            re.captures("<submit_proposal>X</submit_proposal>")
                .is_some()
        );
    }

    #[test]
    fn test_to_xml_regex() {
        let strategy = XmlRegexStrategy::default();
        let tool = ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: "submit_proposal".to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        };

        let regex = strategy.to_xml_regex(&tool);
        assert!(regex.contains("<thought_process>"));
        assert!(regex.contains("</thought_process>"));
        assert!(regex.contains("<submit_proposal>"));
        assert!(regex.contains("</submit_proposal>"));
    }

    #[test]
    fn test_parse_xml_extraction() {
        let strategy = XmlRegexStrategy::default();
        let content = r#"
            <thought_process>
            Thinking about 2+2
            </thought_process>
            <submit_proposal>
            The answer is 4
            </submit_proposal>
        "#;

        let (thought, json_args) = strategy.parse_xml(content, "submit_proposal");

        assert!(thought.is_some());
        assert!(thought.unwrap().contains("Thinking about 2+2"));

        assert!(json_args.is_some());
        let args = json_args.unwrap();
        // submit_proposal wraps as {"solution_content": "...", "thought_process": "..."}
        let parsed: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert!(
            parsed["solution_content"]
                .as_str()
                .unwrap()
                .contains("The answer is 4")
        );
        assert!(
            parsed["thought_process"]
                .as_str()
                .unwrap()
                .contains("Thinking about 2+2")
        );
    }

    #[tokio::test]
    async fn test_parse_response_converts_xml_to_tool_call() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "id": "test-id",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "test-model",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "<thought_process>Thinking...</thought_process><submit_proposal>Result</submit_proposal>"
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": null
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];

        // Should have tool calls now
        assert!(choice.message.tool_calls.is_some());
        let tool_calls = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "submit_proposal");

        let args: serde_json::Value =
            serde_json::from_str(&tool_calls[0].function.arguments).unwrap();
        assert_eq!(args["solution_content"], "Result");

        // Finish reason should be updated
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
    }

    #[test]
    fn test_parse_xml_complex_content() {
        let strategy = XmlRegexStrategy::default();
        let content = r#"
            <thought_process>
            Thinking...
            "Quotes" and
            Newlines
            </thought_process>
            <submit_proposal>
            This contains LaTeX: \frac{a}{b} and JSON characters: {"key": "value"}
            </submit_proposal>
        "#;

        let (thought, json_args) = strategy.parse_xml(content, "submit_proposal");

        assert!(thought.is_some());
        assert!(thought.unwrap().contains("\"Quotes\""));

        assert!(json_args.is_some());
        let args_str = json_args.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&args_str).unwrap();
        let solution = parsed["solution_content"].as_str().unwrap();

        assert!(solution.contains(r"\frac{a}{b}"));
        assert!(solution.contains(r#"{"key": "value"}"#));
    }

    #[tokio::test]
    async fn test_prepare_request_vllm_xml_responses() {
        use async_openai::types::{
            ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent,
        };

        let strategy = XmlRegexStrategy::new(Some("vllm_xml_responses".to_string()));
        let agent = AgentConfig {
            name: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.0,
            max_tokens: 100,
            ..Default::default()
        };
        let request_config = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hello".to_string()),
                    name: None,
                }),
                // Simulate polluted history
                ChatCompletionRequestMessage::Assistant(
                    async_openai::types::ChatCompletionRequestAssistantMessage {
                        content: Some(
                            async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(
                                "Thinking...\n[tool=read_proposal(...)]".to_string(),
                            ),
                        ),
                        tool_calls: None,
                        #[allow(deprecated)]
                        function_call: None,
                        refusal: None,
                        name: None,
                        audio: None,
                    },
                ),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides::default();

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();

        // Verify "messages" -> "input" mapping
        assert!(body.get("messages").is_none());
        assert!(body.get("input").is_some());

        // Verify sanitization
        let input = body["input"].as_array().unwrap();
        let assistant_msg = &input[1];
        let content = assistant_msg["content"].as_str().unwrap();
        assert!(content.contains("[call=read_proposal"));
        assert!(!content.contains("[tool=read_proposal"));
    }

    #[test]
    fn test_parse_xml_no_tool_call() {
        let strategy = XmlRegexStrategy::default();
        // Content without any tool call tags — only plain text
        let content = "This is just some plain text with no XML tags at all.";
        let (thought, json_args) = strategy.parse_xml(content, "submit_proposal");

        assert!(thought.is_none());
        assert!(json_args.is_none());
    }

    #[test]
    fn test_parse_xml_with_thought_and_tool() {
        let strategy = XmlRegexStrategy::default();
        let content = r#"<thought_process>I need to evaluate carefully.</thought_process>
<submit_proposal>My proposed solution here.</submit_proposal>"#;

        let (thought, json_args) = strategy.parse_xml(content, "submit_proposal");

        assert!(thought.is_some());
        assert_eq!(thought.unwrap(), "I need to evaluate carefully.");

        assert!(json_args.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&json_args.unwrap()).unwrap();
        assert_eq!(
            parsed["solution_content"].as_str().unwrap(),
            "My proposed solution here."
        );
        assert_eq!(
            parsed["thought_process"].as_str().unwrap(),
            "I need to evaluate carefully."
        );
    }

    #[test]
    fn test_parse_xml_empty_thought() {
        let strategy = XmlRegexStrategy::default();
        let content =
            "<thought_process></thought_process>\n<submit_proposal>Answer</submit_proposal>";

        let (thought, json_args) = strategy.parse_xml(content, "submit_proposal");

        // Empty thought_process tags should yield an empty string
        assert!(thought.is_some());
        assert_eq!(thought.unwrap(), "");

        assert!(json_args.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&json_args.unwrap()).unwrap();
        assert_eq!(parsed["solution_content"].as_str().unwrap(), "Answer");
    }

    #[test]
    fn test_to_xml_regex_matches_tags() {
        let strategy = XmlRegexStrategy::default();
        let tool = ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: "submit_proposal".to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        };

        let pattern = strategy.to_xml_regex(&tool);
        let re = Regex::new(&pattern).expect("Generated regex should be valid");

        // Should match well-formed XML with thought + tool
        let sample = "<thought_process>Thinking step by step...</thought_process>\n<submit_proposal>The answer is 42.</submit_proposal>";
        assert!(
            re.is_match(sample),
            "Regex should match valid XML with thought and tool tags"
        );

        // Should NOT match if tool tag is missing
        let no_tool = "<thought_process>Thinking...</thought_process>";
        assert!(
            !re.is_match(no_tool),
            "Regex should not match when tool tag is missing"
        );

        // Should NOT match if thought_process tag is missing
        let no_thought = "<submit_proposal>Just the answer</submit_proposal>";
        assert!(
            !re.is_match(no_thought),
            "Regex should not match when thought_process tag is missing"
        );
    }

    // ---------------------------------------------------------------
    // parse_xml with submit_batch_evaluation
    // ---------------------------------------------------------------

    #[test]
    fn test_parse_xml_submit_batch_evaluation_already_json() {
        let strategy = XmlRegexStrategy::default();
        let content = r#"<thought_process>Evaluating candidates</thought_process>
<submit_batch_evaluation>{"evaluations": [{"agent_id": "A", "endorsement_weight": 85}]}</submit_batch_evaluation>"#;

        let (thought, json_args) = strategy.parse_xml(content, "submit_batch_evaluation");
        assert!(thought.is_some());
        assert_eq!(thought.unwrap(), "Evaluating candidates");

        assert!(json_args.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&json_args.unwrap()).unwrap();
        // Already-wrapped JSON should pass through
        assert!(parsed["evaluations"].is_array());
    }

    #[test]
    fn test_parse_xml_submit_batch_evaluation_unwrapped() {
        let strategy = XmlRegexStrategy::default();
        // Content that is NOT already JSON — raw eval text
        let content = r#"<thought_process>My analysis</thought_process>
<submit_batch_evaluation>[{"agent_id": "X", "endorsement_weight": 90}]</submit_batch_evaluation>"#;

        let (thought, json_args) = strategy.parse_xml(content, "submit_batch_evaluation");
        assert!(thought.is_some());
        assert!(json_args.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&json_args.unwrap()).unwrap();
        // Should be wrapped in {"evaluations": ...}
        assert!(parsed.get("evaluations").is_some());
    }

    // ---------------------------------------------------------------
    // parse_response with service_tier: "auto"
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_with_service_tier() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "id": "test-id",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "test-model",
            "service_tier": "auto",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Just plain text without XML"
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            }
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        assert_eq!(
            result.choices[0].message.content.as_deref(),
            Some("Just plain text without XML")
        );
        assert!(result.usage.is_some());
        assert_eq!(result.usage.unwrap().total_tokens, 30);
    }

    // ---------------------------------------------------------------
    // endpoint_suffix for each engine variant
    // ---------------------------------------------------------------

    #[test]
    fn test_endpoint_suffix_default() {
        let strategy = XmlRegexStrategy::default();
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");
    }

    #[test]
    fn test_endpoint_suffix_vllm_responses() {
        let strategy = XmlRegexStrategy::new(Some("vllm_responses".to_string()));
        assert_eq!(strategy.endpoint_suffix(), "/responses");
    }

    #[test]
    fn test_endpoint_suffix_vllm_xml_responses() {
        let strategy = XmlRegexStrategy::new(Some("vllm_xml_responses".to_string()));
        assert_eq!(strategy.endpoint_suffix(), "/responses");
    }

    #[test]
    fn test_endpoint_suffix_vllm_xml_chat() {
        let strategy = XmlRegexStrategy::new(Some("vllm_xml".to_string()));
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");
    }

    #[test]
    fn test_supports_streaming() {
        let strategy = XmlRegexStrategy::default();
        assert!(!strategy.supports_streaming());
    }

    // ---------------------------------------------------------------
    // prepare_request with no system message
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // parse_xml: unknown tool name wraps as {"content": ...}
    // ---------------------------------------------------------------

    #[test]
    fn test_parse_xml_unknown_tool_wraps_as_content() {
        let strategy = XmlRegexStrategy::default();
        let content = r#"<thought_process>Thinking</thought_process>
<custom_tool>Some raw output text</custom_tool>"#;

        let (thought, json_args) = strategy.parse_xml(content, "custom_tool");
        assert!(thought.is_some());
        assert_eq!(thought.unwrap(), "Thinking");

        assert!(json_args.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&json_args.unwrap()).unwrap();
        assert_eq!(parsed["content"].as_str().unwrap(), "Some raw output text");
    }

    // ---------------------------------------------------------------
    // parse_xml: submit_batch_evaluation with non-JSON text
    // ---------------------------------------------------------------

    #[test]
    fn test_parse_xml_submit_batch_evaluation_non_json_text() {
        let strategy = XmlRegexStrategy::default();
        let content = r#"<thought_process>Eval</thought_process>
<submit_batch_evaluation>This is not valid JSON at all</submit_batch_evaluation>"#;

        let (_thought, json_args) = strategy.parse_xml(content, "submit_batch_evaluation");
        assert!(json_args.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&json_args.unwrap()).unwrap();
        // Non-JSON text should be wrapped in {"evaluations": "..."}
        assert_eq!(
            parsed["evaluations"].as_str().unwrap(),
            "This is not valid JSON at all"
        );
    }

    // ---------------------------------------------------------------
    // parse_response: Cloudflare non-standard response with "result" field
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_cloudflare_result_string() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "result": "The answer is 42",
            "success": true
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(content.contains("The answer is 42"));
    }

    #[tokio::test]
    async fn test_parse_response_cloudflare_result_nested() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "result": { "response": "Nested answer" },
            "success": true
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(content.contains("Nested answer"));
    }

    // ---------------------------------------------------------------
    // parse_response: output array format (gpt-oss-120b style)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_output_array_message_only() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "output": [
                {
                    "type": "message",
                    "content": [{"text": "Hello from output array"}]
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(content.contains("Hello from output array"));
    }

    #[tokio::test]
    async fn test_parse_response_output_array_with_reasoning() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "output": [
                {
                    "type": "reasoning",
                    "content": [{"text": "Let me think..."}]
                },
                {
                    "type": "message",
                    "content": [{"text": "The answer"}]
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(content.contains("<thought_process>Let me think...</thought_process>"));
        assert!(content.contains("The answer"));
    }

    // ---------------------------------------------------------------
    // parse_response: usage patching with missing fields
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_usage_patching_missing_fields() {
        let strategy = XmlRegexStrategy::default();
        // Usage object missing prompt_tokens and completion_tokens
        let response_body = json!({
            "id": "test-id",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": "Hi" },
                    "finish_reason": "stop"
                }
            ],
            "usage": {}
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
    }

    // ---------------------------------------------------------------
    // parse_response: fallback for completely malformed body
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_debug_fallback() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "unexpected_field": "no choices, no result, no output"
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        // In debug mode the fallback includes the raw body; in release it's a generic message.
        if cfg!(debug_assertions) {
            assert!(content.contains("DEBUG: Failed to parse response structure"));
        } else {
            assert!(content.contains("Unable to parse response from provider"));
        }
        assert_eq!(result.id, "fallback-response");
    }

    // ---------------------------------------------------------------
    // parse_response: XML with thought_process only (no additional tool tag)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_thought_process_only_no_tool_extraction() {
        let strategy = XmlRegexStrategy::default();
        // Content has thought_process but the last closing tag IS thought_process
        // (no additional tool), so no tool call should be created
        let response_body = json!({
            "id": "test-id",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "<thought_process>Just thinking, no tool call</thought_process>"
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": { "prompt_tokens": 5, "completion_tokens": 10, "total_tokens": 15 }
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];
        // Thought process only => no tool calls should be generated
        assert!(choice.message.tool_calls.is_none());
        // Content should remain as-is
        let content = choice.message.content.as_ref().unwrap();
        assert!(content.contains("<thought_process>"));
    }

    // ---------------------------------------------------------------
    // prepare_request: tool injection with system message
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_prepare_request_injects_xml_instructions_into_system_message() {
        use async_openai::types::{
            ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
            ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent,
        };

        let strategy = XmlRegexStrategy::new(Some("vllm".to_string()));
        let agent = AgentConfig {
            name: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.0,
            max_tokens: 100,
            ..Default::default()
        };
        let tool = ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: "submit_proposal".to_string(),
                description: Some("Submit".to_string()),
                parameters: None,
                strict: None,
            },
        };
        let request_config = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: ChatCompletionRequestSystemMessageContent::Text(
                        "You are helpful.".to_string(),
                    ),
                    name: None,
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "Do something".to_string(),
                    ),
                    name: None,
                }),
            ],
            tools: Some(vec![tool]),
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides::default();

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();

        // The system message should be augmented with XML format instructions
        let messages = body["messages"].as_array().unwrap();
        let system_msg = &messages[0];
        let system_content = system_msg["content"].as_str().unwrap();
        assert!(system_content.contains("You are helpful."));
        assert!(system_content.contains("<thought_process>"));
        assert!(system_content.contains("submit_proposal"));

        // guided_regex should be present
        assert!(body.get("guided_regex").is_some());

        // Standard tools should NOT be sent (to avoid conflicting with guided_regex)
        assert!(body.get("tools").is_none());
    }

    // ---------------------------------------------------------------
    // prepare_request: no tools => no guided_regex
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_prepare_request_no_tools_no_guided_regex() {
        use async_openai::types::{
            ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent,
        };

        let strategy = XmlRegexStrategy::new(Some("vllm".to_string()));
        let agent = AgentConfig {
            name: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.0,
            max_tokens: 100,
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
            service_tier: None,
        };
        let overrides = RequestOverrides::default();

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();

        // Without tools, guided_regex should not be present
        assert!(body.get("guided_regex").is_none());
    }

    #[tokio::test]
    async fn test_prepare_request_no_system_message() {
        use async_openai::types::{
            ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent,
        };

        let strategy = XmlRegexStrategy::new(Some("vllm_xml".to_string()));
        let agent = AgentConfig {
            name: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.5,
            max_tokens: 200,
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
            service_tier: None,
        };
        let overrides = RequestOverrides::default();

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();

        // Should have messages array
        assert!(body.get("messages").is_some());
        let messages = body["messages"].as_array().unwrap();
        // Without system message, should just have user message(s)
        assert!(!messages.is_empty());
    }

    // ---------------------------------------------------------------
    // parse_response: choices is null (treated as missing)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_choices_null() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "id": "test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": null,
            "usage": { "prompt_tokens": 5, "completion_tokens": 10, "total_tokens": 15 }
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        // choices: null should trigger fallback
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(
            content.contains("Failed to parse") || content.contains("Unable to parse"),
            "Should contain fallback message for null choices"
        );
    }

    // ---------------------------------------------------------------
    // parse_response: invalid JSON => error
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_invalid_json() {
        let strategy = XmlRegexStrategy::default();
        let result = strategy.parse_response("totally not json").await;
        assert!(result.is_err(), "Invalid JSON should return error");
    }

    // ---------------------------------------------------------------
    // parse_response: usage with only prompt_tokens
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_usage_partial_fields() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "id": "test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [
                {
                    "index": 0,
                    "message": { "role": "assistant", "content": "Hello" },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 100
            }
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 0); // Patched to 0
        assert_eq!(usage.total_tokens, 100); // Computed: 100 + 0
    }

    // ---------------------------------------------------------------
    // parse_xml: regex cache hit (second call with same tool name)
    // ---------------------------------------------------------------

    #[test]
    fn test_parse_xml_regex_cache_hit() {
        let strategy = XmlRegexStrategy::default();
        let content1 = "<thought_process>Think1</thought_process>\n<my_tool>Result1</my_tool>";
        let content2 = "<thought_process>Think2</thought_process>\n<my_tool>Result2</my_tool>";

        // First call populates cache
        let (thought1, args1) = strategy.parse_xml(content1, "my_tool");
        assert!(thought1.is_some());
        assert!(args1.is_some());

        // Second call with same tool name should use cached regex
        let (thought2, args2) = strategy.parse_xml(content2, "my_tool");
        assert!(thought2.is_some());
        assert_eq!(thought2.unwrap(), "Think2");
        assert!(args2.is_some());
    }

    // ---------------------------------------------------------------
    // parse_response: XML content with thought_process but no
    // closing tag match (tag_re doesn't match)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_xml_no_closing_tag() {
        let strategy = XmlRegexStrategy::default();
        // Content has thought_process but ends without a proper closing tag
        // The regex `</([a-zA-Z0-9_-]+)>\s*$` won't match
        let response_body = json!({
            "id": "test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "<thought_process>Thinking about the problem</thought_process> and then some trailing text without tags"
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": { "prompt_tokens": 5, "completion_tokens": 10, "total_tokens": 15 }
        }).to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let choice = &result.choices[0];
        // No tool call should be generated since there's no matching closing tag
        assert!(choice.message.tool_calls.is_none());
    }

    // ---------------------------------------------------------------
    // prepare_request: with tool + no system message => system created
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_prepare_request_tool_without_system_creates_system_msg() {
        use async_openai::types::{
            ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent,
        };

        let strategy = XmlRegexStrategy::new(Some("vllm".to_string()));
        let agent = AgentConfig {
            name: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.0,
            max_tokens: 100,
            ..Default::default()
        };
        let tool = ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: "submit_proposal".to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        };
        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text("Hello".to_string()),
                    name: None,
                },
            )],
            tools: Some(vec![tool]),
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides::default();

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();

        let messages = body["messages"].as_array().unwrap();
        // A system message should have been inserted at index 0
        assert_eq!(
            messages[0]["role"].as_str().unwrap(),
            "system",
            "System message should be created when tool is present but no system msg exists"
        );
        let sys_content = messages[0]["content"].as_str().unwrap();
        assert!(
            sys_content.contains("<thought_process>"),
            "System message should contain XML format instructions"
        );
        assert!(sys_content.contains("submit_proposal"));
    }

    // ---------------------------------------------------------------
    // prepare_request: overrides max_tokens
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_prepare_request_max_tokens_override() {
        use async_openai::types::{
            ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent,
        };

        let strategy = XmlRegexStrategy::default();
        let agent = AgentConfig {
            name: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.0,
            max_tokens: 100,
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
            presence_penalty: Some(0.5),
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(999),
        };

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();

        assert_eq!(body["max_tokens"], 999);
        assert_eq!(body["presence_penalty"], 0.5);
    }

    // ---------------------------------------------------------------
    // parse_response: output array with empty content arrays
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_output_array_empty_content() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "output": [
                {
                    "type": "message",
                    "content": []
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        // Empty content arrays => no text extracted => falls through to debug fallback
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(
            content.contains("Failed to parse") || content.contains("Unable to parse"),
            "Empty output content should trigger fallback"
        );
    }

    // NOTE: test_parse_xml_submit_batch_evaluation_non_json_text (line ~906) already covers
    // the non-JSON wrapping path for submit_batch_evaluation.

    #[test]
    fn test_parse_xml_submit_batch_evaluation_with_evaluations_key() {
        // When the parsed JSON already has "evaluations", it should be kept as-is
        let strategy = XmlRegexStrategy::default();
        let content = r#"<thought_process>Eval thinking</thought_process>
<submit_batch_evaluation>{"evaluations": [{"agent_id": "A", "score": 90}]}</submit_batch_evaluation>"#;
        let (_, args) = strategy.parse_xml(content, "submit_batch_evaluation");
        assert!(args.is_some());
        let args_json: serde_json::Value = serde_json::from_str(&args.unwrap()).unwrap();
        assert!(args_json["evaluations"].is_array());
    }

    #[test]
    fn test_parse_xml_submit_batch_evaluation_json_without_evaluations_key() {
        // JSON parsed but no "evaluations" key — wraps as {"evaluations": parsed} (line 110)
        let strategy = XmlRegexStrategy::default();
        let content = r#"<thought_process>Think</thought_process>
<submit_batch_evaluation>[{"agent_id": "B", "score": 80}]</submit_batch_evaluation>"#;
        let (_, args) = strategy.parse_xml(content, "submit_batch_evaluation");
        assert!(args.is_some());
        let args_json: serde_json::Value = serde_json::from_str(&args.unwrap()).unwrap();
        assert!(args_json["evaluations"].is_array());
    }

    // NOTE: test_parse_xml_unknown_tool_wraps_as_content (line ~887) already covers
    // the unknown-tool wrapping path.

    fn make_test_agent_config() -> AgentConfig {
        AgentConfig {
            name: "test".to_string(),
            model_name: "model".to_string(),
            temperature: 0.0,
            max_tokens: 100,
            ..Default::default()
        }
    }

    // ---------------------------------------------------------------
    // prepare_request: multiple tools warning (line 148-150)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_prepare_request_multiple_tools_warning() {
        let strategy = XmlRegexStrategy::new(Some("vllm".to_string()));
        let agent = make_test_agent_config();

        let tools = vec![
            async_openai::types::ChatCompletionTool {
                r#type: async_openai::types::ChatCompletionToolType::Function,
                function: async_openai::types::FunctionObject {
                    name: "submit_proposal".to_string(),
                    description: Some("Submit".to_string()),
                    parameters: Some(json!({"type": "object"})),
                    strict: None,
                },
            },
            async_openai::types::ChatCompletionTool {
                r#type: async_openai::types::ChatCompletionToolType::Function,
                function: async_openai::types::FunctionObject {
                    name: "extra_tool".to_string(),
                    description: Some("Extra".to_string()),
                    parameters: None,
                    strict: None,
                },
            },
        ];

        let request_config = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::System(
                async_openai::types::ChatCompletionRequestSystemMessage {
                    content: async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                        "System".to_string(),
                    ),
                    name: None,
                },
            )],
            tools: Some(tools),
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };
        let overrides = RequestOverrides {
            max_tokens: Some(100),
        };

        // Should succeed but log a warning about ignoring extra tools
        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();
        // Guided regex should be set (from first tool only)
        assert!(body.get("guided_regex").is_some());
        assert!(
            body["guided_regex"]
                .as_str()
                .unwrap()
                .contains("submit_proposal")
        );
    }

    // ---------------------------------------------------------------
    // prepare_request: vllm_responses engine remaps messages -> input
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_prepare_request_vllm_responses_engine() {
        let strategy = XmlRegexStrategy::new(Some("vllm_responses".to_string()));
        let agent = make_test_agent_config();

        let request_config = RequestConfig {
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
                        content: async_openai::types::ChatCompletionRequestUserMessageContent::Text(
                            "Hello".to_string(),
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

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();
        // vllm_responses engine should remap messages -> input
        assert!(
            body.get("input").is_some(),
            "Should have 'input' field for vllm_responses engine"
        );
        assert!(
            body.get("messages").is_none(),
            "Should not have 'messages' field"
        );
        assert!(
            body.get("stream").is_none(),
            "Should not have 'stream' field"
        );
    }

    // ---------------------------------------------------------------
    // prepare_request: vllm_responses with tool= sanitization
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_prepare_request_vllm_responses_sanitizes_tool_syntax() {
        let strategy = XmlRegexStrategy::new(Some("vllm_xml_responses".to_string()));
        let agent = make_test_agent_config();

        let request_config = RequestConfig {
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
                // Assistant message with [tool= syntax that needs sanitizing
                ChatCompletionRequestMessage::Assistant(
                    async_openai::types::ChatCompletionRequestAssistantMessage {
                        content: Some(
                            async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(
                                "I called [tool=submit_proposal] with args".to_string(),
                            ),
                        ),
                        tool_calls: None,
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

        let body = strategy
            .prepare_request(&agent, &request_config, &overrides)
            .await
            .unwrap();
        // The input messages should have [tool= replaced with [call=
        let input = &body["input"];
        let input_str = serde_json::to_string(input).unwrap();
        assert!(
            !input_str.contains("[tool="),
            "Should sanitize [tool= to [call="
        );
        assert!(
            input_str.contains("[call="),
            "Should contain sanitized [call= syntax"
        );
    }

    // ---------------------------------------------------------------
    // parse_response: output with reasoning + message
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_output_with_reasoning_and_message() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "output": [
                {
                    "type": "reasoning",
                    "content": [{"text": "Step 1: analyze"}]
                },
                {
                    "type": "message",
                    "content": [{"text": "The answer is 42"}]
                }
            ]
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert!(
            content.contains("<thought_process>"),
            "Should wrap reasoning in thought_process tags"
        );
        assert!(content.contains("Step 1: analyze"));
        assert!(content.contains("The answer is 42"));
    }

    // ---------------------------------------------------------------
    // parse_response: result field extraction
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_result_with_response_field() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "result": {
                "response": "Hello from result.response"
            }
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert_eq!(content, "Hello from result.response");
    }

    #[tokio::test]
    async fn test_parse_response_result_as_string() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "result": "Direct string result"
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert_eq!(content, "Direct string result");
    }

    // ---------------------------------------------------------------
    // parse_response: service_tier "auto" quirk removal
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_parse_response_service_tier_auto_removed() {
        let strategy = XmlRegexStrategy::default();
        let response_body = json!({
            "id": "test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "service_tier": "auto",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })
        .to_string();

        let result = strategy.parse_response(&response_body).await.unwrap();
        let content = result.choices[0].message.content.as_ref().unwrap();
        assert_eq!(content, "Hello");
    }
}
