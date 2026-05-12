//! Pluggable detector for "leaked internal prompt" patterns in LLM output.
//!
//! The SDK defines the [`OutputLeakDetector`] trait; concrete implementations
//! live in downstream crates. The reference NSED implementation
//! (`PromptExposureMiddleware`) is in `nsed-agent` (BSL). OSS users who don't
//! ship a detector get no output guarding — wire one in via
//! [`ProposerEvaluatorAgent::with_output_guard`] when constructing the agent.

use crate::middleware::{MiddlewareContext, MiddlewareVerdict};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

/// Per-category scan stats from an [`OutputLeakDetector::scan`] call.
///
/// Field names mirror the original `PromptExposureMiddleware::ScanResult` so
/// agents that previously consumed it can migrate without renaming.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputScanResult {
    pub hits: Vec<String>,
    pub xml_tag_hits: u32,
    pub tool_name_hits: u32,
    pub instruction_hits: u32,
    pub wrong_acronym_hits: u32,
    pub suspicion_score: f64,
    pub response_length_chars: u32,
}

impl OutputScanResult {
    /// Total indicators across all categories.
    pub fn hit_count(&self) -> u32 {
        self.xml_tag_hits + self.tool_name_hits + self.instruction_hits + self.wrong_acronym_hits
    }
}

/// Detector contract. `scan` returns raw stats (cheap, callable per-snippet);
/// `evaluate` runs the full verdict pipeline (threshold gates, suspicion-score
/// weighting, reason construction).
#[async_trait]
pub trait OutputLeakDetector: Send + Sync + Debug {
    /// Tally per-category indicators in `text`. Used by callers that want
    /// stats for telemetry independently of the block/pass decision.
    fn scan(&self, text: &str) -> OutputScanResult;

    /// Run the detector's verdict gate over a [`MiddlewareContext`].
    /// Returns Pass/Warn/Block with a reason string suitable for surfacing
    /// to operators.
    async fn evaluate(&self, ctx: &MiddlewareContext) -> MiddlewareVerdict;
}
