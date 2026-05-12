//! Shared SSE event types used by both the orchestrator (publisher) and
//! agent workers (consumer).  These live in the SDK so that the dependency
//! direction is always `orchestrator → sdk`, never the reverse.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// SSE event: emitted after evaluation scoring completes each round.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct RoundSummaryEvent {
    /// Round number
    pub round: u32,
    /// Winner conviction *w* ∈ [-1, +1]: the winning proposer's signed eval
    /// mass normalized by total absolute mass across all proposals.
    /// +1 = unanimous endorsement, 0 = split, -1 = unanimous rejection.
    /// Used in evidence accumulation: `Δe = w × u'(t)`.
    pub convergence_score: f32,
    /// **Deprecated.** Previously `Σ|net_support[p]|`. Now set to `|w| × √P`
    /// for backward-compatible display. New consumers should use
    /// `convergence_score` (= *w*) directly.
    #[serde(default)]
    pub decisiveness: f32,
    /// Signed net support per proposal this round ∈ \[-1, +1\].
    /// Positive = ensemble believes correct; negative = believes wrong.
    /// Each entry is `(agent_id, net_support)` sorted descending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net_support: Vec<(String, f32)>,
    /// Running Cesaro mean of net support ∈ \[-1, +1\].
    /// Smoothed trend for dashboard display.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cesaro_support: Vec<(String, f32)>,
    /// Instantaneous distance between consecutive net_support vectors (diagnostics).
    /// Not used for termination — `convergence_score` is the Cesaro-smoothed version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_distance: Option<f32>,
    /// Board-wide claim convergence: fraction of claims (with 2+ evaluators)
    /// that have unanimous verdict. None if no structured claims were present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_convergence: Option<f32>,
    /// Total unique claims assessed by 2+ evaluators across all proposals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_claims: Option<u32>,
    /// Leader-only claim convergence: fraction of the winning proposal's
    /// claims (with 2+ evaluators) that have unanimous verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_claim_convergence: Option<f32>,
    /// Total unique claims assessed for the leader proposal only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_total_claims: Option<u32>,
    /// Per-proposal controversy scores (evaluator score variance).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controversy_scores: Vec<ProposalControversyEntry>,
    /// All proposals with their mean evaluator scores
    pub proposal_scores: Vec<ProposalScoreEntry>,
    /// Thermodynamic evidence accumulated so far (#218).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accumulated_evidence: Option<f32>,
    /// Halting threshold *T* = `effort × positive_budget`.
    /// Display progress = `accumulated_evidence / evidence_target`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_target: Option<f32>,
    /// Total extractable signal (normalises the evidence target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positive_budget: Option<f32>,
    /// Signed marginal utility at this round's time index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub du_dt: Option<f32>,
    /// **Deprecated.** Folded into `convergence_score` (= *w*).
    /// Kept for backward-compatible deserialization of old events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_consensus: Option<f32>,
    /// Optimal stopping round computed from thermodynamic parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_opt: Option<f32>,
    /// Normalized utility `U(t)/U(T_opt)` — thermodynamic probability of
    /// finding the right answer at this round, relative to the peak.
    /// 1.0 at T_opt, ~0.877 at round 1 (HP defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thermo_probability: Option<f32>,
    /// `true` when any proposal received fewer real evaluations than
    /// the number of dispatched evaluators minus one (i.e. at least one
    /// evaluator timed out or returned a partial batch). The threshold
    /// is computed from the pre-filter evaluator roster, not the
    /// post-filter proposer count. Dashboards should surface this as a
    /// data-quality warning alongside the round's metrics.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial_round_coverage: bool,
}

/// Per-proposal evaluator score variance — measures disagreement.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProposalControversyEntry {
    /// ID of the proposing agent
    pub agent_id: String,
    /// Variance of evaluator scores for this proposal (σ²)
    pub variance: f32,
    /// Number of evaluators who scored this proposal
    pub eval_count: u32,
}

/// Score entry within a RoundSummaryEvent.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct ProposalScoreEntry {
    /// ID of the proposing agent
    pub agent_id: String,
    /// Sum of signed QV contributions (`Σ score_q_s`) across all evaluators.
    /// Positive = net endorsement, negative = net opposition.
    pub aggregated_score: f32,
    /// Mean category scores across all evaluators (if structured evaluations present)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_breakdown: Option<CategoryScoreBreakdown>,
    /// Evaluator score variance (σ²) for this proposal
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controversy_score: Option<f32>,
    /// Number of real (non-synthetic) evaluations received for this proposal.
    #[serde(default)]
    pub real_eval_count: u32,
    /// Number of synthetic (injected max-score) evaluations for this proposal.
    /// Non-zero means at least one evaluator timed out or returned partial results.
    #[serde(default)]
    pub synthetic_eval_count: u32,
}

/// Aggregated category scores across all evaluators for a single proposal.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CategoryScoreBreakdown {
    pub correctness: f32,
    pub completeness: f32,
    pub novelty: f32,
    pub feasibility: f32,
    pub evidence_quality: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_summary_serde_roundtrip() {
        let event = RoundSummaryEvent {
            round: 3,
            convergence_score: 0.82,
            decisiveness: 0.82,
            net_support: vec![("alpha".into(), 0.6), ("beta".into(), -0.3)],
            cesaro_support: vec![("alpha".into(), 0.5), ("beta".into(), -0.1)],
            raw_distance: Some(0.15),
            claim_convergence: Some(0.75),
            total_claims: Some(12),
            leader_claim_convergence: None,
            leader_total_claims: None,
            controversy_scores: vec![ProposalControversyEntry {
                agent_id: "alpha".into(),
                variance: 2.5,
                eval_count: 3,
            }],
            proposal_scores: vec![
                ProposalScoreEntry {
                    agent_id: "alpha".into(),
                    aggregated_score: 7.5,
                    category_breakdown: Some(CategoryScoreBreakdown {
                        correctness: 80.0,
                        completeness: 70.0,
                        novelty: 60.0,
                        feasibility: 90.0,
                        evidence_quality: 75.0,
                    }),
                    controversy_score: Some(2.5),
                    ..Default::default()
                },
                ProposalScoreEntry {
                    agent_id: "beta".into(),
                    aggregated_score: 3.2,
                    category_breakdown: None,
                    controversy_score: None,
                    ..Default::default()
                },
            ],
            accumulated_evidence: None,
            evidence_target: None,
            positive_budget: None,
            du_dt: None,
            signed_consensus: None,
            t_opt: None,
            thermo_probability: None,
            ..Default::default()
        };

        let json = serde_json::to_string(&event).unwrap();
        let parsed: RoundSummaryEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.round, 3);
        assert!((parsed.convergence_score - 0.82).abs() < f32::EPSILON);
        assert_eq!(parsed.net_support.len(), 2);
        assert_eq!(parsed.net_support[0].0, "alpha");
        assert!((parsed.net_support[0].1 - 0.6).abs() < f32::EPSILON);
        assert_eq!(parsed.cesaro_support.len(), 2);
        assert_eq!(parsed.raw_distance, Some(0.15));
        assert_eq!(parsed.claim_convergence, Some(0.75));
        assert_eq!(parsed.total_claims, Some(12));
        assert_eq!(parsed.controversy_scores.len(), 1);
        assert_eq!(parsed.proposal_scores.len(), 2);
        assert_eq!(parsed.proposal_scores[0].agent_id, "alpha");
        assert!(parsed.proposal_scores[0].category_breakdown.is_some());
        assert!(parsed.proposal_scores[1].category_breakdown.is_none());
    }

    #[test]
    fn test_round_summary_backward_compat_minimal_json() {
        // Old orchestrator JSON without new fields — must deserialize with defaults
        let json = r#"{
            "round": 1,
            "convergence_score": 0.5,
            "proposal_scores": [
                {"agent_id": "a", "aggregated_score": 6.0}
            ]
        }"#;

        let event: RoundSummaryEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.round, 1);
        assert!((event.convergence_score - 0.5).abs() < f32::EPSILON);
        // decisiveness defaults to 0.0 when absent from old JSON
        assert!(event.decisiveness.abs() < f32::EPSILON);
        assert!(event.net_support.is_empty());
        assert!(event.cesaro_support.is_empty());
        assert_eq!(event.raw_distance, None);
        assert_eq!(event.claim_convergence, None);
        assert_eq!(event.total_claims, None);
        assert!(event.controversy_scores.is_empty());
        assert_eq!(event.proposal_scores.len(), 1);
    }

    #[test]
    fn test_skip_serializing_if_omits_none_and_empty() {
        let event = RoundSummaryEvent {
            round: 1,
            convergence_score: 0.0,
            decisiveness: 0.0,
            net_support: vec![],
            cesaro_support: vec![],
            raw_distance: None,
            claim_convergence: None,
            total_claims: None,
            leader_claim_convergence: None,
            leader_total_claims: None,
            controversy_scores: vec![],
            proposal_scores: vec![],
            accumulated_evidence: None,
            evidence_target: None,
            positive_budget: None,
            du_dt: None,
            signed_consensus: None,
            t_opt: None,
            thermo_probability: None,
            ..Default::default()
        };

        let json = serde_json::to_string(&event).unwrap();
        // convergence_score and decisiveness are non-optional f32 — always serialized
        assert!(
            json.contains("convergence_score"),
            "convergence_score is always present (f32, not Option)"
        );
        assert!(
            json.contains("decisiveness"),
            "decisiveness is always present (f32)"
        );
        assert!(
            !json.contains("claim_convergence"),
            "None should be omitted"
        );
        assert!(!json.contains("total_claims"), "None should be omitted");
        assert!(
            !json.contains("controversy_scores"),
            "empty vec should be omitted"
        );
        assert!(!json.contains("net_support"), "empty vec should be omitted");
        assert!(
            !json.contains("cesaro_support"),
            "empty vec should be omitted"
        );
        assert!(!json.contains("raw_distance"), "None should be omitted");
        assert!(
            !json.contains("leader_claim_convergence"),
            "None should be omitted"
        );
        assert!(
            !json.contains("leader_total_claims"),
            "None should be omitted"
        );
        assert!(
            !json.contains("accumulated_evidence"),
            "None should be omitted"
        );
        assert!(!json.contains("evidence_target"), "None should be omitted");
        assert!(!json.contains("positive_budget"), "None should be omitted");
        assert!(!json.contains("du_dt"), "None should be omitted");
        assert!(!json.contains("signed_consensus"), "None should be omitted");
        assert!(!json.contains("t_opt"), "None should be omitted");
        assert!(
            !json.contains("thermo_probability"),
            "None should be omitted"
        );
    }

    #[test]
    fn test_proposal_score_entry_backward_compat() {
        // Minimal JSON — old format without category_breakdown or controversy_score
        let json = r#"{"agent_id": "x", "aggregated_score": 4.0}"#;
        let entry: ProposalScoreEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.agent_id, "x");
        assert!((entry.aggregated_score - 4.0).abs() < f32::EPSILON);
        assert!(entry.category_breakdown.is_none());
        assert!(entry.controversy_score.is_none());
    }

    #[test]
    fn test_proposal_score_entry_skip_serializing_none() {
        let entry = ProposalScoreEntry {
            agent_id: "y".into(),
            aggregated_score: 5.0,
            category_breakdown: None,
            controversy_score: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("category_breakdown"));
        assert!(!json.contains("controversy_score"));
    }

    #[test]
    fn test_category_score_breakdown_roundtrip() {
        let bd = CategoryScoreBreakdown {
            correctness: 0.0,
            completeness: 100.0,
            novelty: 50.5,
            feasibility: 99.9,
            evidence_quality: 33.3,
        };
        let json = serde_json::to_vec(&bd).unwrap();
        let parsed: CategoryScoreBreakdown = serde_json::from_slice(&json).unwrap();
        assert!((parsed.correctness - 0.0).abs() < f32::EPSILON);
        assert!((parsed.completeness - 100.0).abs() < f32::EPSILON);
        assert!((parsed.novelty - 50.5).abs() < f32::EPSILON);
        assert!((parsed.feasibility - 99.9).abs() < 0.01);
        assert!((parsed.evidence_quality - 33.3).abs() < 0.01);
    }

    #[test]
    fn test_controversy_entry_roundtrip() {
        let entry = ProposalControversyEntry {
            agent_id: "delta".into(),
            variance: 1.234,
            eval_count: 5,
        };
        let json = serde_json::to_vec(&entry).unwrap();
        let parsed: ProposalControversyEntry = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.agent_id, "delta");
        assert!((parsed.variance - 1.234).abs() < 0.001);
        assert_eq!(parsed.eval_count, 5);
    }

    #[test]
    fn test_round_summary_empty_proposal_scores() {
        // Edge case: round completes with no proposals (all agents timed out)
        let event = RoundSummaryEvent {
            round: 2,
            convergence_score: 0.0,
            decisiveness: 0.0,
            net_support: vec![],
            cesaro_support: vec![],
            raw_distance: None,
            claim_convergence: None,
            total_claims: None,
            leader_claim_convergence: None,
            leader_total_claims: None,
            controversy_scores: vec![],
            proposal_scores: vec![],
            accumulated_evidence: None,
            evidence_target: None,
            positive_budget: None,
            du_dt: None,
            signed_consensus: None,
            t_opt: None,
            thermo_probability: None,
            ..Default::default()
        };
        let json = serde_json::to_vec(&event).unwrap();
        let parsed: RoundSummaryEvent = serde_json::from_slice(&json).unwrap();
        assert!(parsed.proposal_scores.is_empty());
    }
}
