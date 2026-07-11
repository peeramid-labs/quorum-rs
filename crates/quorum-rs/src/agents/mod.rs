pub mod claude_recovery;
pub mod config;
pub mod exec_agent;
pub mod mcp_agent;
pub mod mcp_tools;
pub mod nsed_agent;
pub mod output_guard;
pub mod session_store;
pub mod user_tools;

pub use nsed_agent::{AgentResponse, ProposerEvaluatorAgent};
pub use output_guard::{OutputLeakDetector, OutputScanResult};
pub use user_tools::{NatsUserToolHandlerFactory, UserToolHandler};

use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use utoipa::ToSchema;

pub use config::{AgentConfig, TaskPrecision};
// Re-export defaults from config to maintain API compatibility
pub use config::{default_context_window, default_scratchpad_limit};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct AgentContext {
    pub task_description: String,
    pub round_number: u32,
    pub total_rounds: u32,
    pub phase: DeliberationPhase,
    pub target_proposal: Option<Proposal>,
    pub competitor_summaries: Vec<String>,
    pub previous_round_matrix: Option<String>,
    pub previous_own_proposal: Option<Proposal>,
    pub previous_own_score: Option<f32>,
    pub previous_critiques: Vec<String>,
    pub scratchpad: Option<String>,
    #[serde(skip)]
    #[schema(ignore)]
    #[schemars(skip)]
    pub store: Option<Arc<dyn PersistenceStore>>,
    #[serde(default)]
    pub candidates: Vec<CandidateProposal>,
    #[serde(default)]
    pub user_injections: Vec<UserInjection>,
    /// User-defined tool definitions for this job. Empty if none registered.
    #[serde(default)]
    pub user_tools: Vec<UserToolDefinition>,
    /// Remaining phase budget in seconds at the time the task was published.
    /// The agent uses this as the upper bound for user tool call wait times.
    #[serde(default)]
    pub phase_budget_remaining_secs: f64,
    /// The session/job ID for NATS subject construction within the agent worker.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Stable conversation key for the claude-CLI session, when a job belongs to
    /// a longer-lived thread whose `session_id`/`room_id` is minted fresh per turn
    /// (the OpenAI-compat path). The deterministic claude session UUID is keyed on
    /// this so successive turns of one thread `--resume` the same transcript.
    /// `None` falls back to `session_id` (the thread-TUI path, where the room *is*
    /// the stable thread id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// The conversation as an ordered, role-tagged message array (the native
    /// representation). Each provider renders it to what it can send via
    /// [`crate::conversation::render`]: a resumed claude session gets only the
    /// newest turn, everything else gets the whole thing flattened. Empty on the
    /// legacy path, where `task_description` is used instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<crate::conversation::Message>,
    /// Structured feedback from previous round's evaluations (Phase 2 context pipeline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_feedback: Option<StructuredFeedback>,
    /// A JSON schema a `before_prompt` middleware declared for the proposal
    /// submission. When set, the propose tool's `parameters` are constrained to
    /// it and the terminal tool call is forced (`tool_choice: required`) — the
    /// model must return a schema-valid structured proposal. Runtime-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(ignore)]
    #[schemars(skip)]
    pub forced_proposal_schema: Option<serde_json::Value>,
    /// Runtime-only: handler for user tool calls (injected by agent worker, not serialized).
    /// The concrete type is supplied via [`UserToolHandlerFactory`](crate::workers::UserToolHandlerFactory);
    /// this field holds an opaque Arc wrapper.
    #[serde(skip)]
    #[schema(ignore)]
    #[schemars(skip)]
    pub user_tool_handler: Option<Arc<dyn UserToolHandlerTrait>>,
    /// Role assigned to this agent by the broker (from policy-based scheduling).
    /// `None` for static agent list mode or legacy payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Per-role private context content (not visible to other agents).
    /// Populated from the role's `context` files by the orchestrator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_context: Option<String>,
    /// Identity of the agent processing this task. Populated by the
    /// dispatcher (orchestrator at construction time, worker after
    /// deserialize) so the agent and any downstream helpers
    /// (e.g. [`AgentContext::telemetry_for`]) don't need to thread
    /// the same id through every call site. `#[serde(default)]` on
    /// the field tolerates payloads serialized before the field
    /// existed; populated paths keep the value end-to-end.
    #[serde(default)]
    pub agent_id: String,
    /// Unix ms the orchestrator stamped at publish time; `None` on
    /// pre-stamping payloads, synthetic contexts, and resurrected
    /// envelopes. Paired with `task_received` to compute
    /// `TaskAccepted.job_age_at_accept_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_publish_ts: Option<i64>,
    /// Runtime-only: per-task telemetry mux populated by the agent
    /// worker. Agents pass `context.telemetry.as_ref()` into
    /// `generate_structured_output` / `react_loop` so LLM, tool,
    /// retry, and prompt-exposure events fan out across every
    /// configured endpoint under the same `(agent_id, job_id,
    /// round, phase)` envelope as the worker's task-lifecycle
    /// events. Mirrors the `store` / `user_tool_handler`
    /// runtime-only pattern: skipped by serde, excluded from
    /// generated schemas.
    #[serde(skip)]
    #[schema(ignore)]
    #[schemars(skip)]
    pub telemetry: Option<crate::telemetry::TelemetryEmitterMux>,
}

impl AgentContext {
    /// The key the deterministic claude-CLI session UUID is derived from:
    /// `conversation_id` (a thread stable across turns) when set, else
    /// `session_id` (the per-job/room id). This is what makes successive turns
    /// of one thread resume the same transcript.
    pub fn claude_session_key(&self) -> Option<&str> {
        self.conversation_id
            .as_deref()
            .or(self.session_id.as_deref())
    }

    /// The task string a provider should send this call, given whether its
    /// session is resumed. Prefers the native `messages` array (rendered per
    /// resume state via [`crate::conversation::render`]); falls back to the
    /// legacy `task_description` string while payloads migrate.
    pub fn task(&self, resumed: bool) -> String {
        if !self.messages.is_empty() {
            crate::conversation::render(&self.messages, resumed)
        } else {
            self.task_description.clone()
        }
    }

    /// Build a [`TelemetryContext`](crate::telemetry::TelemetryContext)
    /// for telemetry events emitted while processing this task.
    ///
    /// Uses the context's own `agent_id`, `session_id`,
    /// `round_number`, and `phase`. Pair with [`emit_for!`] at the
    /// call site:
    ///
    /// ```ignore
    /// emit_for!(context, ToolCallExecuted {
    ///     tool_name: name, latency_ms: 42, success: true,
    /// });
    /// ```
    ///
    /// # Panics
    ///
    /// `session_id` is a dispatch-time invariant: the orchestrator
    /// populates it on every published task. Emitting telemetry
    /// from a context with no session is a programmer error,
    /// typically a test that constructed an `AgentContext` literal
    /// without setting `session_id`. Panics with a helpful message
    /// rather than synthesising a fake job_id that would silently
    /// break trace correlation across the catalog.
    pub fn telemetry_for(&self) -> crate::telemetry::TelemetryContext {
        let session_id = self.session_id.as_deref().filter(|s| !s.is_empty()).expect(
            "AgentContext::telemetry_for requires session_id; \
                 orchestrator must populate it at dispatch and tests \
                 must set it before emitting telemetry events",
        );
        crate::telemetry::TelemetryContext::new(
            &self.agent_id,
            Some(session_id),
            Some(self.round_number),
            Some(self.phase),
        )
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema, ToSchema, Default,
)]
pub enum DeliberationPhase {
    #[default]
    Proposing,
    Evaluating,
    ConsensusCheck,
}

impl DeliberationPhase {
    /// Canonical short tag used in NATS subjects, trace IDs, and prompts.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeliberationPhase::Proposing => "propose",
            DeliberationPhase::Evaluating => "evaluate",
            DeliberationPhase::ConsensusCheck => "consensus_check",
        }
    }
}

// ---------------------------------------------------------------------------
// Operator annotations (HITL traceability)
// ---------------------------------------------------------------------------

/// The type of operator intervention on a buffered response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationType {
    /// Operator added a comment without modifying the content.
    #[default]
    Comment,
    /// Operator edited the response content (becomes "owner").
    Edit,
}

/// A record of operator intervention on an agent response.
///
/// Attached to [`Proposal`] and [`Evaluation`] payloads when an operator
/// inspects, edits, or annotates a buffered response before release.
/// These annotations provide an audit trail for traceability and transparency.
///
/// In the future, `Edit` annotations will trigger a digital signature swap:
/// the agent's signature is replaced by the operator's higher-order public key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, ToSchema, Default)]
pub struct OperatorAnnotation {
    /// Type of intervention.
    pub annotation_type: AnnotationType,
    /// Operator's commentary / rationale.
    pub comment: String,
    /// ISO-8601 timestamp of when the annotation was made.
    pub timestamp: String,
    /// SHA-256 hash of the original payload before editing (audit trail).
    /// **Required** for `Edit` annotations — enforced by [`Self::validate()`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_content_hash: Option<String>,
}

impl OperatorAnnotation {
    /// Validate the annotation's invariants.
    ///
    /// - `Edit` annotations **must** carry a non-empty `original_content_hash`.
    /// - `Comment` annotations have no additional requirements.
    ///
    /// This is enforced at API boundaries (HITL buffer edit handler), not on
    /// deserialization, to preserve backward compat with `#[serde(default)]`.
    pub fn validate(&self) -> Result<(), String> {
        if self.annotation_type == AnnotationType::Edit {
            match &self.original_content_hash {
                Some(hash) if !hash.is_empty() => Ok(()),
                _ => Err(
                    "Edit annotations must include a non-empty original_content_hash".to_string(),
                ),
            }
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct Proposal {
    pub thought_process: String,
    pub content: String,
    /// Restore scratchpad field to ensure benchmarks can capture it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_scratchpad: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage_stats: Option<TokenUsage>,
    /// Operator annotations from HITL review (traceability audit trail).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_annotations: Vec<OperatorAnnotation>,
    /// Set to `"operator"` when the content was edited by a human operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_by: Option<String>,
    /// Terminal signal from the agent loop. Values:
    ///   - `"stop"` / `"tool_calls"` — normal LLM-driven termination.
    ///   - `"max_iterations"` — graceful ceiling hit (react loop capped
    ///     out without a terminal tool call); content is whatever the
    ///     last iteration produced. Lets the orchestrator + dashboard
    ///     distinguish partial fallbacks from full completions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Wall-clock instant the agent published this proposal, in
    /// milliseconds since the Unix epoch. Populated by the agent worker
    /// at publish time so the orchestrator can compute propagation
    /// latency for the `submission_received` telemetry event. Old
    /// payloads without the field deserialize as `0`.
    #[serde(default)]
    pub published_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// =============================================================================
// Structured Evaluation Types
// =============================================================================

/// Verdict for a specific claim within a proposal.
/// JSON Schema `enum` enables logit-biasing in strict mode inference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClaimVerdict {
    Verified,
    Contested,
    Unverified,
    Wrong,
    #[default]
    Unknown,
}

/// Evaluator's overall position toward a proposal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Stance {
    StrongAgree,
    Agree,
    #[default]
    Neutral,
    Disagree,
    StrongDisagree,
}

/// Confidence level for a disagreement point.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    #[default]
    Medium,
    Low,
}

/// Assessment of a specific claim within a proposal.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct ClaimAssessment {
    /// Stable identifier for cross-round claim tracking. Generated on first
    /// occurrence; evaluators echo it back in subsequent rounds.
    /// Format: 6-char hex hash derived from (target_id, claim_text, round).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    /// The claim being assessed (e.g., "O(n log n) complexity proof").
    /// Models frequently hallucinate "content", "text", "description", or "summary"
    /// instead of "claim". Some models omit the claim text entirely when using
    /// claim_id references, so we default to empty string.
    #[serde(
        default,
        alias = "content",
        alias = "text",
        alias = "claim_text",
        alias = "description",
        alias = "summary"
    )]
    pub claim: String,
    /// Evaluator's verdict on this claim.
    pub verdict: ClaimVerdict,
    /// Brief reasoning for the verdict.
    /// Models sometimes use "disagreement", "explanation", or "reasoning" instead
    /// of "reason".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "disagreement",
        alias = "explanation",
        alias = "reasoning"
    )]
    pub reason: Option<String>,
}

/// A specific point of disagreement between the evaluator and a proposal.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct DisagreementPoint {
    /// References the claim_id of the disputed claim when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    /// What the proposal claims.
    /// Models frequently hallucinate alternative field names for this field:
    ///   "contested_claim", "claim", "claim_text", "proposal", "what_they_claimed"
    /// Some models omit the claim text entirely when referencing by claim_id.
    #[serde(
        default,
        alias = "contested_claim",
        alias = "claim",
        alias = "claim_text",
        alias = "proposal",
        alias = "what_they_claimed"
    )]
    pub proposal_claims: String,
    /// The evaluator's counter-position.
    /// Models frequently hallucinate alternative field names for this field:
    ///   "belief", "details", "counter_position", "position", "explanation",
    ///   "analysis", "counter", "our_position", "your_view", "what_i_believe"
    /// Some models omit this entirely when referencing by claim_id.
    #[serde(
        default,
        alias = "belief",
        alias = "details",
        alias = "counter_position",
        alias = "position",
        alias = "explanation",
        alias = "analysis",
        alias = "counter",
        alias = "our_position",
        alias = "your_view",
        alias = "what_i_believe"
    )]
    pub evaluator_position: String,
    /// How confident the evaluator is in their counter-position.
    pub confidence: Confidence,
}

/// Per-category signed quality scores (-100 to +100 scale, same as endorsement_weight).
///
/// Negative = this dimension actively undermines the proposal;
/// positive = this dimension supports it. Used for diagnostic breakdown.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct CategoryScores {
    pub correctness: f32,
    pub completeness: f32,
    pub novelty: f32,
    pub feasibility: f32,
    pub evidence_quality: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct Evaluation {
    /// Signed fraction ∈ [-1, +1]: `endorsement_weight / Σ|weights|`.
    ///
    /// Positive = endorsement, negative = opposition, zero = neutral.
    /// The orchestrator applies a single signed QV pipeline for both ranking
    /// and convergence: `score_q_s = sign(score) × √(|score| × 100) / 10`.
    /// See `docs/scoring-variables.md`.
    pub score: f32,
    #[serde(default)]
    pub justification: String,
    /// Token usage for the **evaluator's** LLM call that produced this evaluation.
    /// This counts the evaluator agent's tokens, not the evaluated proposal's.
    ///
    /// **Batch semantics**: One evaluator makes a single LLM call to score ALL
    /// candidates at once. Every `Evaluation` from that batch receives the
    /// **same** `token_usage` (the total for the evaluator's batch call, not a
    /// per-candidate share). The orchestrator deduplicates by `evaluator_agent_id`
    /// when summing to avoid double-counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    /// Structured claim-level assessments (2-3 most pivotal claims per candidate).
    #[serde(default)]
    pub claim_assessments: Vec<ClaimAssessment>,
    /// Specific points of disagreement with the proposal.
    #[serde(default)]
    pub disagreements: Vec<DisagreementPoint>,
    /// Evaluator's overall stance toward this proposal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stance: Option<Stance>,
    /// Whether the evaluator considers this a viable final solution.
    #[serde(default)]
    pub is_final_solution: bool,
    /// Per-category quality breakdown. When present, QV transform is applied per-category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_scores: Option<CategoryScores>,
    /// Operator annotations from HITL review (traceability audit trail).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_annotations: Vec<OperatorAnnotation>,
    /// Set to `"operator"` when the content was edited by a human operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_by: Option<String>,
    /// Terminal signal from the agent loop — see `Proposal::finish_reason`
    /// for the value taxonomy. `"max_iterations"` here means the
    /// evaluator's react loop hit the ceiling before emitting a
    /// terminal tool call; score + text are best-effort partials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Wall-clock instant the evaluator published this evaluation, in
    /// milliseconds since the Unix epoch. Mirror of
    /// [`Proposal::published_at_ms`] for evaluator submissions; same
    /// purpose, same default-on-missing semantics.
    #[serde(default)]
    pub published_at_ms: i64,
}

/// Generate a stable 6-char hex claim ID for cross-round tracking.
///
/// The ID is derived from (target_id, normalized_claim_text, round), so the same
/// claim flagged in the same round for the same target always gets the same ID.
pub fn generate_claim_id(target_id: &str, claim_text: &str, round: u32) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (target_id, claim_text.to_lowercase().trim(), round).hash(&mut hasher);
    format!("{:06x}", hasher.finish() & 0xFFFFFF)
}

// =============================================================================
// Structured Feedback (Context Pipeline)
// =============================================================================

/// Aggregated structured feedback for a proposal, built from evaluator data.
/// Placed in the SDK for portability (future crypto-isolated agents build their own).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct StructuredFeedback {
    /// Claims that evaluators contested or marked wrong.
    pub contested_claims: Vec<ContestedClaim>,
    /// Claims that all evaluators verified.
    pub verified_claims: Vec<String>,
    /// Mean stance across evaluators (mapped: StrongAgree=2, Agree=1, Neutral=0, Disagree=-1, StrongDisagree=-2).
    pub mean_stance: f32,
    /// Number of evaluators who provided structured feedback.
    pub evaluator_count: u32,
    /// Averaged category breakdown across evaluators (if any provided category_scores).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_breakdown: Option<CategoryScores>,
}

/// A specific contested claim with evaluator context.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct ContestedClaim {
    /// Stable claim ID for cross-round tracking.
    pub claim_id: String,
    /// What the proposal originally claimed.
    pub what_you_claimed: String,
    /// The evaluator's counter-position.
    pub counter_position: String,
    /// Which evaluator raised this dispute.
    pub evaluator: String,
    /// Evaluator's confidence in their counter-position.
    pub confidence: Confidence,
}

/// Build structured feedback from evaluation records for a given proposal.
///
/// Aggregates claim assessments and disagreements across all evaluators.
/// Designed as a pure function in the SDK so it can run agent-side in
/// future crypto-isolated architectures.
pub fn build_structured_feedback(evaluations: &[EvaluationRecord]) -> StructuredFeedback {
    let mut contested_claims = Vec::new();
    let mut verified_claims = Vec::new();
    let mut stance_sum = 0.0f32;
    let mut stance_count = 0u32;
    let mut cat_totals = CategoryScores::default();
    let mut cat_count = 0u32;

    for er in evaluations {
        let eval = &er.evaluation;

        // Aggregate stance
        if let Some(ref s) = eval.stance {
            stance_sum += match s {
                Stance::StrongAgree => 2.0,
                Stance::Agree => 1.0,
                Stance::Neutral => 0.0,
                Stance::Disagree => -1.0,
                Stance::StrongDisagree => -2.0,
            };
            stance_count += 1;
        }

        // Aggregate category scores
        if let Some(ref cs) = eval.category_scores {
            cat_totals.correctness += cs.correctness;
            cat_totals.completeness += cs.completeness;
            cat_totals.novelty += cs.novelty;
            cat_totals.feasibility += cs.feasibility;
            cat_totals.evidence_quality += cs.evidence_quality;
            cat_count += 1;
        }

        // Process claim assessments
        for ca in &eval.claim_assessments {
            match ca.verdict {
                ClaimVerdict::Verified => {
                    verified_claims.push(ca.claim.clone());
                }
                ClaimVerdict::Contested | ClaimVerdict::Wrong => {
                    let claim_id = ca.claim_id.clone().unwrap_or_else(|| {
                        format!("auto_{:04x}", {
                            let mut h = std::collections::hash_map::DefaultHasher::new();
                            ca.claim.hash(&mut h);
                            h.finish() & 0xFFFF
                        })
                    });
                    contested_claims.push(ContestedClaim {
                        claim_id,
                        what_you_claimed: ca.claim.clone(),
                        counter_position: ca.reason.clone().unwrap_or_default(),
                        evaluator: er.evaluator_agent_id.clone(),
                        confidence: Confidence::Medium,
                    });
                }
                _ => {}
            }
        }

        // Process disagreement points
        for dp in &eval.disagreements {
            let claim_id = dp.claim_id.clone().unwrap_or_else(|| {
                format!("disp_{:04x}", {
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    dp.proposal_claims.hash(&mut h);
                    h.finish() & 0xFFFF
                })
            });
            contested_claims.push(ContestedClaim {
                claim_id,
                what_you_claimed: dp.proposal_claims.clone(),
                counter_position: dp.evaluator_position.clone(),
                evaluator: er.evaluator_agent_id.clone(),
                confidence: dp.confidence.clone(),
            });
        }
    }

    // Deduplicate verified claims
    verified_claims.sort();
    verified_claims.dedup();

    let category_breakdown = if cat_count > 0 {
        Some(CategoryScores {
            correctness: cat_totals.correctness / cat_count as f32,
            completeness: cat_totals.completeness / cat_count as f32,
            novelty: cat_totals.novelty / cat_count as f32,
            feasibility: cat_totals.feasibility / cat_count as f32,
            evidence_quality: cat_totals.evidence_quality / cat_count as f32,
        })
    } else {
        None
    };

    StructuredFeedback {
        contested_claims,
        verified_claims,
        mean_stance: if stance_count > 0 {
            stance_sum / stance_count as f32
        } else {
            0.0
        },
        evaluator_count: evaluations.len() as u32,
        category_breakdown,
    }
}

/// Estimates token count from text. Designed as a pluggable trait so a real
/// tokenizer (tiktoken, sentencepiece) can be swapped in later.
pub trait TokenEstimator: Send + Sync {
    fn estimate_tokens(&self, text: &str) -> u32;
}

/// Heuristic estimator: divides character count by `chars_per_token`.
#[derive(Debug, Clone)]
pub struct HeuristicTokenEstimator {
    pub chars_per_token: f64,
}

impl Default for HeuristicTokenEstimator {
    fn default() -> Self {
        Self {
            chars_per_token: 4.0,
        }
    }
}

impl TokenEstimator for HeuristicTokenEstimator {
    fn estimate_tokens(&self, text: &str) -> u32 {
        if self.chars_per_token <= 0.0 || text.is_empty() {
            return 0;
        }
        // Use chars().count() (Unicode scalar values) rather than len() (bytes)
        // so CJK/emoji text isn't overestimated by multi-byte UTF-8 encoding.
        (text.chars().count() as f64 / self.chars_per_token).ceil() as u32
    }
}

/// Lightweight pricing metadata extracted from AgentConfig for cost computation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentPricingInfo {
    /// USD per million input tokens (0.0 = free/unknown)
    pub input_price_per_mtok: f64,
    /// USD per million output tokens (0.0 = free/unknown)
    pub output_price_per_mtok: f64,
}

impl AgentPricingInfo {
    /// Compute cost for the given token counts.
    pub fn compute_cost(&self, input_tokens: u32, output_tokens: u32) -> f64 {
        (input_tokens as f64 * self.input_price_per_mtok
            + output_tokens as f64 * self.output_price_per_mtok)
            / 1_000_000.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProposalRecord {
    pub round: u32,
    pub author_agent_id: String,
    pub proposal: Proposal,
    pub evaluations: Vec<EvaluationRecord>,
    /// Sum of signed QV contributions (`score_q_s`) across evaluators, unbounded over ℝ.
    ///
    /// `score_q_s = sign(f) × √(|f| × 100) / 10` per evaluator, where `f` is the
    /// signed fraction from the per-evaluator QV pipeline.
    /// Used for proposal ranking, winner selection, and display. See `docs/scoring-variables.md`.
    pub aggregated_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct EvaluationRecord {
    pub evaluator_agent_id: String,
    pub evaluation: Evaluation,
    /// `true` when this record was injected by the orchestrator because the
    /// evaluator timed out or returned a partial batch.  Such evaluations
    /// exist only to unblock the aggregate-score sum so the deliberation
    /// proceeds; they MUST NOT contribute to convergence, variance, or
    /// ranking metrics.
    ///
    /// Defaults to `false` on deserialisation so payloads produced before
    /// this field was introduced are treated as real evaluations (matching
    /// prior behaviour).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema)]
pub struct CandidateProposal {
    pub id: String,
    pub proposal: Proposal,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct UserInjection {
    pub message: String,
    pub injected_at_round: u32,
    pub timestamp: u64,
    #[serde(default)]
    pub priority: InjectionPriority,
    /// Optional tool changes to add/remove user tools mid-deliberation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_changes: Option<ToolChanges>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default, PartialEq)]
pub enum InjectionPriority {
    #[default]
    Normal,
    Urgent,
}

/// Changes to user-defined tools, delivered via the hot-wire injection pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct ToolChanges {
    /// New user tool definitions to add.
    #[serde(default)]
    pub add: Vec<UserToolDefinition>,
    /// Tool names to remove (matched by name, without the `user_` prefix).
    #[serde(default)]
    pub remove: Vec<String>,
}

/// A user-defined tool that agents can call during deliberation.
/// The schema follows the OpenAI function calling format.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema)]
pub struct UserToolDefinition {
    /// Tool name as provided by the user (e.g., "dm_user", "read_user_file").
    /// Will be presented to the LLM with a "user_" prefix.
    pub name: String,
    /// Human-readable description of what this tool does.
    pub description: String,
    /// JSON Schema for tool parameters (OpenAI function calling format).
    /// Follows OpenAI's `parameters` field: `{ "type": "object", "properties": {...}, "required": [...] }`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// Whether the LLM must strictly match the schema. Mirrors OpenAI's `strict` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// A pending, responded, or expired tool call from an agent to a user-defined tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema)]
pub struct PendingToolCall {
    pub call_id: String,
    pub job_id: String,
    pub agent_id: String,
    /// The tool name WITH the "user_" prefix (as the LLM called it).
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub round: u32,
    pub phase: DeliberationPhase,
    pub status: ToolCallStatus,
    /// Epoch milliseconds when the call was created.
    pub created_at: u64,
    /// Epoch milliseconds when the user responded (None if pending/expired).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responded_at: Option<u64>,
    /// The user's response text (None if pending/expired).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default, PartialEq)]
pub enum ToolCallStatus {
    #[default]
    Pending,
    Responded,
    Expired,
}

#[async_trait]
pub trait PersistenceStore: Debug + Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>>;
    async fn append(&self, key: &str, content: &str) -> Result<()>;
    async fn set(&self, key: &str, content: &str) -> Result<()>;
    async fn get_round_history(&self, round: u32) -> Result<Option<Vec<ProposalRecord>>>;
}

#[async_trait]
pub trait NsedAgent: Send + Sync + Debug + dyn_clone::DynClone {
    async fn propose(&self, context: &AgentContext) -> Result<Proposal>;
    async fn evaluate(&self, context: &AgentContext) -> Result<Vec<(String, Evaluation)>>;
    fn name(&self) -> String;
}

dyn_clone::clone_trait_object!(NsedAgent);

/// Optional trait for agents that support direct chat (bypassing NSED deliberation).
///
/// Implemented by [`ProposerEvaluatorAgent`]. Third-party agents can also
/// implement this to enable the dashboard chat feature.
#[async_trait]
pub trait ChatCapable: Send + Sync {
    /// Send a direct conversation to the agent's underlying LLM.
    /// Messages use the agent's persona with an internal-voice wrapper.
    async fn chat(
        &self,
        messages: Vec<async_openai::types::ChatCompletionRequestMessage>,
    ) -> Result<String>;
}

/// Trait for user tool call handling. The reference implementation is
/// [`UserToolHandler`]; this trait lets `AgentContext`
/// hold a handler without leaking NATS internals into the public type.
#[async_trait]
pub trait UserToolHandlerTrait: Send + Sync + Debug {
    /// Handle a user tool call: publish to KV, wait for response, return result string.
    async fn handle_call(
        &self,
        tool_name: &str,
        arguments_json: &str,
        round: u32,
        phase: DeliberationPhase,
    ) -> String;
}

// =============================================================================
// Agent Discovery Protocol
// =============================================================================

/// Live status of an agent, reported via heartbeat.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentLiveStatus {
    /// Agent is connected but not processing any task.
    #[default]
    Idle,
    /// Agent is actively processing a task (propose or evaluate).
    Busy,
}

/// Heartbeat message published by agents to announce their presence on the bus.
///
/// Published to `{prefix}.agent.heartbeat.{agent_id}` every 10 seconds via
/// core NATS pub/sub (not JetStream — fire-and-forget).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema, Default)]
pub struct AgentHeartbeat {
    pub agent_id: String,
    pub status: AgentLiveStatus,
    pub model_name: String,
    pub provider_id: String,
    /// Job ID if currently processing, else None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_job: Option<String>,
    /// Seconds since the agent process started.
    pub uptime_secs: u64,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// USD per million input tokens (for orchestrator cost estimation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_per_mtok: Option<f64>,
    /// USD per million output tokens (for orchestrator cost estimation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_per_mtok: Option<f64>,
    /// Characters per token for heuristic estimation when provider omits usage stats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars_per_token: Option<f64>,

    /// Maximum seconds per task (propose/evaluate) — self-reported from config.
    /// Used by orchestrator for feasibility validation and phase timeout floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_sla_secs: Option<u64>,

    // ── Agent config fields (self-reported for dashboard display) ──
    /// LLM temperature setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Frequency penalty applied to generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    /// Presence penalty applied to generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    /// Max tokens per generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i32>,
    /// Context window size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i32>,

    // ── Reliability stats (self-reported for dashboard display) ──
    /// Total tasks completed successfully since agent start.
    #[serde(default)]
    pub tasks_completed: u64,
    /// Total tasks that failed since agent start.
    #[serde(default)]
    pub tasks_failed: u64,
    /// Most recent error message (truncated), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,

    // ── Agent metadata (for directory/ranking) ──
    /// Free-form capability tags (e.g., `["legal", "audit"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capability_tags: Vec<String>,
    /// Short description of the agent's specialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Signing schemes this agent supports (placeholder for #115).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signing_schemes: Vec<String>,
}

/// Ping message published by the orchestrator so agents can verify it is alive.
///
/// Published to `{prefix}.orchestrator.ping` every 15 seconds via core NATS.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema)]
pub struct OrchestratorPing {
    pub orchestrator_id: String,
    pub timestamp: String,
    pub uptime_secs: u64,
}

/// Normalize a signed endorsement weight to a signed fraction ∈ [-1, +1].
///
/// `total_abs_weight` must be `Σ|w_i|` across all proposals from the same
/// evaluator. Returns `(raw_weight / total_abs_weight).clamp(-1.0, 1.0)`.
/// When `total_abs_weight` is effectively zero (≤ `f32::EPSILON`), returns
/// `0.0` — the evaluator expressed no opinion.
pub fn normalize_score(raw_weight: f32, total_abs_weight: f32) -> f32 {
    if total_abs_weight > f32::EPSILON {
        (raw_weight / total_abs_weight).clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Signed QV-transform a normalized fraction ∈ [-1, +1] → `score_q_s` ∈ [-1, +1].
///
/// Formula: `score_q_s = sign(f) × √(|f| × 100) / 10`.
///
/// Preserves sign (endorsement vs opposition), applies QV diminishing returns
/// to the magnitude. Used for both ranking and convergence — single pipeline.
pub fn calculate_qv_from_fraction(fraction: f32) -> f32 {
    let f = fraction.clamp(-1.0, 1.0);
    if f.abs() <= f32::EPSILON {
        return 0.0;
    }
    let magnitude = (f.abs() * 100.0).sqrt() / 10.0;
    f.signum() * magnitude
}

/// **Legacy unsigned** QV score from raw token weights (pre-signed-pipeline).
///
/// Formula: `score_q_u = sqrt(normalized_tokens) / 10`
///
/// The QV activation function dampens the impact of "extremist" voters who spend
/// 100% of their budget on one option, while the division by 10 normalizes the
/// result to the [0, 1] interval.
///
/// **Note**: The current scoring pipeline uses the signed variant
/// [`calculate_qv_from_fraction`] which produces `score_q_s ∈ ℝ`. This
/// unsigned version is retained for backward-compatible token estimation.
///
/// # Arguments
/// * `raw_weight` - The raw vote weight allocated by an evaluator
/// * `total_weight` - The sum of all weights from this evaluator (for normalization)
///
/// # Returns
/// A tuple of (`score_q_u`, normalized_tokens) where:
/// * `score_q_u` is the unsigned QV-transformed score in [0, 1]
/// * `normalized_tokens` is the budget-clamped input in [0, 100]
pub fn calculate_qv_score(raw_weight: f32, total_weight: f32) -> (f32, f32) {
    let normalized_tokens = if total_weight > 100.0 {
        ((raw_weight / total_weight) * 100.0).clamp(0.0, 100.0)
    } else {
        raw_weight.clamp(0.0, 100.0)
    };
    let strength = normalized_tokens.sqrt();
    let display_influence = strength / 10.0;
    (display_influence, normalized_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Serde Roundtrip Tests — ensure backward compatibility for all new types
    // =========================================================================

    #[test]
    fn claim_verdict_serde_roundtrip() {
        for variant in [
            ClaimVerdict::Verified,
            ClaimVerdict::Contested,
            ClaimVerdict::Unverified,
            ClaimVerdict::Wrong,
            ClaimVerdict::Unknown,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: ClaimVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    #[test]
    fn claim_verdict_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&ClaimVerdict::Verified).unwrap(),
            "\"verified\""
        );
        assert_eq!(
            serde_json::to_string(&ClaimVerdict::Wrong).unwrap(),
            "\"wrong\""
        );
        assert_eq!(
            serde_json::to_string(&Stance::StrongAgree).unwrap(),
            "\"strong_agree\""
        );
        assert_eq!(
            serde_json::to_string(&Stance::StrongDisagree).unwrap(),
            "\"strong_disagree\""
        );
        assert_eq!(
            serde_json::to_string(&Confidence::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&Confidence::Medium).unwrap(),
            "\"medium\""
        );
        assert_eq!(serde_json::to_string(&Confidence::Low).unwrap(), "\"low\"");
    }

    #[test]
    fn stance_serde_roundtrip() {
        for variant in [
            Stance::StrongAgree,
            Stance::Agree,
            Stance::Neutral,
            Stance::Disagree,
            Stance::StrongDisagree,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: Stance = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    #[test]
    fn confidence_serde_roundtrip() {
        for variant in [Confidence::High, Confidence::Medium, Confidence::Low] {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: Confidence = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    #[test]
    fn evaluation_backward_compat_minimal_json() {
        // Legacy format: only score + justification. All new fields must default.
        let json = r#"{"score": 0.75, "justification": "Looks good"}"#;
        let eval: Evaluation = serde_json::from_str(json).unwrap();
        assert!((eval.score - 0.75).abs() < f32::EPSILON);
        assert_eq!(eval.justification, "Looks good");
        assert!(eval.claim_assessments.is_empty());
        assert!(eval.disagreements.is_empty());
        assert!(eval.stance.is_none());
        assert!(!eval.is_final_solution);
        assert!(eval.category_scores.is_none());
        assert!(eval.token_usage.is_none());
    }

    #[test]
    fn evaluation_full_structured_roundtrip() {
        let eval = Evaluation {
            score: 0.82,
            justification: "Well-reasoned".to_string(),
            token_usage: Some(TokenUsage {
                input_tokens: 1500,
                output_tokens: 300,
            }),
            claim_assessments: vec![
                ClaimAssessment {
                    claim_id: Some("abc123".to_string()),
                    claim: "O(n log n) complexity".to_string(),
                    verdict: ClaimVerdict::Verified,
                    reason: Some("Confirmed via analysis".to_string()),
                },
                ClaimAssessment {
                    claim_id: None,
                    claim: "Thread safety guaranteed".to_string(),
                    verdict: ClaimVerdict::Wrong,
                    reason: Some("Missing lock in critical section".to_string()),
                },
            ],
            disagreements: vec![DisagreementPoint {
                claim_id: Some("abc123".to_string()),
                proposal_claims: "No race condition".to_string(),
                evaluator_position: "Race condition on shared state".to_string(),
                confidence: Confidence::High,
            }],
            stance: Some(Stance::Disagree),
            is_final_solution: false,
            category_scores: Some(CategoryScores {
                correctness: 60.0,
                completeness: 80.0,
                novelty: 40.0,
                feasibility: 90.0,
                evidence_quality: 55.0,
            }),
            ..Default::default()
        };

        let json = serde_json::to_string(&eval).unwrap();
        let deserialized: Evaluation = serde_json::from_str(&json).unwrap();

        assert!((deserialized.score - 0.82).abs() < f32::EPSILON);
        assert_eq!(deserialized.claim_assessments.len(), 2);
        assert_eq!(
            deserialized.claim_assessments[0].verdict,
            ClaimVerdict::Verified
        );
        assert_eq!(
            deserialized.claim_assessments[1].verdict,
            ClaimVerdict::Wrong
        );
        assert_eq!(deserialized.disagreements.len(), 1);
        assert_eq!(deserialized.disagreements[0].confidence, Confidence::High);
        assert_eq!(deserialized.stance, Some(Stance::Disagree));
        assert!(!deserialized.is_final_solution);
        let cs = deserialized.category_scores.unwrap();
        assert!((cs.correctness - 60.0).abs() < f32::EPSILON);
        assert!((cs.evidence_quality - 55.0).abs() < f32::EPSILON);
    }

    #[test]
    fn evaluation_skip_serializing_none_fields() {
        let eval = Evaluation {
            score: 0.5,
            justification: "Ok".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&eval).unwrap();
        // Optional None fields should be omitted
        assert!(!json.contains("token_usage"));
        assert!(!json.contains("stance"));
        assert!(!json.contains("category_scores"));
        // claim_assessments defaults to [] but isn't skip_serializing_if
        // so it may appear as empty array — that's fine for backward compat
    }

    #[test]
    fn structured_feedback_serde_roundtrip() {
        let sf = StructuredFeedback {
            contested_claims: vec![ContestedClaim {
                claim_id: "abc123".to_string(),
                what_you_claimed: "X is true".to_string(),
                counter_position: "X is false because Y".to_string(),
                evaluator: "eval_1".to_string(),
                confidence: Confidence::High,
            }],
            verified_claims: vec!["Claim A is correct".to_string()],
            mean_stance: -0.5,
            evaluator_count: 3,
            category_breakdown: Some(CategoryScores {
                correctness: 70.0,
                completeness: 80.0,
                novelty: 50.0,
                feasibility: 90.0,
                evidence_quality: 60.0,
            }),
        };
        let json = serde_json::to_string(&sf).unwrap();
        let deserialized: StructuredFeedback = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.contested_claims.len(), 1);
        assert_eq!(deserialized.verified_claims.len(), 1);
        assert!((deserialized.mean_stance - (-0.5)).abs() < f32::EPSILON);
        assert_eq!(deserialized.evaluator_count, 3);
        assert!(deserialized.category_breakdown.is_some());
    }

    // =========================================================================
    // generate_claim_id() Tests
    // =========================================================================

    #[test]
    fn generate_claim_id_is_deterministic() {
        let id1 = generate_claim_id("agent_1", "O(n log n) proof", 1);
        let id2 = generate_claim_id("agent_1", "O(n log n) proof", 1);
        assert_eq!(id1, id2, "Same inputs must produce identical claim IDs");
    }

    #[test]
    fn generate_claim_id_is_6_hex_chars() {
        let id = generate_claim_id("agent_1", "some claim", 3);
        assert_eq!(id.len(), 6);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_claim_id_differs_for_different_rounds() {
        let r1 = generate_claim_id("agent_1", "same claim", 1);
        let r2 = generate_claim_id("agent_1", "same claim", 2);
        assert_ne!(r1, r2, "Different rounds should produce different IDs");
    }

    #[test]
    fn generate_claim_id_differs_for_different_targets() {
        let a = generate_claim_id("agent_1", "same claim", 1);
        let b = generate_claim_id("agent_2", "same claim", 1);
        assert_ne!(a, b, "Different targets should produce different IDs");
    }

    #[test]
    fn generate_claim_id_case_insensitive_on_claim_text() {
        let lower = generate_claim_id("agent_1", "this is a claim", 1);
        let upper = generate_claim_id("agent_1", "THIS IS A CLAIM", 1);
        assert_eq!(lower, upper, "Claim text should be normalized to lowercase");
    }

    // =========================================================================
    // build_structured_feedback() Tests
    // =========================================================================

    fn make_eval_record(
        evaluator: &str,
        score: f32,
        stance: Option<Stance>,
        claims: Vec<ClaimAssessment>,
        disagreements: Vec<DisagreementPoint>,
        category_scores: Option<CategoryScores>,
    ) -> EvaluationRecord {
        EvaluationRecord {
            evaluator_agent_id: evaluator.to_string(),
            evaluation: Evaluation {
                score,
                justification: format!("Evaluation by {evaluator}"),
                stance,
                claim_assessments: claims,
                disagreements,
                category_scores,
                ..Default::default()
            },
            synthetic: false,
        }
    }

    #[test]
    fn build_structured_feedback_empty_evaluations() {
        let sf = build_structured_feedback(&[]);
        assert!(sf.contested_claims.is_empty());
        assert!(sf.verified_claims.is_empty());
        assert!((sf.mean_stance - 0.0).abs() < f32::EPSILON);
        assert_eq!(sf.evaluator_count, 0);
        assert!(sf.category_breakdown.is_none());
    }

    #[test]
    fn build_structured_feedback_single_verified_claim() {
        let evals = vec![make_eval_record(
            "eval_1",
            0.8,
            Some(Stance::Agree),
            vec![ClaimAssessment {
                claim_id: Some("c1".to_string()),
                claim: "Algorithm is correct".to_string(),
                verdict: ClaimVerdict::Verified,
                reason: Some("Confirmed".to_string()),
            }],
            vec![],
            None,
        )];

        let sf = build_structured_feedback(&evals);
        assert!(sf.contested_claims.is_empty());
        assert_eq!(sf.verified_claims, vec!["Algorithm is correct"]);
        assert!((sf.mean_stance - 1.0).abs() < f32::EPSILON); // Agree = 1.0
        assert_eq!(sf.evaluator_count, 1);
    }

    #[test]
    fn build_structured_feedback_contested_and_wrong_claims() {
        let evals = vec![make_eval_record(
            "eval_1",
            0.3,
            Some(Stance::Disagree),
            vec![
                ClaimAssessment {
                    claim_id: Some("c1".to_string()),
                    claim: "Thread safe".to_string(),
                    verdict: ClaimVerdict::Contested,
                    reason: Some("Missing mutex".to_string()),
                },
                ClaimAssessment {
                    claim_id: None,
                    claim: "O(1) lookup".to_string(),
                    verdict: ClaimVerdict::Wrong,
                    reason: Some("Actually O(n)".to_string()),
                },
            ],
            vec![],
            None,
        )];

        let sf = build_structured_feedback(&evals);
        assert_eq!(sf.contested_claims.len(), 2);
        // First claim keeps its explicit claim_id
        assert_eq!(sf.contested_claims[0].claim_id, "c1");
        assert_eq!(sf.contested_claims[0].what_you_claimed, "Thread safe");
        // Second claim gets auto-generated claim_id
        assert!(sf.contested_claims[1].claim_id.starts_with("auto_"));
        assert_eq!(sf.contested_claims[1].what_you_claimed, "O(1) lookup");
    }

    #[test]
    fn build_structured_feedback_disagreement_points() {
        let evals = vec![make_eval_record(
            "eval_1",
            0.4,
            None,
            vec![],
            vec![DisagreementPoint {
                claim_id: None,
                proposal_claims: "Uses quicksort".to_string(),
                evaluator_position: "Mergesort is better for stability".to_string(),
                confidence: Confidence::High,
            }],
            None,
        )];

        let sf = build_structured_feedback(&evals);
        assert_eq!(sf.contested_claims.len(), 1);
        assert!(sf.contested_claims[0].claim_id.starts_with("disp_"));
        assert_eq!(sf.contested_claims[0].confidence, Confidence::High);
    }

    #[test]
    fn build_structured_feedback_stance_aggregation() {
        let evals = vec![
            make_eval_record("e1", 0.7, Some(Stance::StrongAgree), vec![], vec![], None),
            make_eval_record("e2", 0.3, Some(Stance::Disagree), vec![], vec![], None),
            make_eval_record("e3", 0.5, Some(Stance::Neutral), vec![], vec![], None),
        ];

        let sf = build_structured_feedback(&evals);
        // StrongAgree(2) + Disagree(-1) + Neutral(0) = 1.0 / 3 ≈ 0.333
        assert!((sf.mean_stance - (1.0 / 3.0)).abs() < 0.01);
        assert_eq!(sf.evaluator_count, 3);
    }

    #[test]
    fn build_structured_feedback_stance_strong_disagree() {
        let evals = vec![
            make_eval_record(
                "e1",
                0.2,
                Some(Stance::StrongDisagree),
                vec![],
                vec![],
                None,
            ),
            make_eval_record("e2", 0.9, Some(Stance::Agree), vec![], vec![], None),
        ];

        let sf = build_structured_feedback(&evals);
        // StrongDisagree(-2) + Agree(1) = -1.0 / 2 = -0.5
        assert!((sf.mean_stance - (-0.5)).abs() < f32::EPSILON);
        assert_eq!(sf.evaluator_count, 2);
    }

    #[test]
    fn build_structured_feedback_stance_ignores_none() {
        let evals = vec![
            make_eval_record("e1", 0.8, Some(Stance::Agree), vec![], vec![], None),
            make_eval_record("e2", 0.5, None, vec![], vec![], None), // No stance
        ];

        let sf = build_structured_feedback(&evals);
        // Only e1 has stance: Agree(1.0) / 1 = 1.0
        assert!((sf.mean_stance - 1.0).abs() < f32::EPSILON);
        assert_eq!(sf.evaluator_count, 2); // Both counted as evaluators
    }

    #[test]
    fn build_structured_feedback_category_score_averaging() {
        let cs1 = CategoryScores {
            correctness: 80.0,
            completeness: 60.0,
            novelty: 40.0,
            feasibility: 90.0,
            evidence_quality: 70.0,
        };
        let cs2 = CategoryScores {
            correctness: 60.0,
            completeness: 80.0,
            novelty: 60.0,
            feasibility: 70.0,
            evidence_quality: 50.0,
        };

        let evals = vec![
            make_eval_record("e1", 0.7, None, vec![], vec![], Some(cs1)),
            make_eval_record("e2", 0.6, None, vec![], vec![], Some(cs2)),
        ];

        let sf = build_structured_feedback(&evals);
        let cat = sf
            .category_breakdown
            .expect("Should have category breakdown");
        assert!((cat.correctness - 70.0).abs() < f32::EPSILON);
        assert!((cat.completeness - 70.0).abs() < f32::EPSILON);
        assert!((cat.novelty - 50.0).abs() < f32::EPSILON);
        assert!((cat.feasibility - 80.0).abs() < f32::EPSILON);
        assert!((cat.evidence_quality - 60.0).abs() < f32::EPSILON);
    }

    #[test]
    fn build_structured_feedback_category_scores_skipped_when_none() {
        let evals = vec![make_eval_record("e1", 0.5, None, vec![], vec![], None)];
        let sf = build_structured_feedback(&evals);
        assert!(sf.category_breakdown.is_none());
    }

    #[test]
    fn build_structured_feedback_verified_claims_deduplicated() {
        let evals = vec![
            make_eval_record(
                "e1",
                0.8,
                None,
                vec![ClaimAssessment {
                    claim_id: None,
                    claim: "Earth is round".to_string(),
                    verdict: ClaimVerdict::Verified,
                    reason: None,
                }],
                vec![],
                None,
            ),
            make_eval_record(
                "e2",
                0.9,
                None,
                vec![ClaimAssessment {
                    claim_id: None,
                    claim: "Earth is round".to_string(),
                    verdict: ClaimVerdict::Verified,
                    reason: None,
                }],
                vec![],
                None,
            ),
        ];

        let sf = build_structured_feedback(&evals);
        // Deduplication: same claim verified by two evaluators → appears once
        assert_eq!(sf.verified_claims.len(), 1);
        assert_eq!(sf.verified_claims[0], "Earth is round");
    }

    #[test]
    fn build_structured_feedback_unverified_claims_ignored() {
        let evals = vec![make_eval_record(
            "e1",
            0.5,
            None,
            vec![ClaimAssessment {
                claim_id: None,
                claim: "Might be true".to_string(),
                verdict: ClaimVerdict::Unverified,
                reason: None,
            }],
            vec![],
            None,
        )];

        let sf = build_structured_feedback(&evals);
        assert!(sf.contested_claims.is_empty());
        assert!(sf.verified_claims.is_empty());
    }

    // =========================================================================
    // Default Derive Tests
    // =========================================================================

    #[test]
    fn default_enums_have_expected_defaults() {
        assert_eq!(ClaimVerdict::default(), ClaimVerdict::Unknown);
        assert_eq!(Stance::default(), Stance::Neutral);
        assert_eq!(Confidence::default(), Confidence::Medium);
    }

    #[test]
    fn default_evaluation_is_empty() {
        let eval = Evaluation::default();
        assert!((eval.score - 0.0).abs() < f32::EPSILON);
        assert!(eval.justification.is_empty());
        assert!(eval.claim_assessments.is_empty());
        assert!(eval.disagreements.is_empty());
        assert!(eval.stance.is_none());
        assert!(!eval.is_final_solution);
        assert!(eval.category_scores.is_none());
    }

    // =========================================================================
    // Regression: DisagreementPoint field name aliases
    // Models hallucinate alternative field names for `proposal_claims` and
    // `evaluator_position`. Failures captured in failures/quant-ml_MACRO/
    // and failures/quant-ml_MOMENTUM/.
    // =========================================================================

    /// Regression: Mistral uses "contested_claim" + "belief" instead of
    /// "proposal_claims" + "evaluator_position".
    #[test]
    fn disagreement_point_alias_contested_claim_and_belief() {
        let json = serde_json::json!({
            "contested_claim": "The 38% equity allocation is optimal",
            "belief": "40% equity is more appropriate given historical returns",
            "confidence": "medium"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(dp.proposal_claims, "The 38% equity allocation is optimal");
        assert_eq!(
            dp.evaluator_position,
            "40% equity is more appropriate given historical returns"
        );
        assert_eq!(dp.confidence, Confidence::Medium);
    }

    /// Regression: GPT-OSS uses "claim" + "details" instead of
    /// "proposal_claims" + "evaluator_position".
    #[test]
    fn disagreement_point_alias_claim_and_details() {
        let json = serde_json::json!({
            "claim": "1% hedge provides sufficient protection",
            "details": "A 1% hedge yields at most 0.6% portfolio gain, insufficient to offset losses.",
            "confidence": "high"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(
            dp.proposal_claims,
            "1% hedge provides sufficient protection"
        );
        assert!(dp.evaluator_position.contains("0.6% portfolio gain"));
        assert_eq!(dp.confidence, Confidence::High);
    }

    /// Regression: counter_position alias (close to evaluator_position but not exact)
    #[test]
    fn disagreement_point_alias_counter_position() {
        let json = serde_json::json!({
            "proposal_claims": "Equities should be 50%",
            "counter_position": "40% is safer given volatility",
            "confidence": "low"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(dp.evaluator_position, "40% is safer given volatility");
    }

    /// Canonical field names still work unchanged.
    #[test]
    fn disagreement_point_canonical_fields_still_work() {
        let json = serde_json::json!({
            "claim_id": "abc123",
            "proposal_claims": "The algorithm is O(n)",
            "evaluator_position": "It is O(n^2) due to nested loop",
            "confidence": "high"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(dp.claim_id, Some("abc123".to_string()));
        assert_eq!(dp.proposal_claims, "The algorithm is O(n)");
        assert_eq!(dp.evaluator_position, "It is O(n^2) due to nested loop");
    }

    /// Regression: full evaluation payload from MACRO failure dump line 116
    /// with "contested_claim" + "belief" in disagreements.
    #[test]
    fn regression_macro_evaluation_with_aliased_disagreements() {
        let json = serde_json::json!({
            "evaluations": [{
                "agent_id": "Candidate_B",
                "stance": "strong_disagree",
                "claim_assessments": [
                    {"claim": "38% equity allocation", "verdict": "contested"},
                    {"claim": "Tail risk hedge costs ~45bps", "verdict": "contested"}
                ],
                "disagreements": [
                    {
                        "contested_claim": "38% equity allocation due to elevated valuations",
                        "belief": "40% equity is more appropriate given historical returns",
                        "confidence": "medium"
                    },
                    {
                        "contested_claim": "SPX puts at 100% of equity sleeve costs 45bps",
                        "belief": "5% notional put-spread is more cost-effective",
                        "confidence": "high"
                    }
                ],
                "category_scores": {
                    "correctness": 50, "completeness": 60, "novelty": 70,
                    "feasibility": 60, "evidence_quality": 55
                },
                "endorsement_weight": 55
            }]
        });

        // This is the same struct type used in the agent's evaluate() method
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct BatchResponse {
            evaluations: Vec<BatchItem>,
        }
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct BatchItem {
            agent_id: String,
            #[serde(default)]
            stance: Option<Stance>,
            #[serde(default)]
            claim_assessments: Vec<ClaimAssessment>,
            #[serde(default)]
            disagreements: Vec<DisagreementPoint>,
            #[serde(default)]
            category_scores: Option<CategoryScores>,
            endorsement_weight: f32,
        }

        let resp: BatchResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.evaluations.len(), 1);
        let item = &resp.evaluations[0];
        assert_eq!(item.agent_id, "Candidate_B");
        assert_eq!(item.stance, Some(Stance::StrongDisagree));
        assert_eq!(item.disagreements.len(), 2);
        assert_eq!(
            item.disagreements[0].proposal_claims,
            "38% equity allocation due to elevated valuations"
        );
        assert_eq!(
            item.disagreements[0].evaluator_position,
            "40% equity is more appropriate given historical returns"
        );
        assert_eq!(
            item.disagreements[1].proposal_claims,
            "SPX puts at 100% of equity sleeve costs 45bps"
        );
        assert!((item.endorsement_weight - 55.0).abs() < f32::EPSILON);
    }

    /// Regression: Mistral uses "content" instead of "claim" in ClaimAssessment.
    /// From failures/quant-ml_MACRO/parse_error_r1.md line 289:
    ///   `missing field 'claim' at line 1 column 208`
    #[test]
    fn claim_assessment_alias_content() {
        let json = serde_json::json!({
            "content": "The allocation strategy meets the fund's return targets.",
            "verdict": "verified"
        });
        let ca: ClaimAssessment = serde_json::from_value(json).unwrap();
        assert_eq!(
            ca.claim,
            "The allocation strategy meets the fund's return targets."
        );
        assert_eq!(ca.verdict, ClaimVerdict::Verified);
    }

    /// Regression: model uses "disagreement" field as reason alias on ClaimAssessment.
    /// From failures/quant-ml_MACRO/parse_error_r1.md line 355:
    ///   `{"claim":"...","verdict":"contested","disagreement":"I believe..."}`
    #[test]
    fn claim_assessment_alias_disagreement_as_reason() {
        let json = serde_json::json!({
            "claim": "Alternative allocation is too high",
            "verdict": "contested",
            "disagreement": "I believe allocating 10% to alternatives is more appropriate."
        });
        let ca: ClaimAssessment = serde_json::from_value(json).unwrap();
        assert_eq!(
            ca.reason,
            Some("I believe allocating 10% to alternatives is more appropriate.".to_string())
        );
    }

    /// Regression: full MACRO evaluation payload with "content" alias on claims.
    /// From failures/quant-ml_MACRO/parse_error_r1.md lines 289-296.
    #[test]
    fn regression_macro_evaluation_with_content_alias_claims() {
        let json = serde_json::json!({
            "evaluations": [{
                "agent_id": "Candidate_A",
                "endorsement_weight": 78,
                "stance": "agree",
                "claim_assessments": [
                    {"content": "The allocation strategy meets targets.", "verdict": "verified"},
                    {"content": "Momentum-driven framework is ideal.", "verdict": "verified"},
                    {"content": "OTM put spread is cost-effective.", "verdict": "verified"}
                ],
                "disagreements": [],
                "category_scores": {
                    "correctness": 85, "completeness": 75, "novelty": 80,
                    "feasibility": 80, "evidence_quality": 80
                }
            }]
        });

        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Batch {
            evaluations: Vec<Item>,
        }
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Item {
            agent_id: String,
            #[serde(default)]
            claim_assessments: Vec<ClaimAssessment>,
            endorsement_weight: f32,
        }

        let resp: Batch = serde_json::from_value(json).unwrap();
        let item = &resp.evaluations[0];
        assert_eq!(item.claim_assessments.len(), 3);
        assert_eq!(
            item.claim_assessments[0].claim,
            "The allocation strategy meets targets."
        );
        assert_eq!(item.claim_assessments[0].verdict, ClaimVerdict::Verified);
    }

    // =========================================================================
    // New alias regression tests — from failure dumps analysis
    // =========================================================================

    /// DisagreementPoint: gpt-oss uses "explanation" for evaluator_position
    #[test]
    fn test_disagreement_alias_explanation() {
        let json = serde_json::json!({
            "claim": "Equities 50% allocation will meet targets.",
            "explanation": "A 50% equity exposure is too high for the -8% drawdown limit.",
            "confidence": "high"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(
            dp.evaluator_position,
            "A 50% equity exposure is too high for the -8% drawdown limit."
        );
        assert_eq!(
            dp.proposal_claims,
            "Equities 50% allocation will meet targets."
        );
    }

    /// DisagreementPoint: gpt-oss uses "analysis" for evaluator_position
    #[test]
    fn test_disagreement_alias_analysis() {
        let json = serde_json::json!({
            "claim": "Value factor is appropriate.",
            "analysis": "Current P/E ratios are above average, value tilt is risky.",
            "confidence": "medium"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(
            dp.evaluator_position,
            "Current P/E ratios are above average, value tilt is risky."
        );
    }

    /// DisagreementPoint: gpt-oss uses "counter" for evaluator_position and "proposal" for proposal_claims
    #[test]
    fn test_disagreement_alias_counter_and_proposal() {
        let json = serde_json::json!({
            "claim_id": "C1",
            "proposal": "Equity allocation of 40% of total AUM",
            "counter": "Our analysis indicates 40% equity exceeds the drawdown limit.",
            "confidence": "high"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(dp.proposal_claims, "Equity allocation of 40% of total AUM");
        assert_eq!(
            dp.evaluator_position,
            "Our analysis indicates 40% equity exceeds the drawdown limit."
        );
    }

    /// DisagreementPoint: gpt-oss uses "our_position" for evaluator_position
    #[test]
    fn test_disagreement_alias_our_position() {
        let json = serde_json::json!({
            "proposal": "Provides allocation percentages and strategy.",
            "our_position": "Cannot assess due to missing content.",
            "confidence": "high"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(
            dp.evaluator_position,
            "Cannot assess due to missing content."
        );
    }

    /// DisagreementPoint: gpt-oss uses "your_view" for evaluator_position
    #[test]
    fn test_disagreement_alias_your_view() {
        let json = serde_json::json!({
            "claim_id": "C_value",
            "proposal": "Value factor exposure of 20%.",
            "your_view": "Elevated P/E ratios make value tilt unsupported.",
            "confidence": "medium"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(
            dp.evaluator_position,
            "Elevated P/E ratios make value tilt unsupported."
        );
    }

    /// DisagreementPoint: gpt-oss uses "what_they_claimed" + "what_i_believe"
    #[test]
    fn test_disagreement_alias_what_they_what_i() {
        let json = serde_json::json!({
            "what_they_claimed": "Mean-reversion overlay provides superior risk management.",
            "what_i_believe": "The overlay adds unnecessary complexity.",
            "confidence": "high"
        });
        let dp: DisagreementPoint = serde_json::from_value(json).unwrap();
        assert_eq!(
            dp.proposal_claims,
            "Mean-reversion overlay provides superior risk management."
        );
        assert_eq!(
            dp.evaluator_position,
            "The overlay adds unnecessary complexity."
        );
    }

    /// ClaimAssessment: gpt-oss uses "description" instead of "claim"
    #[test]
    fn test_claim_assessment_alias_description() {
        let json = serde_json::json!({
            "description": "Proposal content is incomplete, preventing verification.",
            "verdict": "unverified"
        });
        let ca: ClaimAssessment = serde_json::from_value(json).unwrap();
        assert_eq!(
            ca.claim,
            "Proposal content is incomplete, preventing verification."
        );
        assert_eq!(ca.verdict, ClaimVerdict::Unverified);
    }

    /// ClaimAssessment: gpt-oss uses "summary" instead of "claim"
    #[test]
    fn test_claim_assessment_alias_summary() {
        let json = serde_json::json!({
            "claim_id": "C1",
            "summary": "Allocation (40/40/15/5) will achieve 12-15% return.",
            "verdict": "unverified",
            "reasoning": "No backtest evidence provided."
        });
        let ca: ClaimAssessment = serde_json::from_value(json).unwrap();
        assert_eq!(
            ca.claim,
            "Allocation (40/40/15/5) will achieve 12-15% return."
        );
        assert_eq!(ca.reason.unwrap(), "No backtest evidence provided.");
    }

    /// ClaimAssessment: "reasoning" alias for "reason"
    #[test]
    fn test_claim_assessment_alias_reasoning() {
        let json = serde_json::json!({
            "claim": "Hedge cost is 35bps.",
            "verdict": "verified",
            "reasoning": "Consistent with our own hedge design."
        });
        let ca: ClaimAssessment = serde_json::from_value(json).unwrap();
        assert_eq!(ca.reason.unwrap(), "Consistent with our own hedge design.");
    }

    /// Full gpt-oss evaluation payload with mixed aliases (MOMENTUM r5 att3)
    #[test]
    fn test_gpt_oss_full_eval_payload_mixed_aliases() {
        let json = serde_json::json!({
            "evaluations": [{
                "candidate_id": "Candidate_C",
                "endorsement_weight": 45.0,
                "stance": "disagree",
                "claim_assessments": [
                    {"claim_id": "C1", "claim_text": "36% equity yields drawdown <8%", "verdict": "wrong", "reason": "Backtest shows 35% is optimal."},
                    {"claim_id": "C2", "claim_text": "Put-spread costs 35bps", "verdict": "verified", "reason": "Consistent with our design."},
                ],
                "disagreements": [
                    {"claim_id": "C1", "our_position": "Equity at 36% breaches beta cap.", "confidence": "high"}
                ],
                "category_scores": {"correctness": 45, "completeness": 50, "novelty": 60, "feasibility": 55, "evidence_quality": 40}
            }, {
                "candidate_id": "Candidate_B",
                "endorsement_weight": 78.0,
                "stance": "agree",
                "claim_assessments": [
                    {"claim_id": "B1", "claim_text": "36% equity, beta 0.43, max dd <8%", "verdict": "verified", "reason": "Results consistent."},
                ],
                "disagreements": [],
                "category_scores": {"correctness": 80, "completeness": 78, "novelty": 85, "feasibility": 80, "evidence_quality": 78}
            }]
        });

        // This payload uses "candidate_id" (aliased), "claim_text" (aliased),
        // "our_position" (newly aliased), and missing proposal_claims in disagreement
        // (uses only claim_id + our_position + confidence)
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Batch {
            evaluations: Vec<Item>,
        }
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct Item {
            #[serde(alias = "candidate_id")]
            agent_id: String,
            endorsement_weight: f32,
            #[serde(default)]
            claim_assessments: Vec<ClaimAssessment>,
            #[serde(default)]
            disagreements: Vec<DisagreementPoint>,
        }

        let resp: Batch = serde_json::from_value(json).unwrap();
        assert_eq!(resp.evaluations.len(), 2);
        assert_eq!(resp.evaluations[0].agent_id, "Candidate_C");
        assert_eq!(resp.evaluations[0].claim_assessments.len(), 2);
        assert_eq!(resp.evaluations[0].disagreements.len(), 1);
        assert_eq!(
            resp.evaluations[0].disagreements[0].evaluator_position,
            "Equity at 36% breaches beta cap."
        );
        assert_eq!(resp.evaluations[1].agent_id, "Candidate_B");
        assert_eq!(resp.evaluations[1].endorsement_weight, 78.0);
    }

    // =========================================================================
    // AgentContext serde tests
    // =========================================================================

    #[test]
    fn agent_context_serde_roundtrip() {
        let ctx = AgentContext {
            task_description: "Solve the halting problem".to_string(),
            round_number: 3,
            total_rounds: 5,
            phase: DeliberationPhase::Evaluating,
            target_proposal: Some(Proposal {
                thought_process: "Think hard".to_string(),
                content: "My proposal".to_string(),
                final_scratchpad: Some("notes".to_string()),
                token_usage_stats: Some(TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                }),
                ..Default::default()
            }),
            competitor_summaries: vec!["Agent A did X".to_string(), "Agent B did Y".to_string()],
            previous_round_matrix: Some("matrix data".to_string()),
            previous_own_proposal: Some(Proposal {
                thought_process: "Previous thought".to_string(),
                content: "Previous content".to_string(),
                final_scratchpad: None,
                token_usage_stats: None,
                ..Default::default()
            }),
            previous_own_score: Some(0.85),
            previous_critiques: vec!["Needs more evidence".to_string()],
            scratchpad: Some("my scratchpad".to_string()),
            store: None, // serde(skip)
            candidates: vec![CandidateProposal {
                id: "c1".to_string(),
                proposal: Proposal {
                    thought_process: "candidate thought".to_string(),
                    content: "candidate content".to_string(),
                    final_scratchpad: None,
                    token_usage_stats: None,
                    ..Default::default()
                },
            }],
            user_injections: vec![UserInjection {
                message: "Focus on feasibility".to_string(),
                injected_at_round: 2,
                timestamp: 1700000000,
                priority: InjectionPriority::Urgent,
                tool_changes: None,
            }],
            user_tools: vec![UserToolDefinition {
                name: "dm_user".to_string(),
                description: "Send a DM".to_string(),
                parameters: Some(serde_json::json!({"type": "object", "properties": {}})),
                strict: Some(true),
            }],
            phase_budget_remaining_secs: 42.5,
            session_id: Some("sess-123".to_string()),
            conversation_id: None,
            messages: Vec::new(),
            structured_feedback: Some(StructuredFeedback {
                contested_claims: vec![],
                verified_claims: vec!["claim A".to_string()],
                mean_stance: 0.5,
                evaluator_count: 2,
                category_breakdown: None,
            }),
            forced_proposal_schema: None,
            user_tool_handler: None, // serde(skip)
            role: Some("security".to_string()),
            role_context: Some("Per-role context content".to_string()),
            telemetry: None, // serde(skip)
            agent_id: String::new(),
            task_publish_ts: Some(1_776_790_000_000),
        };

        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: AgentContext = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.task_description, "Solve the halting problem");
        assert_eq!(deserialized.round_number, 3);
        assert_eq!(deserialized.total_rounds, 5);
        assert_eq!(deserialized.phase, DeliberationPhase::Evaluating);
        assert!(deserialized.target_proposal.is_some());
        assert_eq!(
            deserialized.target_proposal.as_ref().unwrap().content,
            "My proposal"
        );
        assert_eq!(deserialized.competitor_summaries.len(), 2);
        assert_eq!(
            deserialized.previous_round_matrix,
            Some("matrix data".to_string())
        );
        assert!(deserialized.previous_own_proposal.is_some());
        assert!((deserialized.previous_own_score.unwrap() - 0.85).abs() < f32::EPSILON);
        assert_eq!(deserialized.previous_critiques.len(), 1);
        assert_eq!(deserialized.scratchpad, Some("my scratchpad".to_string()));
        assert_eq!(deserialized.candidates.len(), 1);
        assert_eq!(deserialized.user_injections.len(), 1);
        assert_eq!(deserialized.user_tools.len(), 1);
        assert!((deserialized.phase_budget_remaining_secs - 42.5).abs() < f64::EPSILON);
        assert_eq!(deserialized.session_id, Some("sess-123".to_string()));
        assert!(deserialized.structured_feedback.is_some());
        assert_eq!(
            deserialized
                .structured_feedback
                .as_ref()
                .unwrap()
                .evaluator_count,
            2
        );
        // Skipped fields should be None after deserialization
        assert!(deserialized.store.is_none());
        assert!(deserialized.user_tool_handler.is_none());
        // Role fields roundtrip
        assert_eq!(deserialized.role, Some("security".to_string()));
        assert_eq!(
            deserialized.role_context,
            Some("Per-role context content".to_string())
        );
        assert_eq!(deserialized.task_publish_ts, Some(1_776_790_000_000));
    }

    #[test]
    fn agent_context_with_defaults() {
        // Provide the required (non-#[serde(default)]) fields; all #[serde(default)]
        // fields (candidates, user_injections, user_tools, phase_budget_remaining_secs,
        // session_id, structured_feedback) should get their defaults.
        let json = r#"{
            "task_description": "",
            "round_number": 0,
            "total_rounds": 0,
            "phase": "Proposing",
            "target_proposal": null,
            "competitor_summaries": [],
            "previous_round_matrix": null,
            "previous_own_proposal": null,
            "previous_own_score": null,
            "previous_critiques": [],
            "scratchpad": null
        }"#;
        let ctx: AgentContext = serde_json::from_str(json).unwrap();
        assert_eq!(ctx.task_description, "");
        assert_eq!(ctx.round_number, 0);
        assert_eq!(ctx.total_rounds, 0);
        assert_eq!(ctx.phase, DeliberationPhase::Proposing);
        assert!(ctx.target_proposal.is_none());
        assert!(ctx.competitor_summaries.is_empty());
        assert!(ctx.previous_round_matrix.is_none());
        assert!(ctx.previous_own_proposal.is_none());
        assert!(ctx.previous_own_score.is_none());
        assert!(ctx.previous_critiques.is_empty());
        assert!(ctx.scratchpad.is_none());
        assert!(ctx.store.is_none());
        // These fields have #[serde(default)] so they should get defaults when omitted
        assert!(ctx.candidates.is_empty());
        assert!(ctx.user_injections.is_empty());
        assert!(ctx.user_tools.is_empty());
        assert!((ctx.phase_budget_remaining_secs - 0.0).abs() < f64::EPSILON);
        assert!(ctx.session_id.is_none());
        assert!(ctx.structured_feedback.is_none());
        assert!(ctx.user_tool_handler.is_none());
        assert!(ctx.task_publish_ts.is_none());
    }

    // =========================================================================
    // CandidateProposal serde tests
    // =========================================================================

    #[test]
    fn candidate_proposal_serde_roundtrip() {
        let cp = CandidateProposal {
            id: "agent-42".to_string(),
            proposal: Proposal {
                thought_process: "I considered many options".to_string(),
                content: "Use approach X".to_string(),
                final_scratchpad: Some("final notes".to_string()),
                token_usage_stats: Some(TokenUsage {
                    input_tokens: 200,
                    output_tokens: 80,
                }),
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&cp).unwrap();
        let deserialized: CandidateProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "agent-42");
        assert_eq!(deserialized.proposal.content, "Use approach X");
        assert_eq!(
            deserialized.proposal.thought_process,
            "I considered many options"
        );
        assert_eq!(
            deserialized.proposal.final_scratchpad,
            Some("final notes".to_string())
        );
        assert_eq!(
            deserialized
                .proposal
                .token_usage_stats
                .as_ref()
                .unwrap()
                .input_tokens,
            200
        );
    }

    // =========================================================================
    // Proposal serde tests
    // =========================================================================

    #[test]
    fn proposal_with_all_fields_roundtrip() {
        let p = Proposal {
            thought_process: "Deep analysis".to_string(),
            content: "The solution is 42".to_string(),
            final_scratchpad: Some("scratch notes".to_string()),
            token_usage_stats: Some(TokenUsage {
                input_tokens: 500,
                output_tokens: 150,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        let deserialized: Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.thought_process, "Deep analysis");
        assert_eq!(deserialized.content, "The solution is 42");
        assert_eq!(
            deserialized.final_scratchpad,
            Some("scratch notes".to_string())
        );
        let tu = deserialized.token_usage_stats.unwrap();
        assert_eq!(tu.input_tokens, 500);
        assert_eq!(tu.output_tokens, 150);
    }

    #[test]
    fn proposal_defaults_and_skip_serializing() {
        let p = Proposal::default();
        assert_eq!(p.thought_process, "");
        assert_eq!(p.content, "");
        assert!(p.final_scratchpad.is_none());
        assert!(p.token_usage_stats.is_none());
        assert_eq!(p.published_at_ms, 0);

        let json = serde_json::to_string(&p).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        // None fields with skip_serializing_if should be absent
        assert!(val.get("final_scratchpad").is_none());
        assert!(val.get("token_usage_stats").is_none());
    }

    /// Old payloads (pre-published_at_ms) deserialize cleanly with the
    /// field defaulted to `0`. This is the backwards-compat contract
    /// the orchestrator depends on for `submission_received.agent_publish_ts`.
    #[test]
    fn proposal_published_at_ms_defaults_to_zero_for_legacy_payload() {
        let legacy = r#"{"thought_process":"old","content":"old"}"#;
        let p: Proposal = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.published_at_ms, 0);
    }

    /// Set + roundtrip. Field appears in JSON (no skip_if=0) so the
    /// orchestrator can distinguish "agent populated 0 explicitly"
    /// from "field absent" only via the JSON shape.
    #[test]
    fn proposal_published_at_ms_roundtrips_nonzero() {
        let p = Proposal {
            published_at_ms: 1_776_790_692_747,
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"published_at_ms\":1776790692747"));
        let back: Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(back.published_at_ms, 1_776_790_692_747);
    }

    /// Same backwards-compat contract for `Evaluation`.
    #[test]
    fn evaluation_published_at_ms_defaults_to_zero_for_legacy_payload() {
        let legacy = r#"{"score":0.5,"justification":"old"}"#;
        let e: Evaluation = serde_json::from_str(legacy).unwrap();
        assert_eq!(e.published_at_ms, 0);
    }

    #[test]
    fn evaluation_published_at_ms_roundtrips_nonzero() {
        let e = Evaluation {
            published_at_ms: 1_776_790_692_999,
            ..Default::default()
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"published_at_ms\":1776790692999"));
        let back: Evaluation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.published_at_ms, 1_776_790_692_999);
    }

    // =========================================================================
    // TokenUsage serde tests
    // =========================================================================

    #[test]
    fn token_usage_serde_and_defaults() {
        let tu = TokenUsage {
            input_tokens: 1234,
            output_tokens: 567,
        };
        let json = serde_json::to_string(&tu).unwrap();
        let deserialized: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.input_tokens, 1234);
        assert_eq!(deserialized.output_tokens, 567);

        let default_tu = TokenUsage::default();
        assert_eq!(default_tu.input_tokens, 0);
        assert_eq!(default_tu.output_tokens, 0);
    }

    // =========================================================================
    // HeuristicTokenEstimator tests
    // =========================================================================

    #[test]
    fn heuristic_estimator_default_chars_per_token() {
        let estimator = HeuristicTokenEstimator::default();
        assert!((estimator.chars_per_token - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn heuristic_estimator_empty_string() {
        let estimator = HeuristicTokenEstimator::default();
        assert_eq!(estimator.estimate_tokens(""), 0);
    }

    #[test]
    fn heuristic_estimator_ascii_text() {
        let estimator = HeuristicTokenEstimator::default();
        // "hello world" = 11 chars, 11/4.0 = 2.75, ceil = 3
        assert_eq!(estimator.estimate_tokens("hello world"), 3);
    }

    #[test]
    fn heuristic_estimator_cjk_text() {
        let estimator = HeuristicTokenEstimator::default();
        // "你好世界" = 4 Unicode chars, 4/4.0 = 1.0, ceil = 1
        assert_eq!(estimator.estimate_tokens("你好世界"), 1);
    }

    #[test]
    fn heuristic_estimator_emoji() {
        let estimator = HeuristicTokenEstimator::default();
        // Two emoji chars, 2/4.0 = 0.5, ceil = 1
        assert_eq!(estimator.estimate_tokens("\u{1F389}\u{1F38A}"), 1);
    }

    #[test]
    fn heuristic_estimator_custom_chars_per_token() {
        let estimator = HeuristicTokenEstimator {
            chars_per_token: 1.5,
        };
        // "hello" = 5 chars, 5/1.5 = 3.333..., ceil = 4
        assert_eq!(estimator.estimate_tokens("hello"), 4);
    }

    #[test]
    fn heuristic_estimator_zero_chars_per_token() {
        let estimator = HeuristicTokenEstimator {
            chars_per_token: 0.0,
        };
        assert_eq!(estimator.estimate_tokens("hello"), 0);
    }

    #[test]
    fn heuristic_estimator_negative_chars_per_token() {
        let estimator = HeuristicTokenEstimator {
            chars_per_token: -2.0,
        };
        assert_eq!(estimator.estimate_tokens("hello"), 0);
    }

    // =========================================================================
    // AgentPricingInfo::compute_cost() tests
    // =========================================================================

    #[test]
    fn pricing_zero_tokens() {
        let pricing = AgentPricingInfo {
            input_price_per_mtok: 10.0,
            output_price_per_mtok: 30.0,
        };
        assert!((pricing.compute_cost(0, 0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pricing_standard_calculation() {
        let pricing = AgentPricingInfo {
            input_price_per_mtok: 10.0,
            output_price_per_mtok: 30.0,
        };
        // (1000*10 + 500*30) / 1_000_000 = (10_000 + 15_000) / 1_000_000 = 0.025
        let cost = pricing.compute_cost(1000, 500);
        assert!((cost - 0.025).abs() < 1e-10);
    }

    #[test]
    fn pricing_zero_prices() {
        let pricing = AgentPricingInfo {
            input_price_per_mtok: 0.0,
            output_price_per_mtok: 0.0,
        };
        assert!((pricing.compute_cost(1000, 500) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pricing_large_token_counts() {
        let pricing = AgentPricingInfo {
            input_price_per_mtok: 15.0,
            output_price_per_mtok: 60.0,
        };
        // (1_000_000 * 15 + 500_000 * 60) / 1_000_000 = 15 + 30 = 45.0
        let cost = pricing.compute_cost(1_000_000, 500_000);
        assert!((cost - 45.0).abs() < 1e-10);
    }

    #[test]
    fn pricing_default_is_zero() {
        let pricing = AgentPricingInfo::default();
        assert!((pricing.input_price_per_mtok - 0.0).abs() < f64::EPSILON);
        assert!((pricing.output_price_per_mtok - 0.0).abs() < f64::EPSILON);
        assert!((pricing.compute_cost(1000, 1000) - 0.0).abs() < f64::EPSILON);
    }

    // =========================================================================
    // calculate_qv_from_fraction() tests
    // =========================================================================

    #[test]
    fn qv_from_fraction_full() {
        assert!((calculate_qv_from_fraction(1.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_from_fraction_quarter() {
        // √0.25 = 0.5
        assert!((calculate_qv_from_fraction(0.25) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_from_fraction_zero() {
        assert!((calculate_qv_from_fraction(0.0) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_from_fraction_clamps_above_one() {
        assert!((calculate_qv_from_fraction(2.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_from_fraction_full_negative() {
        // sign(-1) × √(1.0 × 100) / 10 = -1.0
        assert!((calculate_qv_from_fraction(-1.0) - (-1.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_from_fraction_negative_quarter() {
        // sign(-0.25) × √(0.25 × 100) / 10 = -0.5
        assert!((calculate_qv_from_fraction(-0.25) - (-0.5)).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_from_fraction_clamps_below_minus_one() {
        // -2.0 clamped to -1.0 → sign(-1) × √(1.0 × 100) / 10 = -1.0
        assert!((calculate_qv_from_fraction(-2.0) - (-1.0)).abs() < f32::EPSILON);
    }

    // =========================================================================
    // calculate_qv_score() tests
    // =========================================================================

    #[test]
    fn qv_score_full_weight() {
        // raw=100, total=100 → total<=100 so normalized=clamp(100,0,100)=100
        // influence = sqrt(100)/10 = 10/10 = 1.0
        let (influence, normalized) = calculate_qv_score(100.0, 100.0);
        assert!((normalized - 100.0).abs() < f32::EPSILON);
        assert!((influence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_score_quarter_weight() {
        // raw=25, total=100 → total<=100 so normalized=clamp(25,0,100)=25
        // influence = sqrt(25)/10 = 5/10 = 0.5
        let (influence, normalized) = calculate_qv_score(25.0, 100.0);
        assert!((normalized - 25.0).abs() < f32::EPSILON);
        assert!((influence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_score_zero_weight() {
        // raw=0, total=100 → normalized=0, influence=0
        let (influence, normalized) = calculate_qv_score(0.0, 100.0);
        assert!((normalized - 0.0).abs() < f32::EPSILON);
        assert!((influence - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_score_total_equals_raw() {
        // raw=50, total=50 → total<=100 so normalized=clamp(50,0,100)=50
        // influence = sqrt(50)/10
        let (influence, normalized) = calculate_qv_score(50.0, 50.0);
        assert!((normalized - 50.0).abs() < f32::EPSILON);
        let expected_influence = (50.0f32).sqrt() / 10.0;
        assert!((influence - expected_influence).abs() < 1e-6);
    }

    #[test]
    fn qv_score_total_over_100_normalizes() {
        // raw=200, total=200 → total>100 so normalized=(200/200)*100=100, clamped to 100
        // influence = sqrt(100)/10 = 1.0
        let (influence, normalized) = calculate_qv_score(200.0, 200.0);
        assert!((normalized - 100.0).abs() < f32::EPSILON);
        assert!((influence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_score_raw_exceeds_total_when_total_lte_100() {
        // raw=200, total=100 → total<=100 so normalized=clamp(200,0,100)=100
        // influence = sqrt(100)/10 = 1.0
        let (influence, normalized) = calculate_qv_score(200.0, 100.0);
        assert!((normalized - 100.0).abs() < f32::EPSILON);
        assert!((influence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn qv_score_negative_raw_clamped() {
        // raw=-50, total=100 → total<=100 so normalized=clamp(-50,0,100)=0
        // influence = sqrt(0)/10 = 0.0
        let (influence, normalized) = calculate_qv_score(-50.0, 100.0);
        assert!((normalized - 0.0).abs() < f32::EPSILON);
        assert!((influence - 0.0).abs() < f32::EPSILON);
    }

    // =========================================================================
    // UserInjection serde roundtrip
    // =========================================================================

    #[test]
    fn user_injection_serde_roundtrip() {
        let inj = UserInjection {
            message: "Please focus on edge cases".to_string(),
            injected_at_round: 2,
            timestamp: 1700000000,
            priority: InjectionPriority::Urgent,
            tool_changes: Some(ToolChanges {
                add: vec![UserToolDefinition {
                    name: "new_tool".to_string(),
                    description: "A new tool".to_string(),
                    parameters: Some(serde_json::json!({"type": "object"})),
                    strict: None,
                }],
                remove: vec!["old_tool".to_string()],
            }),
        };
        let json = serde_json::to_string(&inj).unwrap();
        let deserialized: UserInjection = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.message, "Please focus on edge cases");
        assert_eq!(deserialized.injected_at_round, 2);
        assert_eq!(deserialized.timestamp, 1700000000);
        assert_eq!(deserialized.priority, InjectionPriority::Urgent);
        let tc = deserialized.tool_changes.unwrap();
        assert_eq!(tc.add.len(), 1);
        assert_eq!(tc.add[0].name, "new_tool");
        assert_eq!(tc.remove, vec!["old_tool"]);
    }

    // =========================================================================
    // AgentHeartbeat serde roundtrip
    // =========================================================================

    #[test]
    fn agent_heartbeat_serde_roundtrip_all_fields() {
        let hb = AgentHeartbeat {
            agent_id: "agent-1".to_string(),
            status: AgentLiveStatus::Busy,
            model_name: "gpt-4".to_string(),
            provider_id: "openai".to_string(),
            current_job: Some("job-42".to_string()),
            uptime_secs: 3600,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            input_price_per_mtok: Some(10.0),
            output_price_per_mtok: Some(30.0),
            chars_per_token: Some(3.5),
            response_sla_secs: Some(120),
            temperature: Some(0.7),
            frequency_penalty: Some(0.1),
            presence_penalty: Some(0.2),
            max_tokens: Some(4096),
            context_window: Some(128000),
            tasks_completed: 50,
            tasks_failed: 2,
            last_error: Some("timeout".to_string()),
            capability_tags: vec!["legal".to_string(), "audit".to_string()],
            description: Some("Legal specialist".to_string()),
            signing_schemes: vec!["eip712".to_string()],
        };
        let json = serde_json::to_string(&hb).unwrap();
        let deserialized: AgentHeartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.agent_id, "agent-1");
        assert_eq!(deserialized.status, AgentLiveStatus::Busy);
        assert_eq!(deserialized.model_name, "gpt-4");
        assert_eq!(deserialized.provider_id, "openai");
        assert_eq!(deserialized.current_job, Some("job-42".to_string()));
        assert_eq!(deserialized.uptime_secs, 3600);
        assert!((deserialized.input_price_per_mtok.unwrap() - 10.0).abs() < f64::EPSILON);
        assert!((deserialized.output_price_per_mtok.unwrap() - 30.0).abs() < f64::EPSILON);
        assert!((deserialized.chars_per_token.unwrap() - 3.5).abs() < f64::EPSILON);
        assert_eq!(deserialized.response_sla_secs, Some(120));
        assert!((deserialized.temperature.unwrap() - 0.7).abs() < f32::EPSILON);
        assert!((deserialized.frequency_penalty.unwrap() - 0.1).abs() < f32::EPSILON);
        assert!((deserialized.presence_penalty.unwrap() - 0.2).abs() < f32::EPSILON);
        assert_eq!(deserialized.max_tokens, Some(4096));
        assert_eq!(deserialized.context_window, Some(128000));
        assert_eq!(deserialized.tasks_completed, 50);
        assert_eq!(deserialized.tasks_failed, 2);
        assert_eq!(deserialized.last_error, Some("timeout".to_string()));
        // New fields
        assert_eq!(deserialized.capability_tags, vec!["legal", "audit"]);
        assert_eq!(
            deserialized.description.as_deref(),
            Some("Legal specialist")
        );
        assert_eq!(deserialized.signing_schemes, vec!["eip712"]);
    }

    #[test]
    fn agent_heartbeat_skip_serializing_none_fields() {
        let hb = AgentHeartbeat::default();
        let json = serde_json::to_string(&hb).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Optional fields with skip_serializing_if = "Option::is_none" should be absent
        assert!(val.get("current_job").is_none());
        assert!(val.get("input_price_per_mtok").is_none());
        assert!(val.get("output_price_per_mtok").is_none());
        assert!(val.get("chars_per_token").is_none());
        assert!(val.get("temperature").is_none());
        assert!(val.get("frequency_penalty").is_none());
        assert!(val.get("presence_penalty").is_none());
        assert!(val.get("max_tokens").is_none());
        assert!(val.get("context_window").is_none());
        assert!(val.get("last_error").is_none());
        assert!(val.get("response_sla_secs").is_none());
    }

    // =========================================================================
    // OrchestratorPing serde roundtrip
    // =========================================================================

    #[test]
    fn orchestrator_ping_serde_roundtrip() {
        let ping = OrchestratorPing {
            orchestrator_id: "orch-1".to_string(),
            timestamp: "2025-06-01T12:00:00Z".to_string(),
            uptime_secs: 7200,
        };
        let json = serde_json::to_string(&ping).unwrap();
        let deserialized: OrchestratorPing = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.orchestrator_id, "orch-1");
        assert_eq!(deserialized.timestamp, "2025-06-01T12:00:00Z");
        assert_eq!(deserialized.uptime_secs, 7200);
    }

    // =========================================================================
    // PendingToolCall serde roundtrip
    // =========================================================================

    #[test]
    fn pending_tool_call_serde_roundtrip() {
        let ptc = PendingToolCall {
            call_id: "call-abc".to_string(),
            job_id: "job-xyz".to_string(),
            agent_id: "agent-1".to_string(),
            tool_name: "user_dm_user".to_string(),
            arguments: serde_json::json!({"message": "hello"}),
            round: 2,
            phase: DeliberationPhase::Proposing,
            status: ToolCallStatus::Pending,
            created_at: 1700000000000,
            responded_at: None,
            result: None,
        };
        let json = serde_json::to_string(&ptc).unwrap();
        let deserialized: PendingToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.call_id, "call-abc");
        assert_eq!(deserialized.job_id, "job-xyz");
        assert_eq!(deserialized.agent_id, "agent-1");
        assert_eq!(deserialized.tool_name, "user_dm_user");
        assert_eq!(deserialized.arguments["message"], "hello");
        assert_eq!(deserialized.round, 2);
        assert_eq!(deserialized.phase, DeliberationPhase::Proposing);
        assert_eq!(deserialized.status, ToolCallStatus::Pending);
        assert_eq!(deserialized.created_at, 1700000000000);
        assert!(deserialized.responded_at.is_none());
        assert!(deserialized.result.is_none());

        // With responded fields
        let ptc_responded = PendingToolCall {
            call_id: "call-def".to_string(),
            job_id: "job-xyz".to_string(),
            agent_id: "agent-2".to_string(),
            tool_name: "user_read_file".to_string(),
            arguments: serde_json::json!({"path": "/tmp/test"}),
            round: 1,
            phase: DeliberationPhase::Evaluating,
            status: ToolCallStatus::Responded,
            created_at: 1700000000000,
            responded_at: Some(1700000001000),
            result: Some("file contents here".to_string()),
        };
        let json2 = serde_json::to_string(&ptc_responded).unwrap();
        let des2: PendingToolCall = serde_json::from_str(&json2).unwrap();
        assert_eq!(des2.status, ToolCallStatus::Responded);
        assert_eq!(des2.responded_at, Some(1700000001000));
        assert_eq!(des2.result, Some("file contents here".to_string()));
    }

    // =========================================================================
    // ToolCallStatus serde tests
    // =========================================================================

    #[test]
    fn tool_call_status_serde_all_variants() {
        for (variant, expected_default) in [
            (ToolCallStatus::Pending, true),
            (ToolCallStatus::Responded, false),
            (ToolCallStatus::Expired, false),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: ToolCallStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
            if expected_default {
                assert_eq!(ToolCallStatus::default(), variant);
            }
        }
    }

    // =========================================================================
    // AgentLiveStatus serde tests
    // =========================================================================

    #[test]
    fn agent_live_status_serde() {
        // Idle variant
        let idle_json = serde_json::to_string(&AgentLiveStatus::Idle).unwrap();
        assert_eq!(idle_json, "\"idle\"");
        let idle: AgentLiveStatus = serde_json::from_str(&idle_json).unwrap();
        assert_eq!(idle, AgentLiveStatus::Idle);

        // Busy variant
        let busy_json = serde_json::to_string(&AgentLiveStatus::Busy).unwrap();
        assert_eq!(busy_json, "\"busy\"");
        let busy: AgentLiveStatus = serde_json::from_str(&busy_json).unwrap();
        assert_eq!(busy, AgentLiveStatus::Busy);

        // Default is Idle
        assert_eq!(AgentLiveStatus::default(), AgentLiveStatus::Idle);
    }

    // =========================================================================
    // DeliberationPhase serde tests
    // =========================================================================

    #[test]
    fn deliberation_phase_serde_all_variants() {
        let variants = [
            DeliberationPhase::Proposing,
            DeliberationPhase::Evaluating,
            DeliberationPhase::ConsensusCheck,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let deserialized: DeliberationPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    #[test]
    fn deliberation_phase_default() {
        assert_eq!(DeliberationPhase::default(), DeliberationPhase::Proposing);
    }

    // =========================================================================
    // UserToolDefinition serde roundtrip
    // =========================================================================

    #[test]
    fn user_tool_definition_with_parameters() {
        let tool = UserToolDefinition {
            name: "search_db".to_string(),
            description: "Search the database".to_string(),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"]
            })),
            strict: Some(true),
        };
        let json = serde_json::to_string(&tool).unwrap();
        let deserialized: UserToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "search_db");
        assert_eq!(deserialized.description, "Search the database");
        assert!(deserialized.parameters.is_some());
        let params = deserialized.parameters.unwrap();
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"]["query"]["type"], "string");
        assert_eq!(deserialized.strict, Some(true));
    }

    #[test]
    fn user_tool_definition_without_parameters() {
        let tool = UserToolDefinition {
            name: "ping".to_string(),
            description: "Ping the server".to_string(),
            parameters: None,
            strict: None,
        };
        let json = serde_json::to_string(&tool).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        // None fields with skip_serializing_if should be absent
        assert!(val.get("parameters").is_none());
        assert!(val.get("strict").is_none());

        let deserialized: UserToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "ping");
        assert!(deserialized.parameters.is_none());
        assert!(deserialized.strict.is_none());
    }

    // ── Operator annotation / HITL serde tests ──────────────────────────

    #[test]
    fn test_annotation_type_serde_roundtrip() {
        for variant in [AnnotationType::Comment, AnnotationType::Edit] {
            let json = serde_json::to_string(&variant).unwrap();
            let roundtripped: AnnotationType = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, roundtripped);
        }
        // rename_all = snake_case
        assert_eq!(
            serde_json::to_string(&AnnotationType::Comment).unwrap(),
            "\"comment\""
        );
        assert_eq!(
            serde_json::to_string(&AnnotationType::Edit).unwrap(),
            "\"edit\""
        );
    }

    #[test]
    fn test_operator_annotation_serde_roundtrip() {
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Fixed factual error in claim 3".to_string(),
            timestamp: "2026-03-07T12:00:00Z".to_string(),
            original_content_hash: Some("abc123def456".to_string()),
        };
        let json = serde_json::to_value(&annotation).unwrap();
        let roundtripped: OperatorAnnotation = serde_json::from_value(json).unwrap();
        assert_eq!(annotation, roundtripped);
    }

    #[test]
    fn test_operator_annotation_skip_none_hash() {
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Comment,
            comment: "Looks good".to_string(),
            timestamp: "2026-03-07T12:00:00Z".to_string(),
            original_content_hash: None,
        };
        let json = serde_json::to_value(&annotation).unwrap();
        assert!(
            json.get("original_content_hash").is_none(),
            "None hash should be skipped"
        );
        let roundtripped: OperatorAnnotation = serde_json::from_value(json).unwrap();
        assert_eq!(annotation, roundtripped);
    }

    #[test]
    fn test_proposal_operator_annotations_roundtrip() {
        let proposal = Proposal {
            thought_process: "thinking".to_string(),
            content: "solution".to_string(),
            edited_by: Some("operator".to_string()),
            operator_annotations: vec![
                OperatorAnnotation {
                    annotation_type: AnnotationType::Edit,
                    comment: "Rewrote conclusion".to_string(),
                    timestamp: "2026-03-07T12:00:00Z".to_string(),
                    original_content_hash: Some("deadbeef".to_string()),
                },
                OperatorAnnotation {
                    annotation_type: AnnotationType::Comment,
                    comment: "Approved after edit".to_string(),
                    timestamp: "2026-03-07T12:01:00Z".to_string(),
                    original_content_hash: None,
                },
            ],
            ..Default::default()
        };

        let json = serde_json::to_value(&proposal).unwrap();
        assert_eq!(json["edited_by"], "operator");
        assert_eq!(json["operator_annotations"].as_array().unwrap().len(), 2);

        let roundtripped: Proposal = serde_json::from_value(json).unwrap();
        assert_eq!(roundtripped.edited_by, Some("operator".to_string()));
        assert_eq!(roundtripped.operator_annotations.len(), 2);
        assert_eq!(
            roundtripped.operator_annotations[0].annotation_type,
            AnnotationType::Edit
        );
    }

    #[test]
    fn test_proposal_without_annotations_skips_fields() {
        let proposal = Proposal::default();
        let json = serde_json::to_value(&proposal).unwrap();
        assert!(
            json.get("operator_annotations").is_none(),
            "empty vec should be skipped"
        );
        assert!(
            json.get("edited_by").is_none(),
            "None edited_by should be skipped"
        );
    }

    #[test]
    fn test_evaluation_operator_annotations_roundtrip() {
        let eval = Evaluation {
            justification: "Good proposal".to_string(),
            score: 0.85,
            edited_by: Some("operator".to_string()),
            operator_annotations: vec![OperatorAnnotation {
                annotation_type: AnnotationType::Comment,
                comment: "Score adjusted after review".to_string(),
                timestamp: "2026-03-07T14:00:00Z".to_string(),
                original_content_hash: None,
            }],
            ..Default::default()
        };

        let json = serde_json::to_value(&eval).unwrap();
        let roundtripped: Evaluation = serde_json::from_value(json).unwrap();
        assert_eq!(roundtripped.edited_by, Some("operator".to_string()));
        assert_eq!(roundtripped.operator_annotations.len(), 1);
        assert_eq!(
            roundtripped.operator_annotations[0].comment,
            "Score adjusted after review"
        );
    }

    // ── OperatorAnnotation::validate() tests ────────────────────────────

    #[test]
    fn test_edit_annotation_with_hash_validates() {
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Fixed error".to_string(),
            timestamp: "2026-03-11T00:00:00Z".to_string(),
            original_content_hash: Some("abc123".to_string()),
        };
        assert!(annotation.validate().is_ok());
    }

    #[test]
    fn test_edit_annotation_without_hash_fails() {
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Fixed error".to_string(),
            timestamp: "2026-03-11T00:00:00Z".to_string(),
            original_content_hash: None,
        };
        let err = annotation.validate().unwrap_err();
        assert!(err.contains("original_content_hash"));
    }

    #[test]
    fn test_edit_annotation_with_empty_hash_fails() {
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Fixed error".to_string(),
            timestamp: "2026-03-11T00:00:00Z".to_string(),
            original_content_hash: Some(String::new()),
        };
        assert!(annotation.validate().is_err());
    }

    #[test]
    fn test_comment_annotation_without_hash_validates() {
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Comment,
            comment: "Looks good".to_string(),
            timestamp: "2026-03-11T00:00:00Z".to_string(),
            original_content_hash: None,
        };
        assert!(annotation.validate().is_ok());
    }

    #[test]
    fn test_deserialized_edit_without_hash_still_deserializes() {
        // Backward compat: deserialization succeeds, validation is separate
        let json = serde_json::json!({
            "annotation_type": "edit",
            "comment": "old data",
            "timestamp": "2026-01-01T00:00:00Z"
        });
        let annotation: OperatorAnnotation = serde_json::from_value(json).unwrap();
        assert_eq!(annotation.annotation_type, AnnotationType::Edit);
        assert!(annotation.original_content_hash.is_none());
        // Validation fails — but deserialization succeeded (backward compat)
        assert!(annotation.validate().is_err());
    }

    // =========================================================================
    // normalize_score tests
    // =========================================================================

    #[test]
    fn normalize_score_identity_when_total_is_one() {
        assert!((normalize_score(0.8, 1.0) - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_score_divides_by_total() {
        let result = normalize_score(0.8, 100.0);
        assert!((result - 0.008).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_score_clamps_above_one() {
        assert!((normalize_score(2.0, 1.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_score_preserves_negative() {
        // Signed weights: -1.0 / 1.0 = -1.0 (opposition is valid)
        assert!((normalize_score(-1.0, 1.0) - (-1.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_score_clamps_below_minus_one() {
        // -3.0 / 1.0 = -3.0 → clamped to -1.0
        assert!((normalize_score(-3.0, 1.0) - (-1.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_score_zero_total_returns_zero() {
        assert!((normalize_score(5.0, 0.0) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_score_equal_weights() {
        assert!((normalize_score(50.0, 100.0) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_score_negative_half() {
        // -50.0 / 100.0 = -0.5 (opposition with half budget)
        assert!((normalize_score(-50.0, 100.0) - (-0.5)).abs() < f32::EPSILON);
    }

    #[test]
    fn normalize_score_mixed_sign_total_is_abs_sum() {
        // Evaluator gives +60 to A, -40 to B → Σ|w| = 100
        // normalize(+60, 100) = 0.6, normalize(-40, 100) = -0.4
        assert!((normalize_score(60.0, 100.0) - 0.6).abs() < f32::EPSILON);
        assert!((normalize_score(-40.0, 100.0) - (-0.4)).abs() < f32::EPSILON);
    }

    // =========================================================================
    // AgentContext::telemetry_for
    // =========================================================================

    fn ctx_with_session(session_id: Option<&str>) -> AgentContext {
        AgentContext {
            agent_id: "alice".into(),
            session_id: session_id.map(|s| s.to_string()),
            round_number: 3,
            phase: DeliberationPhase::Evaluating,
            ..Default::default()
        }
    }

    #[test]
    fn task_prefers_messages_else_falls_back_to_legacy_strings() {
        use crate::conversation::Message;
        // Native path: the messages array renders per resume state.
        let mut ctx = ctx_with_session(Some("thread-x"));
        ctx.messages = vec![
            Message::user("t1"),
            Message::noosphera("a1"),
            Message::user("t2 newest"),
        ];
        assert_eq!(
            ctx.task(false),
            "[user] t1\n\n[assistant] a1\n\n[user] t2 newest"
        );
        assert_eq!(ctx.task(true), "t2 newest"); // resumed → newest only
        // Legacy path: no messages → the flat `task_description`, regardless of resume.
        let mut legacy = ctx_with_session(Some("thread-y"));
        legacy.task_description = "FULL HISTORY".into();
        assert_eq!(legacy.task(false), "FULL HISTORY");
        assert_eq!(legacy.task(true), "FULL HISTORY");
    }

    #[test]
    fn claude_session_key_prefers_conversation_over_session() {
        // No conversation_id → falls back to session_id (thread-TUI / native path).
        let mut ctx = ctx_with_session(Some("room-turn-1"));
        assert_eq!(ctx.claude_session_key(), Some("room-turn-1"));
        // A thread key overrides the per-turn room so successive turns resume.
        ctx.conversation_id = Some("thread-abc".into());
        assert_eq!(ctx.claude_session_key(), Some("thread-abc"));
        // Two turns of one thread (different rooms, same conversation) → same key.
        let mut turn2 = ctx_with_session(Some("room-turn-2"));
        turn2.conversation_id = Some("thread-abc".into());
        assert_eq!(ctx.claude_session_key(), turn2.claude_session_key());
    }

    /// Happy path: session_id present → context derives a
    /// TelemetryContext that round-trips the (agent, job, round, phase)
    /// tuple into the AgentEventCommon envelope.
    #[test]
    fn telemetry_for_with_session_populates_envelope() {
        let context = ctx_with_session(Some("job-abc"));
        let tel = context.telemetry_for();
        let common = tel.common();
        assert_eq!(common.agent_id, "alice");
        assert_eq!(common.job_id.as_deref(), Some("job-abc"));
        assert_eq!(common.round, Some(3));
        assert_eq!(common.phase, Some(DeliberationPhase::Evaluating));
        // trace_id is the 32-char (128-bit) hex from derive_trace_id.
        assert_eq!(common.trace_id.len(), 32);
        assert!(common.trace_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Same task hits telemetry_for twice → both envelopes share the
    /// same trace_id (deterministic derivation, not per-call uuid).
    #[test]
    fn telemetry_for_is_deterministic_on_same_session() {
        let context = ctx_with_session(Some("job-abc"));
        let a = context.telemetry_for().common().trace_id;
        let b = context.telemetry_for().common().trace_id;
        assert_eq!(a, b);
    }

    /// Missing session → panic. The orchestrator establishes the
    /// invariant at dispatch; emitting telemetry from a session-less
    /// context is a programmer error and should fail loudly rather
    /// than synthesise a fake trace_id that breaks dashboard joins.
    #[test]
    #[should_panic(expected = "session_id")]
    fn telemetry_for_panics_without_session() {
        let context = ctx_with_session(None);
        let _ = context.telemetry_for();
    }

    /// Empty-string session_id is treated the same as None — same
    /// "no real session" condition, same panic.
    #[test]
    #[should_panic(expected = "session_id")]
    fn telemetry_for_panics_on_empty_session() {
        let context = ctx_with_session(Some(""));
        let _ = context.telemetry_for();
    }
}
