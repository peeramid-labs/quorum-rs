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
    /// Per-category quality scores (correctness, completeness, novelty,
    /// feasibility, evidence_quality, conciseness).
    #[serde(default)]
    pub category_scores: Option<McpCategoryScores>,
}

/// Assessment of a specific claim within a proposal.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct McpClaimAssessment {
    /// Stable claim ID for cross-round tracking (6-char hex).
    #[serde(default)]
    pub claim_id: Option<String>,
    /// The claim, quoted VERBATIM from the proposal — an exact, character-for-
    /// character substring. Copy-paste the span; do NOT paraphrase. Common quote
    /// wrappers (`"…"`, `> …`, `` `…` ``, `Label: "…"`) are tolerated and stripped,
    /// and the quote is resolved + replaced with the exact proposal substring so
    /// the client can locate it. A quote that matches NO span of the proposal is
    /// rejected — you must re-quote before the evaluation is accepted.
    // Agent-facing name is `cite`; the internal field stays `claim` (rename only
    // affects the tool schema/wire name). `claim`/`quote`/… still accepted.
    #[serde(
        rename = "cite",
        default,
        alias = "claim",
        alias = "quote",
        alias = "content",
        alias = "text",
        alias = "claim_text"
    )]
    pub claim: String,
    /// Verdict: "verified", "contested", "unverified", or "wrong".
    pub verdict: String,
    /// Brief reasoning for the verdict.
    #[serde(default, alias = "explanation", alias = "reasoning")]
    pub reason: Option<String>,
    /// Filled in by citation grounding, never by the model — hidden from both
    /// the advertised tool schema and the wire format so it cannot be supplied.
    #[serde(skip)]
    #[schemars(skip)]
    pub anchor: Option<crate::agents::ClaimAnchor>,
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

/// Per-category signed quality scores, -100 to +100. Negative means the
/// dimension actively undermines the proposal; positive means it supports it.
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
    /// Clarity per token, judged against the other candidates this round.
    #[serde(default)]
    pub conciseness: f32,
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
    /// First line of the final solution to return, 1-indexed. Set either bound
    /// to get the solution back as numbered lines — use the anchors from the
    /// candidate block's outline.
    pub from_line: Option<usize>,
    /// Last line to return, 1-indexed and inclusive.
    pub to_line: Option<usize>,
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

/// Input for `nsed_file_history` — a file's revision history.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileHistoryInput {
    /// Path of the file to inspect, relative to the working repository root.
    pub path: String,
    /// Max number of revisions to return (default 20, capped at 200).
    pub limit: Option<usize>,
}

/// Input for `nsed_line_history` — per-line provenance for a range of lines.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LineHistoryInput {
    /// Path of the file to inspect, relative to the working repository root.
    pub path: String,
    /// First line of the range to inspect (1-based).
    pub start_line: u32,
    /// Last line of the range (1-based; defaults to `start_line`).
    pub end_line: Option<u32>,
    /// When set, return how this range EVOLVED across its last N revisions (each
    /// change with its diff), instead of the default single-snapshot provenance
    /// (who last touched each line). Capped at 50.
    pub revisions: Option<usize>,
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
    /// Grounded span, carried through to the internal
    /// [`ClaimAssessment`](crate::agents::ClaimAssessment) so the offsets
    /// computed at match time survive the hop out of the tool handler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<crate::agents::ClaimAnchor>,
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
        let mut s = serializer.serialize_struct("McpCategoryScores", 6)?;
        s.serialize_field("correctness", &self.correctness)?;
        s.serialize_field("completeness", &self.completeness)?;
        s.serialize_field("novelty", &self.novelty)?;
        s.serialize_field("feasibility", &self.feasibility)?;
        s.serialize_field("evidence_quality", &self.evidence_quality)?;
        s.serialize_field("conciseness", &self.conciseness)?;
        s.end()
    }
}

// ExecEvaluationResponse/ExecEvaluationItem live in exec_agent.rs with full
// Evaluation fields via #[serde(flatten)]. Do not duplicate here.

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written serializer declares its own field count; nothing type-checks it.
    #[test]
    fn every_category_the_struct_carries_is_written_out() {
        let scores = McpCategoryScores {
            correctness: 10.0,
            completeness: 20.0,
            novelty: 30.0,
            feasibility: 40.0,
            evidence_quality: 50.0,
            conciseness: -60.0,
        };
        let wire = serde_json::to_value(&scores).expect("category scores serialise");
        let written = wire.as_object().expect("an object");
        for axis in [
            "correctness",
            "completeness",
            "novelty",
            "feasibility",
            "evidence_quality",
            "conciseness",
        ] {
            assert!(written.contains_key(axis), "{axis} was not written out");
        }
        assert_eq!(
            written.len(),
            6,
            "declared count and written fields drifted"
        );
        assert_eq!(wire["conciseness"], -60.0);
    }

    #[test]
    fn claim_assessment_accepts_cite_quote_and_legacy_names() {
        // Agent-facing field is `cite`; legacy `claim`/`quote`/`content` still
        // deserialize into the same internal `claim` field.
        for key in ["cite", "quote", "claim", "content", "text"] {
            let json = format!(r#"{{"{key}":"sorts in O(n log n)","verdict":"verified"}}"#);
            let a: McpClaimAssessment =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("{key}: {e}"));
            assert_eq!(a.claim, "sorts in O(n log n)", "via {key}");
        }
    }

    #[test]
    fn tool_schema_exposes_cite_not_claim() {
        // The generated JSON schema (what the agent sees) must advertise `cite`.
        let schema = schemars::schema_for!(McpClaimAssessment);
        let s = serde_json::to_string(&schema).unwrap();
        assert!(s.contains("\"cite\""), "schema must expose `cite`: {s}");
        assert!(
            !s.contains("\"claim\":{"),
            "schema property should be `cite`, not `claim`"
        );
    }
}
