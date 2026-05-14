//! Agent middleware system — pluggable validation, transformation, and moderation.
//!
//! Middleware runs at lifecycle points in the agent pipeline:
//!
//! | Point | When |
//! |-------|------|
//! | `before_release` | Before buffer entry is published to NATS (edit + release stages) |
//! | `on_provider_response` | After LLM returns, before buffer entry creation |
//! | `before_prompt` | Before constructing the LLM prompt |
//! | `on_completion` | After deliberation result is finalized |
//!
//! Each middleware returns a [`Verdict`]: `Pass`, `Warn` (proceed but annotate), or `Block` (reject).
//!
//! # Middleware types
//!
//! - **Builtin**: compiled into the binary (fastest, zero-overhead)
//! - **BinaryMiddleware** (`nsed-agent`): external process via stdin/stdout JSON protocol
//! - **DylibMiddleware** (`nsed-agent`): FFI dynamic library via C ABI (`nsed_middleware_execute`)
//!
//! # Design influences
//!
//! Inspired by LangChain v1's middleware architecture (6-hook model with
//! `before_model`/`after_model`/`wrap_model_call`), adapted for NSED's
//! buffer-based HITL flow. Key differences:
//! - Binary middleware via stdin/stdout JSON protocol (polyglot, not Python-only)
//! - `Verdict::Warn` for audit trail annotation (LangChain lacks this)
//! - Stage filtering (edit vs release) for early/late feedback
//! - Sequential pipeline (simpler than graph-based composition)

pub mod pipeline;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Debug;
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------

/// Outcome of a middleware execution.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Proceed (optionally with transformed content).
    Pass,
    /// Proceed but annotate for audit trail. Reason should explain the warning.
    Warn,
    /// Reject — entry stays in buffer, API returns 422. Reason is required.
    Block,
}

/// Result returned by a single middleware invocation.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MiddlewareVerdict {
    /// The verdict: pass, warn, or block.
    pub verdict: Verdict,
    /// Optional: modified content. Subsequent middleware see this instead of the original.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    /// Human-readable reason (required for warn/block).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Classification category (e.g., "harassment", "format", "pii").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// State to pass to downstream middleware. Merged into `MiddlewareContext.hook_state`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub hook_state: HashMap<String, serde_json::Value>,
}

impl MiddlewareVerdict {
    /// Create a Pass verdict with no content transformation.
    pub fn pass() -> Self {
        Self {
            verdict: Verdict::Pass,
            content: None,
            reason: None,
            category: None,
            hook_state: HashMap::new(),
        }
    }

    /// Create a Pass verdict with transformed content.
    pub fn pass_with_content(content: serde_json::Value) -> Self {
        Self {
            verdict: Verdict::Pass,
            content: Some(content),
            reason: None,
            category: None,
            hook_state: HashMap::new(),
        }
    }

    /// Create a Warn verdict.
    pub fn warn(category: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Warn,
            content: None,
            reason: Some(reason.into()),
            category: Some(category.into()),
            hook_state: HashMap::new(),
        }
    }

    /// Create a Block verdict.
    pub fn block(category: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            verdict: Verdict::Block,
            content: None,
            reason: Some(reason.into()),
            category: Some(category.into()),
            hook_state: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Stage
// ---------------------------------------------------------------------------

/// Lifecycle stage at which middleware executes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MiddlewareStage {
    /// Buffer edit (PUT without ?release=true) — lightweight, early feedback.
    Edit,
    /// Buffer release (PUT ?release=true or POST .../release) — full validation gate.
    Release,
    /// After LLM provider returns, before buffer entry creation.
    ProviderResponse,
    /// Before constructing the LLM prompt.
    BeforePrompt,
    /// After deliberation result is finalized.
    Completion,
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Context passed to every middleware invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiddlewareContext {
    /// The content being processed (proposal/evaluation JSON, prompt text, etc.).
    pub content: serde_json::Value,
    /// Action type: "propose" or "evaluate".
    pub action: String,
    /// Agent ID.
    pub agent_id: String,
    /// Job/session ID.
    pub job_id: String,
    /// Current deliberation round (0 if not in a round).
    pub round: u32,
    /// Which lifecycle stage this invocation is for.
    pub stage: MiddlewareStage,
    /// Arbitrary metadata (agent config, job metadata, etc.).
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Inter-middleware state — middleware can pass data to downstream middleware.
    /// Ephemeral: does not persist between pipeline runs.
    #[serde(default)]
    pub hook_state: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// A single middleware in the agent pipeline.
///
/// Implement this trait to add custom validation, transformation, or moderation
/// logic. Middleware can be compiled into the binary (builtin) or run as an
/// external process (binary middleware via stdin/stdout JSON protocol).
#[async_trait]
pub trait AgentMiddleware: Send + Sync + Debug {
    /// Execute the middleware. Receives context, returns verdict.
    ///
    /// The pipeline calls this for each middleware in order. If the verdict
    /// includes `content`, subsequent middleware see the transformed content.
    async fn execute(&self, ctx: &MiddlewareContext) -> MiddlewareVerdict;

    /// Which stages this middleware runs at. Default: edit + release + provider_response.
    fn stages(&self) -> Vec<MiddlewareStage> {
        vec![
            MiddlewareStage::Edit,
            MiddlewareStage::Release,
            MiddlewareStage::ProviderResponse,
        ]
    }

    /// Human-readable name for logging and config display.
    fn name(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Warning (accumulated by pipeline)
// ---------------------------------------------------------------------------

/// A warning emitted by a middleware that returned `Verdict::Warn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    /// Name of the middleware that issued the warning.
    pub middleware: String,
    /// Classification category (if provided).
    pub category: Option<String>,
    /// Human-readable reason.
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_pass_default() {
        let v = MiddlewareVerdict::pass();
        assert_eq!(v.verdict, Verdict::Pass);
        assert!(v.content.is_none());
        assert!(v.reason.is_none());
    }

    #[test]
    fn verdict_block_has_reason() {
        let v = MiddlewareVerdict::block("pii", "Contains email addresses");
        assert_eq!(v.verdict, Verdict::Block);
        assert_eq!(v.category.as_deref(), Some("pii"));
        assert_eq!(v.reason.as_deref(), Some("Contains email addresses"));
    }

    #[test]
    fn verdict_warn_has_category() {
        let v = MiddlewareVerdict::warn("format", "Response exceeds recommended length");
        assert_eq!(v.verdict, Verdict::Warn);
        assert_eq!(v.category.as_deref(), Some("format"));
    }

    #[test]
    fn verdict_pass_with_content() {
        let content = serde_json::json!({"cleaned": true});
        let v = MiddlewareVerdict::pass_with_content(content.clone());
        assert_eq!(v.verdict, Verdict::Pass);
        assert_eq!(v.content.unwrap(), content);
    }

    #[test]
    fn middleware_context_serde_roundtrip() {
        let ctx = MiddlewareContext {
            content: serde_json::json!({"text": "hello"}),
            action: "propose".to_string(),
            agent_id: "agent-1".to_string(),
            job_id: "job-42".to_string(),
            round: 2,
            stage: MiddlewareStage::Release,
            metadata: serde_json::json!({}),
            hook_state: HashMap::new(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: MiddlewareContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.agent_id, "agent-1");
        assert_eq!(deserialized.stage, MiddlewareStage::Release);
        assert_eq!(deserialized.round, 2);
    }

    #[test]
    fn middleware_verdict_serde_roundtrip() {
        let v = MiddlewareVerdict::block("harassment", "Violates guidelines");
        let json = serde_json::to_string(&v).unwrap();
        let deserialized: MiddlewareVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.verdict, Verdict::Block);
        assert_eq!(deserialized.category.as_deref(), Some("harassment"));
    }

    #[test]
    fn hook_state_propagation() {
        let mut ctx = MiddlewareContext {
            content: serde_json::json!(null),
            action: "propose".to_string(),
            agent_id: "a".to_string(),
            job_id: "j".to_string(),
            round: 0,
            stage: MiddlewareStage::Edit,
            metadata: serde_json::json!(null),
            hook_state: HashMap::new(),
        };
        ctx.hook_state
            .insert("pii_detected".to_string(), serde_json::json!(true));

        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: MiddlewareContext = serde_json::from_str(&json).unwrap();
        assert_eq!(
            deserialized.hook_state.get("pii_detected"),
            Some(&serde_json::json!(true))
        );
    }
}

pub mod binary;
pub mod builtin;
pub mod config;
pub mod dylib;

pub use binary::BinaryMiddleware;
pub use config::{MiddlewareConfig, MiddlewareEntry};
pub use dylib::DylibMiddleware;
