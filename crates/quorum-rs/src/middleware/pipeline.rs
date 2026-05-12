//! Middleware pipeline — executes middleware sequentially with short-circuit on Block.

use super::{AgentMiddleware, MiddlewareContext, Verdict, Warning};
use serde::{Deserialize, Serialize};

/// Result of running the full middleware pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PipelineResult {
    /// All middleware passed (some may have warned).
    Passed {
        /// Accumulated warnings from middleware that returned `Verdict::Warn`.
        warnings: Vec<Warning>,
    },
    /// A middleware blocked the content. Pipeline short-circuited.
    Blocked {
        /// Classification category from the blocking middleware.
        category: String,
        /// Human-readable reason for the block.
        reason: String,
        /// Name of the middleware that blocked.
        blocked_by: String,
    },
}

impl PipelineResult {
    /// Returns true if the pipeline passed (no blocks).
    pub fn is_passed(&self) -> bool {
        matches!(self, Self::Passed { .. })
    }

    /// Returns the warnings if passed, empty vec if blocked.
    pub fn warnings(&self) -> &[Warning] {
        match self {
            Self::Passed { warnings } => warnings,
            Self::Blocked { .. } => &[],
        }
    }
}

/// Sequential middleware pipeline.
///
/// Executes middleware in order. Short-circuits on the first `Block` verdict.
/// If a middleware returns transformed content, subsequent middleware see it.
/// Warnings are accumulated and returned alongside the final result.
pub struct MiddlewarePipeline {
    middleware: Vec<Box<dyn AgentMiddleware>>,
}

impl MiddlewarePipeline {
    /// Create a new pipeline from an ordered list of middleware.
    pub fn new(middleware: Vec<Box<dyn AgentMiddleware>>) -> Self {
        Self { middleware }
    }

    /// Create an empty pipeline (no-op).
    pub fn empty() -> Self {
        Self {
            middleware: Vec::new(),
        }
    }

    /// Returns true if this pipeline has no middleware configured.
    pub fn is_empty(&self) -> bool {
        self.middleware.is_empty()
    }

    /// Number of middleware in the pipeline.
    pub fn len(&self) -> usize {
        self.middleware.len()
    }

    /// Run all middleware for the given stage.
    ///
    /// - Skips middleware not applicable to `ctx.stage`.
    /// - Short-circuits on the first `Block` verdict.
    /// - Accumulates `Warn` verdicts.
    /// - If a middleware returns transformed content, `ctx.content` is updated
    ///   so subsequent middleware see the new content.
    pub async fn run(&self, ctx: &mut MiddlewareContext) -> PipelineResult {
        let mut warnings = Vec::new();

        for mw in &self.middleware {
            if !mw.stages().contains(&ctx.stage) {
                continue;
            }

            let verdict = mw.execute(ctx).await;

            // Merge hook_state from verdict into context for downstream middleware
            for (k, v) in &verdict.hook_state {
                ctx.hook_state.insert(k.clone(), v.clone());
            }

            match verdict.verdict {
                Verdict::Block => {
                    tracing::warn!(
                        middleware = mw.name(),
                        category = ?verdict.category,
                        reason = ?verdict.reason,
                        agent_id = %ctx.agent_id,
                        stage = ?ctx.stage,
                        "Middleware blocked content"
                    );
                    return PipelineResult::Blocked {
                        category: verdict.category.unwrap_or_default(),
                        reason: verdict.reason.unwrap_or_default(),
                        blocked_by: mw.name().to_string(),
                    };
                }
                Verdict::Warn => {
                    tracing::info!(
                        middleware = mw.name(),
                        category = ?verdict.category,
                        reason = ?verdict.reason,
                        agent_id = %ctx.agent_id,
                        "Middleware warning"
                    );
                    warnings.push(Warning {
                        middleware: mw.name().to_string(),
                        category: verdict.category.clone(),
                        reason: verdict.reason.clone(),
                    });
                    if let Some(ref new_content) = verdict.content {
                        ctx.content = new_content.clone();
                    }
                }
                Verdict::Pass => {
                    if let Some(ref new_content) = verdict.content {
                        ctx.content = new_content.clone();
                    }
                }
            }
        }

        PipelineResult::Passed { warnings }
    }
}

impl std::fmt::Debug for MiddlewarePipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.middleware.iter().map(|m| m.name()).collect();
        f.debug_struct("MiddlewarePipeline")
            .field("middleware", &names)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{MiddlewareStage, MiddlewareVerdict};
    use async_trait::async_trait;
    use std::collections::HashMap;

    // --- Test middleware implementations ---

    #[derive(Debug)]
    struct PassMiddleware;

    #[async_trait]
    impl AgentMiddleware for PassMiddleware {
        async fn execute(&self, _ctx: &MiddlewareContext) -> MiddlewareVerdict {
            MiddlewareVerdict::pass()
        }
        fn name(&self) -> &str {
            "pass"
        }
    }

    #[derive(Debug)]
    struct WarnMiddleware {
        category: String,
    }

    #[async_trait]
    impl AgentMiddleware for WarnMiddleware {
        async fn execute(&self, _ctx: &MiddlewareContext) -> MiddlewareVerdict {
            MiddlewareVerdict::warn(&self.category, "Just a warning")
        }
        fn name(&self) -> &str {
            "warn"
        }
    }

    #[derive(Debug)]
    struct BlockMiddleware {
        category: String,
    }

    #[async_trait]
    impl AgentMiddleware for BlockMiddleware {
        async fn execute(&self, _ctx: &MiddlewareContext) -> MiddlewareVerdict {
            MiddlewareVerdict::block(&self.category, "Content rejected")
        }
        fn name(&self) -> &str {
            "block"
        }
    }

    #[derive(Debug)]
    struct TransformMiddleware {
        new_content: serde_json::Value,
    }

    #[async_trait]
    impl AgentMiddleware for TransformMiddleware {
        async fn execute(&self, _ctx: &MiddlewareContext) -> MiddlewareVerdict {
            MiddlewareVerdict::pass_with_content(self.new_content.clone())
        }
        fn name(&self) -> &str {
            "transform"
        }
    }

    /// Middleware that only runs at Release stage.
    #[derive(Debug)]
    struct ReleaseOnlyMiddleware;

    #[async_trait]
    impl AgentMiddleware for ReleaseOnlyMiddleware {
        async fn execute(&self, _ctx: &MiddlewareContext) -> MiddlewareVerdict {
            MiddlewareVerdict::block("release_only", "Should only run at release")
        }
        fn stages(&self) -> Vec<MiddlewareStage> {
            vec![MiddlewareStage::Release]
        }
        fn name(&self) -> &str {
            "release_only"
        }
    }

    /// Middleware that reads and writes hook_state.
    #[derive(Debug)]
    struct StateWriterMiddleware;

    #[async_trait]
    impl AgentMiddleware for StateWriterMiddleware {
        async fn execute(&self, ctx: &MiddlewareContext) -> MiddlewareVerdict {
            // Write state for downstream middleware
            let mut v = MiddlewareVerdict::pass();
            // Can't mutate ctx directly, but pipeline passes ctx by &mut
            // so we need to use hook_state through the context
            // The pipeline test will verify this externally
            if ctx.hook_state.contains_key("seen") {
                v = MiddlewareVerdict::warn("state", "Already seen");
            }
            v
        }
        fn name(&self) -> &str {
            "state_writer"
        }
    }

    fn make_ctx(stage: MiddlewareStage) -> MiddlewareContext {
        MiddlewareContext {
            content: serde_json::json!({"text": "original"}),
            action: "propose".to_string(),
            agent_id: "test-agent".to_string(),
            job_id: "test-job".to_string(),
            round: 1,
            stage,
            metadata: serde_json::json!({}),
            hook_state: HashMap::new(),
        }
    }

    // --- Pipeline tests ---

    #[tokio::test]
    async fn pipeline_all_pass() {
        let pipeline =
            MiddlewarePipeline::new(vec![Box::new(PassMiddleware), Box::new(PassMiddleware)]);

        let mut ctx = make_ctx(MiddlewareStage::Release);
        let result = pipeline.run(&mut ctx).await;
        assert!(result.is_passed());
        assert!(result.warnings().is_empty());
    }

    #[tokio::test]
    async fn pipeline_pass_warn_pass() {
        let pipeline = MiddlewarePipeline::new(vec![
            Box::new(PassMiddleware),
            Box::new(WarnMiddleware {
                category: "format".to_string(),
            }),
            Box::new(PassMiddleware),
        ]);

        let mut ctx = make_ctx(MiddlewareStage::Release);
        let result = pipeline.run(&mut ctx).await;
        assert!(result.is_passed());
        assert_eq!(result.warnings().len(), 1);
        assert_eq!(result.warnings()[0].middleware, "warn");
        assert_eq!(result.warnings()[0].category.as_deref(), Some("format"));
    }

    #[tokio::test]
    async fn pipeline_short_circuits_on_block() {
        let pipeline = MiddlewarePipeline::new(vec![
            Box::new(PassMiddleware),
            Box::new(BlockMiddleware {
                category: "harassment".to_string(),
            }),
            Box::new(PassMiddleware), // should never run
        ]);

        let mut ctx = make_ctx(MiddlewareStage::Release);
        let result = pipeline.run(&mut ctx).await;
        assert!(!result.is_passed());
        match result {
            PipelineResult::Blocked {
                category,
                reason,
                blocked_by,
            } => {
                assert_eq!(category, "harassment");
                assert_eq!(reason, "Content rejected");
                assert_eq!(blocked_by, "block");
            }
            _ => panic!("Expected Blocked"),
        }
    }

    #[tokio::test]
    async fn pipeline_content_transformation_flows_through() {
        let pipeline = MiddlewarePipeline::new(vec![
            Box::new(TransformMiddleware {
                new_content: serde_json::json!({"text": "transformed"}),
            }),
            Box::new(PassMiddleware), // sees transformed content
        ]);

        let mut ctx = make_ctx(MiddlewareStage::Release);
        let result = pipeline.run(&mut ctx).await;
        assert!(result.is_passed());
        // ctx.content should be the transformed version
        assert_eq!(ctx.content, serde_json::json!({"text": "transformed"}));
    }

    #[tokio::test]
    async fn pipeline_stage_filtering() {
        // ReleaseOnlyMiddleware blocks — but we run at Edit stage, so it should be skipped
        let pipeline = MiddlewarePipeline::new(vec![
            Box::new(PassMiddleware),
            Box::new(ReleaseOnlyMiddleware), // skipped at Edit stage
        ]);

        let mut ctx = make_ctx(MiddlewareStage::Edit);
        let result = pipeline.run(&mut ctx).await;
        assert!(result.is_passed()); // Block was skipped because wrong stage
    }

    #[tokio::test]
    async fn pipeline_stage_filtering_blocks_at_correct_stage() {
        let pipeline = MiddlewarePipeline::new(vec![
            Box::new(PassMiddleware),
            Box::new(ReleaseOnlyMiddleware), // runs at Release stage
        ]);

        let mut ctx = make_ctx(MiddlewareStage::Release);
        let result = pipeline.run(&mut ctx).await;
        assert!(!result.is_passed()); // Block runs at Release
    }

    #[tokio::test]
    async fn pipeline_empty_is_noop() {
        let pipeline = MiddlewarePipeline::empty();
        assert!(pipeline.is_empty());
        assert_eq!(pipeline.len(), 0);

        let mut ctx = make_ctx(MiddlewareStage::Release);
        let result = pipeline.run(&mut ctx).await;
        assert!(result.is_passed());
        assert!(result.warnings().is_empty());
    }

    #[tokio::test]
    async fn pipeline_hook_state_available() {
        let pipeline = MiddlewarePipeline::new(vec![Box::new(StateWriterMiddleware)]);

        let mut ctx = make_ctx(MiddlewareStage::Release);
        // First run: no "seen" key → passes
        let result = pipeline.run(&mut ctx).await;
        assert!(result.is_passed());
        assert!(result.warnings().is_empty());

        // Set state, run again → warns
        ctx.hook_state
            .insert("seen".to_string(), serde_json::json!(true));
        let result = pipeline.run(&mut ctx).await;
        assert!(result.is_passed());
        assert_eq!(result.warnings().len(), 1);
    }

    #[tokio::test]
    async fn pipeline_multiple_warnings_accumulated() {
        let pipeline = MiddlewarePipeline::new(vec![
            Box::new(WarnMiddleware {
                category: "pii".to_string(),
            }),
            Box::new(WarnMiddleware {
                category: "length".to_string(),
            }),
            Box::new(PassMiddleware),
        ]);

        let mut ctx = make_ctx(MiddlewareStage::Release);
        let result = pipeline.run(&mut ctx).await;
        assert!(result.is_passed());
        assert_eq!(result.warnings().len(), 2);
        assert_eq!(result.warnings()[0].category.as_deref(), Some("pii"));
        assert_eq!(result.warnings()[1].category.as_deref(), Some("length"));
    }
}
