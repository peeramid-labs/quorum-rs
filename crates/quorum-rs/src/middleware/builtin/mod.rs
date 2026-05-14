//! Builtin middleware implementations.
//!
//! - [`RuleBasedMiddleware`] — regex blocklist, PII detection, content length limits
//! - [`LlmModerationMiddleware`] — LLM-based content classification via `AiModel` + tool calling
//!
//! Deliberation-output prompt-exposure guards are exposed through the
//! [`crate::agents::OutputLeakDetector`] trait (attached via
//! [`crate::agents::ProposerEvaluatorAgent::with_output_guard`]) rather than
//! this factory, so configs naming `BuiltinMiddlewareType::PromptExposure`
//! get a clear "use OutputLeakDetector instead" error from this factory.

pub mod llm_moderation;
pub mod rules;

use super::config::BuiltinMiddlewareType;
use crate::llms::AiModel;
use crate::middleware::{AgentMiddleware, MiddlewareStage};
use std::sync::Arc;

/// Create a builtin middleware from its type enum, config JSON, active stages,
/// and an optional LLM model (for moderation).
///
/// Returns an error for [`BuiltinMiddlewareType::PromptExposure`]: that
/// guardrail is invoked via the [`OutputLeakDetector`](crate::agents::OutputLeakDetector)
/// trait, attached with
/// [`crate::agents::ProposerEvaluatorAgent::with_output_guard`].
pub fn create_builtin_middleware(
    builtin_type: &BuiltinMiddlewareType,
    config: &serde_json::Value,
    stages: Vec<MiddlewareStage>,
    moderation_model: Option<Arc<dyn AiModel>>,
) -> Result<Box<dyn AgentMiddleware>, String> {
    match builtin_type {
        BuiltinMiddlewareType::RuleBased => {
            let mw = rules::RuleBasedMiddleware::from_config(config, stages)?;
            Ok(Box::new(mw))
        }
        BuiltinMiddlewareType::LlmModeration => {
            let mw = llm_moderation::LlmModerationMiddleware::from_config(
                config,
                stages,
                moderation_model,
            )?;
            Ok(Box::new(mw))
        }
        BuiltinMiddlewareType::PromptExposure => Err(
            "PromptExposure is not a middleware factory entry; attach an \
             OutputLeakDetector implementation via ProposerEvaluatorAgent::with_output_guard."
                .to_string(),
        ),
        BuiltinMiddlewareType::SignatureVerification => {
            tracing::warn!(
                "SignatureVerification builtin requires crypto crate (#115). \
                 Operating in pass-through mode — no signatures verified."
            );
            let mw = rules::RuleBasedMiddleware::passthrough("signature_verification", stages);
            Ok(Box::new(mw))
        }
    }
}
