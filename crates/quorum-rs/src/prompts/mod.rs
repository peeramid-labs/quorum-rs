//! Defines the traits and structs for managing prompt templates.
//!
//! This module decouples the agent's logic from the specific text of the prompts
//! it uses. By implementing the `PromptSet` trait, different sets of prompts can be
//! created and injected into an agent, allowing for model-specific tuning without
//! changing the agent's core implementation.

pub mod defaults;
pub mod shape;

use dyn_clone::DynClone;
use std::fmt::Debug;

use crate::agents::{
    CandidateProposal, DeliberationPhase, Proposal, StructuredFeedback, UserInjection,
};

/// The wall-clock date the task was issued, rendered for the user message.
///
/// Empty when the caller stamped none, so a task from a client that predates
/// the stamp reads exactly as it did before.
///
/// A model has no clock and no sense of how old its own recall is, so a bare
/// date changes nothing: it will still answer a "current prices" question
/// from training data and present the figure as today's. The block therefore
/// states the date AND what the date implies — that anything recalled
/// unaided is out of date by an unknown margin and must be labelled with the
/// period it describes rather than passed off as current.
///
/// It rides in the user message, not the system prompt: the system prefix is
/// kept byte-identical across a session for cache reuse, and a date is not.
pub fn clock_block(issued_at: Option<&str>) -> String {
    let Some(stamp) = issued_at else {
        return String::new();
    };
    format!(
        "<clock>\n\
         The current date is {stamp}. Your training data ends before this, so \
         any fact, price, version or event you recall unaided is out of date by \
         an unknown margin.\n\
         State the period a figure describes rather than presenting it as \
         current, and prefer a source you can retrieve now over one you \
         remember.\n\
         </clock>\n\n"
    )
}

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

#[cfg(test)]
mod tests {
    use super::clock_block;

    #[test]
    fn the_clock_states_the_date_and_what_it_implies_about_recall() {
        let block = clock_block(Some("2026-08-22"));

        assert!(block.contains("2026-08-22"));
        // A bare date is ignorable. What stops a model presenting a
        // training-era figure as current is being told that is what it has.
        assert!(block.to_lowercase().contains("training"));
        assert!(block.to_lowercase().contains("out of date"));
    }

    #[test]
    fn no_stamp_renders_nothing_at_all() {
        assert_eq!(clock_block(None), "");
    }
}
