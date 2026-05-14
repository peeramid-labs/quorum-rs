//! Builtin middleware implementations.
//!
//! - [`RuleBasedMiddleware`] — regex blocklist, PII detection, content length limits
//! - [`LlmModerationMiddleware`] — LLM-based content classification via `AiModel` + tool calling
//!
//! NSED's `PromptExposureMiddleware` (deliberation-output guardrail) is BSL-only
//! and lives in the `nsed-agent` crate. It's invoked via the
//! [`crate::agents::OutputLeakDetector`] trait rather than this factory, so
//! OSS configs that name `BuiltinMiddlewareType::PromptExposure` get a clear
//! "not available in OSS SDK" error from this factory.

pub mod llm_moderation;
pub mod rules;

use super::config::BuiltinMiddlewareType;
use crate::llms::AiModel;
use crate::middleware::{AgentMiddleware, MiddlewareStage};
use std::sync::Arc;

/// Create a builtin middleware from its type enum, config JSON, active stages,
/// and an optional LLM model (for moderation).
///
/// Returns an error for [`BuiltinMiddlewareType::PromptExposure`] — that
/// middleware is BSL-only. Use
/// [`crate::agents::ProposerEvaluatorAgent::with_output_guard`] with an
/// [`crate::agents::OutputLeakDetector`] impl instead (e.g.
/// `nsed_agent::middleware::builtin::prompt_exposure::PromptExposureMiddleware`).
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
            "PromptExposure middleware is BSL-only. Use ProposerEvaluatorAgent::with_output_guard \
             with an OutputLeakDetector impl (e.g. PromptExposureMiddleware from nsed-agent)."
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
