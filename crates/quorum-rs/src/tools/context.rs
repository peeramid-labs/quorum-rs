//! This module implements the read-only context tools that are provided by the
//! orchestrator to the agents.
//!
//! These tools allow agents to inspect the history and state of the deliberation
//! process, such as retrieving past proposals and checking peer critiques.
//! This capability functions as a lightweight, on-demand Retrieval-Augmented Generation (RAG) system.

use crate::agents::{CandidateProposal, ClaimVerdict, PersistenceStore, Proposal, Stance};
use crate::tools::Tool;

use async_openai::types::{ChatCompletionTool, ChatCompletionToolType, FunctionObject};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error;
use std::sync::Arc;

// -----------------------------------------------------------------------------
// ReadProposalTool Implementation
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ReadProposalTool {
    store: Arc<dyn PersistenceStore>,
    current_round: u32,
    /// Current-round candidates AS SHOWN to the evaluator — keyed by their
    /// anonymized id (`Candidate_A`, …). These are inlined in the eval prompt but
    /// live only here at eval time: the persistence store keys real author ids, so
    /// a `read_proposal(current_round, "Candidate_X")` (the truncation hint) can't
    /// resolve them from the store. Serving from this set fulfils that hint with the
    /// full thought_process — the SAME anonymized content, no deanonymization.
    candidates: Vec<CandidateProposal>,
}

impl ReadProposalTool {
    pub fn new(store: Arc<dyn PersistenceStore>, current_round: u32) -> Self {
        Self {
            store,
            current_round,
            candidates: Vec::new(),
        }
    }

    /// Attach the current-round anonymized candidates so `read_proposal` can serve
    /// them by their `Candidate_X` id (see the field doc).
    pub fn with_candidates(mut self, candidates: Vec<CandidateProposal>) -> Self {
        self.candidates = candidates;
        self
    }
}

/// Render a proposal to the paginated `<proposal>` tool output. Shared by the
/// current-round candidate branch and the store-history branch so both format
/// identically.
fn render_proposal(
    round: u32,
    agent_id: &str,
    proposal: &Proposal,
    offset: usize,
    limit: usize,
) -> String {
    let thoughts = &proposal.thought_process;
    let total_chars = thoughts.chars().count();
    if offset >= total_chars {
        return format!(
            "Offset {offset} exceeds thought_process length {total_chars} chars. No more content."
        );
    }
    let snippet: String = thoughts.chars().skip(offset).take(limit).collect();
    let snippet_len = snippet.chars().count();
    let remaining = total_chars.saturating_sub(offset + snippet_len);
    let mut output = format!(
        "<proposal round=\"{}\" agent=\"{}\">\n<final_solution>{}</final_solution>\n\n<thought_process offset=\"{}\" length=\"{}\" total=\"{}\">\n{}\n",
        round, agent_id, proposal.content, offset, snippet_len, total_chars, snippet
    );
    if remaining > 0 {
        output.push_str(&format!(
            "\n... ({} characters remaining. Call read_proposal with offset={} to read more.)",
            remaining,
            offset + snippet_len
        ));
    }
    output.push_str("</thought_process>\n</proposal>");
    output
}

/// Helper function to match requested agent ID against stored ID.
/// Allows for partial matches if the stored ID is "Name (Model)" and request is "Name".
/// Trims whitespace from both IDs to handle LLM tool args with trailing/leading spaces.
fn agent_id_match(stored_id: &str, requested_id: &str) -> bool {
    // Trim both sides to handle whitespace from LLM tool arguments
    let stored_trimmed = stored_id.trim();
    let requested_trimmed = requested_id.trim();

    // 1. Try exact match (case insensitive, after trimming)
    if stored_trimmed.eq_ignore_ascii_case(requested_trimmed) {
        return true;
    }

    // 2. Handle "Name (Model)" format: Check if stored starts with requested + space or paren
    // e.g. stored="Xue (Qwen)", requested="Xue"
    let stored_lower = stored_trimmed.to_lowercase();
    let requested_lower = requested_trimmed.to_lowercase();

    if stored_lower.starts_with(&format!("{requested_lower} ("))
        || stored_lower.starts_with(&format!("{requested_lower} "))
    {
        return true;
    }

    false
}

#[derive(Deserialize, JsonSchema)]
struct ReadProposalArgs {
    /// The round number to retrieve the proposal from. Defaults to the previous round.
    round: Option<u32>,
    /// The ID of the agent who authored the proposal (e.g., "agent_0").
    agent_id: String,
    /// The character offset to start reading from (useful for pagination). Defaults to 0.
    offset: Option<usize>,
    /// The maximum number of characters to return. Defaults to 5000.
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadProposalTool {
    fn name(&self) -> String {
        "read_proposal".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(
                    "[Deprecated — prefer search_deliberation for filtered access] Read the full content of a specific proposal from ANY round (including the current one). Supports pagination for large proposals. If 'round' is omitted, defaults to the previous round.".to_string(),
                ),
                parameters: Some(schemars::schema_for!(ReadProposalArgs).into()),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: ReadProposalArgs = serde_json::from_value(args)?;
        // Defaults to previous round. For Round 1, this clamps to Round 1 (current),
        // allowing access to just-generated proposals if available.
        let target_round = args
            .round
            .unwrap_or_else(|| self.current_round.saturating_sub(1).max(1));
        let offset = args.offset.unwrap_or(0);
        // Enforce hard cap of 15,000 characters to prevent context explosion
        let limit = args.limit.unwrap_or(5000).min(15_000);

        // Current-round anonymized candidates first: the eval prompt inlines them
        // (truncated at 4000 chars) and its truncation hint sends the agent here as
        // `read_proposal(current_round, "Candidate_X")`. The store keys REAL author
        // ids, so it can't resolve that anonymized label — serve it straight from
        // the candidate set instead (the full thought_process, same anonymized
        // content). Without this the hinted call returns "No proposal found", which
        // reads to the agent as a systemic retrieval failure.
        if target_round == self.current_round {
            if let Some(c) = self
                .candidates
                .iter()
                .find(|c| agent_id_match(&c.id, &args.agent_id))
            {
                return Ok(render_proposal(
                    target_round,
                    &args.agent_id,
                    &c.proposal,
                    offset,
                    limit,
                ));
            }
        }

        // Retrieve round history from store
        let history = self
            .store
            .get_round_history(target_round)
            .await
            .map_err(|e| format!("Failed to retrieve history from store: {e}"))?;

        if let Some(records) = history {
            if let Some(record) = records
                .iter()
                .find(|r| agent_id_match(&r.author_agent_id, &args.agent_id))
            {
                Ok(render_proposal(
                    target_round,
                    &args.agent_id,
                    &record.proposal,
                    offset,
                    limit,
                ))
            } else {
                Ok(format!(
                    "No proposal found for agent_id '{}' in round {}.",
                    args.agent_id, target_round
                ))
            }
        } else {
            Ok(format!(
                "No history found for round {target_round}. If this is the current round, it hasn't finished yet. Did you mean to check a previous round?"
            ))
        }
    }
}

// -----------------------------------------------------------------------------
// ReadCritiquesTool Implementation
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ReadCritiquesTool {
    store: Arc<dyn PersistenceStore>,
    current_round: u32,
}

impl ReadCritiquesTool {
    pub fn new(store: Arc<dyn PersistenceStore>, current_round: u32) -> Self {
        Self {
            store,
            current_round,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReadCritiquesArgs {
    /// The round number to retrieve critiques from. Defaults to the previous round.
    round: Option<u32>,
    /// The ID of the agent whose proposal was critiqued.
    target_agent_id: String,
}

#[async_trait]
impl Tool for ReadCritiquesTool {
    fn name(&self) -> String {
        "read_critiques".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(
                    "[Deprecated — prefer search_deliberation with phase='evaluating'] Read all peer critiques/evaluations received by a specific proposal in a previous round. If 'round' is omitted, defaults to the previous round.".to_string(),
                ),
                parameters: Some(schemars::schema_for!(ReadCritiquesArgs).into()),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: ReadCritiquesArgs = serde_json::from_value(args)?;
        // Defaults to previous round.
        let target_round = args
            .round
            .unwrap_or_else(|| self.current_round.saturating_sub(1).max(1));

        // Retrieve round history from store
        let history = self
            .store
            .get_round_history(target_round)
            .await
            .map_err(|e| format!("Failed to retrieve history from store: {e}"))?;

        if let Some(records) = history {
            if let Some(record) = records
                .iter()
                .find(|r| agent_id_match(&r.author_agent_id, &args.target_agent_id))
            {
                if record.evaluations.is_empty() {
                    return Ok(format!(
                        "No evaluations found for agent_id '{}' in round {}.",
                        args.target_agent_id, target_round
                    ));
                }

                let mut output = format!(
                    "<critiques target=\"{}\" round=\"{}\">\n\n",
                    args.target_agent_id, target_round
                );

                for eval_record in &record.evaluations {
                    output.push_str(&format!(
                        "<critique reviewer=\"{}\" score=\"{}\">\n{}\n</critique>\n\n",
                        eval_record.evaluator_agent_id,
                        eval_record.evaluation.score,
                        eval_record.evaluation.justification
                    ));
                }
                output.push_str("</critiques>");

                Ok(output)
            } else {
                Ok(format!(
                    "No proposal found for agent_id '{}' in round {}.",
                    args.target_agent_id, target_round
                ))
            }
        } else {
            Ok(format!(
                "No history found for round {target_round}. If this is the current round, evaluations haven't happened yet. Did you mean to check critiques from a previous round?"
            ))
        }
    }
}

// -----------------------------------------------------------------------------
// ReadOwnProposalTool Implementation
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ReadOwnProposalTool {
    store: Arc<dyn PersistenceStore>,
    round: u32,
    agent_id: String,
}

impl ReadOwnProposalTool {
    pub fn new(store: Arc<dyn PersistenceStore>, round: u32, agent_id: String) -> Self {
        Self {
            store,
            round,
            agent_id,
        }
    }
}

// -----------------------------------------------------------------------------
// SearchDeliberationTool Implementation
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SearchDeliberationTool {
    store: Arc<dyn PersistenceStore>,
    current_round: u32,
    /// Current-round candidates as shown to the evaluator, keyed by anonymized id.
    /// The store has no current-round records during evaluation ("hasn't finished
    /// yet"), so a search scoped to the current round can only surface these from
    /// here — same reason [`ReadProposalTool`] carries them.
    candidates: Vec<CandidateProposal>,
}

impl SearchDeliberationTool {
    pub fn new(store: Arc<dyn PersistenceStore>, current_round: u32) -> Self {
        Self {
            store,
            current_round,
            candidates: Vec::new(),
        }
    }

    /// Attach the current-round anonymized candidates (see the field doc).
    pub fn with_candidates(mut self, candidates: Vec<CandidateProposal>) -> Self {
        self.candidates = candidates;
        self
    }
}

/// Structured filters for narrowing deliberation search results.
#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct SearchFilters {
    /// Filter by specific round numbers.
    #[serde(default)]
    pub rounds: Vec<u32>,
    /// Filter by specific agent IDs.
    #[serde(default)]
    pub agent_ids: Vec<String>,
    /// Filter by phase: "proposing" or "evaluating".
    #[serde(default)]
    pub phase: Option<String>,
    /// Filter evaluations by claim verdict (verified, contested, unverified, wrong).
    #[serde(default)]
    pub verdicts: Vec<String>,
    /// Filter evaluations by stance (strong_agree, agree, neutral, disagree, strong_disagree).
    #[serde(default)]
    pub stances: Vec<String>,
    /// Minimum aggregated score filter.
    #[serde(default)]
    pub min_score: Option<f32>,
    /// Maximum aggregated score filter.
    #[serde(default)]
    pub max_score: Option<f32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchDeliberationArgs {
    /// Structured filters to narrow results.
    #[serde(default)]
    filters: SearchFilters,
    /// Keywords to match in proposal content or justifications (AND logic).
    #[serde(default)]
    keywords: Vec<String>,
    /// Max results to return (default: 10).
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    10
}

#[async_trait]
impl Tool for SearchDeliberationTool {
    fn name(&self) -> String {
        "search_deliberation".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: self.name(),
                description: Some(
                    "Search and filter deliberation history. Returns raw data matching your filters. Instant, zero cost. Use for: reading a specific agent's proposal, finding evaluations with a specific verdict, keyword search across rounds. For simple lookups this is preferred over read_proposal/read_critiques.".to_string(),
                ),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "filters": {
                            "type": "object",
                            "properties": {
                                "rounds": { "type": "array", "items": { "type": "integer" } },
                                "agent_ids": { "type": "array", "items": { "type": "string" } },
                                "phase": { "type": ["string", "null"], "enum": ["proposing", "evaluating", null] },
                                "verdicts": { "type": "array", "items": { "type": "string", "enum": ["verified", "contested", "unverified", "wrong"] } },
                                "stances": { "type": "array", "items": { "type": "string", "enum": ["strong_agree", "agree", "neutral", "disagree", "strong_disagree"] } },
                                "min_score": { "type": ["number", "null"] },
                                "max_score": { "type": ["number", "null"] }
                            },
                            "additionalProperties": false
                        },
                        "keywords": { "type": "array", "items": { "type": "string" } },
                        "limit": { "type": "integer" }
                    },
                    "additionalProperties": false
                })),
                strict: None,
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: SearchDeliberationArgs = serde_json::from_value(args)?;
        let filters = &args.filters;

        // Hard cap on results to prevent runaway scans from malformed tool calls.
        const MAX_RESULTS: usize = 50;
        let safe_limit = args.limit.min(MAX_RESULTS);

        // Determine which rounds to search
        let rounds_to_search: Vec<u32> = if filters.rounds.is_empty() {
            (1..=self.current_round).collect()
        } else {
            filters.rounds.clone()
        };

        // Lowercase keywords for case-insensitive matching
        let keywords_lower: Vec<String> = args.keywords.iter().map(|k| k.to_lowercase()).collect();

        let mut results = Vec::new();
        // Normalize phase to lowercase for case-insensitive matching.
        // Unknown/invalid phase values default to showing both proposals and
        // evaluations rather than silently returning empty results.
        let phase_lower = filters.phase.as_deref().map(|s| s.to_lowercase());
        let want_proposals = phase_lower.is_none()
            || phase_lower.as_deref() == Some("proposing")
            || !matches!(phase_lower.as_deref(), Some("evaluating"));
        let want_evaluations = phase_lower.is_none()
            || phase_lower.as_deref() == Some("evaluating")
            || !matches!(phase_lower.as_deref(), Some("proposing"));

        for round in &rounds_to_search {
            let history = self
                .store
                .get_round_history(*round)
                .await
                .map_err(|e| format!("Failed to retrieve history: {e}"))?;

            let Some(records) = history else { continue };

            for record in &records {
                // Agent ID filter
                if !filters.agent_ids.is_empty()
                    && !filters
                        .agent_ids
                        .iter()
                        .any(|id| agent_id_match(&record.author_agent_id, id))
                {
                    continue;
                }

                // Score filter
                if let Some(min) = filters.min_score {
                    if record.aggregated_score < min {
                        continue;
                    }
                }
                if let Some(max) = filters.max_score {
                    if record.aggregated_score > max {
                        continue;
                    }
                }

                // Emit proposal data if wanted
                if want_proposals {
                    let text = format!(
                        "{} {}",
                        record.proposal.content, record.proposal.thought_process
                    )
                    .to_lowercase();

                    let keyword_match = keywords_lower.is_empty()
                        || keywords_lower.iter().all(|kw| text.contains(kw.as_str()));

                    if keyword_match {
                        results.push(format!(
                            "<proposal round=\"{}\" agent=\"{}\" score=\"{:.2}\">\n<content>{}</content>\n</proposal>",
                            round,
                            record.author_agent_id,
                            record.aggregated_score,
                            truncate_str(&record.proposal.content, 2000),
                        ));
                    }
                }

                // Emit evaluation data if wanted
                if want_evaluations {
                    for eval_rec in &record.evaluations {
                        if results.len() >= safe_limit {
                            break;
                        }
                        let eval = &eval_rec.evaluation;

                        // Verdict filter (case-insensitive)
                        if !filters.verdicts.is_empty() {
                            let has_matching_verdict = eval.claim_assessments.iter().any(|ca| {
                                let v = match ca.verdict {
                                    ClaimVerdict::Verified => "verified",
                                    ClaimVerdict::Contested => "contested",
                                    ClaimVerdict::Unverified => "unverified",
                                    ClaimVerdict::Wrong => "wrong",
                                    ClaimVerdict::Unknown => "unknown",
                                };
                                filters.verdicts.iter().any(|fv| fv.eq_ignore_ascii_case(v))
                            });
                            if !has_matching_verdict {
                                continue;
                            }
                        }

                        // Stance filter (case-insensitive)
                        if !filters.stances.is_empty() {
                            if let Some(ref stance) = eval.stance {
                                let s = match stance {
                                    Stance::StrongAgree => "strong_agree",
                                    Stance::Agree => "agree",
                                    Stance::Neutral => "neutral",
                                    Stance::Disagree => "disagree",
                                    Stance::StrongDisagree => "strong_disagree",
                                };
                                if !filters.stances.iter().any(|fs| fs.eq_ignore_ascii_case(s)) {
                                    continue;
                                }
                            } else {
                                continue; // No stance, skip if filtering by stance
                            }
                        }

                        // Keyword match on justification
                        let text = eval.justification.to_lowercase();
                        let keyword_match = keywords_lower.is_empty()
                            || keywords_lower.iter().all(|kw| text.contains(kw.as_str()));

                        if keyword_match {
                            let mut eval_xml = format!(
                                "<evaluation round=\"{}\" target=\"{}\" evaluator=\"{}\" score=\"{:.2}\"",
                                round,
                                record.author_agent_id,
                                eval_rec.evaluator_agent_id,
                                eval.score,
                            );
                            if let Some(ref stance) = eval.stance {
                                eval_xml.push_str(&format!(" stance=\"{:?}\"", stance));
                            }
                            eval_xml.push_str(&format!(
                                ">\n<justification>{}</justification>",
                                truncate_str(&eval.justification, 1000),
                            ));

                            // Include claim assessments if present
                            if !eval.claim_assessments.is_empty() {
                                eval_xml.push_str("\n<claims>");
                                for ca in &eval.claim_assessments {
                                    eval_xml.push_str(&format!(
                                        "\n  <claim verdict=\"{:?}\"{}>{}</claim>",
                                        ca.verdict,
                                        ca.claim_id
                                            .as_ref()
                                            .map_or(String::new(), |id| format!(" id=\"{id}\"")),
                                        ca.claim,
                                    ));
                                }
                                eval_xml.push_str("\n</claims>");
                            }

                            eval_xml.push_str("\n</evaluation>");
                            results.push(eval_xml);
                        }
                    }
                }

                if results.len() >= safe_limit {
                    break;
                }
            }
            if results.len() >= safe_limit {
                break;
            }
        }

        // Current-round anonymized candidates: not in the store during evaluation,
        // so surface them from the plumbed set when the search covers the current
        // round and wants proposals. Skip when the caller filters on evaluation-only
        // dimensions (verdicts/stances/score) — a not-yet-evaluated candidate has no
        // evaluations to match, so those filters correctly exclude it.
        let candidate_filters_compatible = want_proposals
            && filters.verdicts.is_empty()
            && filters.stances.is_empty()
            && filters.min_score.is_none()
            && filters.max_score.is_none();
        if candidate_filters_compatible && rounds_to_search.contains(&self.current_round) {
            for c in &self.candidates {
                if results.len() >= safe_limit {
                    break;
                }
                if !filters.agent_ids.is_empty()
                    && !filters.agent_ids.iter().any(|id| agent_id_match(&c.id, id))
                {
                    continue;
                }
                let text =
                    format!("{} {}", c.proposal.content, c.proposal.thought_process).to_lowercase();
                let keyword_match = keywords_lower.is_empty()
                    || keywords_lower.iter().all(|kw| text.contains(kw.as_str()));
                if keyword_match {
                    results.push(format!(
                        "<proposal round=\"{}\" agent=\"{}\">\n<content>{}</content>\n</proposal>",
                        self.current_round,
                        c.id,
                        truncate_str(&c.proposal.content, 2000),
                    ));
                }
            }
        }

        results.truncate(safe_limit);

        if results.is_empty() {
            Ok("No results matching your filters.".to_string())
        } else {
            Ok(format!(
                "<search_results count=\"{}\">\n{}\n</search_results>",
                results.len(),
                results.join("\n\n")
            ))
        }
    }
}

/// Truncate a string to max_chars, appending "..." if truncated.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Evaluation, EvaluationRecord, Proposal, ProposalRecord};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[test]
    fn test_agent_id_match() {
        // Exact match (case insensitive)
        assert!(agent_id_match("Agent_1", "Agent_1"));
        assert!(agent_id_match("Agent_1", "agent_1"));
        assert!(agent_id_match("AGENT_1", "Agent_1"));

        // Partial match with parenthesis
        assert!(agent_id_match("Xue (Qwen)", "Xue"));
        assert!(agent_id_match("Xue (Qwen)", "xue"));

        // Partial match with space
        assert!(agent_id_match("Xue Model", "Xue"));

        // Whitespace trimming (common with LLM tool args)
        assert!(agent_id_match("Agent_1", "  Agent_1  "));
        assert!(agent_id_match("  Agent_1  ", "Agent_1"));
        assert!(agent_id_match("Xue (Qwen)", "  Xue  "));
        assert!(agent_id_match("  Xue Model  ", "Xue"));

        // No match
        assert!(!agent_id_match("Xue", "Xu"));
        assert!(!agent_id_match("Agent_2", "Agent_1"));
        assert!(!agent_id_match("Xue (Qwen)", "Qwen")); // We only match prefix
    }

    // Mock PersistenceStore for testing
    #[derive(Debug, Default)]
    struct MockStore {
        round_history: Mutex<HashMap<u32, Vec<ProposalRecord>>>,
    }

    impl MockStore {
        fn with_history(round: u32, records: Vec<ProposalRecord>) -> Self {
            let mut map = HashMap::new();
            map.insert(round, records);
            Self {
                round_history: Mutex::new(map),
            }
        }
    }

    #[async_trait]
    impl PersistenceStore for MockStore {
        async fn get(&self, _key: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn append(&self, _key: &str, _content: &str) -> Result<()> {
            Ok(())
        }
        async fn set(&self, _key: &str, _content: &str) -> Result<()> {
            Ok(())
        }
        async fn get_round_history(&self, round: u32) -> Result<Option<Vec<ProposalRecord>>> {
            let map = self.round_history.lock().unwrap();
            Ok(map.get(&round).cloned())
        }
    }

    fn create_test_record(agent_id: &str, thought: &str, content: &str) -> ProposalRecord {
        ProposalRecord {
            round: 1,
            author_agent_id: agent_id.to_string(),
            proposal: Proposal {
                thought_process: thought.to_string(),
                content: content.to_string(),
                final_scratchpad: None,
                token_usage_stats: None,
                ..Default::default()
            },
            evaluations: vec![EvaluationRecord {
                evaluator_agent_id: "reviewer1".to_string(),
                evaluation: Evaluation {
                    score: 0.8,
                    justification: "Good work!".to_string(),
                    token_usage: None,
                    ..Default::default()
                },
                synthetic: false,
            }],
            aggregated_score: 0.8,
        }
    }

    #[test]
    fn test_read_proposal_tool_name_and_schema() {
        let store = Arc::new(MockStore::default());
        let tool = ReadProposalTool::new(store, 1);

        assert_eq!(tool.name(), "read_proposal");

        let schema = tool.schema();
        assert_eq!(schema.function.name, "read_proposal");
        assert!(schema.function.description.is_some());
        assert!(schema.function.parameters.is_some());
    }

    #[tokio::test]
    async fn test_read_proposal_tool_returns_proposal() {
        let record = create_test_record("agent_1", "My detailed reasoning...", "The answer is 42.");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadProposalTool::new(store, 2);

        let args = serde_json::json!({
            "agent_id": "agent_1",
            "round": 1
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("The answer is 42."));
        assert!(result.contains("My detailed reasoning..."));
        assert!(result.contains("<proposal"));
    }

    #[tokio::test]
    async fn test_read_proposal_tool_not_found() {
        let store = Arc::new(MockStore::default());
        let tool = ReadProposalTool::new(store, 2);

        let args = serde_json::json!({
            "agent_id": "nonexistent",
            "round": 1
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("No history found"));
    }

    #[tokio::test]
    async fn test_read_proposal_tool_pagination() {
        let long_thought = "A".repeat(10000);
        let record = create_test_record("agent_1", &long_thought, "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadProposalTool::new(store, 2);

        // Request with offset and limit
        let args = serde_json::json!({
            "agent_id": "agent_1",
            "round": 1,
            "offset": 1000,
            "limit": 500
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("offset=\"1000\""));
        assert!(result.contains("length=\"500\""));
        assert!(result.contains("characters remaining"));
    }

    #[tokio::test]
    async fn test_read_proposal_tool_offset_exceeds_length() {
        let record = create_test_record("agent_1", "Short", "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadProposalTool::new(store, 2);

        let args = serde_json::json!({
            "agent_id": "agent_1",
            "round": 1,
            "offset": 1000
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Offset 1000 exceeds"));
    }

    #[tokio::test]
    async fn read_proposal_resolves_current_round_anonymized_candidate() {
        // The exact call the truncation hint emits: read_proposal(current_round,
        // "Candidate_B"). The store keys REAL author ids and holds no such record,
        // so this must resolve from the plumbed candidate set — the bug Corepunk03
        // hit was this returning "No proposal found" and looping on ask_user.
        let store = Arc::new(MockStore::default()); // deliberately empty
        let candidate = CandidateProposal {
            id: "Candidate_B".to_string(),
            proposal: Proposal {
                content: "The answer is 42.".to_string(),
                thought_process: "Full un-truncated reasoning for candidate B.".to_string(),
                ..Default::default()
            },
        };
        let tool = ReadProposalTool::new(store, 3).with_candidates(vec![candidate]);

        let args = serde_json::json!({ "agent_id": "Candidate_B", "round": 3 });
        let result = tool.call(args).await.unwrap();
        assert!(
            result.contains("The answer is 42."),
            "serves the candidate's final solution: {result}"
        );
        assert!(
            result.contains("Full un-truncated reasoning for candidate B."),
            "serves the FULL thought_process the truncation hint promised: {result}"
        );
        assert!(!result.contains("No proposal found"));
        assert!(!result.contains("No history found"));
    }

    #[tokio::test]
    async fn read_proposal_anonymized_label_without_candidates_misses() {
        // Regression anchor: with NO candidates plumbed (the old wiring), the store
        // cannot resolve an anonymized label — proving the fix is the plumbing, not
        // agent_id_match. This is the exact failure mode before the fix.
        let store = Arc::new(MockStore::default());
        let tool = ReadProposalTool::new(store, 3);
        let args = serde_json::json!({ "agent_id": "Candidate_B", "round": 3 });
        let result = tool.call(args).await.unwrap();
        assert!(
            result.contains("No history found") || result.contains("No proposal found"),
            "unresolvable anonymized label without the candidate set: {result}"
        );
    }

    #[test]
    fn test_read_critiques_tool_name_and_schema() {
        let store = Arc::new(MockStore::default());
        let tool = ReadCritiquesTool::new(store, 1);

        assert_eq!(tool.name(), "read_critiques");

        let schema = tool.schema();
        assert_eq!(schema.function.name, "read_critiques");
        assert!(schema.function.description.is_some());
    }

    #[tokio::test]
    async fn test_read_critiques_tool_returns_evaluations() {
        let record = create_test_record("agent_1", "Thought", "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadCritiquesTool::new(store, 2);

        let args = serde_json::json!({
            "target_agent_id": "agent_1",
            "round": 1
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Good work!"));
        assert!(result.contains("reviewer1"));
        assert!(result.contains("<critiques"));
    }

    #[tokio::test]
    async fn test_read_critiques_tool_no_evaluations() {
        let mut record = create_test_record("agent_1", "Thought", "Content");
        record.evaluations = vec![]; // No evaluations
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadCritiquesTool::new(store, 2);

        let args = serde_json::json!({
            "target_agent_id": "agent_1",
            "round": 1
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("No evaluations found"));
    }

    #[tokio::test]
    async fn test_read_critiques_tool_agent_not_found() {
        let record = create_test_record("agent_1", "Thought", "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadCritiquesTool::new(store, 2);

        let args = serde_json::json!({
            "target_agent_id": "agent_99",
            "round": 1
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("No proposal found for agent_id"));
    }

    #[test]
    fn test_read_own_proposal_tool_name_and_schema() {
        let store = Arc::new(MockStore::default());
        let tool = ReadOwnProposalTool::new(store, 1, "test_agent".to_string());

        assert_eq!(tool.name(), "read_own_proposal");

        let schema = tool.schema();
        assert_eq!(schema.function.name, "read_own_proposal");
        assert!(schema.function.description.is_some());
    }

    #[tokio::test]
    async fn test_read_own_proposal_tool_returns_own_proposal() {
        let record = create_test_record("test_agent", "My own reasoning", "My answer");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadOwnProposalTool::new(store, 1, "test_agent".to_string());

        let args = serde_json::json!({});

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("My own reasoning"));
        assert!(result.contains("My answer"));
        assert!(result.contains("type=\"own\""));
    }

    #[tokio::test]
    async fn test_read_own_proposal_tool_not_found() {
        let store = Arc::new(MockStore::default());
        let tool = ReadOwnProposalTool::new(store, 1, "test_agent".to_string());

        let args = serde_json::json!({});

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("No history found for round"));
    }

    #[tokio::test]
    async fn test_read_own_proposal_tool_pagination() {
        let long_thought = "B".repeat(8000);
        let record = create_test_record("test_agent", &long_thought, "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadOwnProposalTool::new(store, 1, "test_agent".to_string());

        let args = serde_json::json!({
            "offset": 2000,
            "limit": 3000
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("offset=\"2000\""));
        assert!(result.contains("length=\"3000\""));
        assert!(result.contains("characters remaining"));
    }

    #[tokio::test]
    async fn test_read_proposal_respects_limit_cap() {
        let long_thought = "C".repeat(50000);
        let record = create_test_record("agent_1", &long_thought, "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadProposalTool::new(store, 2);

        // Try to request more than the 15,000 char cap
        let args = serde_json::json!({
            "agent_id": "agent_1",
            "round": 1,
            "limit": 50000
        });

        let result = tool.call(args).await.unwrap();
        // Result should cap at 15,000 chars
        assert!(result.contains("length=\"15000\""));
    }

    #[tokio::test]
    async fn test_read_proposal_defaults_to_previous_round() {
        let record = create_test_record("agent_1", "Round 2 thoughts", "Round 2 answer");
        let store = Arc::new(MockStore::with_history(2, vec![record]));
        let tool = ReadProposalTool::new(store, 3); // Current round is 3

        // Don't specify round - should default to round 2 (current - 1)
        let args = serde_json::json!({
            "agent_id": "agent_1"
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Round 2 thoughts"));
    }

    // =========================================================================
    // SearchDeliberationTool Tests — filter accuracy + retrieval thresholds
    // =========================================================================

    use crate::agents::{ClaimAssessment, ClaimVerdict as Cv, Stance};

    /// Extended MockStore supporting multiple rounds
    #[derive(Debug, Default)]
    struct MultiRoundStore {
        rounds: Mutex<HashMap<u32, Vec<ProposalRecord>>>,
    }

    impl MultiRoundStore {
        fn new(data: Vec<(u32, Vec<ProposalRecord>)>) -> Self {
            let mut map = HashMap::new();
            for (round, records) in data {
                map.insert(round, records);
            }
            Self {
                rounds: Mutex::new(map),
            }
        }
    }

    #[async_trait]
    impl PersistenceStore for MultiRoundStore {
        async fn get(&self, _key: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn append(&self, _key: &str, _content: &str) -> Result<()> {
            Ok(())
        }
        async fn set(&self, _key: &str, _content: &str) -> Result<()> {
            Ok(())
        }
        async fn get_round_history(&self, round: u32) -> Result<Option<Vec<ProposalRecord>>> {
            let map = self.rounds.lock().unwrap();
            Ok(map.get(&round).cloned())
        }
    }

    fn make_structured_eval(
        evaluator: &str,
        score: f32,
        stance: Option<Stance>,
        claims: Vec<ClaimAssessment>,
        justification: &str,
    ) -> EvaluationRecord {
        EvaluationRecord {
            evaluator_agent_id: evaluator.to_string(),
            evaluation: Evaluation {
                score,
                justification: justification.to_string(),
                stance,
                claim_assessments: claims,
                ..Default::default()
            },
            synthetic: false,
        }
    }

    fn make_record_with_evals(
        agent_id: &str,
        round: u32,
        content: &str,
        thought: &str,
        score: f32,
        evals: Vec<EvaluationRecord>,
    ) -> ProposalRecord {
        ProposalRecord {
            round,
            author_agent_id: agent_id.to_string(),
            proposal: Proposal {
                thought_process: thought.to_string(),
                content: content.to_string(),
                final_scratchpad: None,
                token_usage_stats: None,
                ..Default::default()
            },
            evaluations: evals,
            aggregated_score: score,
        }
    }

    fn build_test_corpus() -> Arc<MultiRoundStore> {
        let r1_records = vec![
            make_record_with_evals(
                "Alice",
                1,
                "Quicksort is optimal for this case",
                "Analyzed time complexity: O(n log n)",
                0.85,
                vec![
                    make_structured_eval(
                        "Bob",
                        0.9,
                        Some(Stance::Agree),
                        vec![ClaimAssessment {
                            claim_id: Some("c001".to_string()),
                            claim: "O(n log n) is optimal".to_string(),
                            verdict: Cv::Verified,
                            reason: Some("Confirmed".to_string()),
                        }],
                        "Strong analysis of sorting algorithm",
                    ),
                    make_structured_eval(
                        "Charlie",
                        0.4,
                        Some(Stance::Disagree),
                        vec![ClaimAssessment {
                            claim_id: Some("c002".to_string()),
                            claim: "Quicksort handles duplicates".to_string(),
                            verdict: Cv::Contested,
                            reason: Some("Degenerates to O(n²)".to_string()),
                        }],
                        "Quicksort has known worst-case issues with duplicates",
                    ),
                ],
            ),
            make_record_with_evals(
                "Bob",
                1,
                "Mergesort with guaranteed O(n log n)",
                "Stable sort with predictable performance",
                0.72,
                vec![make_structured_eval(
                    "Alice",
                    0.7,
                    Some(Stance::Neutral),
                    vec![ClaimAssessment {
                        claim_id: Some("c003".to_string()),
                        claim: "Mergesort uses O(n) extra space".to_string(),
                        verdict: Cv::Verified,
                        reason: None,
                    }],
                    "Correct but space overhead is a concern",
                )],
            ),
            make_record_with_evals(
                "Charlie",
                1,
                "Radix sort for integer-only inputs",
                "Linear time for bounded integers",
                0.45,
                vec![make_structured_eval(
                    "Bob",
                    0.3,
                    Some(Stance::StrongDisagree),
                    vec![ClaimAssessment {
                        claim_id: Some("c004".to_string()),
                        claim: "Works for any input type".to_string(),
                        verdict: Cv::Wrong,
                        reason: Some("Only works for integers/strings".to_string()),
                    }],
                    "Radix sort is limited to specific data types",
                )],
            ),
        ];

        let r2_records = vec![make_record_with_evals(
            "Alice",
            2,
            "Revised: Timsort combining mergesort + insertion sort",
            "Addresses duplicate handling from round 1 feedback",
            0.92,
            vec![make_structured_eval(
                "Bob",
                0.95,
                Some(Stance::StrongAgree),
                vec![ClaimAssessment {
                    claim_id: Some("c005".to_string()),
                    claim: "Timsort is adaptive".to_string(),
                    verdict: Cv::Verified,
                    reason: Some("Proven in practice".to_string()),
                }],
                "Excellent revision addressing previous weaknesses",
            )],
        )];

        Arc::new(MultiRoundStore::new(vec![(1, r1_records), (2, r2_records)]))
    }

    // --- Filter Accuracy Tests ---

    #[tokio::test]
    async fn search_no_filters_returns_all() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({ "limit": 50 });
        let result = tool.call(args).await.unwrap();
        // 4 proposals (3 in round 1 + 1 in round 2) + evaluations
        assert!(result.contains("<search_results"));
        assert!(result.contains("Alice"));
        assert!(result.contains("Bob"));
        assert!(result.contains("Charlie"));
    }

    #[tokio::test]
    async fn search_filter_by_round() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "rounds": [2] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Timsort"));
        assert!(!result.contains("Radix sort"));
    }

    #[tokio::test]
    async fn search_filter_by_agent_id() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "agent_ids": ["Charlie"] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Radix sort"));
        // Should not contain proposals by other agents
        assert!(!result.contains("agent=\"Alice\""));
    }

    #[tokio::test]
    async fn search_surfaces_current_round_anonymized_candidates() {
        // Current round (3) is absent from the store during evaluation, so the
        // agent's search_deliberation fallback for a Candidate_X id must resolve
        // from the plumbed candidate set — the second tool Corepunk03 tried.
        let store = build_test_corpus(); // rounds 1,2 only
        let candidate = CandidateProposal {
            id: "Candidate_B".to_string(),
            proposal: Proposal {
                content: "Merge sort keeps stability".to_string(),
                thought_process: "reasoning".to_string(),
                ..Default::default()
            },
        };
        let tool = SearchDeliberationTool::new(store, 3).with_candidates(vec![candidate]);
        let args = serde_json::json!({
            "filters": { "agent_ids": ["Candidate_B"], "rounds": [3] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(
            result.contains("agent=\"Candidate_B\""),
            "surfaces the anonymized candidate: {result}"
        );
        assert!(result.contains("Merge sort keeps stability"));
    }

    #[tokio::test]
    async fn search_candidate_branch_filters_by_keyword() {
        // The candidate-path search (with_candidates) must honor the keyword
        // filter too — not only agent_ids / min_score.
        let store = build_test_corpus(); // rounds 1,2 only
        let candidates = vec![
            CandidateProposal {
                id: "Candidate_B".to_string(),
                proposal: Proposal {
                    content: "Merge sort keeps stability".to_string(),
                    thought_process: "reasoning".to_string(),
                    ..Default::default()
                },
            },
            CandidateProposal {
                id: "Candidate_C".to_string(),
                proposal: Proposal {
                    content: "Quicksort trades stability for speed".to_string(),
                    thought_process: "z".to_string(),
                    ..Default::default()
                },
            },
        ];
        let tool = SearchDeliberationTool::new(store, 3).with_candidates(candidates);
        let args = serde_json::json!({
            "keywords": ["merge sort"],
            "filters": { "rounds": [3] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(
            result.contains("Candidate_B"),
            "the keyword-matching candidate is surfaced: {result}"
        );
        assert!(
            !result.contains("Candidate_C"),
            "the non-matching candidate is filtered out: {result}"
        );
    }

    #[tokio::test]
    async fn search_excludes_candidates_under_evaluation_only_filters() {
        // A not-yet-evaluated candidate has no score/verdicts; an eval-only filter
        // (min_score) must exclude it rather than emit an unscored result.
        let store = build_test_corpus();
        let candidate = CandidateProposal {
            id: "Candidate_B".to_string(),
            proposal: Proposal {
                content: "unscored".to_string(),
                thought_process: "y".to_string(),
                ..Default::default()
            },
        };
        let tool = SearchDeliberationTool::new(store, 3).with_candidates(vec![candidate]);
        let args = serde_json::json!({
            "filters": { "rounds": [3], "min_score": 0.5 },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(
            !result.contains("Candidate_B"),
            "score filter excludes the unscored candidate: {result}"
        );
    }

    #[tokio::test]
    async fn search_filter_by_phase_proposing_only() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "phase": "proposing" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("<proposal"));
        assert!(!result.contains("<evaluation"));
    }

    #[tokio::test]
    async fn search_filter_by_phase_evaluating_only() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "phase": "evaluating" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("<evaluation"));
        assert!(!result.contains("<proposal"));
    }

    #[tokio::test]
    async fn search_filter_by_verdict_contested() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "phase": "evaluating", "verdicts": ["contested"] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Contested"));
        assert!(result.contains("Quicksort handles duplicates"));
        // Should NOT contain evaluations with only Verified/Wrong verdicts
        // (Charlie's "wrong" verdict on radix sort should be excluded)
    }

    #[tokio::test]
    async fn search_filter_by_verdict_wrong() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "phase": "evaluating", "verdicts": ["wrong"] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Wrong"));
        assert!(result.contains("Works for any input type"));
    }

    #[tokio::test]
    async fn search_filter_by_stance() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "phase": "evaluating", "stances": ["strong_disagree"] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("StrongDisagree"));
        // Only Bob's eval of Charlie's radix sort proposal should match
        assert!(result.contains("Radix sort is limited"));
    }

    #[tokio::test]
    async fn search_filter_by_score_range() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "min_score": 0.8, "phase": "proposing" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Alice"));
        // Bob (0.72) and Charlie (0.45) should be excluded
        assert!(!result.contains("agent=\"Bob\""));
        assert!(!result.contains("agent=\"Charlie\""));
    }

    #[tokio::test]
    async fn search_filter_by_keywords() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "keywords": ["timsort"],
            "filters": { "phase": "proposing" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Timsort"));
        assert!(!result.contains("Quicksort is optimal"));
    }

    #[tokio::test]
    async fn search_keywords_are_case_insensitive() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "keywords": ["QUICKSORT"],
            "filters": { "phase": "proposing", "rounds": [1] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Quicksort"));
    }

    #[tokio::test]
    async fn search_keywords_use_and_logic() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "keywords": ["quicksort", "optimal"],
            "filters": { "phase": "proposing" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Quicksort is optimal"));
        // "Radix sort" doesn't contain both keywords
        assert!(!result.contains("Radix sort"));
    }

    #[tokio::test]
    async fn search_no_results_returns_message() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "keywords": ["nonexistent_term_xyz"],
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert_eq!(result, "No results matching your filters.");
    }

    #[tokio::test]
    async fn search_respects_limit() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "limit": 1,
            "filters": { "phase": "proposing" }
        });
        let result = tool.call(args).await.unwrap();
        assert!(result.contains("count=\"1\""));
    }

    #[tokio::test]
    async fn search_combined_filters_verdict_and_round() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": {
                "phase": "evaluating",
                "rounds": [2],
                "verdicts": ["verified"]
            },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        // Only round 2 has Bob's eval with Verified "Timsort is adaptive"
        assert!(result.contains("Timsort is adaptive"));
    }

    #[tokio::test]
    async fn search_includes_claim_ids_in_output() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "phase": "evaluating", "verdicts": ["verified"] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        // Claim IDs should be present in the XML output
        assert!(
            result.contains("id=\"c001\"")
                || result.contains("id=\"c003\"")
                || result.contains("id=\"c005\"")
        );
    }

    // --- Retrieval Accuracy Performance Thresholds ---

    /// Precision test: when filtering by verdict=wrong, all returned results must
    /// actually contain a "wrong" verdict claim. Threshold: 100% precision.
    #[tokio::test]
    async fn search_precision_verdict_filter_is_100_percent() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "phase": "evaluating", "verdicts": ["wrong"] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        // Count evaluation blocks
        let eval_count = result.matches("<evaluation").count();
        let wrong_count = result.matches("Wrong").count();
        assert!(eval_count > 0, "Should return at least one evaluation");
        // Every returned evaluation must contain a "Wrong" verdict claim
        assert_eq!(
            eval_count, wrong_count,
            "Precision must be 100%: every returned evaluation should have a Wrong claim. Got {eval_count} evals, {wrong_count} with Wrong"
        );
    }

    /// Recall test for score range: all proposals with score >= 0.8 must be returned.
    #[tokio::test]
    async fn search_recall_score_range_complete() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "min_score": 0.8, "phase": "proposing" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        // Alice round 1 (0.85) and Alice round 2 (0.92) qualify
        let proposal_count = result.matches("<proposal").count();
        assert!(
            proposal_count >= 2,
            "Recall: expected at least 2 proposals with score >= 0.8, got {proposal_count}"
        );
    }

    /// Latency-style test: ensure the search tool executes within reasonable time
    /// for a corpus of this size (microsecond-level for in-memory operations).
    #[tokio::test]
    async fn search_performance_completes_within_threshold() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);

        let start = std::time::Instant::now();
        for _ in 0..100 {
            let args = serde_json::json!({
                "filters": { "verdicts": ["contested"], "stances": ["disagree"] },
                "keywords": ["quicksort"],
                "limit": 10
            });
            let _ = tool.call(args).await.unwrap();
        }
        let elapsed = start.elapsed();
        // 100 searches should complete in well under 1 second for in-memory store
        assert!(
            elapsed.as_millis() < 1000,
            "100 searches took {}ms, expected < 1000ms",
            elapsed.as_millis()
        );
    }

    /// Deprecation notice test: verify old tools now mention search_deliberation
    #[test]
    fn deprecated_tools_mention_search_deliberation() {
        let store = Arc::new(MockStore::default());

        let read_proposal = ReadProposalTool::new(store.clone(), 1);
        let schema = read_proposal.schema();
        assert!(
            schema
                .function
                .description
                .as_ref()
                .unwrap()
                .contains("search_deliberation"),
            "read_proposal should mention search_deliberation in description"
        );

        let read_critiques = ReadCritiquesTool::new(store.clone(), 1);
        let schema = read_critiques.schema();
        assert!(
            schema
                .function
                .description
                .as_ref()
                .unwrap()
                .contains("search_deliberation"),
            "read_critiques should mention search_deliberation in description"
        );

        let read_own = ReadOwnProposalTool::new(store, 1, "test".to_string());
        let schema = read_own.schema();
        assert!(
            schema
                .function
                .description
                .as_ref()
                .unwrap()
                .contains("search_deliberation"),
            "read_own_proposal should mention search_deliberation in description"
        );
    }

    // =========================================================================
    // SearchDeliberationTool Schema Tests
    // =========================================================================

    #[test]
    fn test_search_deliberation_tool_schema() {
        let store = Arc::new(MockStore::default());
        let tool = SearchDeliberationTool::new(store, 1);

        // Verify tool name
        assert_eq!(tool.name(), "search_deliberation");

        let schema = tool.schema();
        assert_eq!(schema.function.name, "search_deliberation");

        // Verify description exists and is meaningful
        let desc = schema.function.description.as_ref().unwrap();
        assert!(
            desc.contains("Search"),
            "description should mention searching"
        );
        assert!(
            desc.contains("filter"),
            "description should mention filtering"
        );

        // Verify parameters exist and contain expected top-level property names
        let params = schema.function.parameters.as_ref().unwrap();
        let params_obj = params.as_object().unwrap();
        let properties = params_obj.get("properties").unwrap().as_object().unwrap();

        assert!(
            properties.contains_key("filters"),
            "schema should have 'filters' parameter"
        );
        assert!(
            properties.contains_key("keywords"),
            "schema should have 'keywords' parameter"
        );
        assert!(
            properties.contains_key("limit"),
            "schema should have 'limit' parameter"
        );

        // Verify filters sub-properties
        let filters_props = properties
            .get("filters")
            .unwrap()
            .as_object()
            .unwrap()
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();

        assert!(filters_props.contains_key("rounds"));
        assert!(filters_props.contains_key("agent_ids"));
        assert!(filters_props.contains_key("phase"));
        assert!(filters_props.contains_key("verdicts"));
        assert!(filters_props.contains_key("stances"));
        assert!(filters_props.contains_key("min_score"));
        assert!(filters_props.contains_key("max_score"));
    }

    // =========================================================================
    // truncate_str Tests
    // =========================================================================

    #[test]
    fn test_truncate_str_short() {
        // String within limit should be returned unchanged
        let result = truncate_str("hello world", 100);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_truncate_str_long() {
        // String beyond limit should be truncated with "..."
        let input = "abcdefghij"; // 10 chars
        let result = truncate_str(input, 5);
        assert_eq!(result, "abcde...");
        assert_eq!(result.chars().count(), 8); // 5 + 3 for "..."
    }

    #[test]
    fn test_truncate_str_empty() {
        // Empty string should remain empty
        let result = truncate_str("", 100);
        assert_eq!(result, "");
    }

    #[test]
    fn test_truncate_str_exact_limit() {
        // String exactly at limit should be returned unchanged (no truncation)
        let input = "abcde"; // 5 chars
        let result = truncate_str(input, 5);
        assert_eq!(result, "abcde");
        // Should NOT have "..." appended
        assert!(!result.contains("..."));
    }

    #[test]
    fn test_truncate_str_zero_limit() {
        // max_chars=0: all content is "beyond" the limit, so we get just "..."
        let result = truncate_str("hello", 0);
        assert_eq!(result, "...");
    }

    // =========================================================================
    // Store Error Handling Tests
    // =========================================================================

    /// A MockStore that always returns an error from get_round_history
    #[derive(Debug)]
    struct ErrorStore;

    #[async_trait]
    impl PersistenceStore for ErrorStore {
        async fn get(&self, _key: &str) -> Result<Option<String>> {
            Err(anyhow::anyhow!("store get error"))
        }
        async fn append(&self, _key: &str, _content: &str) -> Result<()> {
            Err(anyhow::anyhow!("store append error"))
        }
        async fn set(&self, _key: &str, _content: &str) -> Result<()> {
            Err(anyhow::anyhow!("store set error"))
        }
        async fn get_round_history(&self, _round: u32) -> Result<Option<Vec<ProposalRecord>>> {
            Err(anyhow::anyhow!("simulated store failure"))
        }
    }

    #[tokio::test]
    async fn test_read_proposal_tool_store_error() {
        let store = Arc::new(ErrorStore);
        let tool = ReadProposalTool::new(store, 2);
        let args = serde_json::json!({
            "agent_id": "agent_1",
            "round": 1
        });

        let result = tool.call(args).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to retrieve history from store")
        );
    }

    #[tokio::test]
    async fn test_read_critiques_tool_store_error() {
        let store = Arc::new(ErrorStore);
        let tool = ReadCritiquesTool::new(store, 2);
        let args = serde_json::json!({
            "target_agent_id": "agent_1",
            "round": 1
        });

        let result = tool.call(args).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to retrieve history from store")
        );
    }

    #[tokio::test]
    async fn test_read_own_proposal_tool_store_error() {
        let store = Arc::new(ErrorStore);
        let tool = ReadOwnProposalTool::new(store, 1, "agent_1".to_string());
        let args = serde_json::json!({});

        let result = tool.call(args).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to retrieve history from store")
        );
    }

    #[tokio::test]
    async fn test_search_deliberation_tool_store_error() {
        let store = Arc::new(ErrorStore);
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({ "limit": 10 });

        let result = tool.call(args).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Failed to retrieve history"));
    }

    // =========================================================================
    // ReadOwnProposalTool — offset exceeds bounds
    // =========================================================================

    #[tokio::test]
    async fn test_read_own_proposal_offset_exceeds_length() {
        let record = create_test_record("test_agent", "Short thought", "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadOwnProposalTool::new(store, 1, "test_agent".to_string());

        let args = serde_json::json!({
            "offset": 5000
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("Offset 5000 exceeds"));
        assert!(result.contains("No more content"));
    }

    #[test]
    fn render_proposal_offset_beyond_thoughts_reports_no_more_content() {
        // The candidate-path helper's own offset-overflow branch (separate from
        // the store-backed read_own_proposal one above).
        let p = Proposal {
            content: "final".into(),
            thought_process: "short".into(),
            ..Default::default()
        };
        let out = render_proposal(7, "Candidate_A", &p, 100, 50);
        assert!(out.contains("Offset 100 exceeds"), "offset overflow: {out}");
        assert!(out.contains("No more content"));
    }

    #[test]
    fn render_proposal_windows_thoughts_and_flags_remaining() {
        let p = Proposal {
            content: "the answer".into(),
            thought_process: "0123456789".into(),
            ..Default::default()
        };
        // offset 3, limit 4 → the window "3456", 3 chars remaining.
        let out = render_proposal(2, "Candidate_A", &p, 3, 4);
        assert!(out.contains("3456"), "windowed thoughts: {out}");
        assert!(out.contains("the answer"), "includes final_solution");
        assert!(
            out.contains("characters remaining"),
            "flags more to read: {out}"
        );
        assert!(out.contains("offset=7"), "next offset = 3+4: {out}");
    }

    // =========================================================================
    // ReadOwnProposalTool — agent not found in round data
    // =========================================================================

    #[tokio::test]
    async fn test_read_own_proposal_agent_not_found_in_round() {
        // Round 1 has data but not for our agent
        let record = create_test_record("other_agent", "Thought", "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadOwnProposalTool::new(store, 1, "missing_agent".to_string());

        let args = serde_json::json!({});

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("No proposal found for your ID"));
        assert!(result.contains("missing_agent"));
    }

    // =========================================================================
    // ReadCritiquesTool — no history for round
    // =========================================================================

    #[tokio::test]
    async fn test_read_critiques_tool_no_history() {
        let store = Arc::new(MockStore::default());
        let tool = ReadCritiquesTool::new(store, 2);

        let args = serde_json::json!({
            "target_agent_id": "agent_1",
            "round": 1
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("No history found for round"));
    }

    // =========================================================================
    // ReadProposalTool — agent exists but proposal not found
    // =========================================================================

    #[tokio::test]
    async fn test_read_proposal_agent_not_found_in_round() {
        let record = create_test_record("agent_1", "Thought", "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadProposalTool::new(store, 2);

        let args = serde_json::json!({
            "agent_id": "agent_99",
            "round": 1
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("No proposal found for agent_id 'agent_99'"));
    }

    // =========================================================================
    // ReadProposalTool — round 1 defaults
    // =========================================================================

    #[tokio::test]
    async fn test_read_proposal_round1_defaults_to_round1() {
        // When current_round is 1, default target should be max(1-1, 1) = 1
        let record = create_test_record("agent_1", "R1 thought", "R1 content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadProposalTool::new(store, 1); // current_round = 1

        let args = serde_json::json!({
            "agent_id": "agent_1"
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("R1 content"));
    }

    // =========================================================================
    // ReadOwnProposalTool — limit cap
    // =========================================================================

    #[tokio::test]
    async fn test_read_own_proposal_respects_limit_cap() {
        let long_thought = "D".repeat(50000);
        let record = create_test_record("test_agent", &long_thought, "Content");
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadOwnProposalTool::new(store, 1, "test_agent".to_string());

        // Try to request more than the 15,000 char cap
        let args = serde_json::json!({
            "limit": 50000
        });

        let result = tool.call(args).await.unwrap();
        // Result should cap at 15,000 chars
        assert!(result.contains("length=\"15000\""));
    }

    // =========================================================================
    // SearchDeliberationTool — max_score filter
    // =========================================================================

    #[tokio::test]
    async fn search_filter_by_max_score() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "max_score": 0.5, "phase": "proposing" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        // Only Charlie (0.45) should pass the max_score filter
        assert!(result.contains("Charlie"));
        assert!(!result.contains("agent=\"Alice\""));
        assert!(!result.contains("agent=\"Bob\""));
    }

    // =========================================================================
    // SearchDeliberationTool — stance filter on eval without stance
    // =========================================================================

    #[tokio::test]
    async fn search_stance_filter_skips_evals_without_stance() {
        // Create an eval with no stance set
        let eval_no_stance = make_structured_eval(
            "NoStanceReviewer",
            0.5,
            None, // No stance
            vec![ClaimAssessment {
                claim_id: Some("c100".to_string()),
                claim: "Some claim".to_string(),
                verdict: Cv::Verified,
                reason: Some("Verified".to_string()),
            }],
            "Review without stance",
        );
        let record = make_record_with_evals(
            "TestAgent",
            1,
            "Test content",
            "Test thought",
            0.75,
            vec![eval_no_stance],
        );
        let store = Arc::new(MultiRoundStore::new(vec![(1, vec![record])]));
        let tool = SearchDeliberationTool::new(store, 1);

        // Filter by stance — evals without stance should be skipped
        let args = serde_json::json!({
            "filters": { "phase": "evaluating", "stances": ["agree"] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert_eq!(
            result, "No results matching your filters.",
            "Evals without stance should be skipped when stance filter is active"
        );
    }

    // =========================================================================
    // SearchDeliberationTool — unknown phase defaults to both
    // =========================================================================

    #[tokio::test]
    async fn search_unknown_phase_shows_both() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        // "unknown_phase" is not "proposing" or "evaluating", should default to showing both
        let args = serde_json::json!({
            "filters": { "phase": "unknown_phase" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        // Should contain both proposals and evaluations
        assert!(
            result.contains("<proposal"),
            "Unknown phase should show proposals"
        );
        assert!(
            result.contains("<evaluation"),
            "Unknown phase should show evaluations"
        );
    }

    // =========================================================================
    // SearchDeliberationTool — limit capping at MAX_RESULTS (50)
    // =========================================================================

    /// Build a corpus with >50 items to exercise the MAX_RESULTS cap.
    fn build_large_test_corpus() -> Arc<MultiRoundStore> {
        // 10 rounds × 6 agents = 60 proposals + evaluations → well over 50 items
        let agents = ["A", "B", "C", "D", "E", "F"];
        let mut round_data = Vec::new();
        for round in 1..=10u32 {
            let records: Vec<ProposalRecord> = agents
                .iter()
                .map(|a| {
                    make_record_with_evals(
                        a,
                        round,
                        &format!("Proposal by {} in round {}", a, round),
                        "thought",
                        0.5,
                        vec![],
                    )
                })
                .collect();
            round_data.push((round, records));
        }
        Arc::new(MultiRoundStore::new(round_data))
    }

    #[tokio::test]
    async fn search_limit_capped_at_50() {
        let store = build_large_test_corpus();
        let tool = SearchDeliberationTool::new(store, 10);
        // Request limit > MAX_RESULTS (50)
        let args = serde_json::json!({
            "limit": 1000
        });
        let result = tool.call(args).await.unwrap();
        // Count total results — should not exceed 50
        let proposal_count = result.matches("<proposal").count();
        let eval_count = result.matches("<evaluation").count();
        let total = proposal_count + eval_count;
        assert_eq!(
            total, 50,
            "Results should be capped at exactly 50, got {}",
            total
        );
    }

    // =========================================================================
    // SearchDeliberationTool — keyword in evaluation justification
    // =========================================================================

    #[tokio::test]
    async fn search_keyword_in_justification() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "keywords": ["worst-case"],
            "filters": { "phase": "evaluating" },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        // "worst-case" appears in Charlie's eval justification about quicksort
        assert!(result.contains("<evaluation"));
        assert!(result.contains("worst-case"));
    }

    // =========================================================================
    // SearchDeliberationTool — default limit (no limit specified)
    // =========================================================================

    #[tokio::test]
    async fn search_default_limit() {
        let store = build_large_test_corpus();
        let tool = SearchDeliberationTool::new(store, 10);
        // No limit specified — should use default_search_limit() = 10
        let args = serde_json::json!({});
        let result = tool.call(args).await.unwrap();
        let proposal_count = result.matches("<proposal").count();
        let eval_count = result.matches("<evaluation").count();
        let total = proposal_count + eval_count;
        assert_eq!(
            total, 10,
            "Default limit should cap at exactly 10, got {} results",
            total
        );
    }

    // =========================================================================
    // SearchDeliberationTool — empty results for non-existent round
    // =========================================================================

    #[tokio::test]
    async fn search_nonexistent_round() {
        let store = build_test_corpus();
        let tool = SearchDeliberationTool::new(store, 2);
        let args = serde_json::json!({
            "filters": { "rounds": [99] },
            "limit": 50
        });
        let result = tool.call(args).await.unwrap();
        assert_eq!(result, "No results matching your filters.");
    }

    // =========================================================================
    // agent_id_match — additional cases
    // =========================================================================

    #[test]
    fn test_agent_id_match_empty_strings() {
        assert!(agent_id_match("", ""));
        assert!(!agent_id_match("Agent", ""));
        assert!(!agent_id_match("", "Agent"));
    }

    #[test]
    fn test_agent_id_match_unicode() {
        assert!(agent_id_match("Ägent_1", "Ägent_1"));
        assert!(agent_id_match("  Ägent_1  ", "Ägent_1"));
    }

    // =========================================================================
    // ReadCritiquesTool — defaults to previous round
    // =========================================================================

    #[tokio::test]
    async fn test_read_critiques_defaults_to_previous_round() {
        let record = create_test_record("agent_1", "R2 Thought", "R2 Content");
        let store = Arc::new(MockStore::with_history(2, vec![record]));
        let tool = ReadCritiquesTool::new(store, 3); // current_round = 3

        // Don't specify round — should default to round 2
        let args = serde_json::json!({
            "target_agent_id": "agent_1"
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("reviewer1"));
        assert!(result.contains("Good work!"));
    }

    // =========================================================================
    // ReadProposalTool — invalid args
    // =========================================================================

    #[tokio::test]
    async fn test_read_proposal_invalid_args() {
        let store = Arc::new(MockStore::default());
        let tool = ReadProposalTool::new(store, 2);
        // Missing required "agent_id" field
        let args = serde_json::json!({ "round": 1 });
        let result = tool.call(args).await;
        assert!(result.is_err(), "Missing agent_id should return an error");
    }

    // =========================================================================
    // ReadCritiquesTool — multiple evaluations
    // =========================================================================

    #[tokio::test]
    async fn test_read_critiques_multiple_evaluations() {
        let mut record = create_test_record("agent_1", "Thought", "Content");
        record.evaluations.push(EvaluationRecord {
            evaluator_agent_id: "reviewer2".to_string(),
            evaluation: Evaluation {
                score: 0.6,
                justification: "Needs improvement".to_string(),
                token_usage: None,
                ..Default::default()
            },
            synthetic: false,
        });
        let store = Arc::new(MockStore::with_history(1, vec![record]));
        let tool = ReadCritiquesTool::new(store, 2);

        let args = serde_json::json!({
            "target_agent_id": "agent_1",
            "round": 1
        });

        let result = tool.call(args).await.unwrap();
        assert!(result.contains("reviewer1"));
        assert!(result.contains("reviewer2"));
        assert!(result.contains("Good work!"));
        assert!(result.contains("Needs improvement"));
        assert!(result.contains("score=\"0.8\""));
        assert!(result.contains("score=\"0.6\""));
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReadOwnProposalArgs {
    /// The character offset to start reading from (useful for pagination). Defaults to 0.
    offset: Option<usize>,
    /// The maximum number of characters to return. Defaults to 5000.
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadOwnProposalTool {
    fn name(&self) -> String {
        "read_own_proposal".to_string()
    }

    fn schema(&self) -> ChatCompletionTool {
        ChatCompletionTool {
                r#type: ChatCompletionToolType::Function,
                function: FunctionObject {
                    name: self.name(),
                    description: Some(
                        "[Deprecated — prefer search_deliberation with your agent_id] Read your OWN full thought process and solution from the current round to compare with others. Use this before judging.".to_string(),
                    ),
                    parameters: Some(schemars::schema_for!(ReadOwnProposalArgs).into()),
                    strict: None,
                },
            }
    }

    async fn call(&self, args: Value) -> Result<String, Box<dyn Error + Send + Sync>> {
        let args: ReadOwnProposalArgs = serde_json::from_value(args)?;
        let offset = args.offset.unwrap_or(0);
        // Enforce hard cap of 15,000 characters to prevent context explosion
        let limit = args.limit.unwrap_or(5000).min(15_000);

        // Retrieve round history from store
        let history = self
            .store
            .get_round_history(self.round)
            .await
            .map_err(|e| format!("Failed to retrieve history from store: {e}"))?;

        if let Some(records) = history {
            if let Some(record) = records
                .iter()
                .find(|r| agent_id_match(&r.author_agent_id, &self.agent_id))
            {
                let thoughts = &record.proposal.thought_process;
                let total_chars = thoughts.chars().count();

                if offset >= total_chars {
                    return Ok(format!(
                        "Offset {offset} exceeds thought_process length {total_chars} chars. No more content."
                    ));
                }

                let snippet: String = thoughts.chars().skip(offset).take(limit).collect();
                let snippet_len = snippet.chars().count();
                let remaining = total_chars.saturating_sub(offset + snippet_len);

                let mut output = format!(
                    "<proposal round=\"{}\" agent=\"{}\" type=\"own\">\n<final_solution>{}</final_solution>\n\n<thought_process offset=\"{}\" length=\"{}\" total=\"{}\">\n{}\n",
                    self.round,
                    self.agent_id,
                    record.proposal.content,
                    offset,
                    snippet_len,
                    total_chars,
                    snippet
                );

                if remaining > 0 {
                    output.push_str(&format!(
                            "\n... ({} characters remaining. Call read_own_proposal with offset={} to read more.)",
                            remaining,
                            offset + snippet_len
                        ));
                }
                output.push_str("</thought_process>\n</proposal>");

                Ok(output)
            } else {
                Ok(format!(
                    "No proposal found for your ID '{}' in round {}. This is unexpected.",
                    self.agent_id, self.round
                ))
            }
        } else {
            Ok(format!(
                "No history found for round {}. If this is the current round, it hasn't finished yet. Did you mean to check a previous round?",
                self.round
            ))
        }
    }
}
