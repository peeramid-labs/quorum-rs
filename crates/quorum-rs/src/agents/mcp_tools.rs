//! MCP tool input/output types for the NSED MCP server.
//!
//! These structs define the parameters and return values for each MCP tool
//! exposed by `NsedMcpServer`. They use `schemars::JsonSchema` for automatic
//! MCP tool schema generation via the `rmcp` `#[tool]` macro.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `nsed_propose` — submit a proposal (terminal tool).
///
/// Flexible so a middleware-declared schema (see `NsedMcpServer::list_tools`) can
/// override the advertised shape: the default `{thought_process, content}`, or an
/// envelope like `{rationale, ops}` (captured via `extra` and forwarded verbatim).
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ProposeInput {
    /// The agent's reasoning and analysis process.
    #[serde(default)]
    pub thought_process: String,
    /// The actual proposal content (a string body, or a structured object).
    #[serde(default)]
    pub content: serde_json::Value,
    /// Any other top-level fields (e.g. a `{rationale, ops}` envelope).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Input for `nsed_evaluate` — submit evaluations (terminal tool).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvaluateInput {
    /// One evaluation per candidate proposal.
    pub evaluations: Vec<EvaluationItem>,
}

/// A single evaluation of a candidate proposal.
///
/// Includes structured analysis fields following the NSED Vector Alignment protocol:
/// stance, claim assessments, disagreement points, and per-category quality scores.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EvaluationItem {
    /// The ID of the candidate being evaluated (must match exactly).
    pub target_id: String,
    /// Signed endorsement: -1.0 (strong opposition) to +1.0 (strong endorsement). 0.0 = neutral.
    pub score: f32,
    /// Brief justification for the score.
    pub justification: String,

    /// Evaluator's overall stance toward this proposal.
    #[serde(default)]
    pub stance: Option<String>,
    /// Whether the evaluator considers this a viable final solution.
    #[serde(default)]
    pub is_final_solution: bool,
    /// The 2-3 most pivotal claim assessments with verdicts.
    #[serde(default)]
    pub claim_assessments: Vec<McpClaimAssessment>,
    /// Points of disagreement with the proposal.
    #[serde(default)]
    pub disagreements: Vec<McpDisagreementPoint>,
    /// Per-category quality scores (correctness, completeness, novelty, feasibility, evidence_quality).
    #[serde(default)]
    pub category_scores: Option<McpCategoryScores>,
}

/// Assessment of a specific claim within a proposal.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpClaimAssessment {
    /// Stable claim ID for cross-round tracking (6-char hex).
    #[serde(default)]
    pub claim_id: Option<String>,
    /// The claim being assessed.
    #[serde(default, alias = "content", alias = "text", alias = "claim_text")]
    pub claim: String,
    /// Verdict: "verified", "contested", "unverified", or "wrong".
    pub verdict: String,
    /// Brief reasoning for the verdict.
    #[serde(default, alias = "explanation", alias = "reasoning")]
    pub reason: Option<String>,
}

/// A specific point of disagreement between the evaluator and a proposal.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpDisagreementPoint {
    /// References the claim_id of the disputed claim.
    #[serde(default)]
    pub claim_id: Option<String>,
    /// What the proposal claims.
    #[serde(default, alias = "claim", alias = "what_they_claimed")]
    pub proposal_claims: String,
    /// The evaluator's counter-position.
    #[serde(default, alias = "counter_position", alias = "position")]
    pub evaluator_position: String,
    /// Confidence: "high", "medium", or "low".
    #[serde(default = "default_confidence")]
    pub confidence: String,
}

fn default_confidence() -> String {
    "medium".to_string()
}

/// Per-category quality scores (0-100 scale).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpCategoryScores {
    /// Correctness of reasoning and conclusions.
    #[serde(default)]
    pub correctness: f32,
    /// Completeness of the solution.
    #[serde(default)]
    pub completeness: f32,
    /// Novelty of the approach.
    #[serde(default)]
    pub novelty: f32,
    /// Feasibility and practicality.
    #[serde(default)]
    pub feasibility: f32,
    /// Quality of supporting evidence.
    #[serde(default)]
    pub evidence_quality: f32,
}

/// Input for `nsed_read_proposal` — read a past proposal.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadProposalInput {
    /// Round number (defaults to previous round).
    pub round: Option<u32>,
    /// Agent ID of the proposal author.
    pub agent_id: String,
    /// Character offset for pagination (default 0).
    pub offset: Option<usize>,
    /// Max characters to return (default 5000).
    pub limit: Option<usize>,
}

/// Input for `nsed_read_critiques` — read evaluator feedback.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadCritiquesInput {
    /// Round number (defaults to previous round).
    pub round: Option<u32>,
    /// Filter by evaluator agent ID (optional).
    pub agent_id: Option<String>,
}

/// Input for `nsed_search` — search deliberation history.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchInput {
    /// Free-text search query.
    pub query: String,
    /// Filter to a specific round (optional).
    pub round: Option<u32>,
    /// Filter to a specific agent (optional).
    pub agent_id: Option<String>,
}

/// Input for `nsed_update_scratchpad` — write to persistent agent memory.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateScratchpadInput {
    /// Content to store in the scratchpad (replaces previous value).
    pub content: String,
}

/// Result type captured by terminal tools.
#[derive(Debug, Serialize, Deserialize)]
pub enum McpResult {
    Proposal {
        thought_process: String,
        content: String,
    },
    Evaluations(Vec<McpEvaluationResult>),
}

/// A single evaluation result with full structured data.
#[derive(Debug, Serialize, Deserialize)]
pub struct McpEvaluationResult {
    pub target_id: String,
    pub score: f32,
    pub justification: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stance: Option<String>,
    #[serde(default)]
    pub is_final_solution: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_assessments: Vec<McpClaimAssessmentResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disagreements: Vec<McpDisagreementResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_scores: Option<McpCategoryScores>,
}

/// Serializable claim assessment result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClaimAssessmentResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    pub claim: String,
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Serializable disagreement result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDisagreementResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    pub proposal_claims: String,
    pub evaluator_position: String,
    pub confidence: String,
}

// Also derive Serialize for McpCategoryScores (for result serialization)
impl Serialize for McpCategoryScores {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("McpCategoryScores", 5)?;
        s.serialize_field("correctness", &self.correctness)?;
        s.serialize_field("completeness", &self.completeness)?;
        s.serialize_field("novelty", &self.novelty)?;
        s.serialize_field("feasibility", &self.feasibility)?;
        s.serialize_field("evidence_quality", &self.evidence_quality)?;
        s.end()
    }
}

// ExecEvaluationResponse/ExecEvaluationItem live in exec_agent.rs with full
// Evaluation fields via #[serde(flatten)]. Do not duplicate here.
