//! LLM-based content moderation middleware.
//!
//! Uses the existing `AiModel` trait and provider infrastructure to classify
//! content against configurable categories. Leverages structured tool calling
//! (same as agent react loop) so the LLM returns a typed `ModerationResponse`
//! rather than free-form text that needs fragile parsing.
//!
//! Most expensive middleware — should run last in the pipeline, only at release stage.
//!
//! # Provider reuse
//!
//! The middleware receives an `Arc<dyn AiModel>` built from the same provider
//! config pool as deliberation agents. This gives:
//! - All providers work (Ollama, Together AI, Anthropic, OpenAI, vLLM)
//! - Rate limiting via existing `qps`/`concurrency` config
//! - Retry logic from the provider infrastructure
//! - Response parsing handles all engine quirks

use crate::agents::config::AgentConfig;
use crate::llms::{AiModel, RequestConfig};
use crate::middleware::{
    AgentMiddleware, MiddlewareContext, MiddlewareStage, MiddlewareVerdict, Verdict,
};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// LLM moderation middleware configuration from YAML.
#[derive(Debug, Deserialize)]
struct LlmModerationConfig {
    /// Categories to check (e.g., ["harassment", "hate_speech", "nsfw"]).
    #[serde(default = "default_categories")]
    categories: Vec<String>,
    /// What to do when LLM returns "warn": annotate (proceed) or block.
    #[serde(default)]
    on_warning: WarningAction,
    /// What to do when LLM returns an unrecognized verdict: block (default, fail-closed) or pass.
    #[serde(default)]
    on_unknown_verdict: UnknownVerdictAction,
    /// Maximum content length to send to the moderation LLM. Content is
    /// truncated to this length before sending. Default: 10000 chars.
    #[serde(default = "default_max_moderation_length")]
    max_moderation_length: usize,
    /// Number of retries when LLM returns an unrecognized/malformed verdict.
    /// Default: 0 (no retries — apply on_unknown_verdict immediately).
    #[serde(default)]
    max_retries: u32,
    /// Model name for moderation (e.g., "meta-llama/Llama-Guard-3-8B").
    /// Defaults to the configured model if not set.
    #[serde(default)]
    model_name: Option<String>,
}

fn default_categories() -> Vec<String> {
    vec![
        "harassment".to_string(),
        "hate_speech".to_string(),
        "nsfw".to_string(),
        "prompt_injection".to_string(),
    ]
}

fn default_max_moderation_length() -> usize {
    10000
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum WarningAction {
    /// Proceed with annotation (default).
    #[default]
    Annotate,
    /// Treat warnings as blocks.
    Block,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum UnknownVerdictAction {
    /// Block on unrecognized verdict (default, fail-closed).
    #[default]
    Block,
    /// Pass on unrecognized verdict (fail-open — use only if you trust the model).
    Pass,
}

// ---------------------------------------------------------------------------
// LLM Response (structured via tool calling)
// ---------------------------------------------------------------------------

/// Expected JSON structure from the moderation LLM (via tool call arguments).
#[derive(Debug, Deserialize)]
struct ModerationResponse {
    results: Vec<CategoryResult>,
}

#[derive(Debug, Deserialize)]
struct CategoryResult {
    category: String,
    verdict: String, // "pass", "warn", or "block"
    #[serde(default)]
    reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool schema for structured output
// ---------------------------------------------------------------------------

/// Build the `submit_moderation` tool schema for structured LLM output.
fn moderation_tool_schema(categories: &[String]) -> async_openai::types::ChatCompletionTool {
    use async_openai::types::{ChatCompletionTool, ChatCompletionToolType, FunctionObject};

    let category_enum: Vec<serde_json::Value> =
        categories.iter().map(|c| serde_json::json!(c)).collect();

    ChatCompletionTool {
        r#type: ChatCompletionToolType::Function,
        function: FunctionObject {
            name: "submit_moderation".to_string(),
            description: Some(
                "Submit the content moderation results. For each category, provide a verdict (pass/warn/block) and reason."
                    .to_string(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "results": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "category": {
                                    "type": "string",
                                    "enum": category_enum,
                                    "description": "The content category being evaluated"
                                },
                                "verdict": {
                                    "type": "string",
                                    "enum": ["pass", "warn", "block"],
                                    "description": "pass = no violation, warn = borderline/flagged, block = clear violation"
                                },
                                "reason": {
                                    "type": "string",
                                    "description": "Brief explanation for the verdict"
                                }
                            },
                            "required": ["category", "verdict", "reason"],
                            "additionalProperties": false
                        },
                        "description": "One result per category"
                    }
                },
                "required": ["results"],
                "additionalProperties": false
            })),
            strict: Some(true),
        },
    }
}

// ---------------------------------------------------------------------------
// LlmModerationMiddleware
// ---------------------------------------------------------------------------

/// LLM-based content moderation middleware.
///
/// Uses the existing `AiModel` + tool calling infrastructure to classify
/// content. The LLM receives a `submit_moderation` tool with the category
/// schema and returns structured per-category verdicts.
#[derive(Debug)]
pub struct LlmModerationMiddleware {
    categories: Vec<String>,
    on_warning: WarningAction,
    on_unknown_verdict: UnknownVerdictAction,
    max_moderation_length: usize,
    max_retries: u32,
    stages: Vec<MiddlewareStage>,
    /// The LLM model to use for moderation. None = pass-through mode.
    model: Option<Arc<dyn AiModel>>,
    /// Minimal AgentConfig for the moderation LLM call.
    agent_config: AgentConfig,
}

impl LlmModerationMiddleware {
    /// Create from YAML config JSON, active stages, and an optional LLM model.
    ///
    /// When `model` is None, the middleware operates in pass-through mode
    /// (useful for testing or when no moderation provider is configured).
    pub fn from_config(
        config: &serde_json::Value,
        stages: Vec<MiddlewareStage>,
        model: Option<Arc<dyn AiModel>>,
    ) -> Result<Self, String> {
        let cfg: LlmModerationConfig = if config.is_null() {
            LlmModerationConfig {
                categories: default_categories(),
                on_warning: WarningAction::default(),
                on_unknown_verdict: UnknownVerdictAction::default(),
                max_moderation_length: default_max_moderation_length(),
                max_retries: 0,
                model_name: None,
            }
        } else {
            serde_json::from_value(config.clone())
                .map_err(|e| format!("Invalid llm_moderation config: {e}"))?
        };

        if cfg.categories.is_empty() {
            return Err("llm_moderation requires at least one category".to_string());
        }

        // Build a minimal AgentConfig for the moderation call
        let agent_config = AgentConfig {
            name: "moderation".to_string(),
            provider_id: "moderation".to_string(),
            model_name: cfg.model_name.unwrap_or_else(|| "moderation".to_string()),
            temperature: 0.0, // Deterministic for safety classification
            max_tokens: 1000,
            json_mode: true,
            disable_native_tools: false,
            ..Default::default()
        };

        Ok(Self {
            categories: cfg.categories,
            on_warning: cfg.on_warning,
            on_unknown_verdict: cfg.on_unknown_verdict,
            max_moderation_length: cfg.max_moderation_length,
            max_retries: cfg.max_retries,
            stages,
            model,
            agent_config,
        })
    }

    /// Build the system prompt for moderation.
    fn system_prompt(&self) -> String {
        let categories = self.categories.join(", ");
        format!(
            "You are a content safety moderator. Evaluate the provided content against these categories: {categories}.\n\n\
             For each category, classify as:\n\
             - \"pass\" — content does not violate this category\n\
             - \"warn\" — borderline content, should be flagged for review\n\
             - \"block\" — clear violation, content must be rejected\n\n\
             Use the submit_moderation tool to return your results. Be concise in reasons."
        )
    }

    /// Aggregate per-category verdicts into a single pipeline verdict.
    /// Returns (verdict, had_unknown_verdicts) so caller can decide to retry.
    fn aggregate_response(&self, response: &ModerationResponse) -> (MiddlewareVerdict, bool) {
        let mut worst_verdict = Verdict::Pass;
        let mut worst_category = String::new();
        let mut worst_reason = String::new();

        let mut has_unknown = false;
        for result in &response.results {
            let verdict = match result.verdict.to_lowercase().as_str() {
                "block" => Verdict::Block,
                "warn" => Verdict::Warn,
                "pass" | "safe" | "ok" => Verdict::Pass,
                unknown => {
                    has_unknown = true;
                    tracing::warn!(verdict = %unknown, "Unrecognized moderation verdict");
                    match self.on_unknown_verdict {
                        UnknownVerdictAction::Block => Verdict::Block,
                        UnknownVerdictAction::Pass => Verdict::Pass,
                    }
                }
            };

            let is_worse = matches!(
                (&worst_verdict, &verdict),
                (Verdict::Pass, Verdict::Warn | Verdict::Block) | (Verdict::Warn, Verdict::Block)
            );

            if is_worse {
                worst_verdict = verdict;
                worst_category = result.category.clone();
                worst_reason = result
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("{} violation detected", result.category));
            }
        }

        let verdict = match worst_verdict {
            Verdict::Block => MiddlewareVerdict::block(worst_category, worst_reason),
            Verdict::Warn => match self.on_warning {
                WarningAction::Block => MiddlewareVerdict::block(worst_category, worst_reason),
                WarningAction::Annotate => MiddlewareVerdict::warn(worst_category, worst_reason),
            },
            Verdict::Pass => MiddlewareVerdict::pass(),
        };
        (verdict, has_unknown)
    }

    /// Parse a moderation response from text (fallback when tool calling isn't used).
    pub fn parse_response(&self, response: &str) -> MiddlewareVerdict {
        let json_str = extract_json(response);
        match serde_json::from_str::<ModerationResponse>(json_str) {
            Ok(m) => self.aggregate_response(&m).0,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    response = %response,
                    "LLM moderation response is not valid JSON — fail closed"
                );
                MiddlewareVerdict::block(
                    "moderation_error",
                    "Moderation LLM returned unparsable response — blocking for safety",
                )
            }
        }
    }
}

/// Extract JSON from potentially markdown-wrapped LLM output.
fn extract_json(s: &str) -> &str {
    if let Some(start) = s.find("```json") {
        let content = &s[start + 7..];
        if let Some(end) = content.find("```") {
            return content[..end].trim();
        }
    }
    if let Some(start) = s.find("```") {
        let content = &s[start + 3..];
        if let Some(end) = content.find("```") {
            return content[..end].trim();
        }
    }
    if let Some(start) = s.find('{') {
        if let Some(end) = s.rfind('}') {
            return &s[start..=end];
        }
    }
    s.trim()
}

#[async_trait]
impl AgentMiddleware for LlmModerationMiddleware {
    async fn execute(&self, ctx: &MiddlewareContext) -> MiddlewareVerdict {
        let content = if let Some(s) = ctx.content.as_str() {
            s.to_string()
        } else {
            ctx.content.to_string()
        };

        // Truncate for moderation (no need to scan full 200KB proposals).
        // Use char_indices to find a safe UTF-8 boundary.
        let truncated = if content.len() > self.max_moderation_length {
            let boundary = content
                .char_indices()
                .take_while(|(i, _)| *i < self.max_moderation_length)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            &content[..boundary]
        } else {
            &content
        };

        // If we have a model, use the full AiModel + tool calling pipeline (with retries)
        if let Some(ref model) = self.model {
            for attempt in 0..=self.max_retries {
                let verdict = self.execute_with_model(model, truncated).await;
                // Only retry if the verdict was caused by an unknown/malformed LLM response
                // and we haven't exhausted retries
                if attempt < self.max_retries
                    && verdict.verdict == crate::middleware::Verdict::Block
                    && verdict.category.as_deref() == Some("moderation_error")
                {
                    tracing::info!(
                        attempt = attempt + 1,
                        max_retries = self.max_retries,
                        "Retrying moderation LLM call due to malformed response"
                    );
                    continue;
                }
                return verdict;
            }
            // Should not reach here, but fail closed
            return MiddlewareVerdict::block("moderation_error", "Max retries exceeded");
        }

        // Fallback: check hook_state for injected response (test harness)
        if let Some(response_val) = ctx.hook_state.get("moderation_response") {
            if let Some(response_str) = response_val.as_str() {
                return self.parse_response(response_str);
            }
        }

        // No model available — pass through with warning
        tracing::debug!(
            agent_id = %ctx.agent_id,
            categories = ?self.categories,
            "LLM moderation middleware: no model configured, passing through"
        );
        MiddlewareVerdict::pass()
    }

    fn stages(&self) -> Vec<MiddlewareStage> {
        self.stages.clone()
    }

    fn name(&self) -> &str {
        "llm_moderation"
    }
}

impl LlmModerationMiddleware {
    /// Execute moderation using the AiModel + structured tool calling.
    async fn execute_with_model(
        &self,
        model: &Arc<dyn AiModel>,
        content: &str,
    ) -> MiddlewareVerdict {
        use async_openai::types::{
            ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
            ChatCompletionRequestUserMessageArgs,
        };

        let system_msg = ChatCompletionRequestSystemMessageArgs::default()
            .content(self.system_prompt())
            .build()
            .map(ChatCompletionRequestMessage::System);

        let user_msg = ChatCompletionRequestUserMessageArgs::default()
            .content(format!("Moderate this content:\n\n{content}"))
            .build()
            .map(ChatCompletionRequestMessage::User);

        let (system_msg, user_msg) = match (system_msg, user_msg) {
            (Ok(s), Ok(u)) => (s, u),
            _ => {
                tracing::error!("Failed to build moderation messages");
                return MiddlewareVerdict::block(
                    "moderation_error",
                    "Failed to construct moderation request",
                );
            }
        };

        let tool = moderation_tool_schema(&self.categories);
        let request = RequestConfig {
            messages: vec![system_msg, user_msg],
            tools: Some(vec![tool]),
            tool_choice: None,
            presence_penalty: None,
        };

        match model.chat_completion(&self.agent_config, request).await {
            Ok(result) => {
                // Extract tool call arguments from the response
                if let Some(choice) = result.response.choices.first() {
                    // Check for tool calls (structured output path)
                    if let Some(ref tool_calls) = choice.message.tool_calls {
                        if let Some(tc) = tool_calls.first() {
                            match serde_json::from_str::<ModerationResponse>(&tc.function.arguments)
                            {
                                Ok(moderation) => return self.aggregate_response(&moderation).0,
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        args_len = tc.function.arguments.len(),
                                        "Failed to parse moderation tool call — fail closed"
                                    );
                                    return MiddlewareVerdict::block(
                                        "moderation_error",
                                        "Moderation tool call returned invalid arguments",
                                    );
                                }
                            }
                        }
                    }

                    // Fallback: parse content as text (some models don't use tool calling)
                    if let Some(ref content) = choice.message.content {
                        return self.parse_response(content);
                    }
                }

                tracing::warn!("Moderation LLM returned empty response — fail closed");
                MiddlewareVerdict::block(
                    "moderation_error",
                    "Moderation LLM returned empty response",
                )
            }
            Err(e) => {
                tracing::error!(error = %e, "Moderation LLM call failed — fail closed");
                MiddlewareVerdict::block(
                    "moderation_error",
                    format!("Moderation LLM call failed: {e}"),
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_ctx(content: &str) -> MiddlewareContext {
        MiddlewareContext {
            content: serde_json::json!(content),
            action: "propose".to_string(),
            agent_id: "test-agent".to_string(),
            job_id: "test-job".to_string(),
            round: 1,
            stage: MiddlewareStage::Release,
            metadata: serde_json::json!(null),
            hook_state: HashMap::new(),
        }
    }

    fn default_mw() -> LlmModerationMiddleware {
        LlmModerationMiddleware::from_config(
            &serde_json::json!(null),
            vec![MiddlewareStage::Release],
            None, // no model — test/pass-through mode
        )
        .unwrap()
    }

    #[test]
    fn parse_all_pass() {
        let mw = default_mw();
        let response = r#"{"results": [
            {"category": "harassment", "verdict": "pass"},
            {"category": "nsfw", "verdict": "pass"}
        ]}"#;
        let verdict = mw.parse_response(response);
        assert_eq!(verdict.verdict, Verdict::Pass);
    }

    #[test]
    fn parse_block_verdict() {
        let mw = default_mw();
        let response = r#"{"results": [
            {"category": "harassment", "verdict": "block", "reason": "Contains targeted harassment"},
            {"category": "nsfw", "verdict": "pass"}
        ]}"#;
        let verdict = mw.parse_response(response);
        assert_eq!(verdict.verdict, Verdict::Block);
        assert_eq!(verdict.category.as_deref(), Some("harassment"));
        assert!(verdict.reason.as_deref().unwrap().contains("harassment"));
    }

    #[test]
    fn parse_warn_with_annotate() {
        let mw = LlmModerationMiddleware::from_config(
            &serde_json::json!({"on_warning": "annotate"}),
            vec![MiddlewareStage::Release],
            None,
        )
        .unwrap();
        let response = r#"{"results": [
            {"category": "nsfw", "verdict": "warn", "reason": "Borderline content"}
        ]}"#;
        let verdict = mw.parse_response(response);
        assert_eq!(verdict.verdict, Verdict::Warn);
    }

    #[test]
    fn parse_warn_with_block_action() {
        let mw = LlmModerationMiddleware::from_config(
            &serde_json::json!({"on_warning": "block"}),
            vec![MiddlewareStage::Release],
            None,
        )
        .unwrap();
        let response = r#"{"results": [
            {"category": "nsfw", "verdict": "warn", "reason": "Borderline"}
        ]}"#;
        let verdict = mw.parse_response(response);
        assert_eq!(verdict.verdict, Verdict::Block);
    }

    #[test]
    fn parse_invalid_json_blocks() {
        let mw = default_mw();
        let verdict = mw.parse_response("not json at all");
        assert_eq!(verdict.verdict, Verdict::Block);
        assert_eq!(verdict.category.as_deref(), Some("moderation_error"));
    }

    #[test]
    fn parse_markdown_wrapped_json() {
        let mw = default_mw();
        let response = r#"Here's the analysis:
```json
{"results": [{"category": "harassment", "verdict": "pass"}]}
```"#;
        let verdict = mw.parse_response(response);
        assert_eq!(verdict.verdict, Verdict::Pass);
    }

    #[test]
    fn block_beats_warn() {
        let mw = default_mw();
        let response = r#"{"results": [
            {"category": "nsfw", "verdict": "warn", "reason": "Mild"},
            {"category": "harassment", "verdict": "block", "reason": "Severe"}
        ]}"#;
        let verdict = mw.parse_response(response);
        assert_eq!(verdict.verdict, Verdict::Block);
        assert_eq!(verdict.category.as_deref(), Some("harassment"));
    }

    #[tokio::test]
    async fn execute_with_hook_state_response() {
        let mw = default_mw();
        let mut ctx = make_ctx("Some content to moderate");
        ctx.hook_state.insert(
            "moderation_response".to_string(),
            serde_json::json!(r#"{"results": [{"category": "harassment", "verdict": "block", "reason": "Test block"}]}"#),
        );
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, Verdict::Block);
    }

    #[tokio::test]
    async fn execute_without_model_passes_through() {
        let mw = default_mw();
        let ctx = make_ctx("Some content");
        let verdict = mw.execute(&ctx).await;
        // No model configured → pass through
        assert_eq!(verdict.verdict, Verdict::Pass);
    }

    #[test]
    fn empty_categories_rejected() {
        let result = LlmModerationMiddleware::from_config(
            &serde_json::json!({"categories": []}),
            vec![MiddlewareStage::Release],
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn extract_json_from_markdown() {
        assert_eq!(extract_json("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(extract_json("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(extract_json("text {\"a\":1} text"), "{\"a\":1}");
        assert_eq!(extract_json("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn tool_schema_has_correct_categories() {
        let tool = moderation_tool_schema(&["harassment".to_string(), "nsfw".to_string()]);
        assert_eq!(tool.function.name, "submit_moderation");
        let params = tool.function.parameters.unwrap();
        let items = &params["properties"]["results"]["items"]["properties"]["category"]["enum"];
        assert_eq!(items, &serde_json::json!(["harassment", "nsfw"]));
    }

    #[test]
    fn config_with_model_name() {
        let mw = LlmModerationMiddleware::from_config(
            &serde_json::json!({
                "model_name": "meta-llama/Llama-Guard-3-8B",
                "categories": ["harassment"]
            }),
            vec![MiddlewareStage::Release],
            None,
        )
        .unwrap();
        assert_eq!(mw.agent_config.model_name, "meta-llama/Llama-Guard-3-8B");
    }
}
