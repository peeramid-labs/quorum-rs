//! Defines the traits and structs for managing prompt templates.
//!
//! This module decouples the agent's logic from the specific text of the prompts
//! it uses. By implementing the `PromptSet` trait, different sets of prompts can be
//! created and injected into an agent, allowing for model-specific tuning without
//! changing the agent's core implementation.

pub mod defaults;

use dyn_clone::DynClone;
use std::fmt::Debug;

use crate::agents::{
    CandidateProposal, DeliberationPhase, Proposal, StructuredFeedback, UserInjection,
};

/// A trait for a collection of prompt templates used by an NSED agent.
///
/// This allows for different "personalities" or model-specific instructions
/// to be injected into an agent.
pub trait PromptSet: Send + Sync + Debug + DynClone {
    /// Returns the general system message that sets the agent's overall context.
    /// Implementations MUST keep this static across a session (no per-turn round /
    /// phase) so the system-prompt prefix stays cache-stable; the dynamic per-turn
    /// state belongs in [`PromptSet::get_turn_header`], prepended to the user message.
    fn get_system_message(
        &self,
        agent_name: &str,
        current_round: usize,
        round_numbers: usize,
        phase: DeliberationPhase,
    ) -> String;

    /// The dynamic per-turn header prepended to the user message: the current round,
    /// the phase, and the tool to call, pointing back to the static `<strategy>`
    /// block in the system prompt. Kept OUT of the system message so that prefix
    /// stays byte-identical every turn (KV / prompt-cache reuse).
    fn get_turn_header(
        &self,
        current_round: usize,
        round_numbers: usize,
        phase: DeliberationPhase,
    ) -> String {
        let (label, strategy, tool) = match phase {
            DeliberationPhase::Proposing => ("Proposing", "proposing", "nsed_propose"),
            DeliberationPhase::Evaluating => ("Evaluating", "evaluating", "nsed_evaluate"),
            DeliberationPhase::ConsensusCheck => ("Consensus Check", "evaluating", "nsed_evaluate"),
        };
        format!(
            "<turn>\n\
             Round {current_round} of {round_numbers}. Phase: {label}.\n\
             Follow the `<strategy phase=\"{strategy}\">` block in your system prompt and \
             submit your result via `{tool}`.\n\
             </turn>\n\n"
        )
    }

    /// Returns the prompt for the Proposer module.
    ///
    /// # Arguments
    /// * `task_description` - The high-level description of the user's task.
    /// * `previous_round_matrix` - An optional Markdown table summarizing previous round results.
    /// * `previous_score` - The aggregated score of the agent's previous proposal (0.0 - 1.0).
    #[allow(clippy::too_many_arguments)]
    fn get_proposer_prompt(
        &self,
        task_description: &str,
        previous_round_matrix: Option<String>,
        previous_own_proposal: Option<&Proposal>,
        previous_score: Option<f32>,
        previous_critiques: Vec<String>,
        user_injections: &[UserInjection],
        structured_feedback: Option<&StructuredFeedback>,
    ) -> String;

    /// Returns the prompt for the Evaluator module (Batch Mode).
    ///
    /// # Arguments
    /// * `task_description` - The high-level description of the user's task.
    /// * `candidates` - The list of candidate proposals to evaluate.
    /// * `own_current_proposal` - The agent's current proposal for self-reference during evaluation.
    fn get_batch_evaluator_prompt(
        &self,
        task_description: &str,
        candidates: &[CandidateProposal],
        own_current_proposal: Option<&Proposal>,
        current_round: usize,
        user_injections: &[UserInjection],
    ) -> String;

    /// Returns a delta prompt for the Proposer on resumed sessions (round 2+).
    ///
    /// Omits task description and general instructions that are already in the
    /// session context. Only includes new data: feedback, critiques, matrix.
    /// Default implementation falls back to the full prompt.
    #[allow(clippy::too_many_arguments)]
    fn get_proposer_delta_prompt(
        &self,
        task_description: &str,
        previous_round_matrix: Option<String>,
        previous_own_proposal: Option<&Proposal>,
        previous_score: Option<f32>,
        previous_critiques: Vec<String>,
        user_injections: &[UserInjection],
        structured_feedback: Option<&StructuredFeedback>,
    ) -> String {
        self.get_proposer_prompt(
            task_description,
            previous_round_matrix,
            previous_own_proposal,
            previous_score,
            previous_critiques,
            user_injections,
            structured_feedback,
        )
    }

    /// Returns a delta prompt for the Evaluator on resumed sessions.
    ///
    /// Omits scoring rubric and general instructions that are already in the
    /// session context. Only includes new data: candidates, focus, injections.
    /// Default implementation falls back to the full prompt.
    fn get_evaluator_delta_prompt(
        &self,
        task_description: &str,
        candidates: &[CandidateProposal],
        own_current_proposal: Option<&Proposal>,
        current_round: usize,
        user_injections: &[UserInjection],
    ) -> String {
        self.get_batch_evaluator_prompt(
            task_description,
            candidates,
            own_current_proposal,
            current_round,
            user_injections,
        )
    }

    /// Returns the prompt for the Summarizer module.
    ///
    /// # Arguments
    /// * `task_description` - The high-level description of the user's task.
    /// * `proposal_content` - The content of the proposal to be summarized.
    fn get_summarizer_prompt(&self, task_description: &str, proposal_content: &str) -> String;
}

// Required for `Box<dyn PromptSet>` to be cloneable.
dyn_clone::clone_trait_object!(PromptSet);
