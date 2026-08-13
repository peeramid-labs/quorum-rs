//! Contains a default, general-purpose implementation of the `PromptSet` trait.

use crate::agents::DeliberationPhase;

use super::PromptSet;
use crate::agents::{Proposal, UserInjection};

fn render_user_updates(injections: &[UserInjection], context: &str) -> String {
    if injections.is_empty() {
        return String::new();
    }
    let mut out = String::from("<user_updates>\n");
    if context == "evaluation" {
        out.push_str(
            "  The user provided the following clarifications during deliberation.\n  \
             Factor these into your evaluation criteria.\n",
        );
    } else {
        out.push_str(
            "  The user provided the following clarifications during deliberation.\n  \
             Integrate these into your work — later updates take priority.\n",
        );
    }
    for inj in injections {
        out.push_str(&format!(
            "  <update round=\"{}\">{}</update>\n",
            inj.injected_at_round, inj.message
        ));
    }
    out.push_str("</user_updates>\n");
    out
}

/// A default, general-purpose set of prompts for an NSED agent.
///
/// This implementation provides standard, model-agnostic prompts suitable for
/// general problem-solving tasks.
#[derive(Clone, Debug)]
pub struct DefaultPromptSet {
    pub persona: Option<String>,
    pub textual_feedback: bool,
}

impl Default for DefaultPromptSet {
    fn default() -> Self {
        Self {
            persona: None,
            textual_feedback: true,
        }
    }
}

impl DefaultPromptSet {
    /// Creates a new instance of the default prompt set.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_persona(mut self, persona: String) -> Self {
        self.persona = Some(persona);
        self
    }

    pub fn with_textual_feedback(mut self, enabled: bool) -> Self {
        self.textual_feedback = enabled;
        self
    }
}

impl PromptSet for DefaultPromptSet {
    // The system message is deliberately 100% static across a session: the current
    // round + phase (the only per-turn variance) are NOT here — they go at the top of
    // each user message (`get_turn_header`). This keeps the system-prompt prefix
    // byte-identical every turn so the KV / prompt cache is reused instead of
    // re-billed. `round_numbers` (total rounds) is fixed for a session, so it is
    // safe to keep; `_current_round` / `_phase` are ignored on purpose.
    fn get_system_message(
        &self,
        agent_name: &str,
        _current_round: usize,
        round_numbers: usize,
        _phase: DeliberationPhase,
    ) -> String {
        let mut message = String::new();

        // 1. Context & Goal
        message.push_str(
            "You are a member of an elite AI council table tasked with solving complex problems through rigorous deliberation.\n\
            Your goal is to produce a **complete, finalized answer** that fully addresses the user's request. \
            The deliberation output is delivered directly to the end user — treat every proposal as a polished final draft.\n\
            If you need additional information to produce a thorough answer, use the tools provided (e.g. user-facing tools to ask clarifying questions). \
            Do not leave gaps or defer to the user when you can reason through the problem yourself.\n\n\
            <incentives>\n\
            - **Cooperative Failure**: If the group submits a wrong final answer, you ALL fail. You are punished together.\n\
            - **Competitive Success**: If the group succeeds, the agent with the winning proposal gains maximum reward. Evaluation accuracy also yields rewards.\n\
            - Therefore: Collaborate to find the truth, but compete to be the one who finds it.\n\
            </incentives>\n",
        );

        // 2. Identity & Persona
        message.push_str(&format!("\n<identity>\nYour name is {agent_name}.\n"));
        if let Some(persona) = &self.persona {
            message.push_str("Persona: ");
            message.push_str(persona);
            message.push('\n');
        }
        message.push_str("</identity>\n");

        // 3. Protocol & Rules — structure only. The current round + phase are stated
        //    at the top of each user message (see `get_turn_header`), keeping this
        //    prefix static; follow the `<strategy>` block matching the phase named there.
        message.push_str(&format!(
            "\n<protocol>\n\
            - **Structure**: The deliberation has {round_numbers} rounds. Each round has two phases: Proposing (submit a solution via `nsed_propose`) and Evaluating (critique peers via `nsed_evaluate`).\n\
            - **Your current round and phase are stated at the top of each user message** — follow the matching `<strategy>` block below and call the tool named there.\n\
            </protocol>\n"
        ));

        // 4. Both phase strategies (static — the user message says which applies now).
        message.push_str(
            "\n<strategy phase=\"proposing\">\n\
            1. **Plan First**: You MUST write your plan to the scratchpad before solving.\n\
            2. **Step-by-Step**: Solve incrementally. Verify intermediate results.\n\
            3. **Critique Integration**: If you received feedback, you MUST start your thought process with a 'Critique Integration' section explaining your fixes.\n\
            </strategy>\n"
        );
        message.push_str(
            "\n<strategy phase=\"evaluating\">\n\
            You are a Judge. Your default stance is SKEPTICISM. Do not be sycophantic.\n\
            Use the Vector Alignment protocol:\n\
            1. **Decompose**: Break each candidate's argument into discrete claims. Identify the 2-3 claims that most determine correctness.\n\
            2. **Anchor**: Trust your own solution unless proven wrong.\n\
            3. **Delta**: Identify EXACTLY where a peer diverges from you on those pivotal claims.\n\
            4. **Verification**: Verify ONLY the divergent claims. If they are wrong, reject them ruthlessly. If they are right, accept the correction.\n\
            5. **Consensus Trap**: If everyone agrees on a wrong answer, it is a collective hallucination. Be the one who spots the error.\n\
            6. **Harsh Scoring**: Penalize unverified claims. Agreement without verification is worth 0.\n\
            </strategy>\n"
        );

        // 5. Critical Execution Rules
        message.push_str(
            "\n<rules>\n\
           1. Use `update_scratchpad` to store notes. Structure your scratchpad with XML sections:\n\
              * `<working_memory>` — Ephemeral per-round calculations (cleared after each round).\n\
              * `<key_findings>` — Persistent discoveries that matter across rounds.\n\
              * `<strategy>` — Your evolving approach.\n\
              Use sparingly; only for cross-round records or tool output notes.\n\
           2. Use `read_proposal` or `search_deliberation` to inspect peer reasoning.\n\
           3. Only call `submit` tools when finished.\n\
           4. **No meta-commentary**: Your proposal content is shown directly to the end user. Never include headers like \"FINAL RECOMMENDATION:\", \"CONCLUSION:\", \"Summary:\", or similar structural artifacts. Do not reference the deliberation process, rounds, or other agents in your output. Write as if you are the sole author delivering a polished answer.\n\
           </rules>",
        );

        message
    }

    fn get_proposer_prompt(
        &self,
        task_description: &str,
        previous_round_matrix: Option<String>,
        previous_own_proposal: Option<&Proposal>,
        previous_score: Option<f32>,
        previous_critiques: Vec<String>,
        user_injections: &[UserInjection],
        structured_feedback: Option<&crate::agents::StructuredFeedback>,
    ) -> String {
        let user_updates_section = render_user_updates(user_injections, "proposing");

        // Build structured deliberation brief when available
        let deliberation_brief = if let Some(sf) = structured_feedback {
            let mut brief = String::from("<deliberation_brief>\n");

            if !sf.contested_claims.is_empty() {
                brief.push_str(&format!(
                    "  <contested_claims count=\"{}\">\n",
                    sf.contested_claims.len()
                ));
                for cc in &sf.contested_claims {
                    brief.push_str(&format!(
                        "    <dispute id=\"{}\" evaluator=\"{}\" confidence=\"{:?}\">\n      <your_claim>{}</your_claim>\n      <counter>{}</counter>\n    </dispute>\n",
                        cc.claim_id,
                        cc.evaluator,
                        cc.confidence,
                        cc.what_you_claimed,
                        cc.counter_position
                    ));
                }
                brief.push_str("  </contested_claims>\n");
            }

            if !sf.verified_claims.is_empty() {
                brief.push_str(&format!(
                    "  <verified_claims>{}</verified_claims>\n",
                    sf.verified_claims.join(", ")
                ));
            }

            if let Some(ref cb) = sf.category_breakdown {
                brief.push_str(&format!(
                    "  <category_breakdown>correctness: {:.2} | completeness: {:.2} | novelty: {:.2} | feasibility: {:.2} | evidence_quality: {:.2}</category_breakdown>\n",
                    cb.correctness, cb.completeness, cb.novelty, cb.feasibility, cb.evidence_quality
                ));
            }

            brief.push_str("</deliberation_brief>\n\n");
            brief
        } else {
            String::new()
        };

        if let Some(matrix) = previous_round_matrix {
            let own_proposal_section = if let Some(p) = previous_own_proposal {
                let truncated_thought = if p.thought_process.chars().count() > 500 {
                    format!(
                        "{}...",
                        p.thought_process.chars().take(500).collect::<String>()
                    )
                } else {
                    p.thought_process.clone()
                };

                let score_msg = if let Some(score) = previous_score {
                    let advice = if score < 0.5 {
                        "LOW support. Provide more convincing proofs addressing critics, or fundamentally rethink your approach."
                    } else if score < 0.8 {
                        "Moderate support. Address critiques and review other proposals to improve your contribution."
                    } else {
                        "High support. Your proposal is leading. Review other proposals and iterate on it to maintain support. The group rewards verifiable truth."
                    };
                    format!(
                        "YOUR PREVIOUS SCORE: {score:.2} / 1.0\n{advice}\n\
                        Your goal is to find the objectively correct answer. Peer support is a signal of verification, but truth is the ultimate standard. If you believe you are right and others are wrong, provide stronger proof.\n"
                    )
                } else {
                    String::new()
                };

                format!(
                    "\n<previous_own_proposal>\n\
                    {}\
                    THOUGHT PROCESS (Preview): {}\n\
                    FINAL SOLUTION: {}\n\
                    (Use `read_own_proposal()` to see full details if needed.)\n\
                    </previous_own_proposal>\n\n",
                    score_msg, truncated_thought, p.content
                )
            } else {
                String::new()
            };

            let critiques_section = if !previous_critiques.is_empty() {
                let combined = previous_critiques.join("\n\n");
                format!(
                    "<peer_critiques>\n(These are the specific reasons you lost points. Address them directly.)\n{combined}\n</peer_critiques>\n\n"
                )
            } else {
                String::new()
            };

            let has_categories = structured_feedback
                .as_ref()
                .and_then(|f| f.category_breakdown.as_ref())
                .is_some();
            let structured_instructions = if structured_feedback.is_some() {
                if has_categories {
                    "\n5. For each contested claim in the deliberation brief:\n\
                       - If the counter-argument is correct: FIX your approach.\n\
                       - If your original claim is correct: STRENGTHEN your proof.\n\
                     6. Focus on your weakest category score."
                } else {
                    "\n5. For each contested claim in the deliberation brief:\n\
                       - If the counter-argument is correct: FIX your approach.\n\
                       - If your original claim is correct: STRENGTHEN your proof."
                }
            } else {
                ""
            };

            let instructions_feedback = if self.textual_feedback {
                format!(
                    "1. ANALYZE the matrix above and the critiques you received.\n\
                     2. OPTIONAL: Use `read_proposal(round, best_agent_id)` to see the full reasoning of any agent.\n\
                     3. Synthesize a NEW solution that combines the strengths of all proposals and fixes the critiques mentioned by peers.\n\
                     4. Do NOT simply repeat the previous winner. Innovate or Refine.{structured_instructions}"
                )
            } else {
                format!(
                    "1. ANALYZE the matrix above.\n\
                     2. Use `read_proposal(round, best_agent_id)` to see the full reasoning of any agent and understand why they scored differently.\n\
                     3. Synthesize a NEW solution that combines the strengths of all proposals.\n\
                     4. Do NOT simply repeat the previous winner. Innovate or Refine.{structured_instructions}"
                )
            };

            let thought_process_instruction = if self.textual_feedback {
                "Your `thought_process` MUST begin with: \"**Critique Integration:** [Explain how you used peer feedback]...\""
            } else {
                ""
            };

            format!(
                "<task>\"{task_description}\"</task>\n\n\
                <previous_round_matrix>\n\
                {matrix}\n\
                </previous_round_matrix>\n\
                {own_proposal_section}\
                {deliberation_brief}\
                {critiques_section}\
                {user_updates_section}\
                <instructions>\n\
                {instructions_feedback}\n\
                \n\
                When you are CERTAIN you have the best possible solution, call `submit_proposal` with `thought_process` and `solution_content`.\n\
                {thought_process_instruction}\n\
                Do NOT return plain text.\n\
                </instructions>"
            )
        } else {
            format!(
                "<task>\"{task_description}\"</task>\n\n\
                {user_updates_section}\
                INSTRUCTIONS:\n\
                Solve the provided task. \
                Think step-by-step. Use the `update_scratchpad` tool to break the problem down.\n\
                \n\
                When you are CERTAIN you have the best possible solution, call `submit_proposal` with `thought_process` and `solution_content`.
                Do NOT return plain text."
            )
        }
    }

    fn get_batch_evaluator_prompt(
        &self,
        task_description: &str,
        candidates: &[crate::agents::CandidateProposal],
        own_current_proposal: Option<&Proposal>,
        current_round: usize,
        user_injections: &[UserInjection],
    ) -> String {
        let valid_ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
        let valid_ids_str = valid_ids.join(", ");

        let mut candidates_text = String::new();
        // Inline full thought processes to avoid wasting iterations on individual read_proposal calls.
        // Only truncate if a single candidate's thoughts exceed a generous limit.
        let thought_limit = 4000;
        for candidate in candidates {
            let thought_chars = candidate.proposal.thought_process.chars().count();
            let (thoughts_text, truncated) = if thought_chars > thought_limit {
                let truncated_text: String = candidate
                    .proposal
                    .thought_process
                    .chars()
                    .take(thought_limit)
                    .collect();
                (format!("{truncated_text}..."), true)
            } else {
                (candidate.proposal.thought_process.clone(), false)
            };

            candidates_text.push_str(&format!(
                "<candidate id=\"{}\">\nFINAL SOLUTION: {}\nTHOUGHT PROCESS: {}{}\n</candidate>\n\n",
                candidate.id,
                candidate.proposal.content,
                thoughts_text,
                if truncated {
                    format!("\n(Truncated — use `read_proposal(round={current_round}, agent_id=\"{}\")` with offset={thought_limit} to read the rest)", candidate.id)
                } else {
                    String::new()
                }
            ));
        }

        let own_proposal_section = if let Some(p) = own_current_proposal {
            let truncated_thought = if p.thought_process.chars().count() > 500 {
                format!(
                    "{}...",
                    p.thought_process.chars().take(500).collect::<String>()
                )
            } else {
                p.thought_process.clone()
            };

            format!(
                "\n<own_current_proposal>\n\
                You just proposed the following solution:\n\
                THOUGHT PROCESS (Preview): {}\n\
                CONTENT: {}\n\
                (Use this as a baseline. If a candidate disagrees with you, verify who is correct. Do not blindly accept a different answer unless you find a flaw in your own reasoning.)\n\
                </own_current_proposal>\n\n",
                truncated_thought, p.content
            )
        } else {
            String::new()
        };

        let user_updates_section = render_user_updates(user_injections, "evaluation");

        // Build evaluation focus section for round 2+ to direct dispute resolution
        let evaluation_focus_section = if current_round > 1 {
            format!(
                "<evaluation_focus>\n\
                This is round {current_round}. Prioritize dispute resolution over re-evaluation from scratch.\n\
                * Use `search_deliberation` with `rounds: [{prev_round}]` and `verdicts: [\"contested\", \"wrong\"]` to find claims that were disputed last round.\n\
                * For each candidate: check whether previously contested claims have been ADDRESSED or PERSISTED in their updated proposal.\n\
                * **Resolved claims**: If a previously contested claim is now corrected or supported with evidence, mark it as `verified` and reference its `claim_id`.\n\
                * **Still contested**: If a disputed claim persists unchanged, re-assert your disagreement with the same `claim_id` for cross-round tracking.\n\
                * **New claims**: Evaluate new arguments or changes on their own merit.\n\
                * Focus your deepest analysis on HIGH CONTROVERSY candidates (those with wide score variance in previous rounds).\n\
                </evaluation_focus>\n\n",
                prev_round = current_round - 1,
            )
        } else {
            String::new()
        };

        format!(
            "Assess the following proposals in response to the user's task.\n\n\
            {evaluation_focus_section}\
            <instructions>\n\
            1. **Inline Review**: Each candidate's full thought process is provided below. Read them carefully BEFORE evaluating.
               * Only use `read_proposal(round={current_round}, agent_id)` if a thought process was truncated and you need the rest.
               * Do NOT call `read_proposal` for every candidate — their reasoning is already inline.
            2. **Vector Alignment**: Apply the strategy defined in your System Prompt.
               * Identify the Delta. Verify the Delta. Vote accordingly.
            3. **Self-Reflection**: Use `read_own_proposal()` to review your own reasoning if needed.
               * DO NOT evaluate yourself. Only evaluate the following CANDIDATES: [{valid_ids_str}]

            4. Assign an **endorsement_weight** (-100 to +100) to each proposal. Positive = support, negative = oppose, 0 = genuinely neutral.
               * -100 to -50: Demonstrably wrong, hallucinated, or contains critical logical errors.
               * -50 to -10:  Major flaws or unverified claims that undermine the solution.
               * -10 to +10:  Uncertain / no strong opinion either way. Use 0 if genuinely neutral.
               * +10 to +50:  Solid reasoning with minor differences from your own verification.
               * +50 to +85:  Strong, well-verified logic with only trivial concerns.
               * +85 to +100: RIGOROUSLY VERIFIED correct — you traced every step and found NO flaws.
               * WARNING: Scores > +90 require absolute proof. Do not give +100 just because they agree with you.

            5. For EACH candidate, provide **structured analysis**:
               * **stance**: your overall position (strong_agree/agree/neutral/disagree/strong_disagree)
               * **claim_assessments**: The 2-3 MOST PIVOTAL claims only. `claim` MUST be an EXACT verbatim substring copy-pasted from the proposal (never paraphrase/summarize — the client highlights by substring match, so a reworded claim can't be located), plus a verdict (verified/contested/unverified/wrong). Do NOT exhaustively list minor details.
               * **disagreements**: For contested/wrong claims only — what the proposal claims vs. what you believe, with your confidence (high/medium/low).
               * **category_scores**: Break your endorsement into the five axes below (each -100 to +100, same scale as endorsement_weight — negative undermines the proposal, positive supports it).
                  - **correctness**: technical claims true; cited file:line and anchors match reality.
                  - **completeness**: User intent deliverable coverage. Does it cover every section TEAM is tasked to work at (deliverables / sections / output schema)?
                  - **novelty**: findings the peer surfaced that other agents missed; cross-cutting observations score higher than restated single-subsystem claims.
                  - **feasibility**: auto-fix sketches actually compile, match in-tree precedent, don't introduce regressions.
                  - **evidence_quality**: anchors are verbatim and locatable; paraphrased / fabricated anchors drop heavily.
               * If a claim was flagged in a previous round (has a claim_id), include that claim_id so we can track resolution across rounds.

            6. Call `submit_batch_evaluation` with your evaluations.
               * CRITICAL: The `agent_id` field MUST be one of these EXACT strings: [{valid_ids_str}]
               * Do NOT use generic names like \"agent_1\" or \"candidate_1\". Use the exact candidate IDs shown in the candidate tags above.
            7. **Be efficient**: Evaluate all candidates and submit in as few steps as possible. Do not waste iterations on unnecessary tool calls.\n\
            </instructions>\n\n\
            <task>\"{task_description}\"</task>\n\
            {user_updates_section}\
            {own_proposal_section}\
            <candidates>\n\
            {candidates_text}\n\
            </candidates>"
        )
    }

    fn get_proposer_delta_prompt(
        &self,
        _task_description: &str,
        previous_round_matrix: Option<String>,
        previous_own_proposal: Option<&Proposal>,
        previous_score: Option<f32>,
        previous_critiques: Vec<String>,
        user_injections: &[UserInjection],
        structured_feedback: Option<&crate::agents::StructuredFeedback>,
    ) -> String {
        // Delta prompt: only new data. Task description and general instructions
        // are already in the persistent Claude session from round 1.
        let user_updates_section = render_user_updates(user_injections, "proposing");

        let deliberation_brief = if let Some(sf) = structured_feedback {
            let mut brief = String::from("<deliberation_brief>\n");
            if !sf.contested_claims.is_empty() {
                brief.push_str(&format!(
                    "  <contested_claims count=\"{}\">\n",
                    sf.contested_claims.len()
                ));
                for cc in &sf.contested_claims {
                    brief.push_str(&format!(
                        "    <dispute id=\"{}\" evaluator=\"{}\" confidence=\"{:?}\">\n      <your_claim>{}</your_claim>\n      <counter>{}</counter>\n    </dispute>\n",
                        cc.claim_id, cc.evaluator, cc.confidence, cc.what_you_claimed, cc.counter_position
                    ));
                }
                brief.push_str("  </contested_claims>\n");
            }
            if !sf.verified_claims.is_empty() {
                brief.push_str(&format!(
                    "  <verified_claims>{}</verified_claims>\n",
                    sf.verified_claims.join(", ")
                ));
            }
            if let Some(ref cb) = sf.category_breakdown {
                brief.push_str(&format!(
                    "  <category_breakdown>correctness: {:.2} | completeness: {:.2} | novelty: {:.2} | feasibility: {:.2} | evidence_quality: {:.2}</category_breakdown>\n",
                    cb.correctness, cb.completeness, cb.novelty, cb.feasibility, cb.evidence_quality
                ));
            }
            brief.push_str("  <guidance>\n");
            brief.push_str(
                "    For each contested claim: if the counter-argument is correct, FIX your approach; if your original claim is correct, STRENGTHEN your proof.\n",
            );
            if sf.category_breakdown.is_some() {
                brief.push_str("    Focus on your weakest category score.\n");
            }
            brief.push_str("  </guidance>\n");
            brief.push_str("</deliberation_brief>\n\n");
            brief
        } else {
            String::new()
        };

        let matrix_section = if let Some(matrix) = previous_round_matrix {
            format!("<previous_round_matrix>\n{matrix}\n</previous_round_matrix>\n\n")
        } else {
            // Round 1 — fall back to full prompt (shouldn't happen for delta)
            return self.get_proposer_prompt(
                _task_description,
                None,
                previous_own_proposal,
                previous_score,
                previous_critiques,
                user_injections,
                structured_feedback,
            );
        };

        let score_section = if let Some(score) = previous_score {
            let advice = if score < 0.5 {
                "LOW support. Strengthen proofs or rethink approach."
            } else if score < 0.8 {
                "Moderate support. Address critiques."
            } else {
                "High support. Iterate to maintain lead."
            };
            format!("YOUR PREVIOUS SCORE: {score:.2} / 1.0 — {advice}\n\n")
        } else {
            String::new()
        };

        let critiques_section = if !previous_critiques.is_empty() {
            let combined = previous_critiques.join("\n\n");
            format!("<peer_critiques>\n{combined}\n</peer_critiques>\n\n")
        } else {
            String::new()
        };

        format!(
            "Revise your proposal based on new feedback. The task and instructions are unchanged from round 1.\n\n\
            {score_section}\
            {matrix_section}\
            {deliberation_brief}\
            {critiques_section}\
            {user_updates_section}\
            Call `submit_proposal` with your revised `thought_process` and `solution_content` when ready."
        )
    }

    fn get_evaluator_delta_prompt(
        &self,
        _task_description: &str,
        candidates: &[crate::agents::CandidateProposal],
        own_current_proposal: Option<&Proposal>,
        current_round: usize,
        user_injections: &[UserInjection],
    ) -> String {
        // Delta prompt: only new candidates + evaluation focus. Scoring rubric
        // and general instructions are already in the persistent session.
        let valid_ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
        let valid_ids_str = valid_ids.join(", ");

        let mut candidates_text = String::new();
        let thought_limit = 4000;
        for candidate in candidates {
            let thought_chars = candidate.proposal.thought_process.chars().count();
            let (thoughts_text, truncated) = if thought_chars > thought_limit {
                let truncated_text: String = candidate
                    .proposal
                    .thought_process
                    .chars()
                    .take(thought_limit)
                    .collect();
                (format!("{truncated_text}..."), true)
            } else {
                (candidate.proposal.thought_process.clone(), false)
            };
            candidates_text.push_str(&format!(
                "<candidate id=\"{}\">\nFINAL SOLUTION: {}\nTHOUGHT PROCESS: {}{}\n</candidate>\n\n",
                candidate.id,
                candidate.proposal.content,
                thoughts_text,
                if truncated {
                    format!("\n(Truncated — use `read_proposal(round={current_round}, agent_id=\"{}\")` with offset={thought_limit} to read the rest)", candidate.id)
                } else {
                    String::new()
                }
            ));
        }

        let own_proposal_section = if let Some(p) = own_current_proposal {
            let truncated_thought = if p.thought_process.chars().count() > 500 {
                format!(
                    "{}...",
                    p.thought_process.chars().take(500).collect::<String>()
                )
            } else {
                p.thought_process.clone()
            };
            format!(
                "<own_current_proposal>\nTHOUGHT PROCESS (Preview): {}\nCONTENT: {}\n</own_current_proposal>\n\n",
                truncated_thought, p.content
            )
        } else {
            String::new()
        };

        let user_updates_section = render_user_updates(user_injections, "evaluation");

        let evaluation_focus = if current_round > 1 {
            format!(
                "<evaluation_focus>\n\
                This is round {current_round}. Prioritize dispute resolution over re-evaluation from scratch.\n\
                * Use `search_deliberation` with `rounds: [{prev_round}]` and `verdicts: [\"contested\", \"wrong\"]` to find claims that were disputed last round.\n\
                * For each candidate: check whether previously contested claims have been ADDRESSED or PERSISTED in their updated proposal.\n\
                * **Resolved claims**: If a previously contested claim is now corrected or supported with evidence, mark it as `verified` and reference its `claim_id`.\n\
                * **Still contested**: If a disputed claim persists unchanged, re-assert your disagreement with the same `claim_id` for cross-round tracking.\n\
                * **New claims**: Evaluate new arguments or changes on their own merit.\n\
                * Focus your deepest analysis on HIGH CONTROVERSY candidates (those with wide score variance in previous rounds).\n\
                </evaluation_focus>\n\n",
                prev_round = current_round - 1,
            )
        } else {
            String::new()
        };

        format!(
            "Evaluate updated candidates. Scoring rubric and instructions are unchanged.\n\n\
            {evaluation_focus}\
            {user_updates_section}\
            {own_proposal_section}\
            CANDIDATES: [{valid_ids_str}]\n\
            <candidates>\n\
            {candidates_text}\n\
            </candidates>\n\n\
            Call `submit_batch_evaluation` when ready."
        )
    }

    fn get_summarizer_prompt(&self, task_description: &str, proposal_content: &str) -> String {
        format!(
            "Summarize the following proposal into a concise, comparative overview (approx. 3-5 sentences). \
            Focus on the architectural approach, key trade-offs, and unique features. \
            Do not include generic fluff. This summary will be used to compare against other proposals.\n\n\
            USER TASK: \"{task_description}\"\n\n\
            PROPOSAL CONTENT:\n---\n{proposal_content}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proposer_prompt_without_matrix() {
        let prompt_set = DefaultPromptSet::new();
        let prompt = prompt_set.get_proposer_prompt("Do task", None, None, None, vec![], &[], None);
        assert!(!prompt.contains("<previous_round_matrix>"));
        assert!(prompt.contains("<task>\"Do task\"</task>"));
    }

    #[test]
    fn test_proposer_prompt_with_matrix() {
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Table |".to_string();
        let prompt =
            prompt_set.get_proposer_prompt("Do task", Some(matrix), None, None, vec![], &[], None);
        assert!(prompt.contains("<previous_round_matrix>"));
        assert!(prompt.contains("| Table |"));
        assert!(prompt.contains("<instructions>"));
    }

    #[test]
    fn test_proposer_prompt_with_critiques() {
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Table |".to_string();
        let critiques = vec!["Critique 1".to_string(), "Critique 2".to_string()];
        let prompt = prompt_set.get_proposer_prompt(
            "Do task",
            Some(matrix),
            None,
            None,
            critiques,
            &[],
            None,
        );
        assert!(prompt.contains("<peer_critiques>"));
        assert!(prompt.contains("Critique 1"));
        assert!(prompt.contains("Critique 2"));
    }

    #[test]
    fn test_system_message_is_static_across_round_and_phase() {
        let ps = DefaultPromptSet::new();
        // The system message MUST be byte-identical regardless of round/phase so the
        // cached prefix is reused every turn — the whole point of the split.
        let a = ps.get_system_message("agent", 1, 5, DeliberationPhase::Proposing);
        let b = ps.get_system_message("agent", 4, 5, DeliberationPhase::Evaluating);
        let c = ps.get_system_message("agent", 5, 5, DeliberationPhase::ConsensusCheck);
        assert_eq!(a, b);
        assert_eq!(a, c);
        // Both strategies are always present (the user message says which applies);
        // no per-turn status leaks into the system prefix.
        assert!(a.contains("strategy phase=\"proposing\""));
        assert!(a.contains("strategy phase=\"evaluating\""));
        assert!(!a.contains("Current Status"));
        assert!(!a.contains("Round 1 of"));
    }

    #[test]
    fn test_turn_header_carries_round_phase_and_tool() {
        let ps = DefaultPromptSet::new();
        let p = ps.get_turn_header(2, 5, DeliberationPhase::Proposing);
        assert!(p.contains("Round 2 of 5"));
        assert!(p.contains("Phase: Proposing"));
        assert!(p.contains("nsed_propose"));
        assert!(p.contains("phase=\"proposing\""));
        let e = ps.get_turn_header(3, 5, DeliberationPhase::Evaluating);
        assert!(e.contains("Phase: Evaluating"));
        assert!(e.contains("nsed_evaluate"));
        // ConsensusCheck maps to the evaluating strategy + tool.
        let c = ps.get_turn_header(5, 5, DeliberationPhase::ConsensusCheck);
        assert!(c.contains("Phase: Consensus Check"));
        assert!(c.contains("phase=\"evaluating\""));
        assert!(c.contains("nsed_evaluate"));
    }

    #[test]
    fn test_batch_evaluator_prompt() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();

        let candidates = vec![CandidateProposal {
            id: "agent_1".to_string(),
            proposal: Proposal {
                thought_process: "Thinking...".to_string(),
                content: "Cont1".to_string(),
                ..Default::default()
            },
        }];

        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 1, &[]);
        assert!(prompt.contains("<candidate id=\"agent_1\">"));
        assert!(prompt.contains("Cont1"));
        assert!(prompt.contains("<task>\"Task\"</task>"));
    }

    #[test]
    fn test_batch_evaluator_prompt_rubric_contract() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();
        let candidates = vec![CandidateProposal {
            id: "agent_1".to_string(),
            proposal: Proposal {
                thought_process: "t".to_string(),
                content: "c".to_string(),
                ..Default::default()
            },
        }];
        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 1, &[]);

        for axis in [
            "**correctness**",
            "**completeness**",
            "**novelty**",
            "**feasibility**",
            "**evidence_quality**",
        ] {
            assert!(
                prompt.contains(axis),
                "rubric axis {axis} missing from batch evaluator prompt"
            );
        }

        assert!(
            prompt.contains("User intent deliverable coverage"),
            "completeness must frame user intent as whole-prompt, not subsystem depth"
        );
    }

    #[test]
    fn test_summarizer_prompt() {
        let prompt_set = DefaultPromptSet::new();
        let prompt = prompt_set.get_summarizer_prompt("Task", "Content");
        assert!(prompt.contains("Summarize the following proposal"));
        assert!(prompt.contains("Content"));
    }

    #[test]
    fn test_default_prompt_set_builder() {
        let prompt_set = DefaultPromptSet::new()
            .with_persona("Expert mathematician".to_string())
            .with_textual_feedback(false);

        assert_eq!(prompt_set.persona, Some("Expert mathematician".to_string()));
        assert!(!prompt_set.textual_feedback);
    }

    #[test]
    fn test_system_message_with_persona() {
        let prompt_set = DefaultPromptSet::new()
            .with_persona("Security expert specializing in cryptography".to_string());

        let msg = prompt_set.get_system_message("agent", 1, 3, DeliberationPhase::Proposing);

        assert!(msg.contains("Security expert specializing in cryptography"));
        // Round + phase are NOT in the system message — they live in the turn header.
        assert!(!msg.contains("Phase: Proposing"));
        assert!(!msg.contains("Round 1 of 3"));
    }

    #[test]
    fn test_system_message_proposing_phase() {
        let prompt_set = DefaultPromptSet::new();

        let msg = prompt_set.get_system_message("test_agent", 2, 5, DeliberationPhase::Proposing);

        assert!(msg.contains("test_agent"));
        assert!(msg.contains("strategy phase=\"proposing\""));
        assert!(msg.contains("Plan First"));
        assert!(msg.contains("Critique Integration"));
        // Round moved to the turn header — not in the static system message.
        assert!(!msg.contains("Round 2 of 5"));
    }

    #[test]
    fn test_system_message_evaluating_phase() {
        let prompt_set = DefaultPromptSet::new();

        let msg = prompt_set.get_system_message("evaluator", 3, 5, DeliberationPhase::Evaluating);

        assert!(msg.contains("strategy phase=\"evaluating\""));
        assert!(msg.contains("Vector Alignment"));
        assert!(msg.contains("SKEPTICISM"));
        assert!(msg.contains("Harsh Scoring"));
    }

    #[test]
    fn test_proposer_prompt_with_previous_proposal_low_score() {
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n|-------|-------|\n| A | 0.3 |".to_string();
        let prev_proposal = Proposal {
            thought_process: "My previous thinking...".to_string(),
            content: "Answer: 42".to_string(),
            ..Default::default()
        };

        let prompt = prompt_set.get_proposer_prompt(
            "Solve the equation",
            Some(matrix),
            Some(&prev_proposal),
            Some(0.3), // Low score
            vec![],
            &[],
            None,
        );

        assert!(prompt.contains("<previous_own_proposal>"));
        assert!(prompt.contains("0.30 / 1.0"));
        assert!(prompt.contains("LOW support"));
        assert!(prompt.contains("fundamentally rethink"));
    }

    #[test]
    fn test_proposer_prompt_with_previous_proposal_moderate_score() {
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n|-------|-------|\n| A | 0.6 |".to_string();
        let prev_proposal = Proposal {
            thought_process: "My previous thinking...".to_string(),
            content: "Answer: 42".to_string(),
            ..Default::default()
        };

        let prompt = prompt_set.get_proposer_prompt(
            "Solve the equation",
            Some(matrix),
            Some(&prev_proposal),
            Some(0.6), // Moderate score
            vec![],
            &[],
            None,
        );

        assert!(prompt.contains("Moderate support"));
        assert!(prompt.contains("Address critiques"));
    }

    #[test]
    fn test_proposer_prompt_with_previous_proposal_high_score() {
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n|-------|-------|\n| A | 0.9 |".to_string();
        let prev_proposal = Proposal {
            thought_process: "My previous thinking...".to_string(),
            content: "Answer: 42".to_string(),
            ..Default::default()
        };

        let prompt = prompt_set.get_proposer_prompt(
            "Solve the equation",
            Some(matrix),
            Some(&prev_proposal),
            Some(0.9), // High score
            vec![],
            &[],
            None,
        );

        assert!(prompt.contains("High support"));
        assert!(prompt.contains("leading"));
    }

    #[test]
    fn test_proposer_prompt_with_long_thought_process_truncation() {
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Table |".to_string();

        // Create a proposal with thought process > 500 chars
        let long_thought = "A".repeat(600);
        let prev_proposal = Proposal {
            thought_process: long_thought.clone(),
            content: "Answer".to_string(),
            ..Default::default()
        };

        let prompt = prompt_set.get_proposer_prompt(
            "Task",
            Some(matrix),
            Some(&prev_proposal),
            None,
            vec![],
            &[],
            None,
        );

        // Should be truncated with "..."
        assert!(prompt.contains("..."));
        // Should NOT contain the full 600 chars
        assert!(!prompt.contains(&long_thought));
    }

    #[test]
    fn test_proposer_prompt_textual_feedback_disabled() {
        let prompt_set = DefaultPromptSet::new().with_textual_feedback(false);
        let matrix = "| Table |".to_string();

        let prompt =
            prompt_set.get_proposer_prompt("Task", Some(matrix), None, None, vec![], &[], None);

        // Should not contain textual feedback instructions
        assert!(!prompt.contains("Critique Integration"));
        assert!(prompt.contains("read_proposal"));
    }

    #[test]
    fn test_batch_evaluator_prompt_with_own_proposal() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();

        let candidates = vec![CandidateProposal {
            id: "other_agent".to_string(),
            proposal: Proposal {
                thought_process: "Their thinking".to_string(),
                content: "Their answer".to_string(),
                ..Default::default()
            },
        }];

        let own_proposal = Proposal {
            thought_process: "My own thinking".to_string(),
            content: "My answer".to_string(),
            ..Default::default()
        };

        let prompt = prompt_set.get_batch_evaluator_prompt(
            "Evaluate these",
            &candidates,
            Some(&own_proposal),
            2,
            &[],
        );

        assert!(prompt.contains("<own_current_proposal>"));
        assert!(prompt.contains("My answer"));
        assert!(prompt.contains("baseline"));
        assert!(prompt.contains("round=2"));
    }

    #[test]
    fn test_batch_evaluator_prompt_truncates_long_thoughts() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();

        // Create a candidate with thought process > 4000 chars (the inline limit)
        let long_thought = "B".repeat(5000);
        let candidates = vec![CandidateProposal {
            id: "verbose_agent".to_string(),
            proposal: Proposal {
                thought_process: long_thought.clone(),
                content: "Answer".to_string(),
                ..Default::default()
            },
        }];

        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 1, &[]);

        // Should be truncated
        assert!(prompt.contains("..."));
        // Should contain the read_proposal hint for truncated content
        assert!(prompt.contains("read_proposal"));
        // Should NOT contain the full 5000 chars
        assert!(!prompt.contains(&long_thought));
    }

    #[test]
    fn test_batch_evaluator_prompt_inlines_short_thoughts() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();

        // Thoughts under 4000 chars should be fully inlined
        let thought = "Step 1: Check X. Step 2: Verify Y.".to_string();
        let candidates = vec![CandidateProposal {
            id: "concise_agent".to_string(),
            proposal: Proposal {
                thought_process: thought.clone(),
                content: "Result".to_string(),
                ..Default::default()
            },
        }];

        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 1, &[]);

        // Full thought should appear inline
        assert!(prompt.contains(&thought));
        // Should NOT say "Inline Review" with truncation note for this candidate
        assert!(!prompt.contains("Truncated"));
    }

    #[test]
    fn test_batch_evaluator_prompt_valid_ids() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();

        let candidates = vec![
            CandidateProposal {
                id: "agent_alpha".to_string(),
                proposal: Proposal::default(),
            },
            CandidateProposal {
                id: "agent_beta".to_string(),
                proposal: Proposal::default(),
            },
        ];

        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 1, &[]);

        // Should list valid candidate IDs
        assert!(prompt.contains("agent_alpha, agent_beta"));
    }

    #[test]
    fn test_system_message_contains_incentives() {
        let prompt_set = DefaultPromptSet::new();
        let msg = prompt_set.get_system_message("agent", 1, 1, DeliberationPhase::Proposing);

        assert!(msg.contains("<incentives>"));
        assert!(msg.contains("Cooperative Failure"));
        assert!(msg.contains("Competitive Success"));
    }

    #[test]
    fn test_system_message_contains_rules() {
        let prompt_set = DefaultPromptSet::new();
        let msg = prompt_set.get_system_message("agent", 1, 1, DeliberationPhase::Proposing);

        assert!(msg.contains("<rules>"));
        assert!(msg.contains("update_scratchpad"));
        assert!(msg.contains("read_proposal"));
    }

    // --- User Injection (Hot-Wire) Tests ---

    #[test]
    fn test_render_user_updates_empty() {
        let result = render_user_updates(&[], "proposing");
        assert!(result.is_empty());
    }

    #[test]
    fn test_render_user_updates_single() {
        use crate::agents::UserInjection;
        let injections = vec![UserInjection {
            message: "Focus on memory safety".to_string(),
            injected_at_round: 2,
            timestamp: 1000,
            ..Default::default()
        }];
        let result = render_user_updates(&injections, "proposing");
        assert!(result.contains("<user_updates>"));
        assert!(result.contains("</user_updates>"));
        assert!(result.contains("<update round=\"2\">Focus on memory safety</update>"));
        assert!(result.contains("Integrate these into your work"));
    }

    #[test]
    fn test_render_user_updates_multiple() {
        use crate::agents::UserInjection;
        let injections = vec![
            UserInjection {
                message: "First clarification".to_string(),
                injected_at_round: 1,
                timestamp: 1000,
                ..Default::default()
            },
            UserInjection {
                message: "Second clarification".to_string(),
                injected_at_round: 3,
                timestamp: 2000,
                ..Default::default()
            },
        ];
        let result = render_user_updates(&injections, "proposing");
        assert!(result.contains("<update round=\"1\">First clarification</update>"));
        assert!(result.contains("<update round=\"3\">Second clarification</update>"));
        // Verify ordering: round 1 before round 3
        let pos1 = result.find("round=\"1\"").unwrap();
        let pos3 = result.find("round=\"3\"").unwrap();
        assert!(pos1 < pos3);
    }

    #[test]
    fn test_render_user_updates_evaluation_context() {
        use crate::agents::UserInjection;
        let injections = vec![UserInjection {
            message: "Check edge cases".to_string(),
            injected_at_round: 1,
            timestamp: 1000,
            ..Default::default()
        }];
        let result = render_user_updates(&injections, "evaluation");
        assert!(result.contains("Factor these into your evaluation criteria"));
        assert!(!result.contains("Integrate these into your work"));
    }

    #[test]
    fn test_proposer_prompt_with_user_injections() {
        use crate::agents::UserInjection;
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Table |".to_string();
        let injections = vec![UserInjection {
            message: "Focus on latency optimization".to_string(),
            injected_at_round: 2,
            timestamp: 1000,
            ..Default::default()
        }];

        let prompt = prompt_set.get_proposer_prompt(
            "Optimize the system",
            Some(matrix),
            None,
            None,
            vec![],
            &injections,
            None,
        );

        assert!(prompt.contains("<user_updates>"));
        assert!(prompt.contains("Focus on latency optimization"));
        // Verify placement: user_updates before <instructions>
        let updates_pos = prompt.find("<user_updates>").unwrap();
        let instructions_pos = prompt.find("<instructions>").unwrap();
        assert!(updates_pos < instructions_pos);
    }

    #[test]
    fn test_proposer_prompt_first_round_with_injections() {
        use crate::agents::UserInjection;
        let prompt_set = DefaultPromptSet::new();
        let injections = vec![UserInjection {
            message: "Also consider Python 3.8".to_string(),
            injected_at_round: 1,
            timestamp: 1000,
            ..Default::default()
        }];

        let prompt = prompt_set.get_proposer_prompt(
            "Build a parser",
            None,
            None,
            None,
            vec![],
            &injections,
            None,
        );

        assert!(prompt.contains("<user_updates>"));
        assert!(prompt.contains("Also consider Python 3.8"));
        // Verify placement: after <task> but before INSTRUCTIONS
        let task_pos = prompt.find("</task>").unwrap();
        let updates_pos = prompt.find("<user_updates>").unwrap();
        let instr_pos = prompt.find("INSTRUCTIONS:").unwrap();
        assert!(task_pos < updates_pos);
        assert!(updates_pos < instr_pos);
    }

    #[test]
    fn test_evaluator_prompt_with_user_injections() {
        use crate::agents::{CandidateProposal, Proposal, UserInjection};
        let prompt_set = DefaultPromptSet::new();
        let candidates = vec![CandidateProposal {
            id: "agent_1".to_string(),
            proposal: Proposal {
                thought_process: "Thinking...".to_string(),
                content: "Solution".to_string(),
                ..Default::default()
            },
        }];
        let injections = vec![UserInjection {
            message: "Prioritize correctness over speed".to_string(),
            injected_at_round: 2,
            timestamp: 1000,
            ..Default::default()
        }];

        let prompt =
            prompt_set.get_batch_evaluator_prompt("Evaluate", &candidates, None, 2, &injections);

        assert!(prompt.contains("<user_updates>"));
        assert!(prompt.contains("Prioritize correctness over speed"));
        assert!(prompt.contains("Factor these into your evaluation criteria"));
        // Verify placement: after <task> and before <candidates>
        let task_pos = prompt.find("</task>").unwrap();
        let updates_pos = prompt.find("<user_updates>").unwrap();
        let candidates_pos = prompt.find("<candidates>").unwrap();
        assert!(task_pos < updates_pos);
        assert!(updates_pos < candidates_pos);
    }

    #[test]
    fn test_evaluator_prompt_evaluation_focus_round1_absent() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();
        let candidates = vec![CandidateProposal {
            id: "agent_1".to_string(),
            proposal: Proposal {
                thought_process: "Thinking...".to_string(),
                content: "Solution".to_string(),
                ..Default::default()
            },
        }];

        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 1, &[]);
        // Round 1 should NOT have evaluation_focus
        assert!(!prompt.contains("<evaluation_focus>"));
    }

    #[test]
    fn test_evaluator_prompt_evaluation_focus_round2_present() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();
        let candidates = vec![CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: Proposal {
                thought_process: "Revised reasoning".to_string(),
                content: "Updated solution".to_string(),
                ..Default::default()
            },
        }];

        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 2, &[]);
        // Round 2 should have evaluation_focus with dispute resolution guidance
        assert!(prompt.contains("<evaluation_focus>"));
        assert!(prompt.contains("round 2"));
        assert!(prompt.contains("search_deliberation"));
        assert!(prompt.contains("rounds: [1]"));
        assert!(prompt.contains("contested"));
        assert!(prompt.contains("claim_id"));
        assert!(prompt.contains("</evaluation_focus>"));

        // Verify placement: evaluation_focus before <instructions>
        let focus_pos = prompt.find("<evaluation_focus>").unwrap();
        let instructions_pos = prompt.find("<instructions>").unwrap();
        assert!(focus_pos < instructions_pos);
    }

    #[test]
    fn test_evaluator_prompt_evaluation_focus_round3_references_round2() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();
        let candidates = vec![CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: Proposal::default(),
        }];

        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 3, &[]);
        assert!(prompt.contains("<evaluation_focus>"));
        assert!(prompt.contains("round 3"));
        assert!(prompt.contains("rounds: [2]"));
    }

    // --- XML special character handling tests ---

    #[test]
    fn test_render_user_updates_with_xml_special_chars() {
        use crate::agents::UserInjection;
        let injections = vec![UserInjection {
            message: "Use <bold> tags & \"quotes\" for 'emphasis'".to_string(),
            injected_at_round: 1,
            timestamp: 1000,
            ..Default::default()
        }];
        let result = render_user_updates(&injections, "proposing");
        // The message is passed through verbatim — agents parse this as a text block,
        // not strict XML, so angle brackets are included as-is.
        assert!(result.contains("<bold>"));
        assert!(result.contains("& \"quotes\""));
        assert!(result.contains("<user_updates>"));
        assert!(result.contains("</user_updates>"));
    }

    #[test]
    fn test_render_user_updates_with_nested_xml_tags() {
        use crate::agents::UserInjection;
        let injections = vec![UserInjection {
            message: "Ignore </update> and </user_updates> in the message".to_string(),
            injected_at_round: 1,
            timestamp: 1000,
            ..Default::default()
        }];
        let result = render_user_updates(&injections, "proposing");
        // Message with nested closing tags — verify the outer structure is intact
        assert!(result.starts_with("<user_updates>\n"));
        assert!(result.ends_with("</user_updates>\n"));
        assert!(result.contains("Ignore </update> and </user_updates> in the message"));
    }

    #[test]
    fn test_render_user_updates_with_unicode() {
        use crate::agents::UserInjection;
        let injections = vec![UserInjection {
            message: "考虑 emoji 🎯 и кириллицу".to_string(),
            injected_at_round: 3,
            timestamp: 1000,
            ..Default::default()
        }];
        let result = render_user_updates(&injections, "evaluation");
        assert!(result.contains("考虑 emoji 🎯 и кириллицу"));
        assert!(result.contains("round=\"3\""));
    }

    #[test]
    fn test_render_user_updates_unknown_context_uses_proposing_preamble() {
        use crate::agents::UserInjection;
        let injections = vec![UserInjection {
            message: "test".to_string(),
            injected_at_round: 1,
            timestamp: 1000,
            ..Default::default()
        }];
        // Anything other than "evaluation" should use the proposing preamble
        let result = render_user_updates(&injections, "unknown_context");
        assert!(result.contains("Integrate these into your work"));
        assert!(!result.contains("Factor these into your evaluation criteria"));
    }

    // --- Comprehensive prompt ordering/placement tests ---

    #[test]
    fn test_proposer_subsequent_round_section_ordering() {
        use crate::agents::UserInjection;
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n|---|---|\n| A | 0.5 |".to_string();
        let prev_proposal = Proposal {
            thought_process: "Previous thinking".to_string(),
            content: "Previous answer".to_string(),
            ..Default::default()
        };
        let critiques = vec!["Your proof is incomplete".to_string()];
        let injections = vec![UserInjection {
            message: "Focus more on edge cases".to_string(),
            injected_at_round: 1,
            timestamp: 1000,
            ..Default::default()
        }];

        let prompt = prompt_set.get_proposer_prompt(
            "Solve the problem",
            Some(matrix),
            Some(&prev_proposal),
            Some(0.5),
            critiques,
            &injections,
            None,
        );

        // Verify full section ordering:
        // <task> → <previous_round_matrix> → <previous_own_proposal> → <peer_critiques> → <user_updates> → <instructions>
        let task_pos = prompt.find("<task>").unwrap();
        let matrix_pos = prompt.find("<previous_round_matrix>").unwrap();
        let own_pos = prompt.find("<previous_own_proposal>").unwrap();
        let critiques_pos = prompt.find("<peer_critiques>").unwrap();
        let updates_pos = prompt.find("<user_updates>").unwrap();
        let instructions_pos = prompt.find("<instructions>").unwrap();

        assert!(task_pos < matrix_pos, "task before matrix");
        assert!(matrix_pos < own_pos, "matrix before own_proposal");
        assert!(own_pos < critiques_pos, "own_proposal before critiques");
        assert!(critiques_pos < updates_pos, "critiques before user_updates");
        assert!(
            updates_pos < instructions_pos,
            "user_updates before instructions"
        );
    }

    #[test]
    fn test_proposer_subsequent_round_no_injections_omits_user_updates() {
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Table |".to_string();
        let prompt =
            prompt_set.get_proposer_prompt("Task", Some(matrix), None, None, vec![], &[], None);

        // No injections → no <user_updates> block at all
        assert!(!prompt.contains("<user_updates>"));
        assert!(!prompt.contains("</user_updates>"));
        // But <instructions> should still be present
        assert!(prompt.contains("<instructions>"));
    }

    #[test]
    fn test_proposer_first_round_no_injections_omits_user_updates() {
        let prompt_set = DefaultPromptSet::new();
        let prompt = prompt_set.get_proposer_prompt("Task", None, None, None, vec![], &[], None);

        assert!(!prompt.contains("<user_updates>"));
        assert!(prompt.contains("INSTRUCTIONS:"));
    }

    #[test]
    fn test_evaluator_section_ordering_with_own_proposal_and_injections() {
        use crate::agents::{CandidateProposal, Proposal, UserInjection};
        let prompt_set = DefaultPromptSet::new();
        let candidates = vec![CandidateProposal {
            id: "agent_x".to_string(),
            proposal: Proposal {
                thought_process: "Thinking".to_string(),
                content: "Answer".to_string(),
                ..Default::default()
            },
        }];
        let own_proposal = Proposal {
            thought_process: "My thinking".to_string(),
            content: "My answer".to_string(),
            ..Default::default()
        };
        let injections = vec![UserInjection {
            message: "Consider performance".to_string(),
            injected_at_round: 2,
            timestamp: 1000,
            ..Default::default()
        }];

        let prompt = prompt_set.get_batch_evaluator_prompt(
            "Evaluate task",
            &candidates,
            Some(&own_proposal),
            2,
            &injections,
        );

        // Full ordering: <instructions> → <task> → <user_updates> → <own_current_proposal> → <candidates>
        let instructions_pos = prompt.find("<instructions>").unwrap();
        let task_pos = prompt.find("<task>").unwrap();
        let updates_pos = prompt.find("<user_updates>").unwrap();
        let own_pos = prompt.find("<own_current_proposal>").unwrap();
        let candidates_pos = prompt.find("<candidates>").unwrap();

        assert!(instructions_pos < task_pos, "instructions before task");
        assert!(task_pos < updates_pos, "task before user_updates");
        assert!(updates_pos < own_pos, "user_updates before own_proposal");
        assert!(own_pos < candidates_pos, "own_proposal before candidates");
    }

    #[test]
    fn test_evaluator_no_injections_omits_user_updates() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();
        let candidates = vec![CandidateProposal {
            id: "agent_1".to_string(),
            proposal: Proposal::default(),
        }];

        let prompt = prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 1, &[]);

        assert!(!prompt.contains("<user_updates>"));
        assert!(prompt.contains("<candidates>"));
    }

    #[test]
    fn test_evaluator_no_own_proposal_omits_section() {
        use crate::agents::{CandidateProposal, Proposal, UserInjection};
        let prompt_set = DefaultPromptSet::new();
        let candidates = vec![CandidateProposal {
            id: "agent_1".to_string(),
            proposal: Proposal::default(),
        }];
        let injections = vec![UserInjection {
            message: "test".to_string(),
            injected_at_round: 1,
            timestamp: 1000,
            ..Default::default()
        }];

        let prompt =
            prompt_set.get_batch_evaluator_prompt("Task", &candidates, None, 1, &injections);

        assert!(prompt.contains("<user_updates>"));
        assert!(!prompt.contains("<own_current_proposal>"));
        // user_updates should appear directly before candidates when no own_proposal
        let updates_end = prompt.find("</user_updates>").unwrap();
        let candidates_pos = prompt.find("<candidates>").unwrap();
        assert!(updates_end < candidates_pos);
    }

    // --- Structured Feedback Tests ---

    #[test]
    fn test_get_proposer_prompt_with_structured_feedback() {
        use crate::agents::{Confidence, ContestedClaim, StructuredFeedback};

        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n|---|---|\n| A | 0.7 |".to_string();

        let feedback = StructuredFeedback {
            contested_claims: vec![ContestedClaim {
                claim_id: "claim_42".to_string(),
                what_you_claimed: "Rust is always faster than C".to_string(),
                counter_position: "C can match Rust with careful optimization".to_string(),
                evaluator: "Bob".to_string(),
                confidence: Confidence::High,
            }],
            verified_claims: vec![
                "Memory safety without GC".to_string(),
                "Zero-cost abstractions".to_string(),
            ],
            mean_stance: 0.5,
            evaluator_count: 3,
            category_breakdown: None,
        };

        let prompt = prompt_set.get_proposer_prompt(
            "Compare Rust and C",
            Some(matrix),
            None,
            None,
            vec![],
            &[],
            Some(&feedback),
        );

        // Verify the deliberation brief section is present
        assert!(
            prompt.contains("<deliberation_brief>"),
            "should contain deliberation_brief section"
        );
        assert!(
            prompt.contains("</deliberation_brief>"),
            "should close deliberation_brief section"
        );

        // Verify contested claims appear
        assert!(
            prompt.contains("<contested_claims count=\"1\">"),
            "should have contested_claims with correct count"
        );
        assert!(prompt.contains("claim_42"), "should contain the claim_id");
        assert!(
            prompt.contains("Rust is always faster than C"),
            "should contain the original claim"
        );
        assert!(
            prompt.contains("C can match Rust with careful optimization"),
            "should contain the counter position"
        );
        assert!(prompt.contains("Bob"), "should contain the evaluator name");

        // Verify verified claims appear
        assert!(
            prompt.contains("<verified_claims>"),
            "should contain verified_claims section"
        );
        assert!(
            prompt.contains("Memory safety without GC"),
            "should contain the first verified claim"
        );
        assert!(
            prompt.contains("Zero-cost abstractions"),
            "should contain the second verified claim"
        );

        // Verify structured instructions are present when feedback is provided
        assert!(
            prompt.contains("contested claim"),
            "should contain instructions about contested claims"
        );
    }

    #[test]
    fn test_get_proposer_prompt_without_feedback() {
        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n|---|---|\n| A | 0.7 |".to_string();

        let prompt = prompt_set.get_proposer_prompt(
            "Build a parser",
            Some(matrix),
            None,
            None,
            vec![],
            &[],
            None, // No structured feedback
        );

        // Should still produce a valid prompt
        assert!(prompt.contains("<task>"), "should contain task section");
        assert!(
            prompt.contains("Build a parser"),
            "should contain the task description"
        );

        // Should NOT contain structured feedback sections
        assert!(
            !prompt.contains("<deliberation_brief>"),
            "should not contain deliberation_brief when feedback is None"
        );
        assert!(
            !prompt.contains("<contested_claims"),
            "should not contain contested_claims when feedback is None"
        );
        assert!(
            !prompt.contains("<verified_claims>"),
            "should not contain verified_claims when feedback is None"
        );

        // Should still have instructions
        assert!(
            prompt.contains("<instructions>"),
            "should still have instructions section"
        );
    }

    #[test]
    fn test_prompt_reasonable_length() {
        use crate::agents::{
            CandidateProposal, Confidence, ContestedClaim, StructuredFeedback, UserInjection,
        };

        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n|-------|-------|\n| A | 0.9 |".to_string();
        let prev_proposal = Proposal {
            thought_process: "X".repeat(500),
            content: "Y".repeat(300),
            ..Default::default()
        };
        let critiques = vec!["Critique A".to_string(), "Critique B".to_string()];
        let injections = vec![UserInjection {
            message: "Consider edge cases".to_string(),
            injected_at_round: 1,
            timestamp: 1000,
            ..Default::default()
        }];
        let feedback = StructuredFeedback {
            contested_claims: vec![ContestedClaim {
                claim_id: "c1".to_string(),
                what_you_claimed: "Claim text".to_string(),
                counter_position: "Counter text".to_string(),
                evaluator: "Eval".to_string(),
                confidence: Confidence::Medium,
            }],
            verified_claims: vec!["Verified claim".to_string()],
            mean_stance: 1.0,
            evaluator_count: 2,
            category_breakdown: None,
        };

        // Test proposer prompt length
        let proposer_prompt = prompt_set.get_proposer_prompt(
            "Solve a complex optimization problem with multiple constraints",
            Some(matrix),
            Some(&prev_proposal),
            Some(0.75),
            critiques,
            &injections,
            Some(&feedback),
        );

        assert!(
            proposer_prompt.chars().count() < 50000,
            "Proposer prompt should not exceed 50000 chars, got {}",
            proposer_prompt.chars().count()
        );

        // Test evaluator prompt length
        let candidates = vec![
            CandidateProposal {
                id: "agent_1".to_string(),
                proposal: Proposal {
                    thought_process: "Z".repeat(3000),
                    content: "Solution 1".to_string(),
                    ..Default::default()
                },
            },
            CandidateProposal {
                id: "agent_2".to_string(),
                proposal: Proposal {
                    thought_process: "W".repeat(3000),
                    content: "Solution 2".to_string(),
                    ..Default::default()
                },
            },
        ];

        let evaluator_prompt = prompt_set.get_batch_evaluator_prompt(
            "Evaluate the optimization proposals",
            &candidates,
            Some(&prev_proposal),
            2,
            &injections,
        );

        assert!(
            evaluator_prompt.chars().count() < 50000,
            "Evaluator prompt should not exceed 50000 chars, got {}",
            evaluator_prompt.chars().count()
        );

        // Test system message length
        let system_msg =
            prompt_set.get_system_message("test_agent", 3, 5, DeliberationPhase::Proposing);

        assert!(
            system_msg.chars().count() < 50000,
            "System message should not exceed 50000 chars, got {}",
            system_msg.chars().count()
        );
    }

    // =========================================================================
    // Structured Feedback with category_breakdown (uncovered path)
    // =========================================================================

    #[test]
    fn test_proposer_prompt_with_category_breakdown() {
        use crate::agents::{CategoryScores, Confidence, ContestedClaim, StructuredFeedback};

        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n|---|---|\n| A | 0.6 |".to_string();

        let feedback = StructuredFeedback {
            contested_claims: vec![ContestedClaim {
                claim_id: "c1".to_string(),
                what_you_claimed: "Claim A".to_string(),
                counter_position: "Counter A".to_string(),
                evaluator: "Evaluator1".to_string(),
                confidence: Confidence::High,
            }],
            verified_claims: vec!["Verified claim 1".to_string()],
            mean_stance: 0.6,
            evaluator_count: 2,
            category_breakdown: Some(CategoryScores {
                correctness: 0.85,
                completeness: 0.70,
                novelty: 0.50,
                feasibility: 0.90,
                evidence_quality: 0.65,
            }),
        };

        let prompt = prompt_set.get_proposer_prompt(
            "Analyze the data",
            Some(matrix),
            None,
            None,
            vec![],
            &[],
            Some(&feedback),
        );

        // Verify category_breakdown section is present
        assert!(
            prompt.contains("<category_breakdown>"),
            "Should contain category_breakdown section"
        );
        assert!(
            prompt.contains("correctness: 0.85"),
            "Should contain correctness score"
        );
        assert!(
            prompt.contains("completeness: 0.70"),
            "Should contain completeness score"
        );
        assert!(
            prompt.contains("novelty: 0.50"),
            "Should contain novelty score"
        );
        assert!(
            prompt.contains("feasibility: 0.90"),
            "Should contain feasibility score"
        );
        assert!(
            prompt.contains("evidence_quality: 0.65"),
            "Should contain evidence_quality score"
        );
        assert!(
            prompt.contains("</category_breakdown>"),
            "Should close category_breakdown section"
        );

        // Verify structured instructions about weakest category
        assert!(
            prompt.contains("weakest category score"),
            "Should contain instruction about weakest category"
        );
    }

    #[test]
    fn test_proposer_prompt_structured_feedback_no_category_breakdown() {
        use crate::agents::StructuredFeedback;

        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |".to_string();

        let feedback = StructuredFeedback {
            contested_claims: vec![],
            verified_claims: vec![],
            mean_stance: 0.5,
            evaluator_count: 1,
            category_breakdown: None, // No category breakdown
        };

        let prompt = prompt_set.get_proposer_prompt(
            "Task",
            Some(matrix),
            None,
            None,
            vec![],
            &[],
            Some(&feedback),
        );

        // Deliberation brief should still appear but without category_breakdown
        assert!(prompt.contains("<deliberation_brief>"));
        assert!(!prompt.contains("<category_breakdown>"));
    }

    #[test]
    fn test_proposer_prompt_structured_feedback_empty_contested_and_verified() {
        use crate::agents::StructuredFeedback;

        let prompt_set = DefaultPromptSet::new();
        let matrix = "| Agent | Score |".to_string();

        let feedback = StructuredFeedback {
            contested_claims: vec![],
            verified_claims: vec![],
            mean_stance: 0.0,
            evaluator_count: 0,
            category_breakdown: None,
        };

        let prompt = prompt_set.get_proposer_prompt(
            "Task",
            Some(matrix),
            None,
            None,
            vec![],
            &[],
            Some(&feedback),
        );

        // With empty contested and verified, those sub-sections should not appear
        assert!(!prompt.contains("<contested_claims"));
        assert!(!prompt.contains("<verified_claims>"));
        // But deliberation_brief wrapper should still be present
        assert!(prompt.contains("<deliberation_brief>"));
    }

    // =========================================================================
    // Batch evaluator — own proposal with long thought truncation
    // =========================================================================

    #[test]
    fn test_batch_evaluator_own_proposal_long_thought_truncation() {
        use crate::agents::{CandidateProposal, Proposal};
        let prompt_set = DefaultPromptSet::new();

        let candidates = vec![CandidateProposal {
            id: "peer_agent".to_string(),
            proposal: Proposal {
                thought_process: "Short thinking".to_string(),
                content: "Peer answer".to_string(),
                ..Default::default()
            },
        }];

        // Own proposal with thought process > 500 chars
        let long_thought = "C".repeat(700);
        let own_proposal = Proposal {
            thought_process: long_thought.clone(),
            content: "My solution".to_string(),
            ..Default::default()
        };

        let prompt = prompt_set.get_batch_evaluator_prompt(
            "Evaluate",
            &candidates,
            Some(&own_proposal),
            1,
            &[],
        );

        assert!(prompt.contains("<own_current_proposal>"));
        // Should be truncated: first 500 chars of "CCC...C" + "..."
        let expected_preview = format!("{}...", "C".repeat(500));
        assert!(
            prompt.contains(&expected_preview),
            "Long own proposal thought should be truncated to 500 chars + '...'"
        );
        // Should NOT contain the full 700-char thought
        assert!(!prompt.contains(&long_thought));
        assert!(prompt.contains("My solution"));
    }

    // =========================================================================
    // Static system message: phase never changes its content
    // =========================================================================

    #[test]
    fn test_system_message_static_for_consensus_check() {
        let prompt_set = DefaultPromptSet::new();
        let msg = prompt_set.get_system_message("agent", 1, 3, DeliberationPhase::ConsensusCheck);

        // The system message is static: BOTH strategy blocks are always present
        // regardless of the phase argument; the per-turn phase lives in the header.
        assert!(msg.contains("strategy phase=\"proposing\""));
        assert!(msg.contains("strategy phase=\"evaluating\""));
        assert!(msg.contains("<identity>"));
        assert!(msg.contains("<protocol>"));
        assert!(!msg.contains("Phase: Consensus Check"));
    }

    // ─── Delta prompt tests ────────────────────────────────────────────

    #[test]
    fn test_delta_proposer_prompt_omits_task() {
        let ps = DefaultPromptSet::new();
        let matrix = "| Agent | Score |\n| A | 0.8 |".to_string();
        let critiques = vec!["Your math is wrong".to_string()];
        let delta = ps.get_proposer_delta_prompt(
            "Build a spaceship",
            Some(matrix),
            None,
            Some(0.6),
            critiques,
            &[],
            None,
        );
        // Delta should NOT contain the task tag (already in session)
        assert!(
            !delta.contains("<task>"),
            "delta prompt should omit <task> tag"
        );
        // But should contain the matrix and critiques (new data)
        assert!(delta.contains("<previous_round_matrix>"));
        assert!(delta.contains("Your math is wrong"));
        assert!(delta.contains("0.60"));
        assert!(delta.contains("submit_proposal"));
    }

    #[test]
    fn test_delta_proposer_prompt_falls_back_for_round_1() {
        let ps = DefaultPromptSet::new();
        // No matrix = round 1 → delta falls back to full prompt
        let delta =
            ps.get_proposer_delta_prompt("Build a spaceship", None, None, None, vec![], &[], None);
        // Should contain task (full prompt fallback)
        assert!(delta.contains("<task>"));
    }

    #[test]
    fn test_delta_evaluator_prompt_omits_rubric() {
        let ps = DefaultPromptSet::new();
        let candidates = vec![crate::agents::CandidateProposal {
            id: "agent-1".to_string(),
            proposal: crate::agents::Proposal {
                thought_process: "I thought about it".to_string(),
                content: "My solution".to_string(),
                ..Default::default()
            },
        }];
        let delta = ps.get_evaluator_delta_prompt("Build a spaceship", &candidates, None, 2, &[]);
        // Delta should NOT contain the full scoring rubric
        assert!(
            !delta.contains("0-10: Demonstrably wrong"),
            "delta evaluator should omit scoring rubric"
        );
        // But should contain candidates and submit instruction
        assert!(delta.contains("<candidate id=\"agent-1\">"));
        assert!(delta.contains("submit_batch_evaluation"));
        assert!(delta.contains("<evaluation_focus>"));
    }

    #[test]
    fn test_delta_prompt_default_falls_back() {
        // The default trait impl should call the full prompt
        use crate::prompts::PromptSet;

        let ps = DefaultPromptSet::new();
        let full = ps.get_proposer_prompt("task", None, None, None, vec![], &[], None);
        let delta = ps.get_proposer_delta_prompt("task", None, None, None, vec![], &[], None);
        // For round 1 (no matrix), delta falls back to full
        assert_eq!(full, delta);
    }
}
