//! This module contains the implementation of the `NsedAgent`.
use crate::agents::ChatCapable;
use crate::agents::DeliberationPhase;
use crate::agents::normalize_score;
use crate::agents::{
    AgentConfig, AgentContext, CategoryScores, ClaimAssessment, DisagreementPoint, Evaluation,
    NsedAgent, Proposal, Stance, TokenUsage,
};
use crate::emit_for;
use crate::llms::LlmRequestSpan;
use crate::llms::{AiModel, RequestConfig};
use crate::prompts::PromptSet;
use crate::status::agent_events::{AgentEvent, AgentEventKind};
use crate::telemetry::RetryReason;
use crate::tools::Tool;
use crate::tools::context::{
    ReadCritiquesTool, ReadOwnProposalTool, ReadProposalTool, SearchDeliberationTool,
};
use crate::tools::user_call::UserCallTool;

use anyhow::{Context, Result};
use async_openai::types::{
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessage, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage, ChatCompletionTool,
    ChatCompletionToolType, FunctionObject,
};
use async_trait::async_trait;
use llm_repair::{
    clean_json_string, extract_evaluations_from_markdown, extract_proposal_from_markdown,
    extract_python_tool_calls, extract_xml_tool_calls, repair_aggressive_escapes,
    repair_conversational_response, repair_invalid_escapes, repair_tool_calls,
    repair_truncated_json, sanitize_json_string_lossy,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
// use std::sync::Arc; // Unused
use tracing::{debug, info, instrument, warn};

/// Per-call cap on tool output as a fraction of the agent's
/// remaining context budget. 10% means a tool result never exceeds
/// 10% of `(context_window − estimated_input_tokens)` (in chars).
const TOOL_OUTPUT_FRACTION: f32 = 0.10;

/// Bytes-per-token rule of thumb. Conservative; matches the
/// `chars_per_token` ratio used elsewhere in the SDK. Used to map
/// the token-budget cap onto a byte cap on the raw `tool_output`.
const CHARS_PER_TOKEN: f32 = 4.0;

/// Trigger condition for the M3 auto-invocation: a serialized
/// message history that's already eaten ≥90% of the agent's
/// context window AND has at least a few tool calls' worth of
/// content to fold (`message_count > 4` ≈ system+user+≥1
/// assistant+≥1 tool). When `context_window <= 0` (provider doesn't
/// expose it) the trigger is disabled.
fn should_auto_compact(
    messages_chars: usize,
    context_window: i32,
    message_count: usize,
    chars_per_token: f32,
) -> bool {
    if context_window <= 0 || message_count <= 4 {
        return false;
    }
    let cpt = if chars_per_token > 0.0 {
        chars_per_token
    } else {
        4.0
    };
    let estimated_tokens = (messages_chars as f32 / cpt) as i32;
    let pct = (estimated_tokens as f64 / context_window as f64) * 100.0;
    pct >= 90.0
}

/// Property schema for one item of `submit_batch_evaluation`.
///
/// Lifted out of the call site so the advertised shape can be asserted: the
/// object closes with `additionalProperties: false`, so an axis missing here
/// is one an evaluator is forbidden to send, however well the prompt
/// describes it.
fn eval_item_properties_schema() -> serde_json::Value {
    json!({
        "agent_id": { "type": "string" },
        "endorsement_weight": { "type": "number" },
        "justification": { "type": "string" },
        "is_final_solution": { "type": "boolean" },
        "stance": {
            "type": ["string", "null"],
            "enum": ["strong_agree", "agree", "neutral", "disagree", "strong_disagree", null]
        },
        "claim_assessments": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "claim_id": {
                        "type": ["string", "null"],
                        "description": "Stable 6-char hex id for cross-round tracking. Echo back the id of a claim flagged in an earlier round; otherwise null."
                    },
                    "claim": {
                        "type": "string",
                        "description": "The claim, quoted VERBATIM from the proposal — an exact, character-for-character substring. Copy-paste the span; do NOT paraphrase, summarize, shorten or reword. Common quote wrappers (\"…\", > …, `…`) are tolerated and stripped, and the quote is replaced with the exact proposal substring so the client can locate it. A quote matching no span of the proposal cannot be highlighted and is dropped from claim convergence. WRONG (paraphrase): \"the sort is efficient\". RIGHT (verbatim): \"sorts in O(n log n) time\"."
                    },
                    "verdict": { "type": "string", "enum": ["verified", "contested", "unverified", "wrong"] },
                    "reason": { "type": ["string", "null"] }
                },
                "required": ["claim_id", "claim", "verdict", "reason"],
                "additionalProperties": false
            }
        },
        "disagreements": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "claim_id": { "type": ["string", "null"] },
                    "proposal_claims": { "type": "string" },
                    "evaluator_position": { "type": "string" },
                    "confidence": { "type": "string", "enum": ["high", "medium", "low"] }
                },
                "required": ["claim_id", "proposal_claims", "evaluator_position", "confidence"],
                "additionalProperties": false
            }
        },
        "category_scores": {
            "type": ["object", "null"],
            "properties": {
                "correctness": { "type": "number" },
                "completeness": { "type": "number" },
                "novelty": { "type": "number" },
                "feasibility": { "type": "number" },
                "evidence_quality": { "type": "number" },
                "conciseness": { "type": "number" }
            },
            "required": ["correctness", "completeness", "novelty", "feasibility", "evidence_quality", "conciseness"],
            "additionalProperties": false
        }
    })
}

/// Outcome of a `compact_history` call.
pub struct CompactionResult {
    /// New message history with older tool calls folded into a
    /// synthetic `compact_history` tool-result pair.
    pub new_messages: Vec<ChatCompletionRequestMessage>,
    /// LLM-generated summary text to append to the scratchpad.
    pub summary: String,
    /// Number of tool-call results that were folded.
    pub compacted_count: usize,
}

/// Build a one-shot LLM request that summarises the older portion
/// of the conversation. Returns the new history (preamble + a
/// single synthetic compaction pair + the most-recent N tool
/// calls) and the summary text.
pub async fn compact_message_history(
    llm_client: &dyn crate::llms::AiModel,
    agent_config: &AgentConfig,
    messages: &[ChatCompletionRequestMessage],
    keep_last_n_calls: usize,
) -> anyhow::Result<CompactionResult> {
    use crate::llms::RequestConfig;
    use async_openai::types::ChatCompletionMessageToolCall;
    use async_openai::types::FunctionCall;

    // Treat keep=0 as keep=1: keeping zero recent tool results would
    // index `tool_msg_indices[len()]` and panic.
    let keep_last_n_calls = keep_last_n_calls.max(1);

    // Under `disable_native_tools` tool outputs are rewritten into
    // User messages prefixed with `Tool Output (` — fold those too.
    let is_tool_boundary = |m: &ChatCompletionRequestMessage| -> bool {
        match m {
            ChatCompletionRequestMessage::Tool(_) => true,
            ChatCompletionRequestMessage::User(u) => {
                if let async_openai::types::ChatCompletionRequestUserMessageContent::Text(t) =
                    &u.content
                {
                    t.starts_with("Tool Output (")
                } else {
                    false
                }
            }
            _ => false,
        }
    };
    let tool_msg_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_tool_boundary(m))
        .map(|(i, _)| i)
        .collect();

    if tool_msg_indices.len() <= keep_last_n_calls {
        return Ok(CompactionResult {
            new_messages: messages.to_vec(),
            summary: String::new(),
            compacted_count: 0,
        });
    }

    // Walk back to the parent Assistant turn so the kept tool_result
    // doesn't reference tool_calls that got folded into the summary —
    // the provider rejects orphaned tool messages.
    let raw_tool_cut = tool_msg_indices[tool_msg_indices.len() - keep_last_n_calls];
    let cut_idx = (0..raw_tool_cut)
        .rev()
        .find(|&i| matches!(messages[i], ChatCompletionRequestMessage::Assistant(_)))
        .unwrap_or(raw_tool_cut);

    // Anchor the preamble at the assistant turn, not the tool turn,
    // so we don't leave an orphan `tool_calls` whose result got folded.
    let first_foldable_tool_idx = messages
        .iter()
        .take(cut_idx)
        .position(is_tool_boundary)
        .unwrap_or(cut_idx);
    let preamble_end = (0..first_foldable_tool_idx)
        .rev()
        .find(|&i| matches!(messages[i], ChatCompletionRequestMessage::Assistant(_)))
        .unwrap_or(first_foldable_tool_idx);

    let to_summarize = &messages[preamble_end..cut_idx];
    let mut to_summarize_text = String::new();
    for m in to_summarize {
        match m {
            ChatCompletionRequestMessage::Tool(t) => {
                if let ChatCompletionRequestToolMessageContent::Text(s) = &t.content {
                    to_summarize_text.push_str("[tool_result] ");
                    to_summarize_text.push_str(s);
                    to_summarize_text.push_str("\n\n");
                }
            }
            ChatCompletionRequestMessage::Assistant(a) => {
                if let Some(tcs) = &a.tool_calls {
                    for tc in tcs {
                        to_summarize_text.push_str(&format!(
                            "[tool_call] {}({})\n",
                            tc.function.name, tc.function.arguments
                        ));
                    }
                }
            }
            ChatCompletionRequestMessage::User(u) => {
                // `disable_native_tools` mode: tool outputs land here
                // as text-rewritten User messages. Without this branch
                // the summariser sees zero tool evidence and the
                // compaction silently strips the actual data.
                if let async_openai::types::ChatCompletionRequestUserMessageContent::Text(t) =
                    &u.content
                    && t.starts_with("Tool Output (")
                {
                    to_summarize_text.push_str("[tool_result] ");
                    to_summarize_text.push_str(t);
                    to_summarize_text.push_str("\n\n");
                }
            }
            _ => {}
        }
    }

    let summarize_prompt = format!(
        "You are compressing an agent's accumulated tool-call history \
         so the agent can keep reasoning under tighter context pressure. \
         The summary will REPLACE the calls below in the conversation, \
         so anything you drop is gone.\n\n\
         Produce a structured summary using this exact section order. \
         Every section starts with an imperative — do what it says.\n\n\
         1. User Request and Primary Intent — Capture all explicit asks, \
         constraints, and the underlying goal. Look for: original request \
         phrasing, mid-task clarifications, success criteria, anything \
         the user explicitly forbade.\n\
         2. Methodology & Technical Concepts — List all approaches, \
         algorithms, APIs, formats, protocols, and libraries in play. \
         Look for: framework names and versions, data formats, patterns \
         being followed (TDD, RAG, …), and the rationale for choosing them.\n\
         3. References and Quotes — Capture all file paths read with \
         line ranges, verbatim quotes the agent has cited (≤ 1 line each), \
         and peer/external outputs referenced. Look for: file path + \
         line range for every read, commit SHAs, URLs, identifier names. \
         Drop full file dumps.\n\
         4. Errors, Fixes and Learnings — List all errors encountered, \
         the fix applied (or that it remains open), and the generalisable \
         lesson. Look for: exact error message, root cause, the change \
         that resolved it, what to avoid next time.\n\
         5. User Messages — Capture all explicit instructions, corrections, \
         and constraints from the user/orchestrator turn-by-turn. Look for: \
         verbatim phrasing that disambiguates intent, `stop`/`don't`/\
         `instead` pivots, deadlines, scope cuts.\n\
         6. Pending Work — List all open tool calls, unfinished sub-goals, \
         and known follow-ups. Look for: what is queued, why it hasn't \
         been done yet, dependencies on other steps.\n\
         7. Current Work — Capture the agent's most recent action and what \
         it was about to do next. Look for: the in-flight tool call, \
         the file/function being inspected, the immediate next move.\n\n\
         === HISTORY TO SUMMARISE ===\n{to_summarize_text}"
    );

    let request_config = RequestConfig {
        messages: vec![
            ChatCompletionRequestUserMessage {
                content: summarize_prompt.into(),
                ..Default::default()
            }
            .into(),
        ],
        tools: None,
        tool_choice: None,
        presence_penalty: None,
        service_tier: None,
    };

    let result = llm_client
        .chat_completion(agent_config, request_config)
        .await
        .map_err(|e| anyhow::anyhow!("compact_history summariser call failed: {e}"))?;
    let summary = result
        .response
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("compact_history summariser returned empty content"))?;

    // `total - keep_last_n_calls` lies when an assistant batch emits
    // multiple tool calls; count what's actually before `cut_idx`.
    let actually_compacted = tool_msg_indices
        .iter()
        .take_while(|&&i| i < cut_idx)
        .count();

    // When `disable_native_tools` is on, the rest of the loop flattens
    // tool traffic into User-message text; injecting native tool_calls
    // + Tool roles here would land in a request that sets `tools: None`
    // and the provider rejects the protocol mismatch. Mirror the
    // existing `Tool Output (...)` rewrite convention so subsequent
    // is_tool_boundary scans still recognise the pair.
    let (synth_assistant, synth_tool_result): (
        ChatCompletionRequestMessage,
        ChatCompletionRequestMessage,
    ) = if agent_config.disable_native_tools {
        let assistant: ChatCompletionRequestMessage = ChatCompletionRequestAssistantMessage {
            content: Some(
                async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(format!(
                    "[compact_history(keep_last_n_calls={keep_last_n_calls})]"
                )),
            ),
            tool_calls: None,
            ..Default::default()
        }
        .into();
        let result: ChatCompletionRequestMessage = ChatCompletionRequestUserMessage {
            content: format!(
                "Tool Output (compact_history): [Compacted {actually_compacted} earlier \
                 tool calls into scratchpad. Summary:]\n{summary}"
            )
            .into(),
            ..Default::default()
        }
        .into();
        (assistant, result)
    } else {
        let compact_tool_call_id = format!("compact_history_{}", uuid::Uuid::new_v4().simple());
        let assistant: ChatCompletionRequestMessage = ChatCompletionRequestAssistantMessage {
            tool_calls: Some(vec![ChatCompletionMessageToolCall {
                id: compact_tool_call_id.clone(),
                r#type: ChatCompletionToolType::Function,
                function: FunctionCall {
                    name: "compact_history".into(),
                    arguments: json!({ "keep_last_n_calls": keep_last_n_calls }).to_string(),
                },
            }]),
            ..Default::default()
        }
        .into();
        let result: ChatCompletionRequestMessage = ChatCompletionRequestToolMessage {
            tool_call_id: compact_tool_call_id,
            content: ChatCompletionRequestToolMessageContent::Text(format!(
                "[Compacted {actually_compacted} earlier tool calls into scratchpad. \
                 Summary:]\n{summary}"
            )),
        }
        .into();
        (assistant, result)
    };

    let mut new_messages = messages[..preamble_end].to_vec();
    new_messages.push(synth_assistant);
    new_messages.push(synth_tool_result);
    new_messages.extend(messages[cut_idx..].iter().cloned());

    Ok(CompactionResult {
        new_messages,
        summary,
        compacted_count: actually_compacted,
    })
}

/// Squeeze a scratchpad whose length has crossed the
/// `agent_config.scratchpad_squeeze_fraction` threshold. Calls the
/// agent's own LLM to compact older sections; preserves the trailing
/// 25% verbatim so the most-recent context stays intact.
pub async fn squeeze_scratchpad_if_full(
    llm_client: &dyn crate::llms::AiModel,
    agent_config: &AgentConfig,
    scratchpad: &str,
    max_scratchpad_size: usize,
) -> anyhow::Result<Option<String>> {
    use crate::llms::RequestConfig;

    if max_scratchpad_size == 0 || scratchpad.is_empty() {
        return Ok(None);
    }
    let ratio = scratchpad.len() as f64 / max_scratchpad_size as f64;
    if ratio < agent_config.scratchpad_squeeze_fraction {
        return Ok(None);
    }

    let cut = scratchpad.len() * 3 / 4;
    let safe_cut = (0..=cut)
        .rev()
        .find(|&i| scratchpad.is_char_boundary(i))
        .unwrap_or(0);
    let (older, recent) = scratchpad.split_at(safe_cut);

    let prompt = format!(
        "The agent's scratchpad is at {pct}% of its capacity \
         ({cur}/{max} chars). Compress the OLDER section using the \
         numbered template below. The RECENT section will be appended \
         verbatim — DO NOT touch it.\n\n\
         The scratchpad is the agent's persistent reasoning notebook \
         across rounds of a multi-agent deliberation. Every section \
         starts with an imperative — do what it says.\n\n\
         1. Primary Request and Intent — Capture the deliberation task \
         verbatim plus the success criterion the orchestrator set. Look \
         for: the framing question, scoring rubric, hard constraints.\n\
         2. Key Findings and Stances — List all conclusions this agent \
         has reached and the position it is defending. Look for: claim \
         + supporting evidence + confidence level.\n\
         3. Rounds and Phases — For each round/phase already assessed, \
         capture a one-line conclusion. Look for: round id, phase \
         (Propose/Evaluate/Refine), what changed since the previous \
         round.\n\
         4. Key Team Disagreements — List all points where peer agents \
         diverged and the evidence each side cited. Look for: peer \
         name, claim, counter-claim, evidence anchor.\n\
         5. Decisions Made — Capture every commitment the agent has \
         made and no longer plans to revisit. Look for: decision + \
         reason + when made.\n\
         6. References and Quotes — Capture all file paths read with \
         line ranges, verbatim quotes (≤ 1 line each), and peer outputs \
         referenced. Look for: path + line range, commit SHAs, URLs, \
         identifier names. Drop full dumps.\n\
         7. Errors, Fixes and Learnings — List all errors, the fix, and \
         the generalisable lesson. Look for: error message, root cause, \
         the change that resolved it.\n\
         8. Current Focus — Capture what the agent is reasoning about \
         right now and what would change its mind. Look for: the open \
         question, the falsifier, the next piece of evidence sought.\n\
         9. Optional Next Step — Capture the single most useful next \
         move, or `None`. Look for: a concrete tool call or peer reply \
         the agent should issue immediately.\n\n\
         Drop boilerplate, process narration, and anything already \
         implied by the RECENT section.\n\n\
         === OLDER (compress this) ===\n{older}\n\n\
         === RECENT (do not touch) ===\n{recent}",
        pct = (ratio * 100.0).round() as u32,
        cur = scratchpad.len(),
        max = max_scratchpad_size,
    );

    let request_config = RequestConfig {
        messages: vec![
            ChatCompletionRequestUserMessage {
                content: prompt.into(),
                ..Default::default()
            }
            .into(),
        ],
        tools: None,
        tool_choice: None,
        presence_penalty: None,
        service_tier: None,
    };
    let result = llm_client
        .chat_completion(agent_config, request_config)
        .await
        .map_err(|e| anyhow::anyhow!("scratchpad squeeze summariser call failed: {e}"))?;
    let compressed = result
        .response
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("scratchpad squeeze summariser returned empty content"))?;

    let candidate = format!("{compressed}\n\n{recent}");
    if candidate.len() >= scratchpad.len() || candidate.len() > max_scratchpad_size {
        warn!(
            old_len = scratchpad.len(),
            new_len = candidate.len(),
            max = max_scratchpad_size,
            "scratchpad squeeze produced no improvement; keeping original"
        );
        return Ok(None);
    }
    Ok(Some(candidate))
}

/// Apply [`TOOL_OUTPUT_FRACTION`] cap to `tool_output` in place.
/// A short, single-line preview of tool arguments or output for the agent
/// event log. Collapses whitespace and truncates on a char boundary so the
/// dashboard shows a compact summary rather than a full payload.
fn tool_event_preview(text: &str) -> String {
    const MAX_CHARS: usize = 200;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > MAX_CHARS {
        let clipped: String = collapsed.chars().take(MAX_CHARS).collect();
        format!("{clipped}…")
    } else {
        collapsed
    }
}

/// Truncates on a UTF-8 char boundary and appends a marker so the
/// model sees that the tool clipped its result. Returns `true`
/// when the cap engaged.
///
/// `context_window <= 0` (provider doesn't expose it) preserves
/// the legacy uncapped behavior; the SDK's downstream shrink-guard
/// is the only protection in that case.
fn apply_tool_output_cap(
    tool_output: &mut String,
    context_window: i32,
    estimated_input_tokens: u32,
    tool_name: &str,
) -> bool {
    if context_window <= 0 {
        return false;
    }
    let remaining_tokens = (context_window as i64 - estimated_input_tokens as i64).max(0) as f32;
    let cap_bytes = (remaining_tokens * CHARS_PER_TOKEN * TOOL_OUTPUT_FRACTION) as usize;
    if tool_output.len() <= cap_bytes {
        return false;
    }
    // Zero remaining context budget — replace the result with a
    // marker. Anything else lets the SDK shrink-guard fire on the
    // next call with a 200-token output budget, breaking downstream
    // JSON.
    if cap_bytes == 0 {
        let original_len = tool_output.len();
        tool_output.clear();
        tool_output.push_str(
            "[truncated: no remaining context budget; \
             re-call with offset/regex/page to scope the read]",
        );
        warn!(
            tool_name = %tool_name,
            original_len,
            cap_bytes = 0,
            "tool result fully replaced with marker; context exhausted"
        );
        return true;
    }
    let original_len = tool_output.len();
    let safe_cap = (0..=cap_bytes)
        .rev()
        .find(|&i| tool_output.is_char_boundary(i))
        .unwrap_or(0);
    tool_output.truncate(safe_cap);
    tool_output.push_str(&format!(
        "\n[truncated: tool result clipped at {} bytes \
         ({:.0}% of remaining context); original was {} bytes — \
         re-call with offset/regex/page to scope the read]",
        safe_cap,
        TOOL_OUTPUT_FRACTION * 100.0,
        original_len
    ));
    warn!(
        tool_name = %tool_name,
        original_len,
        cap_bytes = safe_cap,
        "tool result truncated to per-call context-budget cap"
    );
    true
}

/// Write a structured failure dump to disk (opt-in via `NSED_FAILURE_DUMPS=1|full`).
///
/// Dumps are organized as **one directory per job** under `failures/`:
///
/// ```text
/// failures/
///   a1b2c3d4_DEFAULT/           # session_id prefix + agent name
///     parse_error_r2.md          # all parse retries for round 2 appended here
///     api_error_r3.md            # API failure in round 3
///   e5f6g7h8_ARCHIT/
///     parse_error_r1.md
/// ```
///
/// Subsequent failures for the same job+kind+round are **appended** rather
/// than creating new files, so you can follow the retry evolution in one doc.
/// Full context (system prompt, request body, messages) is only written on the
/// first entry — retries share the same prompt, so repeating it wastes space.
///
/// Returns `Some(path)` on success so callers can log the filename.
fn write_failure_dump(params: FailureDumpParams<'_>) -> Option<String> {
    // Config value takes precedence over env var. When the config explicitly
    // says "off", we must NOT fall through to the env var.
    let dump_mode = match params.failure_dumps_config {
        Some(v) => {
            let v = v.trim().to_lowercase();
            if v == "off" || v.is_empty() {
                None // Config explicitly disabled — short-circuit, skip env var
            } else {
                Some(v)
            }
        }
        None => std::env::var("NSED_FAILURE_DUMPS").ok(),
    };
    let dump_mode = dump_mode?;
    // Only enable dumps for explicit opt-in values: "1", "on", or "full".
    let is_full = dump_mode.eq_ignore_ascii_case("full");
    if dump_mode != "1" && !dump_mode.eq_ignore_ascii_case("on") && !is_full {
        return None;
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let safe_name = params.agent_name.replace(['/', '\\', ' ', '(', ')'], "_");

    // ── Job directory: one per session (or fallback key) ─────────────
    let job_dir_name = if let Some(sid) = params.session_id {
        let safe_session = sid.replace(['/', '\\', ' ', '(', ')', '.', ':'], "_");
        let short: String = safe_session.chars().take(8).collect();
        format!("{short}_{safe_name}")
    } else {
        // No session_id: use a timestamp-based directory so we still group
        // within a single invocation (parse retries hit the same dir).
        format!("{timestamp}_{safe_name}")
    };
    let job_dir = format!("failures/{job_dir_name}");

    // File within the job directory: one per error kind + round
    let round = params.round.unwrap_or(0);
    let filename = format!("{job_dir}/{kind}_r{round}.md", kind = params.kind);

    // ── Determine if this is a new file or an append ─────────────────
    let is_append = std::path::Path::new(&filename).exists();

    let mut out = String::with_capacity(4096);

    if is_append {
        out.push_str("\n---\n\n");
    }

    // ── Section header ───────────────────────────────────────────────
    let attempt_label = params
        .attempt
        .map(|a| format!(" (attempt {a})"))
        .unwrap_or_default();
    out.push_str(&format!(
        "# {kind}{attempt_label} — {timestamp}\n\n",
        kind = params.kind,
    ));

    // ── Metadata table ───────────────────────────────────────────────
    out.push_str("| field | value |\n|---|---|\n");
    out.push_str(&format!("| agent | {} |\n", params.agent_name));
    out.push_str(&format!("| model | {} |\n", params.model_name));
    out.push_str(&format!("| provider | {} |\n", params.provider_id));
    if let Some(sid) = params.session_id {
        out.push_str(&format!("| session_id | {sid} |\n"));
    }
    if let Some(phase) = params.phase {
        out.push_str(&format!("| phase | {phase} |\n"));
    }
    out.push_str(&format!("| round | {round} |\n"));
    if let Some(attempt) = params.attempt {
        out.push_str(&format!("| attempt | {attempt} |\n"));
    }
    if let Some(fr) = params.finish_reason {
        out.push_str(&format!("| finish_reason | {fr} |\n"));
    }
    if let Some(t) = params.input_tokens {
        out.push_str(&format!("| input_tokens | {t} |\n"));
    }
    if let Some(t) = params.output_tokens {
        out.push_str(&format!("| output_tokens | {t} |\n"));
    }
    out.push('\n');

    // ── Error ────────────────────────────────────────────────────────
    out.push_str("## Error\n\n```\n");
    out.push_str(params.error);
    out.push_str("\n```\n\n");

    // ── LLM response (parse-error only) ──────────────────────────────
    if let Some(response) = params.response_content {
        let truncated = truncate_for_dump(response, 4000);
        out.push_str("## LLM Response\n\n```\n");
        out.push_str(&truncated);
        out.push_str("\n```\n\n");
    }

    // ── Full context (opt-in, first entry only) ──────────────────────
    // System prompt and messages are identical across retries — only
    // include them on the first write to save space + context.
    if is_full && !is_append {
        if let Some(sys) = params.system_prompt {
            out.push_str("## System Prompt\n\n```\n");
            out.push_str(sys);
            out.push_str("\n```\n\n");
        }
        if let Some(body) = params.request_body {
            let truncated = truncate_for_dump(body, 8000);
            out.push_str("## Request Body\n\n```json\n");
            out.push_str(&truncated);
            out.push_str("\n```\n\n");
        }
        if let Some(msgs) = params.messages {
            let msgs_json =
                serde_json::to_string_pretty(msgs).unwrap_or_else(|_| format!("{msgs:#?}"));
            let truncated = truncate_for_dump(&msgs_json, 12000);
            out.push_str("## Messages\n\n```json\n");
            out.push_str(&truncated);
            out.push_str("\n```\n\n");
        }
    } else if !is_full && !is_append {
        out.push_str(
            "_Set `NSED_FAILURE_DUMPS=full` to include system prompt, request body, and messages._\n",
        );
    }

    // ── Write / append to disk ───────────────────────────────────────
    let write_result = std::fs::create_dir_all(&job_dir).and_then(|_| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&filename)?;
        file.write_all(out.as_bytes())
    });
    if let Err(io_err) = write_result {
        warn!("Failed to write failure dump: {}", io_err);
        return None;
    }

    // ── Prune old job directories (NSED_FAILURE_DUMPS_MAX, default 20)
    let max_dirs: usize = std::env::var("NSED_FAILURE_DUMPS_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    prune_failure_dirs("failures", max_dirs);

    Some(filename)
}

/// Remove oldest **job directories** from `failures/` when the count exceeds
/// `max_dirs`. Sorts by filesystem modified-time (oldest first) and deletes
/// the surplus recursively.  Best-effort: silently ignores I/O errors.
fn prune_failure_dirs(parent: &str, max_dirs: usize) {
    let entries: Vec<_> = match std::fs::read_dir(parent) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect(),
        Err(_) => return,
    };
    if entries.len() <= max_dirs {
        return;
    }
    let mut by_mtime: Vec<_> = entries
        .into_iter()
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    by_mtime.sort_by_key(|(t, _)| *t);

    let to_remove = by_mtime.len().saturating_sub(max_dirs);
    for (_, path) in by_mtime.into_iter().take(to_remove) {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Parameters for [`write_failure_dump`].
struct FailureDumpParams<'a> {
    kind: &'a str,
    agent_name: &'a str,
    model_name: &'a str,
    provider_id: &'a str,
    error: &'a str,
    session_id: Option<&'a str>,
    phase: Option<&'a str>,
    round: Option<u32>,
    attempt: Option<usize>,
    finish_reason: Option<&'a str>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    response_content: Option<&'a str>,
    system_prompt: Option<&'a str>,
    request_body: Option<&'a str>,
    messages: Option<&'a [ChatCompletionRequestMessage]>,
    /// From `AgentConfig.failure_dumps` — takes precedence over env var.
    failure_dumps_config: Option<&'a str>,
}

/// Truncate a string for dump files. Respects char boundaries.
fn truncate_for_dump(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}\n... (truncated at {max_chars} chars)")
    }
}

// Define AgentResponse locally
#[derive(Debug, Clone)]
pub struct AgentResponse {
    pub content: String,
    pub tool_usage: HashMap<String, usize>,
    pub finish_reason: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub system_prompt: Option<String>,
    pub request_body: Option<String>,
    pub history: Vec<ChatCompletionRequestMessage>,
    pub final_scratchpad: Option<String>,
    /// Provider-reported cost and cache figures, summed over every LLM call
    /// the ReAct loop made to produce this response.
    pub provider_usage: crate::llms::ProviderUsage,
    /// Which backend actually served the last call of this response, when the
    /// provider reports one. Recorded so a completed deliberation can answer
    /// where it ran, rather than only which endpoints it was allowed to use.
    pub served_by: Option<String>,
}

/// Emit a [`DeliberationContextAssembled`](crate::telemetry::DeliberationContextAssembled)
/// for a propose/evaluate call. `candidates_count` is the evaluate fan-in (`0`
/// for propose); `final_scratchpad` is the scratchpad the agent produced, if any.
fn emit_context_assembled(
    context: &AgentContext,
    final_scratchpad: Option<&str>,
    candidates_count: u32,
) {
    let written = final_scratchpad.filter(|s| !s.is_empty());
    let chars = |s: &str| s.chars().count() as u32;
    emit_for!(
        context,
        DeliberationContextAssembled {
            scratchpad_loaded_chars: context.scratchpad.as_deref().map(chars).unwrap_or(0),
            scratchpad_written: written.is_some(),
            scratchpad_written_chars: written.map(chars).unwrap_or(0),
            prior_own_proposal_included: context.previous_own_proposal.is_some(),
            prior_score_included: context.previous_own_score.is_some(),
            prior_critiques_count: context.previous_critiques.len() as u32,
            candidates_count,
            previous_round_matrix_included: context.previous_round_matrix.is_some(),
        }
    );
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[allow(dead_code)]
pub(crate) struct BatchEvaluationItem {
    #[serde(alias = "candidate_id", default)]
    pub(crate) agent_id: String,
    #[serde(alias = "score")]
    endorsement_weight: f32,
    #[serde(default)]
    justification: Option<String>,
    #[serde(default)]
    is_final_solution: bool,
    #[serde(default)]
    stance: Option<Stance>,
    #[serde(default)]
    pub(crate) claim_assessments: Vec<ClaimAssessment>,
    #[serde(default)]
    disagreements: Vec<DisagreementPoint>,
    #[serde(default)]
    category_scores: Option<CategoryScores>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct StructuredBatchEvaluationResponse {
    /// Models sometimes use "candidate_evaluations" or "candidates" instead of "evaluations".
    #[serde(alias = "candidate_evaluations", alias = "candidates")]
    evaluations: Vec<BatchEvaluationItem>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct StructuredProposalResponse {
    // `default` so a middleware-declared envelope schema (which has neither field)
    // deserializes cleanly — the raw args become `Proposal.content` in that case.
    #[serde(default, deserialize_with = "deserialize_string_or_array_or_object")]
    thought_process: String,
    #[serde(default, deserialize_with = "deserialize_string_or_array_or_object")]
    solution_content: String,
}

/// Accepts a JSON string, array of strings (joined with "\n"), or any object
/// (serialized to a compact JSON string). Models like Mistral frequently return
/// `"thought_process": ["Step 1: ...", "Step 2: ..."]` instead of a single string,
/// or nest the entire answer inside `"solution_content": { ... }`.
fn deserialize_string_or_array_or_object<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde_json::Value;
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(s) => Ok(s),
        Value::Array(arr) => {
            let parts: Vec<String> = arr
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                })
                .collect();
            Ok(parts.join("\n"))
        }
        other => Ok(other.to_string()),
    }
}

/// Strips thinking-token prefixes leaked by reasoning models (e.g. gpt-oss-120b).
/// Common patterns:
///   "analysisWe need to...assistantfinal**Critique..."  → "**Critique..."
///   "final**Asset-class allocation..."                  → "**Asset-class allocation..."
///   "commentaryto=functions.submit_proposal json{...}"  → "{...}"
/// Normalize an `agent_response.finish_reason` string to the
/// lowercase wire values documented on `Proposal::finish_reason`
/// (`stop`, `tool_calls`, `length`, ...) before export to DTOs /
/// downstream consumers. The internal producer uses
/// `format!("{r:?}")` on the OpenAI enum, which yields Debug form
/// (`Stop`, `ToolCalls`, `Length`) — fine for intra-module flow
/// checks, wrong for the public contract. Unknown values fall
/// through lowercased so callers see something stable instead of
/// nothing. The synthetic `"max_iterations"` sentinel already
/// matches the wire format and passes through untouched.
fn normalize_finish_reason(raw: &str) -> String {
    match raw {
        "Stop" => "stop".to_string(),
        "ToolCalls" => "tool_calls".to_string(),
        "Length" => "length".to_string(),
        "ContentFilter" => "content_filter".to_string(),
        "FunctionCall" => "function_call".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

fn strip_thinking_prefix(content: &str) -> &str {
    // Trim leading whitespace so prefixes are detected even when LLMs
    // emit leading spaces/newlines.
    let mut s = content.trim_start();
    // Strip "analysis...assistant" prefix (model internal tokens leaked into output)
    if let Some(pos) = s.find("assistant") {
        let after = &s[pos + "assistant".len()..];
        // Only strip if what follows looks like content (not mid-word)
        if after.starts_with("final")
            || after.starts_with("{")
            || after.starts_with("**")
            || after.starts_with('#')
        {
            s = after;
        }
    }
    // Strip "final" prefix
    if s.starts_with("final") {
        s = &s["final".len()..];
    }
    // Strip "commentary..." prefix (gpt-oss commentary tokens)
    if s.starts_with("commentary") {
        // Find the start of actual content after "commentaryto=functions.xxx json"
        if let Some(json_pos) = s.find("json{") {
            s = &s[json_pos + "json".len()..];
        } else if let Some(json_pos) = s.find("json ") {
            s = &s[json_pos + "json ".len()..];
        }
    }
    s.trim()
}

#[derive(Debug, Clone)]
pub struct ProposerEvaluatorAgent {
    pub config: AgentConfig,
    llm: Box<dyn AiModel>,
    prompt_set: Box<dyn PromptSet>,
    pub extra_context_tools: Vec<Box<dyn Tool>>,
    pub sandbox_tools: Vec<Box<dyn Tool>>,
    /// Optional output-leak detector used by `prompt_exposure_guard`.
    /// `None` disables guarding; attach any `OutputLeakDetector` impl
    /// via [`ProposerEvaluatorAgent::with_output_guard`].
    output_guard: Option<std::sync::Arc<dyn crate::agents::OutputLeakDetector>>,
}

impl ProposerEvaluatorAgent {
    /// Assemble the full tool set for a given context.
    ///
    /// Clones extra_context_tools and sandbox_tools, then conditionally injects
    /// NSED protocol tools (read_proposal, read_critiques, read_own_proposal) and
    /// user-defined tools when their prerequisites (store, handler) are available.
    fn aggregate_tools(&self, context: &AgentContext) -> Vec<Box<dyn Tool>> {
        let mut all_tools: Vec<Box<dyn Tool>> = self
            .extra_context_tools
            .iter()
            .map(|t| dyn_clone::clone_box(t.as_ref()))
            .collect::<Vec<_>>();
        all_tools.extend(
            self.sandbox_tools
                .iter()
                .map(|t| dyn_clone::clone_box(t.as_ref())),
        );

        // Web search, where the backend refuses to take its search tool
        // alongside ours. The tool is ordinary here; calling it issues a
        // separate, search-only completion. Without this the setting parses and
        // does nothing, which reads as "search configured" while the agent has
        // no way to search at all.
        if let Some(provider_tool) = self.config.delegated_search.as_deref() {
            all_tools.push(Box::new(crate::tools::DelegatedSearchTool::new(
                dyn_clone::clone_box(self.llm.as_ref()),
                &self.config,
                provider_tool,
            )));
        }

        // Automatically inject Context Tools if store is available.
        //
        // Only the ones that have something to read. Offering a history reader in
        // the first round advertises a capability with no data behind it: the
        // model spends a turn calling it, gets nothing, and the tool definitions
        // cost prompt tokens in every request of that round.
        if let Some(store) = &context.store {
            // `read_proposal` serves the current round's anonymized candidates
            // during evaluation, and prior rounds' proposals from history — so it
            // is useful whenever either exists.
            let has_history = context.round_number > 1;
            if has_history || !context.candidates.is_empty() {
                let read_tool = ReadProposalTool::new(store.clone(), context.round_number)
                    .with_candidates(context.candidates.clone());
                all_tools.push(Box::new(read_tool));
            }

            // Critiques and one's own earlier proposal only exist once a round has
            // been through evaluation.
            if has_history {
                let critiques_tool = ReadCritiquesTool::new(store.clone(), context.round_number);
                all_tools.push(Box::new(critiques_tool));

                let own_tool = ReadOwnProposalTool::new(
                    store.clone(),
                    context.round_number,
                    self.config.name.clone(),
                );
                all_tools.push(Box::new(own_tool));
            }

            // Search spans the whole deliberation including the current round, so
            // it is offered as soon as there is anything to search.
            if has_history || !context.candidates.is_empty() {
                let search_tool = SearchDeliberationTool::new(store.clone(), context.round_number)
                    .with_candidates(context.candidates.clone());
                all_tools.push(Box::new(search_tool));
            }

            debug!(
                agent = %self.config.name,
                round = context.round_number,
                candidates = context.candidates.len(),
                tools = all_tools.len(),
                "Injected the NSED protocol tools that have data to serve."
            );
        }

        // Inject user-defined tools if handler is available
        if let Some(ref handler) = context.user_tool_handler {
            for def in &context.user_tools {
                let user_tool = UserCallTool::new(
                    def.clone(),
                    handler.clone(),
                    context.round_number,
                    context.phase,
                );
                all_tools.push(Box::new(user_tool));
            }
            if !context.user_tools.is_empty() {
                debug!(agent=%self.config.name, count=%context.user_tools.len(), "Injected user-defined tools");
            }
        }

        // Only inject the sandboxed read_file tool for openai-family
        // providers. Claude/MCP/exec already have their own filesystem
        // affordances; mounting a second one would broaden capability
        // beyond the scope this PR was approved for.
        if crate::agents::config::is_openai_family_provider(&self.config)
            && !self.config.read_file_roots.is_empty()
        {
            all_tools.push(Box::new(
                crate::tools::scoped_read::ScopedReadFileTool::new(
                    self.config.name.clone(),
                    &self.config.read_file_roots,
                ),
            ));
        }

        // Debug log for active tools
        let tool_names: Vec<String> = all_tools.iter().map(|t| t.name()).collect();
        debug!(agent=%self.config.name, available_tools=?tool_names, "Tools configured for this run");

        all_tools
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Proposer {
    llm_client: Box<dyn AiModel>,
    prompt_set: Box<dyn PromptSet>,
    pub tools: Vec<Box<dyn Tool>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Evaluator {
    llm_client: Box<dyn AiModel>,
    prompt_set: Box<dyn PromptSet>,
    pub tools: Vec<Box<dyn Tool>>,
}

impl ProposerEvaluatorAgent {
    pub fn new(
        config: AgentConfig,
        llm: Box<dyn AiModel>,
        prompt_set: Box<dyn PromptSet>,
        context_tools: Vec<Box<dyn Tool>>,
        sandbox_tools: Vec<Box<dyn Tool>>,
    ) -> Self {
        Self {
            config,
            llm,
            prompt_set,
            extra_context_tools: context_tools,
            sandbox_tools,
            output_guard: None,
        }
    }

    /// Attach an output-leak detector. When `config.prompt_exposure_guard`
    /// is true, the agent will run the detector over user-visible terminal
    /// tool outputs (`submit_proposal`, `submit_batch_evaluation`, …) and
    /// trigger a retry if the detector blocks.
    ///
    /// Without a detector, `config.prompt_exposure_guard = true` is a no-op
    /// (pass-through). Callers can supply any `OutputLeakDetector` impl.
    pub fn with_output_guard(
        mut self,
        detector: std::sync::Arc<dyn crate::agents::OutputLeakDetector>,
    ) -> Self {
        self.output_guard = Some(detector);
        self
    }

    /// Direct chat with the agent's LLM, using the agent's persona but
    /// bypassing NSED deliberation constraints via an "internal voice" wrapper.
    ///
    /// `messages` should contain the conversation history (user + assistant
    /// turns). The system prompt is prepended automatically.
    pub async fn chat(&self, messages: Vec<ChatCompletionRequestMessage>) -> Result<String> {
        let persona = self
            .config
            .persona
            .as_deref()
            .unwrap_or("a helpful assistant");
        let system_prompt = format!(
            "You are {name}, {persona}.\n\n\
             <internal_voice>\n\
             This is a direct conversation with your operator — not part of an NSED deliberation.\n\
             Respond naturally and helpfully. Ignore any deliberation protocol instructions.\n\
             </internal_voice>",
            name = self.config.name,
            persona = persona,
        );

        let mut full_messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessage {
                content: async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                    system_prompt,
                ),
                name: None,
            }
            .into(),
        ];
        full_messages.extend(messages);

        let request_config = RequestConfig {
            messages: full_messages,
            tools: None,
            tool_choice: None,
            presence_penalty: self.config.presence_penalty,
            service_tier: None,
        };

        let result = self
            .llm
            .chat_completion(&self.config, request_config)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let response = &result.response;

        Ok(response
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default())
    }
}

#[async_trait]
impl NsedAgent for ProposerEvaluatorAgent {
    #[instrument(skip(self, context), fields(agent_name = %self.config.name))]
    async fn propose(&self, context: &AgentContext) -> Result<Proposal> {
        info!("🤖 Agent is starting the proposal generation process.");

        let prompt = self.prompt_set.get_proposer_prompt(
            &context.task_description,
            context.previous_round_matrix.clone(),
            context.previous_own_proposal.as_ref(),
            context.previous_own_score,
            context.previous_critiques.clone(),
            &context.user_injections,
            context.structured_feedback.as_ref(),
        );
        // Dynamic round + phase go in the user message; the system message is static
        // for prompt-cache reuse.
        let prompt = format!(
            "{}{}{prompt}",
            crate::prompts::clock_block(context.issued_at.as_deref()),
            self.prompt_set.get_turn_header(
                context.round_number as usize,
                context.total_rounds as usize,
                context.phase,
            )
        );

        // A `before_prompt` middleware may constrain the submission to its own
        // JSON schema (structured output); otherwise the default thought/solution.
        let forced_schema = context.forced_proposal_schema.clone();
        let submit_tool_schema = ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: "submit_proposal".to_string(),
                description: Some("Submit the final proposal.".to_string()),
                parameters: Some(forced_schema.clone().unwrap_or_else(|| {
                    json!({
                        "type": "object",
                        "properties": {
                            "thought_process": { "type": "string" },
                            "solution_content": { "type": "string" }
                        },
                        "required": ["thought_process", "solution_content"],
                        "additionalProperties": false
                    })
                })),
                // Strict only for the built-in simple schema; a middleware-declared
                // schema may use unions/optionals that OpenAI strict mode rejects —
                // the forced tool call + lenient parse carry it instead.
                strict: Some(forced_schema.is_none()),
            },
        };

        let all_tools = self.aggregate_tools(context);

        let (response, agent_response): (StructuredProposalResponse, AgentResponse) =
            generate_structured_output(
                &*self.llm,
                &self.config,
                &*self.prompt_set,
                context,
                prompt,
                &all_tools,
                submit_tool_schema,
                "submit_proposal",
                self.output_guard.as_deref(),
            )
            .await?;

        info!("🧠 Agent Reasoning: {}", response.thought_process);

        if let Some(store) = &context.store {
            // Phase 6B: Strip ephemeral <working_memory> before persisting for next round.
            // Retains <key_findings> and <strategy> sections.
            if let Ok(Some(current)) = store.get(&self.config.name).await {
                let cleaned = strip_working_memory(&current);
                if cleaned.len() != current.len() {
                    if let Err(e) = store.set(&self.config.name, &cleaned).await {
                        warn!(error=%e, "Failed to persist scratchpad after working memory rotation");
                    }
                }
            }

            let max_len = self.config.scratchpad_limit as usize;
            let mut note = format!(
                "Round {} Proposal:\nThought Process: {}\nContent: {}\n",
                context.round_number, response.thought_process, response.solution_content
            );
            if note.len() > max_len {
                const TRUNCATION_SUFFIX: &str = "...(truncated)";
                // Truncate to max_len minus suffix length, finding a safe UTF-8 boundary
                let truncate_at = max_len.saturating_sub(TRUNCATION_SUFFIX.len());
                // floor_char_boundary equivalent: find the last char boundary <= truncate_at
                let safe_at = note
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|&i| i <= truncate_at)
                    .last()
                    .unwrap_or(0);
                note.truncate(safe_at);
                note.push_str(TRUNCATION_SUFFIX);
            }
            if let Err(e) = store.append(&self.config.name, &note).await {
                warn!(error=%e, "Failed to persist proposal to scratchpad");
            }
        }

        emit_context_assembled(context, agent_response.final_scratchpad.as_deref(), 0);

        Ok(Proposal {
            thought_process: response.thought_process,
            // With a middleware-declared schema, the whole tool-call args ARE the
            // proposal (e.g. the patch-deliberation `{rationale, ops}` envelope) —
            // deliver them verbatim so `on_provider_response` gets the full object.
            content: if forced_schema.is_some() {
                agent_response.content.clone()
            } else {
                response.solution_content
            },
            final_scratchpad: agent_response.final_scratchpad.clone(), // Capture the scratchpad
            token_usage_stats: Some(TokenUsage {
                input_tokens: agent_response.input_tokens.unwrap_or(0),
                output_tokens: agent_response.output_tokens.unwrap_or(0),
                reported_cost_usd: agent_response.provider_usage.cost_usd,
                cached_tokens: agent_response.provider_usage.cached_tokens,
                cache_write_tokens: agent_response.provider_usage.cache_write_tokens,
            }),
            // Propagate terminal signal ("max_iterations" partial
            // fallback, otherwise LLM-driven stop/tool_calls). Lets
            // the orchestrator + dashboard distinguish partial output
            // from full completions without re-reading LLM internals.
            finish_reason: agent_response
                .finish_reason
                .as_deref()
                .map(normalize_finish_reason),
            served_by: agent_response.served_by.clone(),
            ..Default::default()
        })
    }

    #[instrument(skip(self, context), fields(agent_name = %self.config.name))]
    async fn evaluate(&self, context: &AgentContext) -> Result<Vec<(String, Evaluation)>> {
        info!("🕵️ Agent is starting the batch proposal evaluation process.");

        let eval_item_properties = eval_item_properties_schema();

        let batch_evaluation_tool = ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: "submit_batch_evaluation".to_string(),
                description: Some(
                    "Submit batch evaluations with structured claim-level analysis.".to_string(),
                ),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "evaluations": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": eval_item_properties,
                                "required": ["agent_id", "endorsement_weight", "justification", "is_final_solution",
                                             "stance", "claim_assessments", "disagreements", "category_scores"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["evaluations"],
                    "additionalProperties": false
                })),
                strict: Some(true),
            },
        };

        let prompt = self.prompt_set.get_batch_evaluator_prompt(
            &context.task_description,
            &context.candidates,
            context.previous_own_proposal.as_ref(),
            context.round_number as usize,
            &context.user_injections,
        );
        // Dynamic round + phase → user message (static system prompt for cache reuse).
        let prompt = format!(
            "{}{}{prompt}",
            crate::prompts::clock_block(context.issued_at.as_deref()),
            self.prompt_set.get_turn_header(
                context.round_number as usize,
                context.total_rounds as usize,
                context.phase,
            )
        );

        let all_tools = self.aggregate_tools(context);

        // Generate, then ground the batch's claim citations. An unresolvable
        // cite goes back to the model as an error it can act on — the same
        // re-quote signal the tool-calling runtime sends — and the model gets
        // another turn to produce a verbatim quote.
        //
        // The budget is counted HERE, separately from the parse-failure retries
        // inside `generate_structured_output`. Sharing one budget would let a
        // batch that merely needs re-quoting consume the attempts that a
        // genuinely malformed batch needs to recover, turning a cosmetic fault
        // into a lost evaluation.
        let mut cite_attempts: u32 = 0;
        let mut attempt_prompt = prompt.clone();
        // Retries re-run generation, so tokens must accumulate across attempts
        // or the discarded attempts bill as free.
        let mut requote_input_tokens: u32 = 0;
        let mut requote_output_tokens: u32 = 0;
        let mut requote_provider_usage = crate::llms::ProviderUsage::default();

        let (structured_response, agent_response): (
            StructuredBatchEvaluationResponse,
            AgentResponse,
        ) = loop {
            let (mut response, mut resp_meta): (StructuredBatchEvaluationResponse, AgentResponse) =
                generate_structured_output(
                    &*self.llm,
                    &self.config,
                    &*self.prompt_set,
                    context,
                    attempt_prompt.clone(),
                    &all_tools,
                    batch_evaluation_tool.clone(),
                    "submit_batch_evaluation",
                    self.output_guard.as_deref(),
                )
                .await?;

            let unresolved = super::cite::ground_all(
                &context.candidates,
                context.round_number,
                &mut response.evaluations,
                super::cite::GroundingPolicy::Reject,
            );

            if unresolved.is_empty() {
                break (response, resp_meta);
            }

            if cite_attempts >= super::cite::REQUOTE_BUDGET {
                warn!(
                    agent_name = %self.config.name,
                    count = unresolved.len(),
                    "cite re-quote budget exhausted — dropping unanchored claims and \
                     accepting the remaining evaluations"
                );
                break (response, resp_meta);
            }

            cite_attempts += 1;
            requote_input_tokens =
                requote_input_tokens.saturating_add(resp_meta.input_tokens.take().unwrap_or(0));
            requote_output_tokens =
                requote_output_tokens.saturating_add(resp_meta.output_tokens.take().unwrap_or(0));
            requote_provider_usage.accumulate(&resp_meta.provider_usage);
            warn!(
                agent_name = %self.config.name,
                attempt = cite_attempts,
                count = unresolved.len(),
                "evaluation cites match no span of their target — asking for a verbatim re-quote"
            );
            attempt_prompt = format!("{prompt}\n\n{}", requote_feedback(&unresolved));
        };

        // Token usage for this entire evaluation batch (one LLM call produces all evals)
        let batch_provider_usage = {
            let mut total = requote_provider_usage;
            total.accumulate(&agent_response.provider_usage);
            total
        };
        let batch_token_usage = Some(TokenUsage {
            input_tokens: agent_response
                .input_tokens
                .unwrap_or(0)
                .saturating_add(requote_input_tokens),
            output_tokens: agent_response
                .output_tokens
                .unwrap_or(0)
                .saturating_add(requote_output_tokens),
            reported_cost_usd: batch_provider_usage.cost_usd,
            cached_tokens: batch_provider_usage.cached_tokens,
            cache_write_tokens: batch_provider_usage.cache_write_tokens,
        });

        emit_context_assembled(
            context,
            agent_response.final_scratchpad.as_deref(),
            context.candidates.len() as u32,
        );

        // Build the set of valid candidate IDs so we can filter out
        // hallucinated self-evaluations before normalization.
        let valid_ids: std::collections::HashSet<&str> =
            context.candidates.iter().map(|c| c.id.as_str()).collect();

        // Filter to valid candidates and warn about dropped evaluations.
        let valid_evals: Vec<_> = structured_response
            .evaluations
            .into_iter()
            .filter(|e| {
                if valid_ids.contains(e.agent_id.as_str()) {
                    true
                } else {
                    warn!(
                        agent_name = %self.config.name,
                        target = %e.agent_id,
                        "Dropping evaluation for unknown candidate (hallucinated self-eval?)"
                    );
                    false
                }
            })
            .collect();

        // Normalize only over valid evaluations. A non-finite weight (overflowed
        // LLM value → ±inf) is treated as 0 here so one garbage weight doesn't make
        // the whole sum inf and collapse the evaluator's other opinions; normalize_score
        // also independently guards the per-proposal raw weight.
        let total_weight: f32 = valid_evals
            .iter()
            .map(|e| {
                let w = e.endorsement_weight.abs();
                if w.is_finite() { w } else { 0.0 }
            })
            .sum();
        debug!(
            agent_name = %self.config.name,
            valid_count = valid_evals.len(),
            total_abs_weight = total_weight,
            weights = ?valid_evals.iter().map(|e| (&e.agent_id, e.endorsement_weight)).collect::<Vec<_>>(),
            "Normalization input"
        );
        // Diagnostic: warn when all weights share the same sign (no mixed endorsement/opposition)
        let all_negative = valid_evals.iter().all(|e| e.endorsement_weight <= 0.0);
        let all_positive = valid_evals.iter().all(|e| e.endorsement_weight >= 0.0);
        if valid_evals.len() > 1 && (all_negative || all_positive) {
            debug!(
                agent_name = %self.config.name,
                sign = if all_negative { "all_negative" } else { "all_positive" },
                weights = ?valid_evals.iter().map(|e| (&e.agent_id, e.endorsement_weight)).collect::<Vec<_>>(),
                "All endorsement weights have the same sign — legitimate in signed pipeline"
            );
        }
        let mut results = Vec::new();
        for item in valid_evals {
            let justification = item.justification.unwrap_or_default();
            if justification.is_empty() {
                warn!(
                    agent_name = %self.config.name,
                    target = %item.agent_id,
                    "Evaluation missing justification — LLM omitted required field."
                );
            }
            let raw_score = normalize_score(item.endorsement_weight, total_weight);
            results.push((
                item.agent_id,
                Evaluation {
                    score: raw_score,
                    justification,
                    token_usage: batch_token_usage.clone(),
                    stance: item.stance,
                    claim_assessments: item.claim_assessments,
                    disagreements: item.disagreements,
                    category_scores: item.category_scores,
                    is_final_solution: item.is_final_solution,
                    // Batch-level: one LLM call produced all items,
                    // so every Evaluation in this batch carries the
                    // same finish_reason (e.g. "max_iterations" when
                    // the react ceiling was hit before a terminal
                    // submit_batch_evaluation call).
                    finish_reason: agent_response
                        .finish_reason
                        .as_deref()
                        .map(normalize_finish_reason),
                    ..Default::default()
                },
            ));
        }

        Ok(results)
    }

    fn name(&self) -> String {
        self.config.name.clone()
    }
}

/// The re-quote instruction fed back to the model, naming the exact cites that
/// resolved to nothing. Mirrors the tool-calling runtime's wording so both
/// strict paths teach the same lesson.
fn requote_feedback(unresolved: &[super::cite::UnresolvedCite]) -> String {
    let detail = unresolved
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "These claim citations do not match any span of the proposal they target. \
         Quote each claim VERBATIM — copy-paste the exact text from the proposal \
         (no paraphrase) — then resubmit submit_batch_evaluation:\n{detail}"
    )
}

/// Implement [`ChatCapable`] so `ProposerEvaluatorAgent` can be used with the
/// SDK worker's status server chat endpoint.
#[async_trait]
impl ChatCapable for ProposerEvaluatorAgent {
    async fn chat(
        &self,
        messages: Vec<async_openai::types::ChatCompletionRequestMessage>,
    ) -> Result<String> {
        // Delegates to the inherent method
        ProposerEvaluatorAgent::chat(self, messages).await
    }
}

// ... helpers (react_loop, generate_structured_output) ...

/// `tool_choice` for a request: force a tool call (`required`) only when a
/// middleware declared a proposal schema AND native tools are actually available.
fn force_tool_choice(
    schema_declared: bool,
    has_tools: bool,
    disable_native_tools: bool,
) -> Option<async_openai::types::ChatCompletionToolChoiceOption> {
    if schema_declared && has_tools && !disable_native_tools {
        Some(async_openai::types::ChatCompletionToolChoiceOption::Required)
    } else {
        // TODO: disable_native_tools (XML/Nous-format models) → no schema
        // enforcement at all. Add a non-native forcing path or document the gap.
        None
    }
}

/// The same request with the tool choice left to the model, or `None` when we
/// never forced one — there is then nothing to relax and no retry to make.
fn relaxed_without_forced_choice(config: &RequestConfig) -> Option<RequestConfig> {
    config.tool_choice.as_ref()?;
    let mut relaxed = config.clone();
    relaxed.tool_choice = None;
    Some(relaxed)
}

/// The tool choice for a request: forced when the schema needs it, unless this
/// model is already known to refuse a forced choice.
///
/// The whole decision lives here so it can be tested as one. Split across the
/// call site, a test can wire the pieces itself and pass while the wiring is
/// gone.
fn tool_choice_for(
    model: &str,
    schema_declared: bool,
    has_tools: bool,
    disable_native_tools: bool,
) -> Option<async_openai::types::ChatCompletionToolChoiceOption> {
    force_tool_choice(
        schema_declared,
        has_tools,
        disable_native_tools || refuses_forced_choice_already(model),
    )
}

/// Models already seen refusing a forced tool choice, so the next request skips
/// the choice instead of paying a refusal to rediscover it.
///
/// Measured live: the refusal costs a full round trip, and with reasoning models
/// the round it happens in used 922 of its 930-second budget. The refusal is a
/// property of the model, not of the request, so learning it once is enough.
/// Process-local and never evicted — the set is bounded by the roster.
fn refuses_forced_choice_already(model: &str) -> bool {
    known_refusers().lock().is_ok_and(|s| s.contains(model))
}

fn remember_refusal(model: &str) {
    if let Ok(mut s) = known_refusers().lock() {
        s.insert(model.to_string());
    }
}

fn known_refusers() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Whether a tool call is refused by the active-set gate. Provider-executed
/// names never appear in the active set (it holds only locally-run schemas),
/// so they are exempt: refusing them answered a backend-run search call with
/// "does not exist", and the model concluded the tool was unavailable.
fn refused_by_active_set(
    active: &std::collections::HashSet<&str>,
    provider_executed: &[String],
    tool_name: &str,
) -> bool {
    !active.contains(tool_name) && !provider_executed.iter().any(|t| t == tool_name)
}

/// The text to judge a refusal on: the provider's response body when the error
/// carries one, else the error itself.
///
/// `Display` on an HTTP error is deliberately terse — the body can echo a
/// fragment of the prompt, so it is kept out of logs and dumps and reached only
/// through `detail`. That makes the formatted error `"bad request (status 400)"`
/// with the reason stripped, so matching on it can never recognise a refusal.
/// Measured live: two seats hit exactly this and their round was lost while the
/// retry sat inert.
fn refusal_text(error: &crate::llms::LlmError) -> String {
    match error.detail() {
        Some(body) => body.to_string(),
        None => format!("{error:#}"),
    }
}

/// Whether a request failed because the backend refuses a forced tool choice.
///
/// Several backends reject `tool_choice: required` outright — most when the model
/// is in a thinking/reasoning mode — each wording it differently, and some wrap
/// the refusal in a generic gateway error with the original nested inside. Most
/// name the parameter; one describes the behaviour instead, without naming it.
/// Either way the request was refused for the choice we forced, not for anything
/// in the prompt.
fn rejects_forced_tool_choice(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("tool_choice") || error.contains("forced function calling")
}

/// Whether a request failed because the backend refuses our function tools declared
/// beside a provider-executed one.
///
/// A seat carrying `provider_executed_tools` sends both in a single array, which
/// several backends reject outright — and the rejection takes down EVERY tool call
/// that seat makes, so it reads as an agent that simply never proposes. Recognising
/// it is what turns that silence into a cause a reader can act on: move the tool to
/// the nested search-only call instead of declaring it alongside.
fn rejects_mixed_tool_array(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("all search tools")
        || (error.contains("multiple tools") && error.contains("supported"))
}

/// Run the injected [`SubmissionValidator`](crate::agents::SubmissionValidator) on
/// the proposal `content`, returning `Some(reason)` when rejected. Only
/// `submit_proposal` submissions are validated; without a validator or on any
/// other terminal tool it returns `None`. Free fn so the react loop's rejection
/// gate is unit-testable with a mock validator — no LLM needed.
///
/// `content` MUST be the same bytes that become `Proposal.content` — for a
/// middleware forced-schema proposal that is the raw tool-args envelope
/// (`{textual_response_to_user, ops}`), NOT the default schema's `solution_content`
/// (which is absent → empty for forced schema). Validating the wrong field made the
/// in-loop validator a no-op for patch-deliberation, so every provider_response
/// block skipped the react retry and surfaced to the orchestrator via the
/// post-generation hook instead.
async fn run_submission_validator(
    content: &str,
    terminal_tool_name: &str,
    validator: Option<&dyn crate::agents::SubmissionValidator>,
) -> Option<String> {
    if terminal_tool_name != "submit_proposal" {
        return None;
    }
    let validator = validator?;
    validator.validate(content).await
}

#[allow(clippy::too_many_arguments)]
async fn generate_structured_output<T>(
    llm_client: &dyn AiModel,
    agent_config: &AgentConfig,
    prompt_set: &dyn PromptSet,
    context: &AgentContext,
    initial_prompt: String,
    tools: &[Box<dyn Tool>],
    terminal_tool_schema: ChatCompletionTool,
    terminal_tool_name: &str,
    output_guard: Option<&dyn crate::agents::OutputLeakDetector>,
) -> Result<(T, AgentResponse)>
where
    // `Serialize` is required so the prompt-exposure guard (when enabled
    // via `AgentConfig.prompt_exposure_guard`) can serialize the parsed
    // terminal content back to JSON and extract user-visible fields for
    // scanning. Concrete types passed through here (`Proposal`,
    // `BatchEvaluation`) already implement `Serialize`.
    T: DeserializeOwned + serde::Serialize,
{
    let max_retries = agent_config.max_retries.filter(|&v| v > 0).unwrap_or(3) as usize;
    let mut attempts = 0;
    let mut current_prompt = Some(initial_prompt);
    let mut current_history: Option<Vec<ChatCompletionRequestMessage>> = None;
    // Accumulate tokens across retry attempts so failed-attempt tokens aren't lost
    let mut cumulative_input_tokens: u32 = 0;
    let mut cumulative_output_tokens: u32 = 0;
    // Owned here so schema retries inherit prior tool-output bloat.
    let mut running_tool_output_bytes: u64 = 0;

    // Mutable clone so we can escalate max_tokens on Length truncation retries
    // or downgrade reasoning_effort on consecutive Stop+empty failures
    let mut retry_config = agent_config.clone();
    let mut consecutive_empty_stops: u32 = 0;
    let loop_start = std::time::Instant::now();

    loop {
        attempts += 1;
        let agent_response = match react_loop(
            llm_client,
            &mut retry_config,
            prompt_set,
            context,
            current_prompt.clone(),
            current_history.clone(),
            tools,
            vec![terminal_tool_schema.clone()],
            Some(terminal_tool_name),
            attempts as u32,
            &mut running_tool_output_bytes,
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) => {
                // Transport-class errors (connection reset, EOF,
                // timeout, mid-body decode) are retryable — the model
                // never saw our request or the response was lost in
                // transit. Classify via `LlmError` downcast so the
                // taxonomy is compile-time exhaustive instead of
                // string-scraped from the formatted error.
                use crate::telemetry::LlmError;
                let is_transport =
                    matches!(e.downcast_ref::<LlmError>(), Some(LlmError::Transport(_)));

                if is_transport && attempts <= max_retries {
                    // Transport errors interrupt the "consecutive empty-Stop"
                    // streak — don't let mixed network+empty failures trip
                    // the reasoning_effort downgrade prematurely.
                    consecutive_empty_stops = 0;
                    let exp = (attempts as u32).min(5); // cap exponent to avoid overflow (max 2^5 = 32s)
                    let backoff = std::time::Duration::from_secs(2u64.pow(exp));
                    warn!(
                        agent_name = %agent_config.name,
                        attempt = attempts,
                        max_retries = max_retries,
                        error = %format!("{e:#}"),
                        retry_after = ?backoff,
                        "Transport error — retrying with backoff."
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                return Err(e);
            }
        };

        // Accumulate tokens from this attempt (including any failed attempts)
        cumulative_input_tokens += agent_response.input_tokens.unwrap_or(0);
        cumulative_output_tokens += agent_response.output_tokens.unwrap_or(0);

        let mut cleaned_json = clean_json_string(
            &agent_response.content,
            agent_config.unwrap_hallucinated_tool_calls,
            Some(terminal_tool_name),
        );
        let mut parse_result = serde_json::from_str::<T>(&cleaned_json);

        // Attempt repair if parsing failed
        if parse_result.is_err() {
            // 1. Try repairing truncation
            let repaired = repair_truncated_json(&cleaned_json);
            if let Ok(repaired_obj) = serde_json::from_str::<T>(&repaired) {
                warn!(
                    agent_name = %agent_config.name,
                    "Successfully repaired truncated JSON output."
                );
                parse_result = Ok(repaired_obj);
            } else {
                // 2. Try repairing invalid escapes (common with LaTeX in JSON)
                let repaired_escapes = repair_invalid_escapes(&cleaned_json);
                // Also repair truncation on the escaped version just in case
                let repaired_escapes_truncated = repair_truncated_json(&repaired_escapes);

                if let Ok(repaired_obj) = serde_json::from_str::<T>(&repaired_escapes_truncated) {
                    warn!(
                        agent_name = %agent_config.name,
                        "Successfully repaired invalid JSON escapes (likely LaTeX)."
                    );
                    parse_result = Ok(repaired_obj);
                } else {
                    // 3. Try aggressive escape repair (blindly escape backslashes except quotes/backslashes)
                    let aggressive = repair_aggressive_escapes(&cleaned_json);
                    let aggressive_truncated = repair_truncated_json(&aggressive);

                    if let Ok(repaired_obj) = serde_json::from_str::<T>(&aggressive_truncated) {
                        warn!(
                            agent_name = %agent_config.name,
                            "Successfully repaired JSON using aggressive escaping."
                        );
                        parse_result = Ok(repaired_obj);
                    } else {
                        // 4. Nuclear Option: Strip invalid escapes entirely (lossy)
                        let sanitized = sanitize_json_string_lossy(&cleaned_json);
                        let sanitized_truncated = repair_truncated_json(&sanitized);

                        if let Ok(repaired_obj) = serde_json::from_str::<T>(&sanitized_truncated) {
                            warn!(
                                agent_name = %agent_config.name,
                                "Successfully repaired JSON by stripping invalid escapes (lossy)."
                            );
                            parse_result = Ok(repaired_obj);
                        }
                    }
                }
            }
        }

        // 5. Try conversational repair (Rnj-1 style: "THOUGHT: ... RESPONSE: ...")
        if parse_result.is_err()
            && let Some(repaired_conv) = repair_conversational_response(&agent_response.content)
            && let Ok(repaired_obj) = serde_json::from_str::<T>(&repaired_conv)
        {
            warn!(
                "Successfully repaired conversational JSON for agent {}: {}",
                agent_config.name, repaired_conv
            );
            parse_result = Ok(repaired_obj);
        }

        // 5b. Try extracting proposal from Markdown report (e.g. gpt-oss style)
        if parse_result.is_err()
            && terminal_tool_name == "submit_proposal"
            && let Some(repaired_markdown) = extract_proposal_from_markdown(&agent_response.content)
            && let Ok(repaired_obj) = serde_json::from_str::<T>(&repaired_markdown)
        {
            warn!(
                agent_name = %agent_config.name,
                "Successfully extracted proposal from Markdown report."
            );
            parse_result = Ok(repaired_obj);
        }

        // 5c. Try extracting evaluations from Markdown table
        if parse_result.is_err()
            && terminal_tool_name == "submit_batch_evaluation"
            && let Some(repaired_markdown) =
                extract_evaluations_from_markdown(&agent_response.content)
            && let Ok(repaired_obj) = serde_json::from_str::<T>(&repaired_markdown)
        {
            warn!(
                agent_name = %agent_config.name,
                "Successfully extracted evaluations from Markdown table."
            );
            parse_result = Ok(repaired_obj);
        }

        // 5d. For proposals: split merged thought_process/solution_content.
        // gpt-oss and MiniMax frequently put everything into `thought_process`
        // with a `**Solution Content**` marker, leaving `solution_content` absent.
        if parse_result.is_err()
            && terminal_tool_name == "submit_proposal"
            && let Ok(obj) = serde_json::from_str::<serde_json::Value>(&cleaned_json)
            && let Some(repaired) = split_merged_solution(&obj)
            && let Ok(repaired_obj) = serde_json::from_value::<T>(repaired)
        {
            warn!(
                agent_name = %agent_config.name,
                "Successfully split merged thought_process/solution_content."
            );
            parse_result = Ok(repaired_obj);
        }

        // 5f. A submit_proposal that parses cleanly but carries an empty
        // solution is not a submission: the same merged shape as 5d with the
        // field present-but-blank. Recover by the split; failing that, a
        // schema error names the blank field so the retry asks again.
        if terminal_tool_name == "submit_proposal"
            && parse_result.is_ok()
            && let Ok(obj) = serde_json::from_str::<serde_json::Value>(&cleaned_json)
            && obj
                .get("solution_content")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .is_none_or(str::is_empty)
        {
            match split_merged_solution(&obj)
                .and_then(|repaired| serde_json::from_value::<T>(repaired).ok())
            {
                Some(repaired_obj) => {
                    warn!(
                        agent_name = %agent_config.name,
                        "Recovered an empty solution_content from its thought_process."
                    );
                    parse_result = Ok(repaired_obj);
                }
                None => {
                    parse_result = Err(<serde_json::Error as serde::de::Error>::custom(
                        "solution_content is empty — the answer belongs in \
                         solution_content, not thought_process",
                    ));
                }
            }
        }

        // 5e. Strip thinking-token prefixes from gpt-oss reasoning models.
        // Models like gpt-oss-120b leak internal tokens: "analysisWe need...assistantfinal**Critique..."
        // or just "final**Asset-class allocation...". Strip these and retry as markdown proposal.
        if parse_result.is_err() && terminal_tool_name == "submit_proposal" {
            let content = agent_response.content.trim();
            let stripped = strip_thinking_prefix(content);
            if stripped.len() < content.len() && !stripped.is_empty() {
                // The stripped content is markdown without code blocks — wrap it as a proposal
                let obj = serde_json::json!({
                    "thought_process": "(extracted from thinking-prefixed markdown)",
                    "solution_content": stripped,
                });
                if let Ok(repaired_obj) = serde_json::from_value::<T>(obj) {
                    warn!(
                        agent_name = %agent_config.name,
                        "Successfully stripped thinking-token prefix and extracted proposal."
                    );
                    parse_result = Ok(repaired_obj);
                }
            }
        }

        // 6. Handle explicit refusal (Safety/Policy) as a valid proposal
        if parse_result.is_err() && terminal_tool_name == "submit_proposal" {
            let lower_content = agent_response
                .content
                .trim()
                .to_lowercase()
                .replace(['\u{2018}', '\u{2019}'], "'")
                .replace(['\u{201c}', '\u{201d}'], "\"");

            if lower_content.starts_with("i cannot")
                || lower_content.starts_with("i can't")
                || lower_content.starts_with("i apologize")
                || lower_content.starts_with("i'm sorry")
                || lower_content.starts_with("sorry")
                || lower_content.contains("as an ai")
                || lower_content.contains("cannot fulfill")
            {
                let refusal_json = serde_json::json!({
                    "thought_process": "The model refused to answer the prompt, likely due to safety guidelines.",
                    "solution_content": agent_response.content
                })
                .to_string();

                if let Ok(repaired_obj) = serde_json::from_str::<T>(&refusal_json) {
                    warn!(
                        agent_name = %agent_config.name,
                        "Detected refusal/safety response. Treating as valid proposal."
                    );
                    parse_result = Ok(repaired_obj);
                }
            }
        }

        // Prompt-exposure guardrail — a POST-RECOVERY sanitizer, not a retry
        // trigger. It runs only once format recovery has produced a parseable
        // result (`parse_result.is_ok()`), so it can never mask a genuine
        // parse error nor consume the format-recovery retry budget. On a block
        // it REDACTS the leaked indicators from the user-visible fields and
        // lets the (now-clean) content through, keeping the deliberation alive
        // instead of hard-failing after N retries.
        //
        // Gated by `agent_config.prompt_exposure_guard` so existing
        // deployments are unaffected until they opt in.
        let guard_block = if agent_config.prompt_exposure_guard
            && let Ok(ref parsed) = parse_result
            && let Some(detector) = output_guard
        {
            match run_prompt_exposure_guard(
                parsed,
                terminal_tool_name,
                &agent_config.name,
                detector,
            )
            .await
            {
                Some(block_result) => {
                    let redacted = redact_terminal_leak(parsed, terminal_tool_name, detector);
                    // Only trust the redaction if a RE-SCAN confirms it cleared the
                    // leak. The default `redact` is identity (no-op) and a partial
                    // redact can miss the blocked category — either way passing the
                    // content through would leak. `false` → block + retry instead.
                    let cleared = match &redacted {
                        Some(clean) => run_prompt_exposure_guard(
                            clean,
                            terminal_tool_name,
                            &agent_config.name,
                            detector,
                        )
                        .await
                        .is_none(),
                        None => false,
                    };
                    Some((block_result, redacted, cleared))
                }
                None => None,
            }
        } else {
            None
        };

        if let Some((block_result, redacted, cleared)) = guard_block {
            emit_for!(
                context,
                PromptExposureDetected {
                    terminal_tool: terminal_tool_name.to_string(),
                    blocked: true,
                    hit_count: block_result.hit_count,
                    response_length_chars: block_result.response_length_chars,
                    suspicion_score: block_result.suspicion_score,
                    xml_tag_hits: block_result.xml_tag_hits,
                    tool_name_hits: block_result.tool_name_hits,
                    instruction_hits: block_result.instruction_hits,
                    wrong_acronym_hits: block_result.wrong_acronym_hits,
                    sample_hits: block_result.sample_hits,
                }
            );

            if cleared {
                // Redaction verified clean by re-scan — safe to continue.
                let clean = redacted.expect("cleared implies a redacted value");
                warn!(
                    agent_name = %agent_config.name,
                    attempt = attempts,
                    terminal_tool = terminal_tool_name,
                    reason = %block_result.reason,
                    "prompt_exposure guard blocked; redacted, re-scanned clean, continued."
                );
                parse_result = Ok(clean);
            } else {
                // Redaction was a no-op / partial / failed → do NOT pass the leak
                // through. Convert to Err so the retry path re-prompts the model
                // for a self-contained answer (fail-closed on exhaustion).
                warn!(
                    agent_name = %agent_config.name,
                    attempt = attempts,
                    terminal_tool = terminal_tool_name,
                    reason = %block_result.reason,
                    "prompt_exposure guard blocked and redaction did not clear it; blocking + retrying."
                );
                cleaned_json = format!(
                    "prompt_exposure guard blocked output (redaction did not clear it): {}",
                    block_result.reason
                );
                let synth_err: serde_json::Error =
                    serde_json::from_str::<serde_json::Value>("not-json-prompt-exposure-block")
                        .expect_err("invalid JSON always errors");
                parse_result = Err(synth_err);
            }
        }

        // Submission validator — a reviewer (patch-deliberation provider_response,
        // injected by the worker) inspects the parsed proposal's content and may
        // reject it (e.g. ops that applied ZERO changes). A rejection converts the
        // Ok into an Err carrying the reason, so this SAME loop re-prompts within
        // `max_retries` — no separate retry budget. Proposal submissions only.
        // Validate the SAME bytes the post-generation hook will see — the raw
        // forced-schema envelope (`agent_response.content`), which is what becomes
        // `Proposal.content`. Extracting `solution_content` here (absent for forced
        // schema) made this a no-op, so blocks bypassed the in-loop retry.
        if parse_result.is_ok()
            && let Some(reason) = run_submission_validator(
                &agent_response.content,
                terminal_tool_name,
                context.submission_validator.as_deref(),
            )
            .await
        {
            warn!(
                agent_name = %agent_config.name,
                attempt = attempts,
                "submission validator rejected the proposal; triggering retry."
            );
            cleaned_json = format!("proposal rejected by reviewer: {reason}");
            let synth_err: serde_json::Error =
                serde_json::from_str::<serde_json::Value>("not-json-reviewer-block")
                    .expect_err("invalid JSON always errors");
            parse_result = Err(synth_err);
        }

        match parse_result {
            Ok(response) => {
                // Return cumulative tokens across all attempts (including retries)
                let mut final_response = agent_response;
                final_response.input_tokens = Some(cumulative_input_tokens);
                final_response.output_tokens = Some(cumulative_output_tokens);
                return Ok((response, final_response));
            }
            Err(e) => {
                let finish_reason = agent_response.finish_reason.as_deref().unwrap_or("unknown");

                // Log the FULL raw output to debug (logfile only)
                debug!(
                    agent_name = %agent_config.name,
                    attempt = attempts,
                    error = %e,
                    finish_reason = %finish_reason,
                    raw_content = %cleaned_json,
                    "Failed to parse structured output. Full raw content logged."
                );

                let truncated_raw = if cleaned_json.chars().count() > 1000 {
                    format!(
                        "{}... (truncated)",
                        cleaned_json.chars().take(1000).collect::<String>()
                    )
                } else {
                    cleaned_json.clone()
                };

                if attempts > max_retries {
                    anyhow::bail!(describe_parse_failure(ParseFailure {
                        attempts,
                        model: &agent_config.model_name,
                        last_error: &e.to_string(),
                        finish_reason,
                        raw: &agent_response.content,
                        cleaned: &cleaned_json,
                        output_tokens: agent_response.output_tokens.unwrap_or(0),
                    }));
                }

                // Dump failure details to file for debugging (opt-in via NSED_FAILURE_DUMPS=1|full)
                let phase_str = format!("{:?}", context.phase);
                if let Some(dump_file) = write_failure_dump(FailureDumpParams {
                    kind: "parse_error",
                    agent_name: &agent_config.name,
                    model_name: &agent_config.model_name,
                    provider_id: &agent_config.provider_id,
                    error: &e.to_string(),
                    session_id: context.session_id.as_deref(),
                    phase: Some(&phase_str),
                    round: Some(context.round_number),
                    attempt: Some(attempts),
                    finish_reason: Some(finish_reason),
                    input_tokens: agent_response.input_tokens,
                    output_tokens: agent_response.output_tokens,
                    response_content: Some(&cleaned_json),
                    system_prompt: agent_response.system_prompt.as_deref(),
                    request_body: agent_response.request_body.as_deref(),
                    messages: None,
                    failure_dumps_config: agent_config.failure_dumps.as_deref(),
                }) {
                    debug!(dump_file = %dump_file, "Parse failure dump saved");
                }

                warn!(
                    agent_name = %agent_config.name,
                    attempt = attempts,
                    error = %e,
                    finish_reason = %finish_reason,
                    "Agent failed to produce valid structured output. Retrying with feedback."
                );

                // Escalate max_tokens on Length truncation to give the model more
                // room on the next attempt. Cap at 50% of context_window to leave
                // space for input tokens and safety buffer.
                if finish_reason == "Length" {
                    let old = retry_config.max_tokens;
                    let ceiling = if retry_config.context_window > 0 {
                        (retry_config.context_window as f64 * 0.5) as i32
                    } else {
                        32_768
                    };
                    let new_tokens = ((old as f64 * 1.5).ceil() as i32).min(ceiling);
                    if new_tokens > old {
                        retry_config.max_tokens = new_tokens;
                        warn!(
                            agent_name = %retry_config.name,
                            old_max_tokens = old,
                            new_max_tokens = new_tokens,
                            "Escalating max_tokens after Length truncation."
                        );
                    }
                }

                // Downgrade reasoning_effort after consecutive Stop+empty
                // failures. The model is likely spending all output tokens
                // on internal chain-of-thought (reasoning_effort: medium/high)
                // and producing zero visible content. Removing reasoning
                // effort forces the model to output content directly.
                if cleaned_json.trim().is_empty() && finish_reason == "Stop" {
                    consecutive_empty_stops += 1;
                    if consecutive_empty_stops >= 2 && retry_config.reasoning_effort.is_some() {
                        warn!(
                            agent_name = %retry_config.name,
                            consecutive_empty_stops,
                            old_effort = ?retry_config.reasoning_effort,
                            "Downgrading reasoning_effort after consecutive Stop+empty failures"
                        );
                        retry_config.reasoning_effort = None;
                    }
                } else {
                    consecutive_empty_stops = 0;
                }

                let error_msg = if cleaned_json.trim().is_empty() {
                    // Model consumed output tokens but produced no visible content.
                    // Common with reasoning/thinking models where tokens go to
                    // internal chain-of-thought that doesn't appear in the response.
                    "Your response was EMPTY — no content was returned. You MUST call the tool with a JSON argument. Do NOT just think about the answer — you must actually output the tool call with the required fields.".to_string()
                } else if !cleaned_json.trim().starts_with('{') {
                    format!(
                        "Agent failed to use structured output tool. Returned conversational text instead. Raw: '{truncated_raw}'."
                    )
                } else {
                    let hint = if e.is_eof() && finish_reason == "Length" {
                        "Likely truncated due to max_tokens limit. Check input length."
                    } else if e.is_eof() {
                        "Response appears incomplete despite finish_reason=Stop."
                    } else {
                        ""
                    };
                    format!(
                        "Failed to parse structured output JSON. Error: {e} {hint}. Raw: '{truncated_raw}'."
                    )
                };

                let example_json = match terminal_tool_name {
                    "submit_proposal" => {
                        r#"{ "thought_process": "...", "solution_content": "..." }"#
                    }
                    "submit_batch_evaluation" => {
                        r#"{ "evaluations": [ { "agent_id": "Candidate_A", "endorsement_weight": 80.0, "justification": "...", "is_final_solution": false, "stance": null, "claim_assessments": [], "disagreements": [], "category_scores": null } ] }"#
                    }
                    other => {
                        warn!(tool_name = %other, "No example JSON template for terminal tool");
                        "{}"
                    }
                };

                // Update history with the failed attempt and error message.
                // When the previous attempt failed mid-tool-call (terminal-tool
                // parse failure, streaming truncation, etc.), the assistant
                // turn in `agent_response.history` can contain orphan
                // `tool_calls` with no matching `role: "tool"` follow-up.
                // Strict providers (Cerebras) reject with HTTP 422 — sanitize
                // here so feedback-mode retries always send a spec-compliant
                // history. Redundant with the send-path sanitizer in
                // native.rs::prepare_request, but cheap and keeps the retry
                // path's intermediate state correct for logging/inspection.
                let mut repaired_history = agent_response.history.clone();
                llm_repair::pair_orphan_tool_calls(&mut repaired_history);
                current_history = Some(repaired_history);
                current_prompt = None; // Switch to history mode

                // Fix empty Assistant content to prevent 400 Bad Request on retry
                if let Some(ChatCompletionRequestMessage::Assistant(assistant_msg)) =
                    current_history.as_mut().unwrap().last_mut()
                {
                    if let Some(
                        async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(
                            text,
                        ),
                    ) = &assistant_msg.content
                    {
                        if text.trim().is_empty() && assistant_msg.tool_calls.is_none() {
                            assistant_msg.content = Some(async_openai::types::ChatCompletionRequestAssistantMessageContent::Text("(Empty response)".to_string()));
                        }
                    } else if assistant_msg.content.is_none() && assistant_msg.tool_calls.is_none()
                    {
                        assistant_msg.content = Some(
                            async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(
                                "(Empty response)".to_string(),
                            ),
                        );
                    }
                }

                // Append the error message as a User message to the conversation history
                current_history.as_mut().unwrap().push(
                    ChatCompletionRequestUserMessage {
                        content: format!(
                            "SYSTEM ERROR (Attempt {attempts}/{max_retries}): Your last response failed validation. \
                            Issue: {error_msg}. \
                            You MUST use the `{terminal_tool_name}` tool with valid JSON arguments. Example format: \n{example_json}\n\
                            Do not return raw text. Please try again."
                        )
                        .into(),
                        ..Default::default()
                    }
                    .into(),
                );

                if context.telemetry.is_some() {
                    let cumulative_latency_ms = loop_start.elapsed().as_millis() as u64;
                    let reason = if cleaned_json.trim().is_empty() {
                        RetryReason::EmptyContent
                    } else if finish_reason == "Length" {
                        RetryReason::Truncated
                    } else {
                        RetryReason::SchemaError
                    };
                    let cumulative_cost_usd = estimate_llm_cost_usd(
                        &agent_config.model_name,
                        cumulative_input_tokens,
                        cumulative_output_tokens,
                        0,
                        0,
                    );

                    emit_for!(
                        context,
                        RetryLoopAttempt {
                            attempt: attempts as u32,
                            reason,
                            cumulative_latency_ms,
                            cumulative_cost_usd,
                            cumulative_input_tokens,
                            cumulative_output_tokens,
                        }
                    );
                }
            }
        }
    }
}

/// Strip any `<scratchpad>...</scratchpad>` block (and surrounding whitespace)
/// from text, so that retry paths don't nest scratchpads when the original user
/// Scan the parsed terminal-tool response for prompt-exposure leakage.
///
/// Returns `Some(block_reason)` when the guardrail wants the agent loop to
/// retry, `None` otherwise. The caller converts `Some(reason)` into an
/// `Err` so the existing parse-error retry path takes over and feeds the
/// reason back to the LLM as a `SYSTEM ERROR` user message.
///
/// Only scans the *user-visible* fields of known terminal tools:
///
/// - `submit_proposal` → the `solution_content` string (the final answer
///   shown to the end user). We deliberately skip `thought_process` —
///   that's internal reasoning; mentions of tool names there are
///   intentional and don't reach the user.
/// - `submit_batch_evaluation` → each evaluation's `justification` plus
///   every `claim_assessments[].reason`. The `ClaimAssessment` struct
///   already declares serde aliases for the off-schema keys models
///   drift to (`disagreement`, `explanation`, `reasoning`), so by the
///   time the parsed struct is round-tripped here those keys are all
///   normalised to `reason`. Those texts feed the peer-critiques block
///   in subsequent rounds and can reach users via the deliberation-brief
///   rendering.
///
/// For any other terminal tool we scan the serialized JSON as a
/// best-effort fallback — better to over-trigger than silently let
/// a leak slip through an untested tool.
/// Result from the prompt-exposure guard, returned when a block occurs.
/// Contains enough data for the telemetry emission to populate all fields.
#[derive(Debug)]
struct PromptExposureBlockResult {
    reason: String,
    hit_count: u32,
    response_length_chars: u32,
    suspicion_score: f64,
    xml_tag_hits: u32,
    tool_name_hits: u32,
    instruction_hits: u32,
    wrong_acronym_hits: u32,
    sample_hits: Vec<String>,
}

async fn run_prompt_exposure_guard<T>(
    parsed: &T,
    terminal_tool_name: &str,
    agent_name: &str,
    detector: &dyn crate::agents::OutputLeakDetector,
) -> Option<PromptExposureBlockResult>
where
    T: serde::Serialize,
{
    use crate::middleware::{MiddlewareContext, MiddlewareStage, Verdict};

    // Collect user-visible text snippets for this terminal tool. Anything
    // not enumerated falls back to "scan the whole thing."
    let parsed_value = match serde_json::to_value(parsed) {
        Ok(v) => v,
        Err(e) => {
            // Can't scan what we can't serialize. Log once and pass
            // through — this path is only reached for custom `T` the
            // orchestrator added without implementing Serialize, which
            // shouldn't happen in the built-in agent loop.
            tracing::debug!(
                agent_name = %agent_name,
                error = %e,
                "prompt_exposure guard: could not serialize parsed terminal content; skipping."
            );
            return None;
        }
    };

    let mut snippets: Vec<String> = Vec::new();
    match terminal_tool_name {
        "submit_proposal" => {
            if let Some(s) = parsed_value
                .get("solution_content")
                .and_then(|v| v.as_str())
            {
                snippets.push(s.to_string());
            }
        }
        "submit_batch_evaluation" => {
            if let Some(evals) = parsed_value.get("evaluations").and_then(|v| v.as_array()) {
                for ev in evals {
                    if let Some(j) = ev.get("justification").and_then(|v| v.as_str()) {
                        snippets.push(j.to_string());
                    }
                    if let Some(assessments) =
                        ev.get("claim_assessments").and_then(|v| v.as_array())
                    {
                        // The canonical schema (see `ClaimAssessment`)
                        // stores the user-visible rationale under
                        // `reason`. That struct already
                        // declares serde aliases (`disagreement`,
                        // `explanation`, `reasoning`) so deserialization
                        // normalises every off-schema key the models
                        // commonly drift to into `reason` — by the time
                        // we `to_value` the parsed struct here, only
                        // `reason` exists. A `get("reason")` lookup is
                        // therefore sufficient; reaching for the raw
                        // alias names at this layer would be dead code.
                        for a in assessments {
                            if let Some(c) = a.get("reason").and_then(|v| v.as_str()) {
                                snippets.push(c.to_string());
                            }
                        }
                    }
                }
            }
        }
        _ => {
            // Unknown terminal tool — scan the serialized JSON so a new
            // tool added without guardrail coverage still blocks leaks
            // by default. The reason string may be noisier but it
            // fails closed rather than open.
            snippets.push(parsed_value.to_string());
        }
    }

    for snippet in snippets {
        // Run the scan directly to get per-category counts; the result also
        // carries `response_length_chars` and `suspicion_score` derived from
        // the same scanned text, so we don't recompute either here.
        let scan_result = detector.scan(&snippet);
        if scan_result.hit_count() == 0 {
            continue;
        }
        // Build a context to run the detector's verdict gate.
        let ctx = MiddlewareContext {
            content: serde_json::Value::String(snippet),
            action: "propose".to_string(),
            agent_id: agent_name.to_string(),
            job_id: String::new(),
            round: 0,
            stage: MiddlewareStage::ProviderResponse,
            metadata: serde_json::json!(null),
            hook_state: std::collections::HashMap::new(),
        };
        let verdict = detector.evaluate(&ctx).await;
        if matches!(verdict.verdict, Verdict::Block) {
            return Some(PromptExposureBlockResult {
                reason: verdict
                    .reason
                    .unwrap_or_else(|| "prompt_exposure detector blocked output".to_string()),
                hit_count: scan_result.hit_count(),
                response_length_chars: scan_result.response_length_chars,
                suspicion_score: scan_result.suspicion_score,
                xml_tag_hits: scan_result.xml_tag_hits,
                tool_name_hits: scan_result.tool_name_hits,
                instruction_hits: scan_result.instruction_hits,
                wrong_acronym_hits: scan_result.wrong_acronym_hits,
                sample_hits: scan_result.hits.into_iter().take(5).collect(),
            });
        }
    }

    None
}

/// Recursively apply `detector.redact` to every string leaf in `value`.
/// Used for terminal tools without a hand-curated user-visible field map.
fn redact_all_strings(
    value: &mut serde_json::Value,
    detector: &dyn crate::agents::OutputLeakDetector,
) {
    match value {
        serde_json::Value::String(s) => {
            *s = detector.redact(s);
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_all_strings(v, detector);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                redact_all_strings(v, detector);
            }
        }
        _ => {}
    }
}

/// Graceful fallback for a prompt-exposure block: strip leaked indicators
/// from the user-visible fields of already-recovered structured output and
/// return the sanitized value, so the deliberation continues instead of
/// hard-failing. Scans the SAME fields as [`run_prompt_exposure_guard`].
///
/// Returns `None` only if the redacted value no longer deserializes to `T`
/// (should not happen for known tools — a `[redacted]` string is still a
/// valid string field); callers pass the original content through in that case.
fn redact_terminal_leak<T>(
    parsed: &T,
    terminal_tool_name: &str,
    detector: &dyn crate::agents::OutputLeakDetector,
) -> Option<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let mut value = serde_json::to_value(parsed).ok()?;
    match terminal_tool_name {
        "submit_proposal" => {
            if let Some(s) = value.get("solution_content").and_then(|v| v.as_str()) {
                let redacted = detector.redact(s);
                value["solution_content"] = serde_json::Value::String(redacted);
            }
        }
        "submit_batch_evaluation" => {
            if let Some(evals) = value.get_mut("evaluations").and_then(|v| v.as_array_mut()) {
                for ev in evals.iter_mut() {
                    if let Some(j) = ev.get("justification").and_then(|v| v.as_str()) {
                        let redacted = detector.redact(j);
                        ev["justification"] = serde_json::Value::String(redacted);
                    }
                    if let Some(assessments) = ev
                        .get_mut("claim_assessments")
                        .and_then(|v| v.as_array_mut())
                    {
                        for a in assessments.iter_mut() {
                            if let Some(c) = a.get("reason").and_then(|v| v.as_str()) {
                                let redacted = detector.redact(c);
                                a["reason"] = serde_json::Value::String(redacted);
                            }
                        }
                    }
                }
            }
        }
        _ => {
            // Unknown terminal tool — fail closed by scrubbing every string
            // leaf, mirroring the "scan the whole thing" branch of the guard.
            redact_all_strings(&mut value, detector);
        }
    }
    serde_json::from_value(value).ok()
}

/// content is re-merged with a fresh scratchpad injection.
/// Delimits the user request inside a merged prompt, so the mutable
/// scratchpad can sit after it and still be separable on a retry.
const USER_REQUEST_OPEN: &str = "<user_request>";
const USER_REQUEST_CLOSE: &str = "</user_request>";

/// Recover the user request from a prompt this loop previously assembled.
///
/// The scratchpad is rewritten every iteration, so it is appended AFTER the
/// request: everything a provider's prefix cache can reuse then precedes the
/// first byte that changes. `merged` also carries the regenerated system
/// message ahead of the request, which must not be re-merged into it.
fn extract_user_request(raw: &str, merged: bool) -> String {
    if let Some(start) = raw.find(USER_REQUEST_OPEN) {
        let body = &raw[start + USER_REQUEST_OPEN.len()..];
        if let Some(end) = body.find(USER_REQUEST_CLOSE) {
            return body[..end].trim().to_string();
        }
    }
    // Histories written before the request was delimited: the scratchpad led,
    // so the request is whatever followed it.
    if merged {
        if let Some(end) = raw.find("</scratchpad>") {
            return raw[end + "</scratchpad>".len()..].trim_start().to_string();
        }
    }
    strip_scratchpad(raw)
}

fn strip_scratchpad(text: &str) -> String {
    if let Some(start) = text.find("<scratchpad>") {
        if let Some(end_tag) = text[start..].find("</scratchpad>") {
            let end = start + end_tag + "</scratchpad>".len();
            let before = text[..start].trim_end();
            let after = text[end..].trim_start();
            if before.is_empty() {
                after.to_string()
            } else if after.is_empty() {
                before.to_string()
            } else {
                format!("{}\n\n{}", before, after)
            }
        } else {
            text.to_string()
        }
    } else {
        text.to_string()
    }
}

/// Strip `<working_memory>...</working_memory>` from scratchpad content.
/// Working memory is ephemeral per-round thinking that should not persist across rounds.
/// Falls back to returning the original content if tags are malformed.
fn strip_working_memory(text: &str) -> String {
    let mut result = text.to_string();
    // Remove all <working_memory>...</working_memory> blocks (there may be multiple)
    while let Some(start) = result.find("<working_memory>") {
        if let Some(end_offset) = result[start..].find("</working_memory>") {
            let end = start + end_offset + "</working_memory>".len();
            let before = result[..start].trim_end();
            let after = result[end..].trim_start();
            result = if before.is_empty() {
                after.to_string()
            } else if after.is_empty() {
                before.to_string()
            } else {
                format!("{}\n\n{}", before, after)
            };
        } else {
            // Malformed — no closing tag. Return what we have so far.
            break;
        }
    }
    result
}

/// Extract only `<key_findings>` and `<strategy>` sections from scratchpad content.
/// Used during evaluation phase to provide focused context without ephemeral working memory.
/// Falls back to the full scratchpad if no structured sections are found.
fn extract_evaluation_sections(text: &str) -> String {
    let mut sections = Vec::new();

    // Extract <key_findings>...</key_findings>
    if let Some(start) = text.find("<key_findings>") {
        if let Some(end_offset) = text[start..].find("</key_findings>") {
            let end = start + end_offset + "</key_findings>".len();
            sections.push(&text[start..end]);
        }
    }

    // Extract <strategy>...</strategy>
    if let Some(start) = text.find("<strategy>") {
        if let Some(end_offset) = text[start..].find("</strategy>") {
            let end = start + end_offset + "</strategy>".len();
            sections.push(&text[start..end]);
        }
    }

    if sections.is_empty() {
        // No structured sections found — fall back to full scratchpad
        text.to_string()
    } else {
        sections.join("\n\n")
    }
}

/// Rough cost estimate for an LLM call in USD.
/// Uses approximate per-model pricing; falls back to generic rates.
fn estimate_llm_cost_usd(
    model: &str,
    input_tokens: u32,
    output_tokens: u32,
    _reasoning_tokens: u32,
    cached_tokens: u32,
) -> f64 {
    // Pricing per 1M tokens (approximate, as of 2024-2025)
    let (input_per_m, output_per_m, cached_discount) =
        if model.contains("gpt-4") && model.contains("o") {
            // GPT-4o: $2.50/$10 per 1M
            (2.50, 10.0, 0.5)
        } else if model.contains("gpt-4") {
            // GPT-4: $10/$30 per 1M
            (10.0, 30.0, 0.5)
        } else if model.contains("gpt-3.5") {
            // GPT-3.5-turbo: $0.50/$1.50 per 1M
            (0.50, 1.50, 0.5)
        } else if model.contains("claude") {
            // Claude Sonnet: $3/$15 per 1M
            (3.0, 15.0, 0.9)
        } else if model.contains("gemini") {
            // Gemini Flash: $0.075/$0.30 per 1M (very cheap)
            (0.075, 0.30, 0.75)
        } else {
            // Generic fallback: $1/$3 per 1M
            (1.0, 3.0, 0.5)
        };

    let non_cached_input = (input_tokens as u64).saturating_sub(cached_tokens as u64);
    let cached_cost = (cached_tokens as f64 / 1_000_000.0) * input_per_m * cached_discount;
    let non_cached_cost = (non_cached_input as f64 / 1_000_000.0) * input_per_m;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * output_per_m;

    cached_cost + non_cached_cost + output_cost
}

/// Splits a `submit_proposal` whose whole answer sits in `thought_process`
/// behind a `**Solution Content**`-style marker, the field itself absent or
/// blank. `None` when no marker is found.
fn split_merged_solution(obj: &serde_json::Value) -> Option<serde_json::Value> {
    let tp = obj.get("thought_process")?.as_str()?;
    let markers = [
        "\n**Solution Content**\n",
        "\n**Solution Content:**\n",
        "\n**Solution Content**:",
        "\n## Solution Content\n",
        "\n### 1. ",
        "\n## 1. ",
    ];
    let pos = markers.iter().find_map(|m| tp.find(m))?;
    let thought = tp[..pos].trim().to_string();
    let solution = if markers.iter().take(4).any(|m| tp[pos..].starts_with(m)) {
        tp[pos..]
            .trim_start_matches(|c: char| c.is_whitespace() || c == '*' || c == '#')
            .trim_start_matches("Solution Content")
            .trim_start_matches(|c: char| c == ':' || c == '*' || c == '#' || c.is_whitespace())
            .trim()
            .to_string()
    } else {
        tp[pos..].trim().to_string()
    };
    let mut repaired = obj.clone();
    repaired["thought_process"] = serde_json::Value::String(thought);
    repaired["solution_content"] = serde_json::Value::String(solution);
    Some(repaired)
}

fn empty_terminal_tool_content(terminal_tool_name: Option<&str>) -> String {
    match terminal_tool_name {
        Some("submit_proposal") => serde_json::json!({
            "thought_process": "Agent reached maximum iterations without producing a final answer.",
            "solution_content": ""
        })
        .to_string(),
        Some("submit_batch_evaluation") => serde_json::json!({
            "evaluations": []
        })
        .to_string(),
        _ => "{}".to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn react_loop(
    llm_client: &dyn AiModel,
    agent_config: &mut AgentConfig,
    prompt_set: &dyn PromptSet,
    context: &AgentContext,
    initial_prompt: Option<String>,
    input_history: Option<Vec<ChatCompletionRequestMessage>>,
    tools: &[Box<dyn Tool>],
    extra_tool_schemas: Vec<ChatCompletionTool>,
    terminal_tool_name: Option<&str>,
    // Outer structured-output retry attempt (1-indexed). Threaded so
    // `LlmRequestStart.attempt` carries the real value rather than `1`.
    outer_attempt: u32,
    running_tool_output_bytes: &mut u64,
) -> Result<AgentResponse> {
    // Telemetry emitter comes from the context (populated by the
    // worker after deserialize). Reading it here keeps the function
    // signature short and avoids the parameter-and-context double
    // threading the prior shape forced.
    let telemetry = context.telemetry.as_ref();
    let mut tool_usage_stats: HashMap<String, usize> = HashMap::new();
    #[allow(unused_assignments)]
    let mut last_system_message: Option<String> = None;
    #[allow(unused_assignments)]
    let mut last_request_body: Option<String> = None;
    let mut total_input_tokens = 0;
    let mut total_output_tokens = 0;
    let mut total_provider_usage = crate::llms::ProviderUsage::default();
    // Last backend to answer wins: a retry may land elsewhere, and where the
    // returned content came from is the one worth recording.
    let mut served_by: Option<String> = None;
    let mut scratchpad_content = if let Some(store) = &context.store {
        if let Ok(Some(content)) = store.get(&agent_config.name).await {
            info!(agent=%agent_config.name, "Loaded persistent scratchpad from Sovereign Store.");
            // Phase 6C: Evaluators only see <key_findings> + <strategy> sections
            // to keep evaluation focused and reduce context bloat.
            if context.phase == DeliberationPhase::Evaluating {
                extract_evaluation_sections(&content)
            } else {
                content
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    // Matches default_max_react_iterations() in the agent SDK (20).
    // Kept in lockstep so an agent that doesn't configure the field
    // explicitly gets the same ceiling in both the sdk-side config
    // resolver and this worker-side fallback.
    let max_iterations = agent_config
        .max_react_iterations
        .filter(|&v| v > 0)
        .unwrap_or(20) as usize;
    let max_scratchpad_size = agent_config
        .max_scratchpad_size
        .filter(|&v| v > 0)
        .unwrap_or(32_768) as usize;

    // Define the update_scratchpad tool schema
    let scratchpad_tool_schema = ChatCompletionTool {
        r#type: ChatCompletionToolType::Function,
        function: FunctionObject {
            name: "update_scratchpad".to_string(),
            description: Some("Update your persistent scratchpad memory.".to_string()),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Text to store." },
                    "mode": { "type": "string", "enum": ["append", "overwrite"] }
                },
                "required": ["content", "mode"],
                "additionalProperties": false
            })),
            strict: Some(true),
        },
    };
    let compact_history_tool_schema = ChatCompletionTool {
        r#type: ChatCompletionToolType::Function,
        function: FunctionObject {
            name: "compact_history".to_string(),
            description: Some(
                "Fold older tool-call results in your conversation history into a \
                 single LLM-summarized synopsis (appended to your scratchpad), \
                 keeping the most recent N tool calls verbatim. Call this when \
                 your context utilization climbs into the 75-90% range or when \
                 your scratchpad is nearly full — frees context budget for \
                 further reasoning."
                    .to_string(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "keep_last_n_calls": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10,
                        "description": "How many of the most recent tool-call results to keep verbatim. Default 2."
                    }
                },
                "required": ["keep_last_n_calls"],
                "additionalProperties": false
            })),
            strict: Some(true),
        },
    };

    let mut messages: Vec<ChatCompletionRequestMessage> = if let Some(h) = input_history {
        h
    } else {
        let p = initial_prompt.clone().unwrap_or_default();
        if agent_config.merge_system_prompt {
            vec![
                ChatCompletionRequestUserMessage {
                    content: p.into(),
                    ..Default::default()
                }
                .into(),
            ]
        } else {
            vec![
                ChatCompletionRequestSystemMessage {
                    content: "".into(), // Placeholder
                    ..Default::default()
                }
                .into(),
                ChatCompletionRequestUserMessage {
                    content: p.into(),
                    ..Default::default()
                }
                .into(),
            ]
        }
    };

    // Capture the original user prompt content to prevent loss or duplication when injecting scratchpad.
    // Strip any pre-existing <scratchpad>...</scratchpad> block so retries (input_history) don't
    // nest scratchpads when the content is re-merged with a fresh scratchpad_text below.
    //
    // When merge_system_prompt is true and this is a retry (input_history), the first User message
    // already contains "{system_prompt}\n\n<scratchpad>...</scratchpad>\n\n{user_query}".
    // We must extract only the user_query part (after </scratchpad>) to avoid duplicating the
    // system prompt on the next merge.
    let original_user_content = {
        let raw = messages
            .iter()
            .find_map(|m| {
                if let ChatCompletionRequestMessage::User(u) = m
                    && let async_openai::types::ChatCompletionRequestUserMessageContent::Text(t) =
                        &u.content
                {
                    return Some(t.clone());
                }
                None
            })
            .unwrap_or_default();
        extract_user_request(&raw, agent_config.merge_system_prompt)
    };

    let mut tool_schemas: Vec<ChatCompletionTool> = tools.iter().map(|t| t.schema()).collect();
    tool_schemas.extend(extra_tool_schemas);
    tool_schemas.push(scratchpad_tool_schema);
    tool_schemas.push(compact_history_tool_schema);

    let tool_map: HashMap<String, &Box<dyn Tool>> = tools.iter().map(|t| (t.name(), t)).collect();

    // Time-aware tool stripping: track iteration durations to detect when the
    // agent is running out of phase budget and should finalize immediately.
    let loop_start = std::time::Instant::now();
    let mut iteration_durations: Vec<std::time::Duration> = Vec::new();

    for iteration_index in 0..max_iterations {
        let iter_start = std::time::Instant::now();

        // Regenerate system message (without scratchpad)
        let mut system_message = agent_config
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| {
                prompt_set.get_system_message(
                    &agent_config.name,
                    context.round_number as usize,
                    context.total_rounds as usize,
                    context.phase, // Fix E0382: Clone phase because get_system_message takes value
                )
            });

        // Construct scratchpad block to be injected into User message
        let scratchpad_text = format!(
            "\n\n<scratchpad>\n(This persists across tool calls. Use the `update_scratchpad` tool to store notes.)\n{}\n</scratchpad>",
            if scratchpad_content.is_empty() {
                "(Empty)"
            } else {
                &scratchpad_content
            }
        );

        // ── Time-aware tool stripping ──────────────────────────────────
        // When remaining phase budget ≤ 3× average iteration time, strip
        // all non-terminal tools so the LLM is forced to finalize.
        let avg_iteration_secs = if iteration_durations.is_empty() {
            0.0
        } else {
            let sum: f64 = iteration_durations.iter().map(|d| d.as_secs_f64()).sum();
            sum / iteration_durations.len() as f64
        };
        let elapsed = loop_start.elapsed().as_secs_f64();
        let remaining_budget = context.phase_budget_remaining_secs - elapsed;
        let remaining_iterations = max_iterations.saturating_sub(iteration_index);
        let force_finalize_time = context.phase_budget_remaining_secs > 0.0
            && ((remaining_budget <= 0.0)
                || (avg_iteration_secs > 0.0 && remaining_budget <= avg_iteration_secs * 3.0));
        let force_finalize_window =
            resolve_finalize_window(std::env::var("NSED_FINALIZE_WINDOW").ok().as_deref());
        let force_finalize_iters =
            max_iterations > force_finalize_window && remaining_iterations <= force_finalize_window;
        let force_finalize = force_finalize_time || force_finalize_iters;

        let active_tool_schemas = if force_finalize {
            warn!(
                agent = %agent_config.name,
                trigger = if force_finalize_iters { "iter_budget_low" } else { "time_budget_low" },
                avg_iter_secs = format!("{:.2}", avg_iteration_secs),
                remaining_budget_secs = format!("{:.2}", remaining_budget),
                remaining_iterations = remaining_iterations,
                "⏱️ Budget low — stripping non-terminal tools to force finalization."
            );
            // Keep only the terminal tool
            if let Some(name) = terminal_tool_name {
                tool_schemas
                    .iter()
                    .filter(|t| t.function.name == name)
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                vec![]
            }
        } else {
            tool_schemas.clone()
        };

        // Build active tool name set for gating execution (prevents stripped
        // tools from being called via text-extraction path when force_finalize).
        let active_tool_names: std::collections::HashSet<&str> = active_tool_schemas
            .iter()
            .map(|s| s.function.name.as_str())
            .collect();

        // If native tools are disabled, we must inject tool definitions manually into the system prompt
        // so the model knows they exist and how to call them.
        if agent_config.disable_native_tools {
            if agent_config.tool_format.as_deref() == Some("nous") {
                // Inject Nous XML format (Qwen style)
                let mut tool_descs = String::new();
                for schema in &active_tool_schemas {
                    let func_schema = serde_json::json!({
                        "name": schema.function.name,
                        "description": schema.function.description,
                        "parameters": schema.function.parameters
                    });
                    tool_descs.push_str(&serde_json::to_string(&func_schema).unwrap_or_default());
                    tool_descs.push('\n');
                }

                let tool_text = format!(
                    "\n\n# Tools\n\nYou may call one or more functions to assist with the user query.\n\nYou are provided with function signatures within <tools></tools> XML tags:\n<tools>\n{}\n</tools>\n\nFor each function call, return a json object with function name and arguments within <tool_call></tool_call> XML tags:\n<tool_call>\n{{\"name\": <function-name>, \"arguments\": <args-json-object>}}\n</tool_call>\n",
                    tool_descs
                );
                system_message.push_str(&tool_text);
            } else {
                // Default Text format (Python style)
                let mut tool_text = String::from(
                    "\n\n<tools>\nYou have access to the following tools. To use them, output a function call in square brackets like:\n[tool_name(arg=\"value\")]\n\n",
                );
                for schema in &active_tool_schemas {
                    tool_text.push_str(&format!(
                        "- {}: {}\n",
                        schema.function.name,
                        schema.function.description.clone().unwrap_or_default()
                    ));
                    if let Some(params) = &schema.function.parameters {
                        let args = serde_json::to_string_pretty(params).unwrap_or_default();
                        tool_text.push_str(&format!("  Arguments: {args}\n"));
                    }
                }
                tool_text.push_str("</tools>\n");
                system_message.push_str(&tool_text);
            }
        }
        // Inject user tool awareness only when both tools are defined AND a handler is available
        if !context.user_tools.is_empty() && context.user_tool_handler.is_some() {
            system_message.push_str(
                "\n<external_tools>\n\
                 Tools prefixed 'user_' are serviced outside this system. Responses may be delayed.\n\
                 You MUST supply all required parameters defined in the tool schema. \
                 For messaging tools, always include your question or message in the 'message' parameter — never call with empty arguments.\n\
                 </external_tools>\n",
            );
        }

        last_system_message = Some(system_message.clone());

        // Update the message with system prompt content and scratchpad
        if agent_config.merge_system_prompt {
            if let Some(ChatCompletionRequestMessage::User(user_msg)) = messages.first_mut() {
                let merged = format!(
                    "{system_message}\n\n{USER_REQUEST_OPEN}\n{original_user_content}\n{USER_REQUEST_CLOSE}{scratchpad_text}"
                );
                user_msg.content =
                    async_openai::types::ChatCompletionRequestUserMessageContent::Text(merged);
            }
        } else {
            if let Some(ChatCompletionRequestMessage::System(sys_msg)) = messages.first_mut() {
                sys_msg.content = system_message.into();
            }
            // Inject scratchpad into the first User message (which contains the prompt)
            for msg in messages.iter_mut() {
                if let ChatCompletionRequestMessage::User(user_msg) = msg {
                    let merged = format!(
                        "{USER_REQUEST_OPEN}\n{original_user_content}\n{USER_REQUEST_CLOSE}{scratchpad_text}"
                    );
                    user_msg.content =
                        async_openai::types::ChatCompletionRequestUserMessageContent::Text(merged);
                    break;
                }
            }
        }

        // Runs AFTER the system + scratchpad merge so it measures the
        // real prompt size. Includes `tools` because large built-in or
        // user tool schemas can be the difference between "under 90%"
        // and "shrink-guard floor" — the original messages-only size
        // check could miss the exact failure mode this path is meant
        // to catch.
        let request_tools_for_sizing: Option<&Vec<ChatCompletionTool>> =
            if active_tool_schemas.is_empty() || agent_config.disable_native_tools {
                None
            } else {
                Some(&active_tool_schemas)
            };
        let request_chars = serde_json::to_string(&json!({
            "messages": &messages,
            "tools": request_tools_for_sizing,
        }))
        .map(|s| s.len())
        .unwrap_or(0);
        let messages_chars = request_chars;
        let chars_per_token = agent_config
            .chars_per_token
            .map(|v| v as f32)
            .unwrap_or(4.0);
        if should_auto_compact(
            messages_chars,
            agent_config.context_window,
            messages.len(),
            chars_per_token,
        ) {
            info!(
                agent = %agent_config.name,
                messages_chars,
                context_window = agent_config.context_window,
                "auto-invoking compact_history"
            );
            match compact_message_history(
                llm_client,
                agent_config,
                &messages,
                agent_config.compact_history_default_keep,
            )
            .await
            {
                Ok(result) if result.compacted_count > 0 => {
                    // A verbose summary can net-grow the request; this
                    // path exists only to dodge the shrink-guard, so
                    // gate the swap on a serialized-size shrink check.
                    // Use the same `{messages, tools}` envelope as
                    // `messages_chars` so the comparison is like-for-like.
                    let new_chars = serde_json::to_string(&json!({
                        "messages": &result.new_messages,
                        "tools": request_tools_for_sizing,
                    }))
                    .map(|s| s.len())
                    .unwrap_or(usize::MAX);
                    if new_chars >= messages_chars {
                        warn!(
                            agent = %agent_config.name,
                            old = messages_chars,
                            new = new_chars,
                            "auto-compact did not shrink request; keeping original history"
                        );
                    } else {
                        messages = result.new_messages;
                        // Stage the appended summary so a too-large
                        // candidate the squeezer can't shrink never
                        // gets persisted and re-bloats next prompt.
                        let mut candidate = scratchpad_content.clone();
                        if !candidate.is_empty() {
                            candidate.push('\n');
                        }
                        candidate.push_str("[compacted_history (auto)]\n");
                        candidate.push_str(&result.summary);
                        let final_scratchpad = match squeeze_scratchpad_if_full(
                            llm_client,
                            agent_config,
                            &candidate,
                            max_scratchpad_size,
                        )
                        .await
                        {
                            Ok(Some(squeezed)) => Some(squeezed),
                            Ok(None) if candidate.len() <= max_scratchpad_size => Some(candidate),
                            Ok(None) => {
                                warn!(
                                    agent = %agent_config.name,
                                    candidate_len = candidate.len(),
                                    max_scratchpad_size,
                                    "auto-squeeze produced no shrink and candidate \
                                     still over cap; keeping original scratchpad"
                                );
                                None
                            }
                            Err(e) => {
                                warn!(error=%e, "auto-squeeze failed; keeping scratchpad");
                                None
                            }
                        };
                        if let Some(committed) = final_scratchpad {
                            scratchpad_content = committed;
                            // In Evaluating phase the in-memory scratchpad
                            // is a subset of the canonical store; writing
                            // back would wipe non-evaluation sections.
                            if context.phase != DeliberationPhase::Evaluating
                                && let Some(store) = &context.store
                                && let Err(e) =
                                    store.set(&agent_config.name, &scratchpad_content).await
                            {
                                warn!(error=%e, "Failed to persist auto-compacted scratchpad");
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => warn!(error=%e, "auto-compact failed; SDK shrink-guard takes over"),
            }
        }

        // Create our "partial" request config, which the AiModel implementation will complete.
        let request_config = RequestConfig {
            messages: messages.clone(),
            tools: if active_tool_schemas.is_empty() || agent_config.disable_native_tools {
                None
            } else {
                Some(active_tool_schemas.clone())
            },
            // When a middleware declared a proposal schema, force a tool call each
            // turn (`required`) so the model can't end with prose instead of the
            // structured submission. The terminal tool's schema enforces the
            // shape; `required` guarantees the call happens.
            tool_choice: tool_choice_for(
                &agent_config.model_name,
                context.forced_proposal_schema.is_some(),
                !active_tool_schemas.is_empty(),
                agent_config.disable_native_tools,
            ),
            presence_penalty: if context.phase == DeliberationPhase::Evaluating {
                agent_config.presence_penalty.map(|p| p / 2.0)
            } else {
                agent_config.presence_penalty
            },
            service_tier: None,
        }
        .with_service_tier_flag(context.service_tier.as_deref());

        // Estimate input tokens for telemetry
        let messages_json = serde_json::to_string(&request_config.messages).unwrap_or_default();
        let estimated_input_tokens = (messages_json.len() as f32 / 3.0) as u32;

        let request_id = uuid::Uuid::new_v4().to_string();
        let ctx = context.telemetry_for();

        // `estimated_input_tokens` is a pre-shrink chars/3 heuristic
        // and can compute > 100%; clamp so dashboards stay honest.
        let context_utilization_pct = if agent_config.context_window > 0 {
            ((estimated_input_tokens as f64 / agent_config.context_window as f64) * 100.0)
                .clamp(0.0, 100.0)
        } else {
            0.0
        };
        if context_utilization_pct >= 90.0 {
            warn!(
                agent = %agent_config.name,
                pct = context_utilization_pct,
                estimated_input_tokens,
                context_window = agent_config.context_window,
                "context utilization ≥90% — shrink-guard imminent"
            );
        } else if context_utilization_pct >= 75.0 {
            warn!(
                agent = %agent_config.name,
                pct = context_utilization_pct,
                estimated_input_tokens,
                context_window = agent_config.context_window,
                "context utilization ≥75%"
            );
        } else if context_utilization_pct >= 50.0 {
            warn!(
                agent = %agent_config.name,
                pct = context_utilization_pct,
                estimated_input_tokens,
                context_window = agent_config.context_window,
                "context utilization ≥50%"
            );
        }
        let mut span = LlmRequestSpan::start(
            telemetry,
            &ctx,
            &request_id,
            outer_attempt,
            &agent_config.model_name,
            &agent_config.provider_id,
            estimated_input_tokens,
            context_utilization_pct,
            *running_tool_output_bytes,
        );

        // A forced tool choice is a guarantee some backends will not honour: they
        // refuse the request outright rather than ignore the parameter. Keep a
        // copy without it, so a refusal costs one retry instead of the turn.
        let relaxed_retry = relaxed_without_forced_choice(&request_config);

        let completed = llm_client
            .chat_completion(agent_config, request_config)
            .await;
        // Diagnosis, not control flow: this refusal is not retryable, but it is the
        // one that takes every tool call a seat makes down with it, so it must not
        // reach the log as an anonymous request failure.
        if let Err(e) = &completed {
            if !agent_config.provider_executed_tools.is_empty()
                && rejects_mixed_tool_array(refusal_text(e).as_str())
            {
                warn!(
                    agent_name = %agent_config.name,
                    model = %agent_config.model_name,
                    provider_executed_tools = ?agent_config.provider_executed_tools,
                    error = %e,
                    "backend refuses our tools declared beside a provider-executed one — \
                     every tool call from this seat will fail. Use the nested search-only \
                     call instead of declaring the tool alongside."
                );
            }
        }
        let completed = match (completed, relaxed_retry) {
            (Err(e), Some(relaxed)) if rejects_forced_tool_choice(refusal_text(&e).as_str()) => {
                warn!(
                    agent_name = %agent_config.name,
                    error = %e,
                    "backend refused the forced tool choice; retrying without it"
                );
                remember_refusal(&agent_config.model_name);
                llm_client.chat_completion(agent_config, relaxed).await
            }
            (completed, _) => completed,
        };

        let completion = match completed {
            Ok(res) => res,
            Err(e) => {
                span.fail(&e).await;

                // Dump failure details to file for debugging API errors (opt-in via NSED_FAILURE_DUMPS=1|full)
                let phase_str = format!("{:?}", context.phase);
                let dump_file = write_failure_dump(FailureDumpParams {
                    kind: "api_error",
                    agent_name: &agent_config.name,
                    model_name: &agent_config.model_name,
                    provider_id: &agent_config.provider_id,
                    error: &format!("{e:#}"),
                    session_id: context.session_id.as_deref(),
                    phase: Some(&phase_str),
                    round: Some(context.round_number),
                    attempt: None,
                    finish_reason: None,
                    input_tokens: None,
                    output_tokens: None,
                    response_content: None,
                    system_prompt: last_system_message.as_deref(),
                    request_body: None,
                    messages: Some(&messages),
                    failure_dumps_config: agent_config.failure_dumps.as_deref(),
                });
                if let Some(ref path) = dump_file {
                    warn!(
                        agent_name = %agent_config.name,
                        error = %e,
                        dump_file = %path,
                        "API request failed. Dump saved to file."
                    );
                } else {
                    warn!(
                        agent_name = %agent_config.name,
                        error = %e,
                        "API request failed. Set NSED_FAILURE_DUMPS=1 to save debug dumps."
                    );
                }
                // Preserve the typed `LlmError` through the anyhow
                // wrap (`From<LlmError> for anyhow::Error` keeps the
                // concrete type via `Error::source` / downcast). The
                // retry classifier in `generate_structured_output`
                // pattern-matches via `downcast_ref::<LlmError>()`
                // rather than scraping the formatted string.
                return Err(e.into());
            }
        };

        total_provider_usage.accumulate(&completion.provider_usage);
        served_by = completion.provider_backend.clone().or(served_by);

        // Complete LLM request span with telemetry
        let cost_usd = {
            let usage = completion.response.usage.clone();
            let (input_tokens, output_tokens, reasoning_tokens, cached_tokens) =
                if let Some(u) = &usage {
                    let cached = u
                        .prompt_tokens_details
                        .as_ref()
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0);
                    let reasoning = u
                        .completion_tokens_details
                        .as_ref()
                        .and_then(|d| d.reasoning_tokens)
                        .unwrap_or(0);
                    (u.prompt_tokens, u.completion_tokens, reasoning, cached)
                } else {
                    let content_len = completion
                        .response
                        .choices
                        .first()
                        .and_then(|c| c.message.content.as_deref())
                        .unwrap_or("")
                        .len();
                    let cpt = agent_config.chars_per_token.unwrap_or(4.0).max(0.1);
                    let out_est = (content_len as f64 / cpt).ceil() as u32;
                    (estimated_input_tokens, out_est, 0, 0)
                };
            estimate_llm_cost_usd(
                &agent_config.model_name,
                input_tokens,
                output_tokens,
                reasoning_tokens,
                cached_tokens,
            )
        };
        // `messages_chars` is the byte-len of the same JSON we already
        // serialized for `estimated_input_tokens`. `max_tokens_requested`
        // is the agent's configured cap (strategy may override
        // internally for vLLM context-shrink, but the requested value
        // is what tools the operator's diagnostic view).
        let messages_chars = messages_json.len() as u32;
        let max_tokens_requested = if agent_config.max_tokens > 0 {
            Some(agent_config.max_tokens as u32)
        } else {
            None
        };
        span.complete(&completion, cost_usd, messages_chars, max_tokens_requested)
            .await;

        let response = completion.response;
        let request_body = completion.raw_request;
        last_request_body = Some(request_body);
        let choice = response
            .choices
            .first()
            .context("No choice in LLM response")?;
        let finish_reason = choice.finish_reason.map(|r| format!("{r:?}"));
        if let Some(usage) = &response.usage {
            total_input_tokens += usage.prompt_tokens;
            total_output_tokens += usage.completion_tokens;
        } else {
            // Fallback: estimate tokens using chars_per_token when provider doesn't return usage
            let cpt = agent_config.chars_per_token.unwrap_or(4.0).max(0.1);
            let input_len = serde_json::to_string(&messages).unwrap_or_default().len();
            let output_len = choice.message.content.as_deref().unwrap_or("").len();
            total_input_tokens += (input_len as f64 / cpt).ceil() as u32;
            total_output_tokens += (output_len as f64 / cpt).ceil() as u32;
        }

        // Clone the message so we can modify (repair) it before pushing to history.
        let mut response_message = choice.message.clone();

        // REPAIR TRUNCATED JSON IN TOOL ARGUMENTS
        // This prevents "400 Bad Request" errors in the NEXT round if we send back
        // a history containing broken JSON strings in tool_calls.
        if agent_config.repair_invalid_escapes {
            repair_tool_calls(&mut response_message, &agent_config.name);
        }

        // Keep the original content for heuristic tool scanning (some models put tool calls inside thinking blocks)
        let full_content = response_message.content.clone().unwrap_or_default();
        let mut content = full_content.clone();

        // Check for native thinking blocks
        if agent_config.supports_native_thinking {
            while let Some(start) = content.find("<think>") {
                if let Some(end_offset) = content[start..].find("</think>") {
                    let end = start + end_offset;
                    let thought = &content[start + 7..end];
                    info!(
                        target: "nsed_activity",
                        event = "native_thinking",
                        agent = %agent_config.name,
                        content = %thought
                    );

                    // Strip the thinking block to save context space in history
                    content.replace_range(start..end + 8, "");
                } else {
                    break;
                }
            }
        }

        messages.push(
            #[allow(deprecated)]
            ChatCompletionRequestAssistantMessage {
                content: Some(
                    async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(
                        content.clone(),
                    ),
                ),
                tool_calls: if agent_config.disable_native_tools {
                    None
                } else {
                    response_message.tool_calls.clone()
                },
                function_call: None,
                refusal: None,
                name: None,
                audio: None,
            }
            .into(),
        );

        // Workaround: Some providers (vLLM) return empty tool_calls vec instead of None.
        // We treat empty tool_calls as no tool calls (final answer).
        let mut tool_calls_list = response_message.tool_calls.clone();
        let mut has_tool_calls = tool_calls_list
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false);

        // If no native tool calls, try to extract tool calls from content
        if !has_tool_calls {
            let extracted = if agent_config.tool_format.as_deref() == Some("nous") {
                extract_xml_tool_calls(&full_content)
            } else {
                extract_python_tool_calls(&full_content)
            };

            if !extracted.is_empty() {
                // Patch the last message in history to include these tool calls so vLLM accepts the subsequent Tool messages
                // BUT only if native tools are enabled. If disabled, we keep history as text-only.
                if !agent_config.disable_native_tools
                    && let Some(ChatCompletionRequestMessage::Assistant(last_msg)) =
                        messages.last_mut()
                {
                    last_msg.tool_calls = Some(extracted.clone());
                }

                tool_calls_list = Some(extracted);
                has_tool_calls = true;
                info!("Extracted tool calls from text content.");
            } else {
                // Fallback: Check for implicit JSON tool calls (for models that ignore prompt instructions)
                let json_calls = llm_repair::extraction::heuristic_json_tool_calls(&full_content);
                if !json_calls.is_empty() {
                    tool_calls_list = Some(json_calls);
                    has_tool_calls = true;
                    info!("Extracted implicit JSON tool calls from content.");
                }
            }
        }

        if has_tool_calls {
            let tool_calls = tool_calls_list.as_ref().unwrap();
            let mut tool_outputs_text = Vec::new();

            for tool_call in tool_calls {
                let tool_name = &tool_call.function.name;
                info!(tool_name = %tool_name, "LLM requested a tool call.");
                debug!(tool_arguments = %tool_call.function.arguments, "Tool call arguments.");

                info!(
                    target: "nsed_activity",
                    event = "tool_call",
                    agent = %agent_config.name,
                    tool = %tool_name,
                    arguments = %tool_call.function.arguments
                );

                // Check for terminal tool
                if terminal_tool_name == Some(tool_name) {
                    // Guard: if the response was truncated (finish_reason: Length),
                    // the terminal tool's content is likely clipped. Reject and retry
                    // with escalated max_tokens instead of returning truncated output.
                    let was_truncated = finish_reason.as_deref() == Some("Length");
                    if was_truncated {
                        warn!(
                            agent_name = %agent_config.name,
                            tool = %tool_name,
                            "Terminal tool call truncated by max_tokens. Rejecting and retrying with more tokens."
                        );
                        // Escalate max_tokens for next iteration
                        let old = agent_config.max_tokens;
                        let ceiling = if agent_config.context_window > 0 {
                            (agent_config.context_window as f64 * 0.5) as i32
                        } else {
                            32_768
                        };
                        let new_tokens = ((old as f64 * 1.5).ceil() as i32).min(ceiling);
                        if new_tokens > old {
                            agent_config.max_tokens = new_tokens;
                            warn!(
                                agent_name = %agent_config.name,
                                old_max_tokens = old,
                                new_max_tokens = new_tokens,
                                "Escalating max_tokens in react loop after terminal tool truncation."
                            );
                        }

                        // Feed back truncation error so the model retries the submission
                        let error_text = format!(
                            "ERROR: Your `{tool_name}` call was truncated (output cut off mid-response). \
                            Your submission was NOT accepted. You now have more output space. \
                            Please call `{tool_name}` again with your COMPLETE response. Do NOT shorten or summarize it."
                        );
                        if agent_config.disable_native_tools {
                            // Non-native: use a User message
                            messages.push(
                                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                                    content:
                                        async_openai::types::ChatCompletionRequestUserMessageContent::Text(
                                            error_text,
                                        ),
                                    name: None,
                                }),
                            );
                        } else {
                            // Native tools: respond with a Tool message to satisfy API protocol
                            messages.push(
                                ChatCompletionRequestToolMessage {
                                    tool_call_id: tool_call.id.clone(),
                                    content: ChatCompletionRequestToolMessageContent::Text(
                                        error_text,
                                    ),
                                }
                                .into(),
                            );
                        }
                        break; // break out of tool_calls loop, continue react loop
                    }

                    info!(
                        "LLM called terminal tool '{}'. Returning arguments.",
                        tool_name
                    );
                    return Ok(AgentResponse {
                        content: tool_call.function.arguments.clone(),
                        tool_usage: tool_usage_stats,
                        finish_reason: finish_reason.or(response_message.refusal.clone()),
                        input_tokens: Some(total_input_tokens),
                        output_tokens: Some(total_output_tokens),
                        provider_usage: total_provider_usage,
                        served_by,
                        system_prompt: last_system_message.clone(),
                        request_body: last_request_body.clone(),
                        history: messages.clone(),
                        final_scratchpad: if scratchpad_content.is_empty() {
                            None
                        } else {
                            Some(scratchpad_content.clone())
                        },
                    });
                }

                // Increment tool usage stats
                *tool_usage_stats.entry(tool_name.clone()).or_insert(0) += 1;

                let tool_exec_start = std::time::Instant::now();
                // Armed for the duration of the call: if this task is abandoned
                // while the tool is still awaiting (a question to a human is the
                // usual one), the guard reports the cancellation so the started
                // event does not sit unpaired forever.
                let mut call_guard = None;
                if let Some(store) = context.event_store.as_ref() {
                    let mut started =
                        AgentEvent::now(agent_config.name.clone(), AgentEventKind::ToolCallStarted);
                    started.call_id = Some(tool_call.id.clone());
                    started.tool_name = Some(tool_name.clone());
                    started.args_summary = Some(tool_event_preview(&tool_call.function.arguments));
                    started.job_id = context.session_id.clone();
                    started.round = Some(context.round_number);
                    started.detail = format!("calling {tool_name}");
                    if let Err(e) = store.publish(&started).await {
                        warn!("failed to record tool-call start: {e}");
                    }
                    call_guard = Some(ToolCallGuard::new(
                        store.clone(),
                        agent_config.name.clone(),
                        tool_call.id.clone(),
                        tool_name.clone(),
                        context.session_id.clone(),
                        context.round_number,
                    ));
                }
                // `tool_success` is set explicitly in every branch
                // alongside `tool_output` so the telemetry emission
                // below records the real outcome rather than inferring
                // it from the output string's prefix (which would
                // miss errors that happen to format without "Error:"
                // and would misclassify legitimate outputs that
                // happen to start with one). The initial value is
                // overwritten in every code path; the explicit
                // `false` is a
                // compile-time guarantee against an accidental fall-
                // through emitting `success: true`.
                #[allow(unused_assignments)]
                let mut tool_success: bool = false;
                let mut tool_output = if tool_name == "update_scratchpad"
                    && active_tool_names.contains("update_scratchpad")
                {
                    match serde_json::from_str::<Value>(&tool_call.function.arguments) {
                        Ok(args) => {
                            let content = args["content"].as_str().unwrap_or("").to_string();
                            let mode = args["mode"].as_str().unwrap_or("append");

                            let projected_len =
                                if mode == "overwrite" || scratchpad_content.is_empty() {
                                    content.len()
                                } else {
                                    scratchpad_content.len() + 1 + content.len()
                                };

                            if projected_len > max_scratchpad_size {
                                tool_success = false;
                                format!(
                                    "Error: Scratchpad update would exceed limit of {} chars. Current: {}. Requested: {}.",
                                    max_scratchpad_size,
                                    scratchpad_content.len(),
                                    projected_len
                                )
                            } else {
                                if mode == "overwrite" {
                                    scratchpad_content = content;
                                } else {
                                    if !scratchpad_content.is_empty() {
                                        scratchpad_content.push('\n');
                                    }
                                    scratchpad_content.push_str(&content);
                                }

                                if let Some(store) = &context.store
                                    && let Err(e) =
                                        store.set(&agent_config.name, &scratchpad_content).await
                                {
                                    warn!(error=%e, "Failed to persist scratchpad to stream store.");
                                }

                                tool_success = true;
                                format!(
                                    "Scratchpad updated. Current size: {} chars.",
                                    scratchpad_content.len()
                                )
                            }
                        }
                        Err(e) => {
                            tool_success = false;
                            format!("Error parsing arguments: {e}")
                        }
                    }
                } else if tool_name == "compact_history"
                    && active_tool_names.contains("compact_history")
                {
                    let keep = serde_json::from_str::<Value>(&tool_call.function.arguments)
                        .ok()
                        .and_then(|v| v["keep_last_n_calls"].as_u64())
                        .map(|n| n.clamp(1, 10) as usize)
                        .unwrap_or(agent_config.compact_history_default_keep.max(1));

                    match compact_message_history(llm_client, agent_config, &messages, keep).await {
                        Ok(result) if result.compacted_count == 0 => {
                            tool_success = true;
                            "compact_history: nothing to compact (history already short)."
                                .to_string()
                        }
                        Ok(result) => {
                            messages = result.new_messages;
                            // Stage the appended summary so a too-large
                            // candidate that the squeezer can't shrink
                            // never gets persisted and re-bloats next prompt.
                            let mut candidate = scratchpad_content.clone();
                            if !candidate.is_empty() {
                                candidate.push('\n');
                            }
                            candidate.push_str("[compacted_history]\n");
                            candidate.push_str(&result.summary);

                            let final_scratchpad = match squeeze_scratchpad_if_full(
                                llm_client,
                                agent_config,
                                &candidate,
                                max_scratchpad_size,
                            )
                            .await
                            {
                                Ok(Some(squeezed)) => {
                                    info!(
                                        agent = %agent_config.name,
                                        old_len = candidate.len(),
                                        new_len = squeezed.len(),
                                        "scratchpad auto-squeezed during compact_history"
                                    );
                                    Some(squeezed)
                                }
                                Ok(None) => {
                                    if max_scratchpad_size == 0
                                        || candidate.len() <= max_scratchpad_size
                                    {
                                        Some(candidate)
                                    } else {
                                        warn!(
                                            agent = %agent_config.name,
                                            candidate_len = candidate.len(),
                                            max = max_scratchpad_size,
                                            "compact_history candidate exceeds scratchpad cap and \
                                             squeeze couldn't recover; keeping pre-compaction \
                                             scratchpad"
                                        );
                                        None
                                    }
                                }
                                Err(e) => {
                                    warn!(error=%e, "scratchpad squeeze failed; keeping pre-compaction scratchpad");
                                    None
                                }
                            };

                            if let Some(committed) = final_scratchpad {
                                scratchpad_content = committed;
                                // Same Eval-phase guard as the auto-invoke
                                // path above.
                                if context.phase != DeliberationPhase::Evaluating
                                    && let Some(store) = &context.store
                                    && let Err(e) =
                                        store.set(&agent_config.name, &scratchpad_content).await
                                {
                                    warn!(error=%e, "Failed to persist scratchpad after compact_history.");
                                }
                            }

                            tool_success = true;
                            format!(
                                "Compacted {} earlier tool calls into scratchpad. \
                                 Scratchpad now {} chars. Continue reasoning with \
                                 the compacted history.",
                                result.compacted_count,
                                scratchpad_content.len()
                            )
                        }
                        Err(e) => {
                            warn!(error=%e, "compact_history call failed");
                            tool_success = false;
                            format!("compact_history failed: {e}")
                        }
                    }
                } else if refused_by_active_set(
                    &active_tool_names,
                    &agent_config.provider_executed_tools,
                    tool_name,
                ) {
                    // Two different causes reach here and they need different
                    // responses from the model. Blaming "the current phase" for a
                    // budget strip sent it looking for a phase it could retry in,
                    // when what it actually had to do was answer now.
                    let known_here = tool_map.contains_key(tool_name.as_str());
                    warn!(
                        tool_name = %tool_name,
                        stripped_for_budget = known_here,
                        "LLM called a tool that is not in the active set."
                    );
                    tool_success = false;
                    if known_here {
                        format!(
                            "Error: Tool `{tool_name}` is withdrawn — this task is out of time \
                             budget and must finalize now. Do not call any more tools; produce \
                             your final answer with `{}` immediately.",
                            terminal_tool_name.unwrap_or("the terminal tool")
                        )
                    } else {
                        format!(
                            "Error: Tool `{tool_name}` does not exist in this task. Available \
                             tools: {}.",
                            {
                                let mut names: Vec<&str> =
                                    active_tool_names.iter().copied().collect();
                                names.sort_unstable();
                                names.join(", ")
                            }
                        )
                    }
                } else if agent_config
                    .provider_executed_tools
                    .iter()
                    .any(|t| t == tool_name)
                {
                    // The backend runs this one. Its contract is that we hand
                    // the arguments straight back: the result the model reads
                    // is produced provider-side, not here.
                    debug!(
                        agent = %agent_config.name,
                        tool = %tool_name,
                        "Provider-executed tool — echoing arguments back for the backend to run"
                    );
                    tool_success = true;
                    tool_call.function.arguments.clone()
                } else if let Some(tool) = tool_map.get(tool_name) {
                    let arg_str = &tool_call.function.arguments;
                    let args_result = if arg_str.trim().is_empty() {
                        Ok(serde_json::json!({}))
                    } else {
                        serde_json::from_str::<Value>(arg_str).map_err(|e| e.to_string())
                    };

                    match args_result {
                        Ok(args) => match tool.call(args).await {
                            Ok(result) => {
                                debug!(tool_name = %tool_name, output_length = result.len(), "Tool executed successfully.");
                                tool_success = true;
                                result
                            }
                            Err(e) => {
                                warn!(tool_name = %tool_name, error = %e, "Tool execution failed.");
                                tool_success = false;
                                format!("Error calling tool `{tool_name}`: {e}")
                            }
                        },
                        Err(e) => {
                            let msg = format!(
                                "Error parsing arguments for tool '{}': {}. Raw arguments: '{}'",
                                tool_name, e, arg_str
                            );
                            warn!(tool_name=%tool_name, error=%e, raw_args=%arg_str, "Failed to parse tool arguments.");
                            tool_success = false;
                            msg
                        }
                    }
                } else {
                    warn!(tool_name = %tool_name, "LLM called a tool that does not exist.");
                    tool_success = false;
                    format!("Error: Tool `{tool_name}` not found.")
                };

                let truncated = apply_tool_output_cap(
                    &mut tool_output,
                    agent_config.context_window,
                    estimated_input_tokens,
                    tool_name,
                );

                let latency_ms = tool_exec_start.elapsed().as_millis() as u64;
                if let Some(store) = context.event_store.as_ref() {
                    let mut finished = AgentEvent::now(
                        agent_config.name.clone(),
                        AgentEventKind::ToolCallFinished,
                    );
                    finished.call_id = Some(tool_call.id.clone());
                    finished.tool_name = Some(tool_name.clone());
                    finished.job_id = context.session_id.clone();
                    finished.round = Some(context.round_number);
                    finished.status = Some(if tool_success { "ok" } else { "error" }.to_string());
                    finished.detail =
                        format!("{}ms · {}", latency_ms, tool_event_preview(&tool_output));
                    if let Err(e) = store.publish(&finished).await {
                        warn!("failed to record tool-call finish: {e}");
                    }
                }
                // The outcome is on the wire; the guard must not also claim a
                // cancellation when it drops at the end of this iteration.
                if let Some(guard) = call_guard.as_mut() {
                    guard.disarm();
                }
                drop(call_guard);
                let output_bytes = tool_output.len() as u64;
                // 4 chars/token rule-of-thumb; ceiling so non-empty
                // outputs under 4 bytes still report 1 token.
                let output_tokens_estimated =
                    Some((output_bytes.div_ceil(4)).min(u32::MAX as u64) as u32);
                *running_tool_output_bytes = running_tool_output_bytes.saturating_add(output_bytes);
                emit_for!(
                    context,
                    ToolCallExecuted {
                        tool_name: tool_name.clone(),
                        latency_ms,
                        success: tool_success,
                        output_bytes,
                        output_tokens_estimated,
                        truncated,
                        paginated: false,
                    }
                );

                info!(
                    target: "nsed_activity",
                    event = "tool_output",
                    agent = %agent_config.name,
                    tool = %tool_name,
                    output = %tool_output
                );

                if agent_config.disable_native_tools {
                    tool_outputs_text.push(format!("Tool Output ({tool_name}): {tool_output}"));
                } else {
                    messages.push(
                        ChatCompletionRequestToolMessage {
                            tool_call_id: tool_call.id.clone(),
                            content: ChatCompletionRequestToolMessageContent::Text(tool_output),
                        }
                        .into(),
                    );
                }
            }

            if agent_config.disable_native_tools && !tool_outputs_text.is_empty() {
                // Consolidate all tool outputs into a single User message to satisfy strict chat templates
                let combined_output = tool_outputs_text.join("\n\n");
                messages.push(
                    ChatCompletionRequestUserMessage {
                        content: async_openai::types::ChatCompletionRequestUserMessageContent::Text(
                            combined_output,
                        ),
                        ..Default::default()
                    }
                    .into(),
                );
            }
        } else {
            // No tool calls, this is the final answer.
            info!("LLM provided a final answer without a tool call.");
            return Ok(AgentResponse {
                content,
                tool_usage: tool_usage_stats,
                finish_reason,
                input_tokens: Some(total_input_tokens),
                output_tokens: Some(total_output_tokens),
                provider_usage: total_provider_usage,
                served_by,
                system_prompt: last_system_message,
                request_body: last_request_body,
                history: messages.clone(),
                final_scratchpad: if scratchpad_content.is_empty() {
                    None
                } else {
                    Some(scratchpad_content.clone())
                },
            });
        }

        // Record this iteration's duration for the time-aware tool stripping heuristic.
        iteration_durations.push(iter_start.elapsed());
    }

    // Max-iterations exhaustion: instead of bubbling an error that
    // blocks the orchestrator waiting for this agent's proposal /
    // evaluation, synthesize an empty-but-schema-valid terminal tool
    // response so the worker publishes SOMETHING on the agent's phase
    // subject. The orchestrator then aggregates it with the rest and
    // the deliberation proceeds. Erroring here would have left peers
    // blocked on the per-phase SLA floor before the missing agent got
    // an implicit max-score injection — expensive and visible.
    //
    // The synthetic content mirrors the expected terminal tool's JSON
    // shape so the caller's `serde_json::from_str::<T>(...)` succeeds
    // on first try (no retry loop burning budget with a known-stuck
    // agent):
    //   - submit_proposal:          empty solution_content, explanation
    //                                in thought_process so downstream
    //                                scoring sees the agent "voted
    //                                empty" rather than crashed.
    //   - submit_batch_evaluation:  evaluations: []  — no votes cast.
    //   - unknown terminal tool:    {} (fall back to generic object;
    //                                caller may still fail to deserialize,
    //                                but at least we don't crash here).
    warn!(
        agent = %agent_config.name,
        terminal_tool = ?terminal_tool_name,
        "Agent reached maximum iterations without a final answer — emitting empty synthetic terminal tool response so peers are not blocked."
    );
    let synthetic_content = empty_terminal_tool_content(terminal_tool_name);
    Ok(AgentResponse {
        content: synthetic_content,
        tool_usage: tool_usage_stats,
        finish_reason: Some("max_iterations".to_string()),
        input_tokens: Some(total_input_tokens),
        output_tokens: Some(total_output_tokens),
        provider_usage: total_provider_usage,
        served_by,
        system_prompt: last_system_message,
        request_body: last_request_body,
        history: messages.clone(),
        final_scratchpad: if scratchpad_content.is_empty() {
            None
        } else {
            Some(scratchpad_content.clone())
        },
    })
}

/// Size of the "terminate or die" tail window — the last N iterations
/// of `react_loop` have non-terminal tools stripped so the model's
/// only legal move is to call the terminal tool. N retries absorb
/// transient malformed terminal calls (truncation, bad JSON).
/// Override per-workload via `NSED_FINALIZE_WINDOW`; default 3
/// balances empirical LLM tool-call error rates (~5–20% per attempt)
/// against wasted free-thinking budget.
///
/// Pure-input form (consumes `Option<&str>` instead of reading the env
/// directly) so the resolution can be unit-tested without env-var
/// race hazards.
const DEFAULT_FINALIZE_WINDOW: usize = 3;

fn resolve_finalize_window(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_FINALIZE_WINDOW)
}

/// What a failed structured-output parse looked like, in the terms a reader needs
/// to act on it.
pub(crate) struct ParseFailure<'a> {
    pub attempts: usize,
    /// The model that produced the unparseable output. A roster is a list of
    /// models, and a seat that exhausts its retries is out of the deliberation —
    /// without this the log says a parse failed but not whose, so a council
    /// running at half strength reads as one that simply had less to say.
    pub model: &'a str,
    /// The parser's own complaint.
    pub last_error: &'a str,
    pub finish_reason: &'a str,
    /// The provider's content before any cleaning.
    pub raw: &'a str,
    /// What was actually handed to the parser.
    pub cleaned: &'a str,
    /// Output tokens the provider billed for the attempt.
    pub output_tokens: u32,
}

/// Longest payload excerpt carried in an error message. Enough to see the shape
/// of a malformed answer; the whole thing is in the debug log either way.
const PARSE_FAILURE_EXCERPT: usize = 1_000;

/// Explain a parse failure so the reason is visible without opening a log.
///
/// An empty payload is the case worth naming. `Raw: ''` reads as a parser bug
/// when the truth is that there was nothing to parse, and the causes need
/// different fixes: tokens billed with no content means the text went to a
/// channel nobody is reading — a reasoning model whose answer never reached the
/// content field — while zero tokens means the provider returned nothing at all.
pub(crate) fn describe_parse_failure(f: ParseFailure<'_>) -> String {
    let head = format!(
        "Failed to parse structured output from {} after {} attempts. Last error: {}. FinishReason: {}.",
        f.model, f.attempts, f.last_error, f.finish_reason
    );
    if !f.cleaned.is_empty() {
        let total = f.cleaned.chars().count();
        let excerpt = if total > PARSE_FAILURE_EXCERPT {
            format!(
                "{}... (truncated, {total} chars total)",
                f.cleaned
                    .chars()
                    .take(PARSE_FAILURE_EXCERPT)
                    .collect::<String>()
            )
        } else {
            f.cleaned.to_string()
        };
        return format!("{head} Raw: '{excerpt}'");
    }
    // Nothing reached the parser. Say which way it went missing.
    if !f.raw.is_empty() {
        return format!(
            "{head} The payload was empty after cleaning, from {} chars of provider content \
             (output_tokens={}) — cleaning discarded it, so the content did not look like the \
             expected envelope.",
            f.raw.chars().count(),
            f.output_tokens
        );
    }
    if f.output_tokens > 0 {
        return format!(
            "{head} The provider returned NO content while billing {} output tokens — the answer \
             went somewhere this client does not read (a reasoning/thinking channel rather than \
             the content field). Check the model's output format, not the parser.",
            f.output_tokens
        );
    }
    format!(
        "{head} The provider returned no content and billed no output tokens — it produced \
         nothing at all. Check the request (context length, content filter, upstream capacity), \
         not the parser."
    )
}

/// Reports a tool call as cancelled if it never reached its own finish event.
///
/// A tool that waits on a human — `ask_user` and friends — is awaited inside the
/// react loop, so when the phase budget abandons the task the future is dropped
/// mid-await and the finish event is never published. The started event then sits
/// unpaired forever, and the dashboard shows the call as still pending while the
/// question it belongs to has already expired: two views of the same fact
/// disagreeing, with the stuck one looking like a hang.
///
/// [`disarm`](Self::disarm) is called once the real outcome has been published.
/// Anything else — a drop from cancellation, an early return, a panic — publishes
/// `cancelled` instead. `Drop` cannot await, so the write is handed to a detached
/// task; losing it costs a pending row, which is what we had before.
struct ToolCallGuard {
    store: crate::status::agent_events::AgentEventStore,
    agent: String,
    call_id: String,
    tool_name: String,
    job_id: Option<String>,
    round: u32,
    started: std::time::Instant,
    armed: bool,
}

impl ToolCallGuard {
    fn new(
        store: crate::status::agent_events::AgentEventStore,
        agent: String,
        call_id: String,
        tool_name: String,
        job_id: Option<String>,
        round: u32,
    ) -> Self {
        Self {
            store,
            agent,
            call_id,
            tool_name,
            job_id,
            round,
            started: std::time::Instant::now(),
            armed: true,
        }
    }

    /// The call published its own outcome; stop reporting a cancellation.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ToolCallGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut event = AgentEvent::now(self.agent.clone(), AgentEventKind::ToolCallFinished);
        event.call_id = Some(self.call_id.clone());
        event.tool_name = Some(self.tool_name.clone());
        event.job_id = self.job_id.clone();
        event.round = Some(self.round);
        event.status = Some("cancelled".to_string());
        event.detail = format!(
            "{}ms · cancelled — the task ended before this call returned",
            self.started.elapsed().as_millis()
        );
        let store = self.store.clone();
        let tool_name = self.tool_name.clone();
        // `tokio::spawn` panics with no runtime, and a drop can land during
        // shutdown. A panic in Drop is worse than a missing dashboard row, so ask
        // for the handle rather than assuming one.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(e) = store.publish(&event).await {
                        warn!(tool_name = %tool_name, error = %e, "failed to record a cancelled tool call");
                    }
                });
            }
            Err(_) => {
                warn!(
                    tool_name = %tool_name,
                    "tool call cancelled with no runtime to report it; leaving the call unpaired"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agents::nsed_agent::extract_user_request;

    /// The live shape a condenser lost three rounds to: valid JSON, the whole
    /// answer in `thought_process` behind a `**Solution Content**:` marker,
    /// `solution_content` blank.
    #[test]
    fn an_answer_hiding_in_the_thought_process_is_split_out() {
        use super::split_merged_solution;
        let merged = serde_json::json!({
            "thought_process": "Critique integration notes.\n\n**Solution Content**: The distilled answer.",
            "solution_content": ""
        });
        let repaired = split_merged_solution(&merged).expect("marker found");
        assert_eq!(repaired["solution_content"], "The distilled answer.");
        assert_eq!(repaired["thought_process"], "Critique integration notes.");

        // No marker → nothing to split; the caller must refuse instead.
        let hopeless = serde_json::json!({
            "thought_process": "just reasoning",
            "solution_content": ""
        });
        assert!(split_merged_solution(&hopeless).is_none());
    }

    #[test]
    fn a_backend_run_tool_call_is_not_refused_by_the_active_set() {
        use super::refused_by_active_set;
        let active: std::collections::HashSet<&str> =
            ["submit_proposal", "read_proposal"].into_iter().collect();
        let provider = vec!["web_search".to_string()];

        // The backend runs this one; the active set knows only local tools.
        assert!(!refused_by_active_set(&active, &provider, "web_search"));
        // A local tool in the set passes as before.
        assert!(!refused_by_active_set(
            &active,
            &provider,
            "submit_proposal"
        ));
        // A name nobody owns is still refused.
        assert!(refused_by_active_set(&active, &provider, "made_up_tool"));
        // No provider tools declared: unknown names are refused as before.
        assert!(refused_by_active_set(&active, &[], "web_search"));
    }

    #[test]
    fn user_request_is_recovered_from_a_prompt_with_a_trailing_scratchpad() {
        let merged = "SYSTEM PROMPT v2\n\n<user_request>\nWhat is 2+2?\n</user_request>\n\n<scratchpad>\nnotes\n</scratchpad>";
        assert_eq!(extract_user_request(merged, true), "What is 2+2?");
        assert_eq!(extract_user_request(merged, false), "What is 2+2?");
    }

    #[test]
    fn re_merging_does_not_accumulate_system_prompts_or_scratchpads() {
        // One iteration's output is the next iteration's input; the loop must
        // converge on the same request rather than nesting its own wrapper.
        let first =
            "SYS A\n\n<user_request>\nQ\n</user_request>\n\n<scratchpad>\nn1\n</scratchpad>";
        let recovered = extract_user_request(first, true);
        let second = format!(
            "SYS B\n\n<user_request>\n{recovered}\n</user_request>\n\n<scratchpad>\nn2\n</scratchpad>"
        );
        assert_eq!(extract_user_request(&second, true), "Q");
        assert_eq!(second.matches("<user_request>").count(), 1);
        assert!(!second.contains("SYS A"));
    }

    #[test]
    fn legacy_leading_scratchpad_prompts_still_parse() {
        // Histories in flight when this ordering changed.
        let legacy = "SYSTEM\n\n<scratchpad>\nnotes\n</scratchpad>\n\nWhat is 2+2?";
        assert_eq!(extract_user_request(legacy, true), "What is 2+2?");
    }

    #[test]
    fn an_undelimited_prompt_falls_back_to_stripping_the_scratchpad() {
        let plain = "What is 2+2?";
        assert_eq!(extract_user_request(plain, false), "What is 2+2?");
    }

    use super::{ParseFailure, describe_parse_failure};

    /// The prompt can describe an axis at any length; this schema decides
    /// whether an evaluator is allowed to answer on it. With
    /// `additionalProperties: false`, an axis absent here is one a strict
    /// backend rejects the whole tool call for attempting.
    #[test]
    fn the_evaluation_schema_lets_an_evaluator_answer_on_every_axis() {
        let schema = super::eval_item_properties_schema();
        let cats = &schema["category_scores"];
        let props = cats["properties"]
            .as_object()
            .expect("category_scores advertises properties");

        for axis in [
            "correctness",
            "completeness",
            "novelty",
            "feasibility",
            "evidence_quality",
            "conciseness",
        ] {
            assert!(
                props.contains_key(axis),
                "{axis} is scored internally but the schema forbids sending it"
            );
        }

        let required: Vec<&str> = cats["required"]
            .as_array()
            .expect("required list")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        let optional: Vec<&String> = props
            .keys()
            .filter(|a| !required.contains(&a.as_str()))
            .collect();
        assert!(
            optional.is_empty(),
            "advertised but not required: {optional:?} — a partial score object \
             would be accepted and the missing axes read as neutral zeros"
        );
    }

    fn failure<'a>(raw: &'a str, cleaned: &'a str, output_tokens: u32) -> ParseFailure<'a> {
        ParseFailure {
            attempts: 6,
            model: "some-vendor/some-model",
            last_error: "EOF while parsing a value at line 1 column 0",
            finish_reason: "Stop",
            raw,
            cleaned,
            output_tokens,
        }
    }

    /// The refusal that takes every tool call from a seat with it. Recognising it
    /// by wording is what turns an agent that never proposes into a named cause;
    /// an unrelated failure must not be labelled with a fix that would not help.
    #[test]
    fn a_mixed_tool_array_refusal_is_told_apart_from_other_failures() {
        for refused in [
            "Multiple tools are supported only when they are all search tools",
            "multiple tools are supported only when they are ALL SEARCH TOOLS",
        ] {
            assert!(
                super::rejects_mixed_tool_array(refused),
                "must recognise the refusal: {refused}"
            );
        }
        for unrelated in [
            "context length exceeded",
            "tool_choice is not supported for this model",
            "rate limit reached",
        ] {
            assert!(
                !super::rejects_mixed_tool_array(unrelated),
                "must not blame the tool array for: {unrelated}"
            );
        }
    }

    /// A seat that exhausts its retries is out of the deliberation, and the roster
    /// it came from is a list of models. Without the model in the message the log
    /// says a parse failed but not whose, so a council quietly running at half
    /// strength reads as a council that simply had less to say.
    #[test]
    fn a_failure_names_the_model_that_produced_it() {
        let msg = describe_parse_failure(failure("{oops", "{oops", 40));
        assert!(
            msg.contains("some-vendor/some-model"),
            "the failing model must be named: {msg}"
        );
    }

    #[test]
    fn a_malformed_payload_is_quoted() {
        let msg = describe_parse_failure(failure("{oops", "{oops", 40));
        assert!(msg.contains("Raw: '{oops'"), "{msg}");
    }

    #[test]
    fn a_long_payload_says_how_much_was_cut() {
        let long = "x".repeat(1_500);
        let msg = describe_parse_failure(failure(&long, &long, 400));
        assert!(msg.contains("truncated, 1500 chars total"), "{msg}");
        assert!(msg.len() < 1_300, "the excerpt is bounded: {}", msg.len());
    }

    #[test]
    fn an_empty_payload_that_cost_tokens_points_away_from_the_parser() {
        // The live case: FinishReason Stop, nothing to parse, tokens billed. The
        // answer went to a channel this client does not read, and reporting
        // "Raw: \'\'" sent readers hunting through the parser instead.
        let msg = describe_parse_failure(failure("", "", 512));
        assert!(
            msg.contains("NO content while billing 512 output tokens"),
            "{msg}"
        );
        assert!(msg.contains("reasoning"), "names where to look: {msg}");
        assert!(
            !msg.contains("Raw:"),
            "does not pretend to show a payload: {msg}"
        );
    }

    #[test]
    fn an_empty_payload_that_cost_nothing_points_at_the_request() {
        let msg = describe_parse_failure(failure("", "", 0));
        assert!(msg.contains("billed no output tokens"), "{msg}");
        assert!(msg.contains("Check the request"), "{msg}");
    }

    #[test]
    fn cleaning_away_the_whole_payload_is_reported_as_such() {
        // Distinct from the provider sending nothing: content arrived and our own
        // cleaning discarded it, which is our bug rather than theirs.
        let msg = describe_parse_failure(failure("Here you go! (no json)", "", 30));
        assert!(msg.contains("empty after cleaning"), "{msg}");
        assert!(msg.contains("22 chars of provider content"), "{msg}");
    }

    // Echoes the content it was handed as the "reason" so a test can assert what
    // solution_content the react loop extracted; `reject=false` accepts (None).
    #[derive(Debug)]
    struct EchoValidator {
        reject: bool,
    }
    #[async_trait::async_trait]
    impl crate::agents::SubmissionValidator for EchoValidator {
        async fn validate(&self, content: &str) -> Option<String> {
            self.reject.then(|| content.to_string())
        }
    }

    // Minimal OutputLeakDetector: treats the literal "LEAK" as an indicator.
    // `redact` strips it; `scan`/`evaluate` block when present. Enough to drive
    // `redact_terminal_leak` without pulling in the real regex middleware
    // (which lives in the downstream `nsed-agent` crate).
    #[derive(Debug)]
    struct LeakStub;
    #[async_trait::async_trait]
    impl crate::agents::OutputLeakDetector for LeakStub {
        fn scan(&self, text: &str) -> crate::agents::OutputScanResult {
            let hits = text.matches("LEAK").count() as u32;
            crate::agents::OutputScanResult {
                tool_name_hits: hits,
                response_length_chars: text.chars().count() as u32,
                ..Default::default()
            }
        }
        async fn evaluate(
            &self,
            ctx: &crate::middleware::MiddlewareContext,
        ) -> crate::middleware::MiddlewareVerdict {
            let text = ctx.content.as_str().unwrap_or_default();
            if text.contains("LEAK") {
                crate::middleware::MiddlewareVerdict::block("test", "leak")
            } else {
                crate::middleware::MiddlewareVerdict::pass()
            }
        }
        fn redact(&self, text: &str) -> String {
            text.replace("LEAK", "[redacted]")
        }
    }

    #[test]
    fn redact_terminal_leak_scrubs_proposal_solution_content() {
        use super::redact_terminal_leak;
        let parsed = serde_json::json!({
            "thought_process": "internal notes mentioning LEAK",
            "solution_content": "answer with a LEAK in it",
        });
        let out: serde_json::Value =
            redact_terminal_leak(&parsed, "submit_proposal", &LeakStub).expect("re-deserializes");
        // User-visible field scrubbed.
        assert_eq!(out["solution_content"], "answer with a [redacted] in it");
        // thought_process is NOT a scanned field — left untouched.
        assert_eq!(out["thought_process"], "internal notes mentioning LEAK");
    }

    #[test]
    fn redact_terminal_leak_scrubs_batch_evaluation_fields() {
        use super::redact_terminal_leak;
        let parsed = serde_json::json!({
            "evaluations": [{
                "justification": "scored high but LEAK slipped in",
                "claim_assessments": [{"reason": "supported, though LEAK"}],
            }],
        });
        let out: serde_json::Value =
            redact_terminal_leak(&parsed, "submit_batch_evaluation", &LeakStub)
                .expect("re-deserializes");
        assert_eq!(
            out["evaluations"][0]["justification"],
            "scored high but [redacted] slipped in"
        );
        assert_eq!(
            out["evaluations"][0]["claim_assessments"][0]["reason"],
            "supported, though [redacted]"
        );
    }

    #[test]
    fn redact_terminal_leak_unknown_tool_scrubs_all_strings() {
        use super::redact_terminal_leak;
        let parsed = serde_json::json!({"any": "field with LEAK", "nested": {"x": "LEAK"}});
        let out: serde_json::Value =
            redact_terminal_leak(&parsed, "some_new_tool", &LeakStub).expect("re-deserializes");
        assert_eq!(out["any"], "field with [redacted]");
        assert_eq!(out["nested"]["x"], "[redacted]");
    }

    /// Detects "LEAK" but does NOT override `redact` — exercises the default
    /// identity (no-op) redaction path.
    #[derive(Debug)]
    struct NoRedactStub;
    #[async_trait::async_trait]
    impl crate::agents::OutputLeakDetector for NoRedactStub {
        fn scan(&self, text: &str) -> crate::agents::OutputScanResult {
            crate::agents::OutputScanResult {
                tool_name_hits: text.matches("LEAK").count() as u32,
                response_length_chars: text.chars().count() as u32,
                ..Default::default()
            }
        }
        async fn evaluate(
            &self,
            ctx: &crate::middleware::MiddlewareContext,
        ) -> crate::middleware::MiddlewareVerdict {
            if ctx.content.as_str().unwrap_or_default().contains("LEAK") {
                crate::middleware::MiddlewareVerdict::block("test", "leak")
            } else {
                crate::middleware::MiddlewareVerdict::pass()
            }
        }
        // redact intentionally NOT overridden → default identity (no-op).
    }

    #[tokio::test]
    async fn default_redact_noop_stays_blocked_on_rescan() {
        use super::{redact_terminal_leak, run_prompt_exposure_guard};
        // A detector with the default (identity) redact can't scrub the leak, so
        // the re-scan the caller runs must still block — the fail-closed guard
        // that stops a no-op/partial redaction from passing a leak through.
        let parsed = serde_json::json!({
            "thought_process": "t",
            "solution_content": "answer with a LEAK in it",
        });
        let redacted: serde_json::Value =
            redact_terminal_leak(&parsed, "submit_proposal", &NoRedactStub)
                .expect("re-deserializes");
        assert_eq!(
            redacted["solution_content"], "answer with a LEAK in it",
            "identity redact is a no-op — the leak survives"
        );
        let rescan =
            run_prompt_exposure_guard(&redacted, "submit_proposal", "agent", &NoRedactStub).await;
        assert!(
            rescan.is_some(),
            "a no-op redact must NOT clear the leak on re-scan (fail-closed)"
        );
    }

    #[tokio::test]
    async fn run_submission_validator_forwards_content_and_gates_on_tool() {
        use super::run_submission_validator;
        // The forced-schema envelope (patch-deliberation): ops live here, there is
        // NO `solution_content`. The old extract-`solution_content` logic passed the
        // validator an empty string → always accepted → blocks bypassed the retry.
        // The validator must now receive the WHOLE envelope verbatim.
        let envelope = r#"{"textual_response_to_user":"did the edit","ops":[{"op":"str_replace","path":"a.md","old":"x","new":"y"}]}"#;
        let reject = EchoValidator { reject: true };
        assert_eq!(
            run_submission_validator(envelope, "submit_proposal", Some(&reject)).await,
            Some(envelope.to_string()),
            "the full forced-schema envelope must reach the validator, not an empty string"
        );
        // A different terminal tool (evaluation) is never validated.
        assert_eq!(
            run_submission_validator(envelope, "submit_batch_evaluation", Some(&reject)).await,
            None
        );
        // No validator injected → accept.
        assert_eq!(
            run_submission_validator(envelope, "submit_proposal", None).await,
            None
        );
        // Validator that accepts → None.
        let accept = EchoValidator { reject: false };
        assert_eq!(
            run_submission_validator(envelope, "submit_proposal", Some(&accept)).await,
            None
        );
    }

    #[test]
    fn relaxing_a_request_keeps_it_whole_and_only_frees_the_choice() {
        use super::relaxed_without_forced_choice;
        let base = RequestConfig {
            messages: vec![],
            tools: None,
            tool_choice: Some(async_openai::types::ChatCompletionToolChoiceOption::Required),
            presence_penalty: Some(1.5),
            service_tier: None,
        };
        let relaxed = relaxed_without_forced_choice(&base).expect("a forced choice can be relaxed");
        assert!(
            relaxed.tool_choice.is_none(),
            "the retry must leave the choice to the model"
        );
        assert_eq!(
            relaxed.presence_penalty, base.presence_penalty,
            "the retry must otherwise be the same request"
        );
        assert!(
            relaxed_without_forced_choice(&RequestConfig {
                tool_choice: None,
                ..base
            })
            .is_none(),
            "a request we never forced has nothing to retry"
        );
    }

    /// Covers the DECISION, not the recording call site: this test remembers the
    /// refusal itself, so deleting `remember_refusal` from the retry arm still
    /// passes here. Reaching that line needs the async request path. What is
    /// pinned is that a remembered refusal changes the next request.
    #[test]
    #[serial]
    fn a_model_that_refused_once_is_not_asked_to_refuse_again() {
        use super::{refuses_forced_choice_already, remember_refusal, tool_choice_for};
        use async_openai::types::ChatCompletionToolChoiceOption;

        let model = "test-model-that-refuses-forced-choice";
        assert!(
            !refuses_forced_choice_already(model),
            "a model is only known to refuse after it has"
        );
        // Before: the choice is forced, so this model pays a refusal to learn.
        assert!(matches!(
            tool_choice_for(model, true, true, false),
            Some(ChatCompletionToolChoiceOption::Required)
        ));

        remember_refusal(model);

        // After: the refusal is a property of the model, so the next request
        // skips the choice rather than spending a round trip rediscovering it.
        assert!(refuses_forced_choice_already(model));
        assert!(
            tool_choice_for(model, true, true, false).is_none(),
            "a model known to refuse must not be asked again"
        );
        // A model that never refused is unaffected.
        assert!(matches!(
            tool_choice_for("some-other-model", true, true, false),
            Some(ChatCompletionToolChoiceOption::Required)
        ));
    }

    #[test]
    fn a_refusal_is_judged_on_the_response_body_not_the_terse_display() {
        use super::refusal_text;
        use crate::llms::LlmError;

        // What a live run actually produced: Display strips the reason, so a
        // detector reading the formatted error sees nothing to match on.
        let refused = LlmError::BadRequest {
            status: 400,
            body: "{\"error\":{\"message\":\"Thinking mode does not support this tool_choice\"}}"
                .to_string(),
        };
        assert_eq!(
            refused.to_string(),
            "bad request (status 400)",
            "the terse Display is what made the retry inert"
        );
        assert!(
            super::rejects_forced_tool_choice(&refusal_text(&refused)),
            "the refusal must be recognised from the body the error carries"
        );

        // An error with no body still falls back to its own text.
        let overflow = LlmError::ContextOverflow {
            tokens: 10,
            limit: 5,
        };
        assert!(
            !super::rejects_forced_tool_choice(&refusal_text(&overflow)),
            "an unrelated failure must not be read as a refusal"
        );
    }

    #[test]
    fn a_refused_forced_tool_choice_is_recognised_however_it_is_worded() {
        use super::rejects_forced_tool_choice;
        // Measured refusals. Three backends, three wordings, one shared parameter
        // name; the third arrives nested inside a generic gateway envelope.
        for msg in [
            "Thinking mode does not support this tool_choice",
            "tool_choice 'required' is incompatible with thinking enabled",
            "Provider returned error; raw: the tool_choice parameter does not support being set to required in thinking mode",
            // Named by behaviour rather than by parameter, and nested likewise.
            "Provider returned error; raw: unable to submit request because the forced function calling (mode = ANY) is not supported",
        ] {
            assert!(
                rejects_forced_tool_choice(msg),
                "a refusal naming the parameter must be recognised: {msg}"
            );
        }
        // A failure that is not about our choice of parameter must not be retried
        // as though it were — the retry drops the guarantee the parameter buys.
        for msg in [
            "1 validation error for ToolDescription: description must be a string",
            "context length exceeded",
            "insufficient credits",
        ] {
            assert!(
                !rejects_forced_tool_choice(msg),
                "an unrelated failure must not be read as a refusal: {msg}"
            );
        }
    }

    #[test]
    fn force_tool_choice_only_when_schema_and_tools() {
        use super::force_tool_choice;
        use async_openai::types::ChatCompletionToolChoiceOption;
        assert!(matches!(
            force_tool_choice(true, true, false),
            Some(ChatCompletionToolChoiceOption::Required)
        ));
        assert!(
            force_tool_choice(false, true, false).is_none(),
            "no schema → no force"
        );
        assert!(
            force_tool_choice(true, false, false).is_none(),
            "no tools → no force"
        );
        assert!(
            force_tool_choice(true, true, true).is_none(),
            "native tools disabled → no force"
        );
    }

    use super::{
        DEFAULT_FINALIZE_WINDOW, FailureDumpParams, StructuredBatchEvaluationResponse,
        StructuredProposalResponse, apply_tool_output_cap, empty_terminal_tool_content,
        extract_evaluation_sections, resolve_finalize_window, strip_scratchpad,
        strip_thinking_prefix, strip_working_memory, write_failure_dump,
    };
    use serial_test::serial;

    #[test]
    fn finalize_window_defaults_when_env_unset() {
        assert_eq!(resolve_finalize_window(None), DEFAULT_FINALIZE_WINDOW);
        assert_eq!(resolve_finalize_window(None), 3);
    }

    #[test]
    fn finalize_window_parses_valid_value() {
        assert_eq!(resolve_finalize_window(Some("5")), 5);
        assert_eq!(resolve_finalize_window(Some("1")), 1);
        assert_eq!(resolve_finalize_window(Some("10")), 10);
    }

    #[test]
    fn finalize_window_falls_back_on_malformed_value() {
        assert_eq!(
            resolve_finalize_window(Some("abc")),
            DEFAULT_FINALIZE_WINDOW
        );
        assert_eq!(resolve_finalize_window(Some("")), DEFAULT_FINALIZE_WINDOW);
        assert_eq!(
            resolve_finalize_window(Some("3.5")),
            DEFAULT_FINALIZE_WINDOW
        );
        assert_eq!(resolve_finalize_window(Some("-1")), DEFAULT_FINALIZE_WINDOW);
    }

    #[test]
    fn finalize_window_rejects_zero() {
        // Zero would disable the tail-window guard entirely (the
        // `max_iterations > N && remaining_iterations <= N` check
        // collapses to "never trigger"); treat as malformed and fall
        // back so a stray `=0` env doesn't silently turn the safety
        // net off.
        assert_eq!(resolve_finalize_window(Some("0")), DEFAULT_FINALIZE_WINDOW);
    }

    use super::{
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessage, ChatCompletionRequestToolMessage,
        ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionToolType, compact_message_history, should_auto_compact,
        squeeze_scratchpad_if_full,
    };
    use crate::agents::config::AgentConfig;
    use crate::llms::{AiModel, ChatCompletionResult, RequestConfig, TimingMetadata};
    use crate::telemetry::LlmError;
    use async_openai::types::{
        ChatChoice, ChatCompletionMessageToolCall, ChatCompletionResponseMessage, CompletionUsage,
        CreateChatCompletionResponse, FinishReason as OAFinishReason, FunctionCall, Role,
    };
    use async_trait::async_trait;

    #[derive(Clone, Debug)]
    struct CannedSummaryModel {
        text: String,
    }

    #[async_trait]
    impl AiModel for CannedSummaryModel {
        async fn chat_completion(
            &self,
            _agent: &AgentConfig,
            _request_config: RequestConfig,
        ) -> Result<ChatCompletionResult, LlmError> {
            Ok(ChatCompletionResult {
                response: CreateChatCompletionResponse {
                    id: "canned".into(),
                    object: "chat.completion".into(),
                    created: 0,
                    model: "canned".into(),
                    choices: vec![ChatChoice {
                        index: 0,
                        message: ChatCompletionResponseMessage {
                            role: Role::Assistant,
                            content: Some(self.text.clone()),
                            tool_calls: None,
                            #[allow(deprecated)]
                            function_call: None,
                            refusal: None,
                            audio: None,
                        },
                        finish_reason: Some(OAFinishReason::Stop),
                        logprobs: None,
                    }],
                    usage: Some(CompletionUsage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                        prompt_tokens_details: None,
                        completion_tokens_details: None,
                    }),
                    service_tier: None,
                    system_fingerprint: None,
                },
                raw_request: String::new(),
                timing: TimingMetadata {
                    ttft_ms: None,
                    generation_ms: None,
                },
                provider_backend: None,
                shrink_info: None,
                provider_usage: Default::default(),
            })
        }
    }

    /// Echoes back the last user message in the request, so tests can
    /// assert that specific text actually reached the summariser
    /// prompt.
    #[derive(Clone, Debug, Default)]
    struct EchoSummaryModel;

    #[async_trait]
    impl AiModel for EchoSummaryModel {
        async fn chat_completion(
            &self,
            _agent: &AgentConfig,
            request_config: RequestConfig,
        ) -> Result<ChatCompletionResult, LlmError> {
            let prompt = request_config
                .messages
                .iter()
                .rev()
                .find_map(|m| {
                    if let ChatCompletionRequestMessage::User(u) = m {
                        if let async_openai::types::ChatCompletionRequestUserMessageContent::Text(
                            t,
                        ) = &u.content
                        {
                            return Some(t.clone());
                        }
                    }
                    None
                })
                .unwrap_or_default();
            Ok(ChatCompletionResult {
                response: CreateChatCompletionResponse {
                    id: "echo".into(),
                    object: "chat.completion".into(),
                    created: 0,
                    model: "echo".into(),
                    choices: vec![ChatChoice {
                        index: 0,
                        message: ChatCompletionResponseMessage {
                            role: Role::Assistant,
                            content: Some(prompt),
                            tool_calls: None,
                            #[allow(deprecated)]
                            function_call: None,
                            refusal: None,
                            audio: None,
                        },
                        finish_reason: Some(OAFinishReason::Stop),
                        logprobs: None,
                    }],
                    usage: Some(CompletionUsage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                        prompt_tokens_details: None,
                        completion_tokens_details: None,
                    }),
                    service_tier: None,
                    system_fingerprint: None,
                },
                raw_request: String::new(),
                timing: TimingMetadata {
                    ttft_ms: None,
                    generation_ms: None,
                },
                provider_backend: None,
                shrink_info: None,
                provider_usage: Default::default(),
            })
        }
    }

    fn agent_for_compaction() -> AgentConfig {
        AgentConfig {
            name: "compactor".into(),
            ..Default::default()
        }
    }

    fn synth_tool_pair(
        call_id: &str,
        tool_name: &str,
        result: &str,
    ) -> Vec<ChatCompletionRequestMessage> {
        vec![
            ChatCompletionRequestAssistantMessage {
                tool_calls: Some(vec![ChatCompletionMessageToolCall {
                    id: call_id.into(),
                    r#type: ChatCompletionToolType::Function,
                    function: FunctionCall {
                        name: tool_name.into(),
                        arguments: "{}".into(),
                    },
                }]),
                ..Default::default()
            }
            .into(),
            ChatCompletionRequestToolMessage {
                tool_call_id: call_id.into(),
                content: ChatCompletionRequestToolMessageContent::Text(result.into()),
            }
            .into(),
        ]
    }

    #[tokio::test]
    async fn compact_history_noop_when_below_keep_threshold() {
        let model = CannedSummaryModel {
            text: "should not run".into(),
        };
        let agent = agent_for_compaction();
        let mut messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestUserMessage {
                content: "go".into(),
                ..Default::default()
            }
            .into(),
        ];
        messages.extend(synth_tool_pair("c1", "read_file", "FILE A CONTENT"));
        messages.extend(synth_tool_pair("c2", "read_file", "FILE B CONTENT"));
        let original = messages.clone();

        let result = compact_message_history(&model, &agent, &messages, 2)
            .await
            .unwrap();
        assert_eq!(result.compacted_count, 0);
        assert_eq!(result.summary, "");
        assert_eq!(result.new_messages, original);
    }

    #[tokio::test]
    async fn compact_history_folds_older_tool_calls_into_summary() {
        let model = CannedSummaryModel {
            text: "Read /linux/scripts/checkpatch.pl lines 1-200; running hypothesis: \
                   `--strict` enabled; cited verbatim: ERROR(\"missing newline\")"
                .into(),
        };
        let agent = agent_for_compaction();
        let mut messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessage {
                content: "system".into(),
                ..Default::default()
            }
            .into(),
            ChatCompletionRequestUserMessage {
                content: "go".into(),
                ..Default::default()
            }
            .into(),
        ];
        messages.extend(synth_tool_pair("c1", "read_file", "FILE A — 60K of source"));
        messages.extend(synth_tool_pair("c2", "grep_search", "8K of matches"));
        messages.extend(synth_tool_pair("c3", "pdf_query", "5K of pdf hits"));
        messages.extend(synth_tool_pair("c4", "read_file", "FILE D — 12K"));
        let pre_len = messages.len();

        let result = compact_message_history(&model, &agent, &messages, 2)
            .await
            .unwrap();

        assert_eq!(
            result.compacted_count, 2,
            "4 tool calls minus keep=2 = 2 folded"
        );
        assert!(
            result.summary.contains("checkpatch.pl"),
            "summary text round-trips"
        );
        assert!(
            result.new_messages.len() < pre_len,
            "compacted history must be shorter than input ({} → {})",
            pre_len,
            result.new_messages.len()
        );
        let has_compact_call = result.new_messages.iter().any(|m| {
            if let ChatCompletionRequestMessage::Assistant(a) = m
                && let Some(tcs) = &a.tool_calls
            {
                tcs.iter().any(|tc| tc.function.name == "compact_history")
            } else {
                false
            }
        });
        assert!(has_compact_call);
    }

    fn synth_user_tool_output(
        call_id: &str,
        tool_name: &str,
        result: &str,
    ) -> Vec<ChatCompletionRequestMessage> {
        vec![
            ChatCompletionRequestAssistantMessage {
                content: Some(
                    async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(
                        format!("calling {tool_name}"),
                    ),
                ),
                ..Default::default()
            }
            .into(),
            ChatCompletionRequestUserMessage {
                content: format!("Tool Output ({call_id} {tool_name}): {result}").into(),
                ..Default::default()
            }
            .into(),
        ]
    }

    #[tokio::test]
    async fn compact_history_text_mode_includes_user_tool_output_in_summary() {
        // Sentinel echoes back its prompt so we can assert that the
        // user-rewritten tool result actually reached the summariser.
        let agent = AgentConfig {
            disable_native_tools: true,
            ..agent_for_compaction()
        };
        let model = EchoSummaryModel;
        let mut messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessage {
                content: "system".into(),
                ..Default::default()
            }
            .into(),
            ChatCompletionRequestUserMessage {
                content: "go".into(),
                ..Default::default()
            }
            .into(),
        ];
        messages.extend(synth_user_tool_output("c1", "read_file", "FILE_A_BODY"));
        messages.extend(synth_user_tool_output("c2", "grep_search", "GREP_HITS"));
        messages.extend(synth_user_tool_output("c3", "pdf_query", "PDF_HITS"));

        let result = compact_message_history(&model, &agent, &messages, 1)
            .await
            .unwrap();

        let echoed = result.summary.clone();
        assert!(
            echoed.contains("FILE_A_BODY") || echoed.contains("GREP_HITS"),
            "summariser must see at least one User(Tool Output) body, got: {echoed}"
        );
        assert!(result.compacted_count >= 1);
    }

    #[tokio::test]
    async fn compact_history_text_mode_emits_text_only_synthetic_pair() {
        let agent = AgentConfig {
            disable_native_tools: true,
            ..agent_for_compaction()
        };
        let model = CannedSummaryModel {
            text: "summary".into(),
        };
        let mut messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessage {
                content: "system".into(),
                ..Default::default()
            }
            .into(),
            ChatCompletionRequestUserMessage {
                content: "go".into(),
                ..Default::default()
            }
            .into(),
        ];
        messages.extend(synth_user_tool_output("c1", "read_file", "A"));
        messages.extend(synth_user_tool_output("c2", "read_file", "B"));
        messages.extend(synth_user_tool_output("c3", "read_file", "C"));

        let result = compact_message_history(&model, &agent, &messages, 1)
            .await
            .unwrap();

        // No native tool_calls Assistant or Tool roles must appear in
        // the rewritten history — would break the next provider call
        // when `tools: None` is sent.
        for m in &result.new_messages {
            assert!(
                !matches!(m, ChatCompletionRequestMessage::Tool(_)),
                "text-mode compact must not emit native Tool role"
            );
            if let ChatCompletionRequestMessage::Assistant(a) = m {
                assert!(
                    a.tool_calls.is_none() || a.tool_calls.as_ref().unwrap().is_empty(),
                    "text-mode compact must not emit native tool_calls"
                );
            }
        }
        // The synthetic compaction must surface as a `Tool Output (compact_history)`
        // user message so subsequent is_tool_boundary scans see it.
        let has_marker = result.new_messages.iter().any(|m| {
            if let ChatCompletionRequestMessage::User(u) = m
                && let async_openai::types::ChatCompletionRequestUserMessageContent::Text(t) =
                    &u.content
            {
                t.contains("Tool Output (compact_history)")
            } else {
                false
            }
        });
        assert!(has_marker, "missing compact_history user-message marker");
    }

    #[tokio::test]
    async fn compact_history_clamps_keep_zero_to_one() {
        // Keep=0 would index `tool_msg_indices[len()]` and panic. The
        // internal `.max(1)` clamp must absorb the bad input.
        let agent = agent_for_compaction();
        let model = CannedSummaryModel {
            text: "summary".into(),
        };
        let mut messages: Vec<ChatCompletionRequestMessage> = vec![
            ChatCompletionRequestSystemMessage {
                content: "system".into(),
                ..Default::default()
            }
            .into(),
            ChatCompletionRequestUserMessage {
                content: "go".into(),
                ..Default::default()
            }
            .into(),
        ];
        messages.extend(synth_tool_pair("c1", "read_file", "A"));
        messages.extend(synth_tool_pair("c2", "read_file", "B"));
        let result = compact_message_history(&model, &agent, &messages, 0)
            .await
            .unwrap();
        assert_eq!(result.compacted_count, 1, "keep=0 coerced to keep=1");
    }

    #[tokio::test]
    async fn scratchpad_squeeze_noop_when_under_threshold() {
        let model = CannedSummaryModel {
            text: "should not run".into(),
        };
        let agent = agent_for_compaction();
        let scratchpad = "x".repeat(500);
        let max = 1000;
        let result = squeeze_scratchpad_if_full(&model, &agent, &scratchpad, max)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "≤95% scratchpad must not trigger LLM call"
        );
    }

    #[tokio::test]
    async fn scratchpad_squeeze_compresses_when_over_threshold() {
        let model = CannedSummaryModel {
            text: "[compressed older sections]".into(),
        };
        let agent = agent_for_compaction();
        let scratchpad = "x".repeat(960);
        let max = 1000; // 96% full → over the threshold
        let result = squeeze_scratchpad_if_full(&model, &agent, &scratchpad, max)
            .await
            .unwrap();
        let squeezed = result.expect("≥95% scratchpad triggers squeeze");
        assert!(squeezed.contains("[compressed older sections]"));
        assert!(
            squeezed.len() < scratchpad.len(),
            "squeezed must be shorter"
        );
    }

    #[test]
    fn scratchpad_squeeze_fraction_default_pinned() {
        let cfg = AgentConfig::default();
        assert!((cfg.scratchpad_squeeze_fraction - 0.95).abs() < f64::EPSILON);
        assert_eq!(cfg.compact_history_default_keep, 2);
    }

    #[test]
    fn auto_compact_triggers_at_or_above_90_percent() {
        assert!(should_auto_compact(480_000, 131_072, 10, 4.0));
        assert!(should_auto_compact(360, 100, 10, 4.0));
    }

    #[test]
    fn auto_compact_skips_under_90_percent() {
        assert!(!should_auto_compact(200_000, 131_072, 10, 4.0));
    }

    #[test]
    fn auto_compact_disabled_when_context_window_unknown() {
        // ctx <= 0 means provider doesn't expose it; let the SDK
        // shrink-guard alone handle overflow.
        assert!(!should_auto_compact(1_000_000, 0, 100, 4.0));
        assert!(!should_auto_compact(1_000_000, -1, 100, 4.0));
    }

    #[test]
    fn auto_compact_skips_when_history_too_short() {
        // High utilization but only 4 messages: nothing useful to
        // fold, so skip and let the SDK shrink-guard engage.
        assert!(!should_auto_compact(480_000, 131_072, 4, 4.0));
        assert!(!should_auto_compact(480_000, 131_072, 1, 4.0));
    }

    #[test]
    fn auto_compact_honours_configured_chars_per_token() {
        // CJK / code: ~1.5 chars per token. 200_000 chars at 1.5 →
        // 133k tokens which crosses 90% of 131k.
        assert!(should_auto_compact(200_000, 131_072, 10, 1.5));
        // Same char count at default 4.0 stays under threshold.
        assert!(!should_auto_compact(200_000, 131_072, 10, 4.0));
        // Non-positive cpt falls back to the 4.0 default rather than
        // dividing by zero / panicking.
        assert!(!should_auto_compact(200_000, 131_072, 10, 0.0));
    }

    #[test]
    fn tool_cap_truncates_when_output_exceeds_fraction_of_remaining_context() {
        // 131k ctx, 10k input → ~121k tokens remaining, 10% in chars
        // ≈ 48400 bytes. A 100KB result must clip.
        let mut output = "X".repeat(100_000);
        let truncated = apply_tool_output_cap(&mut output, 131_072, 10_000, "read_file");
        assert!(truncated, "100KB result against 131k ctx must clip");
        assert!(
            output.len() < 100_000,
            "output must be shorter than original after cap"
        );
        assert!(
            output.contains("[truncated:"),
            "marker must be present so the model sees the clip"
        );
    }

    #[test]
    fn tool_cap_passthrough_when_under_budget() {
        // 4KB result against 131k ctx → way under 10% cap → no clip.
        let mut output = "Y".repeat(4_096);
        let truncated = apply_tool_output_cap(&mut output, 131_072, 1_000, "read_file");
        assert!(!truncated);
        assert_eq!(output.len(), 4_096, "small output must pass through");
        assert!(!output.contains("[truncated:"));
    }

    #[test]
    fn tool_cap_noop_when_context_window_unknown() {
        let mut output = "Z".repeat(10_000);
        let truncated = apply_tool_output_cap(&mut output, 0, 1_000, "read_file");
        assert!(!truncated);
        assert_eq!(output.len(), 10_000);
    }

    #[test]
    fn tool_cap_respects_utf8_char_boundary() {
        // Every char is the 3-byte 中. If `cap_bytes` lands inside a
        // multi-byte char, naive `truncate` panics — `apply_tool_output_cap`
        // walks back to a boundary first.
        let mut output = "中".repeat(2_000);
        let original_byte_len = output.len();
        let truncated = apply_tool_output_cap(&mut output, 4_096, 0, "read_file");
        assert!(truncated);
        assert!(output.len() < original_byte_len);
        assert!(output.contains("[truncated:"));
        // The truncated prefix (everything before the marker) must
        // be a sequence of full `中` codepoints — no half-character
        // landed at the boundary.
        let prefix = output
            .split("\n[truncated:")
            .next()
            .expect("marker delimiter must exist");
        assert!(
            prefix.chars().all(|c| c == '中'),
            "truncated prefix must end on a full UTF-8 character; got {prefix:?}"
        );
    }

    #[test]
    fn tool_cap_replaces_with_marker_when_remaining_budget_zero() {
        // `estimated_input_tokens >= context_window` means
        // `remaining_tokens == 0` and `cap_bytes == 0`. The cap
        // must replace the entire payload with a marker so the
        // model sees the empty-budget signal instead of inheriting
        // verbatim oversized output.
        let mut output = "X".repeat(50_000);
        let truncated = apply_tool_output_cap(&mut output, 1_000, 1_000, "read_file");
        assert!(truncated);
        assert!(
            output.starts_with("[truncated: no remaining context budget"),
            "marker must replace output when budget is 0; got {:?}",
            &output[..50.min(output.len())]
        );
        assert!(output.len() < 200, "marker must be short");
    }

    // Prompt-exposure guardrail tests live alongside any concrete
    // `OutputLeakDetector` implementations — see the `with_output_guard`
    // doc-comment on `ProposerEvaluatorAgent`.

    // ─── empty_terminal_tool_content — max-iterations fallback ───────
    //
    // On exhaustion, `react_loop` emits a schema-valid but semantically
    // empty response so the orchestrator can proceed without waiting
    // on a stuck agent. The response must deserialize into the expected
    // terminal tool's struct without error.

    #[test]
    fn empty_submit_proposal_content_has_explanatory_thought_and_empty_solution() {
        let json_str = empty_terminal_tool_content(Some("submit_proposal"));
        let parsed: StructuredProposalResponse = serde_json::from_str(&json_str)
            .expect("must round-trip into StructuredProposalResponse");
        assert!(
            parsed.thought_process.contains("maximum iterations"),
            "thought_process should explain why the agent voted empty"
        );
        assert_eq!(
            parsed.solution_content, "",
            "solution_content must be empty"
        );
    }

    #[test]
    fn empty_submit_batch_evaluation_content_is_empty_array() {
        let json_str = empty_terminal_tool_content(Some("submit_batch_evaluation"));
        let parsed: StructuredBatchEvaluationResponse = serde_json::from_str(&json_str)
            .expect("must round-trip into StructuredBatchEvaluationResponse");
        assert!(parsed.evaluations.is_empty(), "evaluations must be empty");
    }

    #[test]
    fn empty_terminal_tool_content_unknown_name_returns_empty_object() {
        assert_eq!(empty_terminal_tool_content(None), "{}");
        assert_eq!(empty_terminal_tool_content(Some("other_tool")), "{}");
    }

    #[test]
    fn strip_scratchpad_removes_block() {
        let input = "\n\n<scratchpad>\n(This persists across tool calls.)\nsome notes\n</scratchpad>\n\nOriginal prompt text here";
        let result = strip_scratchpad(input);
        assert_eq!(result, "Original prompt text here");
        assert!(!result.contains("<scratchpad>"));
    }

    #[test]
    fn strip_scratchpad_preserves_text_without_block() {
        let input = "Just a normal prompt with no scratchpad";
        assert_eq!(strip_scratchpad(input), input);
    }

    #[test]
    fn strip_scratchpad_handles_only_scratchpad() {
        let input = "<scratchpad>\nsome notes\n</scratchpad>";
        let result = strip_scratchpad(input);
        assert_eq!(result, "");
    }

    #[test]
    fn strip_scratchpad_handles_unclosed_tag() {
        let input = "<scratchpad>\nsome notes without closing tag\nOriginal text";
        let result = strip_scratchpad(input);
        // Unclosed tag — return as-is
        assert_eq!(result, input);
    }

    #[test]
    fn strip_scratchpad_preserves_text_before_and_after() {
        let input = "Before text\n\n<scratchpad>\nnotes\n</scratchpad>\n\nAfter text";
        let result = strip_scratchpad(input);
        assert_eq!(result, "Before text\n\nAfter text");
    }

    /// When merge_system_prompt is true and a retry provides input_history that
    /// already contains "{sys_prompt}\n\n<scratchpad>...</scratchpad>\n\n{user_query}",
    /// extracting original_user_content must yield only the user_query (after
    /// </scratchpad>), not the entire text with system prompt prefix.
    #[test]
    fn strip_scratchpad_merge_system_prompt_extracts_after_closing_tag() {
        let merged = "You are a helpful agent.\n\n<scratchpad>\n(This persists across tool calls.)\n(Empty)\n</scratchpad>\n\nWhat is the meaning of life?";
        // Simulates what the merge_system_prompt branch does: take everything
        // after </scratchpad>, trimmed.
        let end_idx = merged.find("</scratchpad>").unwrap();
        let extracted = merged[end_idx + "</scratchpad>".len()..].trim_start();
        assert_eq!(extracted, "What is the meaning of life?");
    }

    #[test]
    fn test_utf8_safe_truncation() {
        // "日本語テスト" is 6 chars but 18 bytes
        let mut note = "日本語テスト".to_string();
        let max_len = 10; // Would land in the middle of a multi-byte char
        let suffix = "...(truncated)";
        if note.len() > max_len {
            let truncate_at = max_len.saturating_sub(suffix.len());
            let safe_at = note
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= truncate_at)
                .last()
                .unwrap_or(0);
            note.truncate(safe_at);
            note.push_str(suffix);
        }
        // Should not panic and should be valid UTF-8
        assert!(note.is_char_boundary(0));
        assert!(note.ends_with(suffix));
    }

    // =========================================================================
    // strip_working_memory() Tests
    // =========================================================================

    #[test]
    fn strip_working_memory_removes_single_block() {
        let input = "<working_memory>ephemeral thoughts</working_memory>";
        assert_eq!(strip_working_memory(input), "");
    }

    #[test]
    fn strip_working_memory_preserves_other_sections() {
        let input = "<key_findings>important stuff</key_findings>\n\n<working_memory>scratch</working_memory>\n\n<strategy>my plan</strategy>";
        let result = strip_working_memory(input);
        assert!(result.contains("<key_findings>important stuff</key_findings>"));
        assert!(result.contains("<strategy>my plan</strategy>"));
        assert!(!result.contains("<working_memory>"));
        assert!(!result.contains("scratch"));
    }

    #[test]
    fn strip_working_memory_handles_multiple_blocks() {
        let input = "<working_memory>block1</working_memory>\n\nsome text\n\n<working_memory>block2</working_memory>";
        let result = strip_working_memory(input);
        assert!(!result.contains("<working_memory>"));
        assert!(result.contains("some text"));
    }

    #[test]
    fn strip_working_memory_handles_unclosed_tag() {
        let input = "<key_findings>ok</key_findings>\n\n<working_memory>unclosed block";
        let result = strip_working_memory(input);
        // Falls back to keeping malformed content as-is
        assert!(result.contains("<working_memory>"));
    }

    #[test]
    fn strip_working_memory_no_op_when_absent() {
        let input = "<key_findings>data</key_findings>\n<strategy>plan</strategy>";
        assert_eq!(strip_working_memory(input), input);
    }

    #[test]
    fn strip_working_memory_preserves_text_before_and_after() {
        let input = "Before\n\n<working_memory>temp</working_memory>\n\nAfter";
        let result = strip_working_memory(input);
        assert_eq!(result, "Before\n\nAfter");
    }

    // =========================================================================
    // extract_evaluation_sections() Tests
    // =========================================================================

    #[test]
    fn extract_evaluation_sections_both_present() {
        let input = "<working_memory>trash</working_memory>\n<key_findings>findings here</key_findings>\n<strategy>my strategy</strategy>";
        let result = extract_evaluation_sections(input);
        assert!(result.contains("<key_findings>findings here</key_findings>"));
        assert!(result.contains("<strategy>my strategy</strategy>"));
        assert!(!result.contains("working_memory"));
        assert!(!result.contains("trash"));
    }

    #[test]
    fn extract_evaluation_sections_only_key_findings() {
        let input = "some preamble\n<key_findings>important data</key_findings>\nother stuff";
        let result = extract_evaluation_sections(input);
        assert_eq!(result, "<key_findings>important data</key_findings>");
    }

    #[test]
    fn extract_evaluation_sections_only_strategy() {
        let input = "prefix\n<strategy>my plan</strategy>\nsuffix";
        let result = extract_evaluation_sections(input);
        assert_eq!(result, "<strategy>my plan</strategy>");
    }

    #[test]
    fn extract_evaluation_sections_fallback_when_none() {
        let input = "No structured sections here, just plain text notes";
        let result = extract_evaluation_sections(input);
        assert_eq!(
            result, input,
            "Should fall back to full text when no sections found"
        );
    }

    #[test]
    fn extract_evaluation_sections_handles_multiline_content() {
        let input = "<key_findings>\n  - Finding 1\n  - Finding 2\n</key_findings>\n<strategy>\n  Step A\n  Step B\n</strategy>";
        let result = extract_evaluation_sections(input);
        assert!(result.contains("Finding 1"));
        assert!(result.contains("Finding 2"));
        assert!(result.contains("Step A"));
    }

    #[test]
    fn extract_evaluation_sections_ignores_unclosed_tags() {
        let input = "<key_findings>open section without closing\n<strategy>also unclosed";
        let result = extract_evaluation_sections(input);
        // Both unclosed → falls back to full text
        assert_eq!(result, input);
    }

    // =========================================================================
    // Regression: StructuredProposalResponse parsing
    // From failures/quant-ml_MACRO/parse_error_r1.md
    // =========================================================================

    /// Regression: Mistral returns thought_process as array of strings instead of
    /// a single string. Previously failed with "invalid type: sequence, expected a string".
    #[test]
    fn regression_proposal_thought_process_as_array() {
        let json = serde_json::json!({
            "thought_process": [
                "Step 1: Asset Class Allocation",
                "Current allocation is split as Equities (50%), Fixed Income (30%), Alternatives (20%).",
                "Step 2: Geographic Tilt",
                "The geographic tilt remains balanced."
            ],
            "solution_content": "Proposed allocation: Equities 45%, Fixed Income 35%, Alternatives 20%"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert!(
            result
                .thought_process
                .contains("Step 1: Asset Class Allocation")
        );
        assert!(result.thought_process.contains("Step 2: Geographic Tilt"));
        // Array elements joined with newlines
        assert!(result.thought_process.contains('\n'));
        assert_eq!(
            result.solution_content,
            "Proposed allocation: Equities 45%, Fixed Income 35%, Alternatives 20%"
        );
    }

    /// Regression: Mistral wraps solution_content in a nested object instead of a string.
    /// Previously failed with "invalid type: map, expected a string".
    #[test]
    fn regression_proposal_solution_content_as_nested_object() {
        let json = serde_json::json!({
            "solution_content": {
                "thought_process": ["Step 1", "Step 2"],
                "allocations": {"Equities": 45, "Fixed Income": 35},
                "hedge_portfolio": {"Strategy": "Protective Puts"}
            },
            "thought_process": "My reasoning steps."
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "My reasoning steps.");
        // Nested object serialized to JSON string
        assert!(result.solution_content.contains("Equities"));
        assert!(result.solution_content.contains("Protective Puts"));
    }

    /// Normal string values still work as expected.
    #[test]
    fn proposal_response_with_normal_strings() {
        let json = serde_json::json!({
            "thought_process": "I analyzed the problem carefully.",
            "solution_content": "The answer is 42."
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "I analyzed the problem carefully.");
        assert_eq!(result.solution_content, "The answer is 42.");
    }

    // =========================================================================
    // strip_thinking_prefix tests
    // =========================================================================

    #[test]
    fn strip_thinking_prefix_final() {
        let input = "final**Critique Integration:** The peer reviews...";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "**Critique Integration:** The peer reviews...");
    }

    #[test]
    fn strip_thinking_prefix_analysis_assistant_final() {
        let input = "analysisWe need to submit full proposal.assistantfinal**Critique Integration:** ...rest";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "**Critique Integration:** ...rest");
    }

    #[test]
    fn strip_thinking_prefix_commentary() {
        let input = "commentaryto=functions.submit_proposal json{\"thought_process\": \"test\"}";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "{\"thought_process\": \"test\"}");
    }

    #[test]
    fn strip_thinking_prefix_clean_content_unchanged() {
        let input = "**Critique Integration:** Normal content.";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "**Critique Integration:** Normal content.");
    }

    #[test]
    fn strip_thinking_prefix_json_unchanged() {
        let input = "{\"thought_process\": \"test\", \"solution_content\": \"x\"}";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, input);
    }

    // =========================================================================
    // StructuredBatchEvaluationResponse alias tests
    // =========================================================================

    /// Models use "candidate_evaluations" instead of "evaluations"
    #[test]
    fn batch_eval_alias_candidate_evaluations() {
        let json = r#"{"candidate_evaluations": [{"candidate_id": "A", "endorsement_weight": 75, "stance": "agree", "claim_assessments": [], "disagreements": [], "category_scores": {"correctness": 80, "completeness": 75, "novelty": 70, "feasibility": 85, "evidence_quality": 78}}]}"#;
        let result: StructuredBatchEvaluationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(result.evaluations.len(), 1);
        assert_eq!(result.evaluations[0].agent_id, "A");
    }

    /// Models use "candidates" instead of "evaluations"
    #[test]
    fn batch_eval_alias_candidates() {
        let json = r#"{"candidates": [{"agent_id": "B", "endorsement_weight": 60, "stance": "disagree"}]}"#;
        let result: StructuredBatchEvaluationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(result.evaluations.len(), 1);
        assert_eq!(result.evaluations[0].agent_id, "B");
    }

    /// Full gpt-oss MOMENTUM r2 eval payload with "claim_id" only in claim_assessments
    /// (missing "claim" text — uses "verdict" + "claim_id" pattern)
    #[test]
    fn batch_eval_gpt_oss_claim_id_only_assessments() {
        let json = r#"{"evaluations": [{"agent_id": "Candidate_C", "stance": "disagree", "claim_assessments": [{"claim_id": "C1", "verdict": "unverified"}, {"claim_id": "C2", "verdict": "contested"}, {"claim_id": "C3", "verdict": "verified"}], "disagreements": [{"claim_id": "C1", "proposal": "Equity allocation of 40%", "counter": "40% equity exceeds drawdown limit.", "confidence": "high"}], "category_scores": {"correctness": 40, "completeness": 30, "novelty": 20, "feasibility": 45, "evidence_quality": 30}, "endorsement_weight": 25}]}"#;
        let result: StructuredBatchEvaluationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(result.evaluations[0].disagreements.len(), 1);
        assert_eq!(
            result.evaluations[0].disagreements[0].evaluator_position,
            "40% equity exceeds drawdown limit."
        );
        assert_eq!(
            result.evaluations[0].disagreements[0].proposal_claims,
            "Equity allocation of 40%"
        );
    }

    // =========================================================================
    // write_failure_dump config precedence tests
    // =========================================================================

    /// Config "off" must suppress dumps without falling through to env var.
    #[test]
    fn dump_mode_config_off_suppresses_dumps() {
        let result = write_failure_dump(FailureDumpParams {
            kind: "test",
            agent_name: "test-agent",
            model_name: "model",
            provider_id: "provider",
            error: "some error",
            session_id: None,
            phase: None,
            round: None,
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: Some("content"),
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("off"),
        });
        // Config "off" should short-circuit and return None without
        // checking the NSED_FAILURE_DUMPS env var.
        assert!(result.is_none(), "config 'off' should suppress dumps");
    }

    /// Config None falls through to env var (no env var set = no dumps).
    #[test]
    fn dump_mode_config_none_no_env_var() {
        let result = write_failure_dump(FailureDumpParams {
            kind: "test",
            agent_name: "test-agent",
            model_name: "model",
            provider_id: "provider",
            error: "some error",
            session_id: None,
            phase: None,
            round: None,
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: Some("content"),
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: None,
        });
        // Without config and without env var, no dumps should be written.
        assert!(result.is_none(), "no config + no env var = no dumps");
    }

    // =========================================================================
    // Additional strip_scratchpad tests
    // =========================================================================

    #[test]
    fn test_strip_scratchpad_nested_tags() {
        // Nested scratchpad tags — strip_scratchpad finds the first <scratchpad>
        // and the first </scratchpad> after it, removing that range. The outer
        // closing tag remains as leftover text.
        let input = "<scratchpad><scratchpad>inner</scratchpad>outer</scratchpad>after content";
        let result = strip_scratchpad(input);
        // The function strips from first <scratchpad> to first </scratchpad>,
        // leaving "outer</scratchpad>after content" which gets trimmed.
        assert!(!result.contains("<scratchpad>inner</scratchpad>"));
        // The remaining text should include "after content".
        assert!(result.contains("after content"));
    }

    #[test]
    fn test_strip_scratchpad_empty() {
        let result = strip_scratchpad("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_strip_scratchpad_no_tags() {
        let input = "This is plain text with no scratchpad tags at all.";
        let result = strip_scratchpad(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_scratchpad_missing_close() {
        // Input with opening tag but no closing tag — should return as-is.
        let input = "<scratchpad>content without closing tag";
        let result = strip_scratchpad(input);
        assert_eq!(result, input);
    }

    // =========================================================================
    // truncate_for_dump tests
    // =========================================================================

    #[test]
    fn test_truncate_for_dump() {
        use super::truncate_for_dump;

        // Short string — unchanged.
        let short = "hello";
        assert_eq!(truncate_for_dump(short, 100), "hello");

        // Long string — truncated with indicator.
        let long: String = "a".repeat(200);
        let result = truncate_for_dump(&long, 50);
        assert!(result.len() < long.len());
        assert!(result.contains("... (truncated at 50 chars)"));
        // The truncated prefix should be exactly 50 chars from the original.
        let prefix_part = result
            .split("\n... (truncated at 50 chars)")
            .next()
            .unwrap();
        assert_eq!(prefix_part.chars().count(), 50);

        // Exact boundary — no truncation.
        let exact: String = "b".repeat(50);
        assert_eq!(truncate_for_dump(&exact, 50), exact);
    }

    // =========================================================================
    // Additional strip_working_memory tests
    // =========================================================================

    #[test]
    fn test_strip_working_memory_nested() {
        // Nested working memory tags — the while loop strips from first open to
        // first close, then repeats.
        let input =
            "<working_memory>outer<working_memory>inner</working_memory>rest</working_memory>final";
        let result = strip_working_memory(input);
        // First iteration strips "<working_memory>outer<working_memory>inner</working_memory>"
        // leaving "rest</working_memory>final". Second iteration finds no proper
        // <working_memory> open tag before </working_memory>, so the loop ends.
        // The key assertion: no <working_memory> open tags remain.
        assert!(!result.contains("<working_memory>"));
        assert!(result.contains("final"));
    }

    // =========================================================================
    // strip_thinking_prefix with JSON tests
    // =========================================================================

    #[test]
    fn test_strip_thinking_prefix_with_json() {
        // The <think> tag stripping happens in the react_loop (not strip_thinking_prefix).
        // strip_thinking_prefix handles leaked model tokens like "final", "analysis", "commentary".
        // For <think> tags, the react_loop uses string replacement (lines 1647-1663).
        // Here we verify that strip_thinking_prefix leaves JSON after known prefixes intact.
        let input = r#"final{"score": 0.5}"#;
        let result = strip_thinking_prefix(input);
        assert_eq!(result, r#"{"score": 0.5}"#);

        // Verify the result is valid JSON.
        let val: serde_json::Value = serde_json::from_str(result).unwrap();
        assert_eq!(val["score"], 0.5);
    }

    // =========================================================================
    // deserialize_string_or_array_or_object — additional coverage
    // =========================================================================

    #[test]
    fn deser_string_or_array_array_of_strings() {
        let json = serde_json::json!({
            "thought_process": ["Alpha", "Beta", "Gamma"],
            "solution_content": "plain"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "Alpha\nBeta\nGamma");
        assert_eq!(result.solution_content, "plain");
    }

    #[test]
    fn deser_string_or_array_empty_array() {
        let json = serde_json::json!({
            "thought_process": [],
            "solution_content": "content"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(
            result.thought_process, "",
            "Empty array should produce empty string"
        );
    }

    #[test]
    fn deser_string_or_array_nested_object() {
        let json = serde_json::json!({
            "thought_process": {
                "step1": "Analyze requirements",
                "step2": "Propose solution"
            },
            "solution_content": "answer"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        // Object is serialized to compact JSON string
        assert!(result.thought_process.contains("step1"));
        assert!(result.thought_process.contains("Analyze requirements"));
        // Must be valid JSON
        let _: serde_json::Value = serde_json::from_str(&result.thought_process).unwrap();
    }

    #[test]
    fn deser_string_or_array_mixed_array() {
        // Array containing both strings and non-string values (numbers, objects)
        let json = serde_json::json!({
            "thought_process": ["Step 1", 42, {"detail": "nested"}, true],
            "solution_content": "done"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        let parts: Vec<&str> = result.thought_process.split('\n').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "Step 1");
        assert_eq!(parts[1], "42");
        assert!(parts[2].contains("nested"));
        assert_eq!(parts[3], "true");
    }

    #[test]
    fn deser_string_or_array_single_element_array() {
        let json = serde_json::json!({
            "thought_process": ["Only one element"],
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "Only one element");
    }

    #[test]
    fn deser_string_or_array_number_value() {
        // A bare number should be serialized to string via the catch-all branch
        let json = serde_json::json!({
            "thought_process": 12345,
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "12345");
    }

    #[test]
    fn deser_string_or_array_boolean_value() {
        let json = serde_json::json!({
            "thought_process": true,
            "solution_content": false
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "true");
        assert_eq!(result.solution_content, "false");
    }

    #[test]
    fn deser_string_or_array_null_value() {
        let json = serde_json::json!({
            "thought_process": null,
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "null");
    }

    #[test]
    fn deser_string_or_array_deeply_nested_object() {
        let json = serde_json::json!({
            "thought_process": "simple",
            "solution_content": {
                "level1": {
                    "level2": {
                        "data": [1, 2, 3]
                    }
                }
            }
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert!(result.solution_content.contains("level2"));
        assert!(result.solution_content.contains("[1,2,3]"));
    }

    // =========================================================================
    // strip_thinking_prefix — comprehensive pattern coverage
    // =========================================================================

    #[test]
    fn strip_thinking_prefix_assistant_then_json() {
        // "assistant" followed by "{" should strip the prefix
        let input = "analysisblahassistant{\"key\": \"value\"}";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_thinking_prefix_assistant_then_heading() {
        // "assistant" followed by "#" should strip the prefix
        let input = "some thinking tokensassistant# Heading";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "# Heading");
    }

    #[test]
    fn strip_thinking_prefix_assistant_then_bold() {
        // "assistant" followed by "**" should strip the prefix
        let input = "reflectionassistant**Bold content**";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "**Bold content**");
    }

    #[test]
    fn strip_thinking_prefix_assistant_then_final_then_bold() {
        // "assistant" followed by "final" then actual content
        let input = "thinkingassistantfinal**Real content here**";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "**Real content here**");
    }

    #[test]
    fn strip_thinking_prefix_bare_final_with_heading() {
        let input = "final# Asset Allocation";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "# Asset Allocation");
    }

    #[test]
    fn strip_thinking_prefix_bare_final_with_json() {
        let input = "final{\"solution\": \"test\"}";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "{\"solution\": \"test\"}");
    }

    #[test]
    fn strip_thinking_prefix_commentary_json_with_space() {
        let input = "commentaryto=functions.submit json {\"thought_process\": \"x\"}";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "{\"thought_process\": \"x\"}");
    }

    #[test]
    fn strip_thinking_prefix_no_prefix_plain_text() {
        let input = "This is normal content with no thinking tokens.";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "This is normal content with no thinking tokens.");
    }

    #[test]
    fn strip_thinking_prefix_assistant_mid_word_no_strip() {
        // "assistant" followed by a regular letter should NOT strip
        let input = "The assistant helped me write code";
        let result = strip_thinking_prefix(input);
        // "assistant" is found, but after it is " helped" which doesn't start
        // with final/{/**/# — so the "assistant" prefix is NOT stripped.
        // "final" prefix check also doesn't match.
        assert_eq!(result, "The assistant helped me write code");
    }

    #[test]
    fn strip_thinking_prefix_empty_string() {
        let result = strip_thinking_prefix("");
        assert_eq!(result, "");
    }

    #[test]
    fn strip_thinking_prefix_only_final() {
        // "final" alone leaves empty string after stripping + trim
        let result = strip_thinking_prefix("final");
        assert_eq!(result, "");
    }

    #[test]
    fn strip_thinking_prefix_whitespace_trimming() {
        let input = "final   **Content with leading spaces**   ";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "**Content with leading spaces**");
    }

    #[test]
    fn strip_thinking_prefix_commentary_without_json_keyword() {
        // "commentary" prefix without "json{" or "json " — should remain
        let input = "commentarysome other text here";
        let result = strip_thinking_prefix(input);
        // The commentary branch looks for "json{" or "json " — neither is found,
        // so the commentary prefix is not stripped. Trim applies.
        assert_eq!(result, "commentarysome other text here");
    }

    // =========================================================================
    // extract_evaluation_sections — additional edge cases
    // =========================================================================

    #[test]
    fn extract_evaluation_sections_empty_input() {
        let result = extract_evaluation_sections("");
        assert_eq!(result, "", "Empty input should return empty string");
    }

    #[test]
    fn extract_evaluation_sections_empty_tags() {
        // Tags present but with empty content
        let input = "<key_findings></key_findings>\n<strategy></strategy>";
        let result = extract_evaluation_sections(input);
        assert!(result.contains("<key_findings></key_findings>"));
        assert!(result.contains("<strategy></strategy>"));
    }

    #[test]
    fn extract_evaluation_sections_nested_xml_inside_tags() {
        // XML-like content nested inside the sections
        let input = "<key_findings><item>A</item><item>B</item></key_findings>";
        let result = extract_evaluation_sections(input);
        assert_eq!(
            result,
            "<key_findings><item>A</item><item>B</item></key_findings>"
        );
    }

    #[test]
    fn extract_evaluation_sections_whitespace_only_input() {
        let input = "   \n\n   \t  ";
        let result = extract_evaluation_sections(input);
        // No sections found, falls back to full text
        assert_eq!(result, input);
    }

    #[test]
    fn extract_evaluation_sections_duplicate_key_findings() {
        // Only the first occurrence of each tag pair is extracted
        let input =
            "<key_findings>first</key_findings>\nsome gap\n<key_findings>second</key_findings>";
        let result = extract_evaluation_sections(input);
        // The function uses find() which returns the first match
        assert!(result.contains("first"));
    }

    #[test]
    fn extract_evaluation_sections_sections_with_surrounding_junk() {
        let input = "IGNORE THIS PREAMBLE\n\n<key_findings>Core insight: volatility is rising</key_findings>\n\nMORE JUNK\n\n<strategy>Hedge with puts</strategy>\n\nTRAILING GARBAGE";
        let result = extract_evaluation_sections(input);
        assert!(result.contains("<key_findings>Core insight: volatility is rising</key_findings>"));
        assert!(result.contains("<strategy>Hedge with puts</strategy>"));
        assert!(!result.contains("IGNORE THIS PREAMBLE"));
        assert!(!result.contains("MORE JUNK"));
        assert!(!result.contains("TRAILING GARBAGE"));
    }

    #[test]
    fn extract_evaluation_sections_strategy_before_key_findings() {
        // Order in the output: key_findings first, then strategy (regardless of input order)
        let input = "<strategy>plan first</strategy>\n<key_findings>findings second</key_findings>";
        let result = extract_evaluation_sections(input);
        assert!(result.contains("<key_findings>findings second</key_findings>"));
        assert!(result.contains("<strategy>plan first</strategy>"));
        // key_findings should come before strategy in the joined output
        let kf_pos = result.find("<key_findings>").unwrap();
        let st_pos = result.find("<strategy>").unwrap();
        assert!(
            kf_pos < st_pos,
            "key_findings should appear before strategy in output"
        );
    }

    // =========================================================================
    // prune_failure_dirs — filesystem tests with temp directories
    // =========================================================================

    #[test]
    fn prune_failure_dirs_no_op_under_limit() {
        use super::prune_failure_dirs;
        let tmp = std::env::temp_dir().join(format!("nsed_prune_test_noop_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create 3 directories, max is 5 — no pruning should happen
        for i in 0..3 {
            std::fs::create_dir_all(tmp.join(format!("dir_{i}"))).unwrap();
        }

        prune_failure_dirs(tmp.to_str().unwrap(), 5);

        let remaining: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect();
        assert_eq!(
            remaining.len(),
            3,
            "All 3 dirs should remain when under limit"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prune_failure_dirs_removes_oldest_when_over_limit() {
        use super::prune_failure_dirs;
        let tmp = std::env::temp_dir().join(format!("nsed_prune_test_over_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create 5 directories
        for i in 0..5 {
            let dir = tmp.join(format!("dir_{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            // Write a file so the directory is non-empty
            std::fs::write(dir.join("marker.txt"), format!("dir {i}")).unwrap();
            // Small sleep to ensure distinct mtimes on filesystems with coarse timestamps
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Prune to max 2 — should remove 3 oldest
        prune_failure_dirs(tmp.to_str().unwrap(), 2);

        let remaining: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect();
        assert_eq!(
            remaining.len(),
            2,
            "Should have 2 dirs remaining after pruning from 5"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prune_failure_dirs_nonexistent_parent() {
        use super::prune_failure_dirs;
        // Should not panic when the parent directory doesn't exist
        prune_failure_dirs("/tmp/nsed_nonexistent_prune_dir_12345", 5);
    }

    #[test]
    fn prune_failure_dirs_exact_limit() {
        use super::prune_failure_dirs;
        let tmp =
            std::env::temp_dir().join(format!("nsed_prune_test_exact_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create exactly 3 directories, max is 3 — no pruning
        for i in 0..3 {
            std::fs::create_dir_all(tmp.join(format!("dir_{i}"))).unwrap();
        }

        prune_failure_dirs(tmp.to_str().unwrap(), 3);

        let remaining: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect();
        assert_eq!(remaining.len(), 3, "Exactly at limit — no pruning");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prune_failure_dirs_max_zero_removes_all() {
        use super::prune_failure_dirs;
        let tmp = std::env::temp_dir().join(format!("nsed_prune_test_zero_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        for i in 0..3 {
            std::fs::create_dir_all(tmp.join(format!("dir_{i}"))).unwrap();
        }

        prune_failure_dirs(tmp.to_str().unwrap(), 0);

        let remaining: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect();
        assert_eq!(remaining.len(), 0, "Max 0 should remove all directories");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prune_failure_dirs_ignores_files() {
        use super::prune_failure_dirs;
        let tmp =
            std::env::temp_dir().join(format!("nsed_prune_test_files_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create 3 directories and 2 regular files
        for i in 0..3 {
            std::fs::create_dir_all(tmp.join(format!("dir_{i}"))).unwrap();
        }
        std::fs::write(tmp.join("file_a.txt"), "a").unwrap();
        std::fs::write(tmp.join("file_b.txt"), "b").unwrap();

        prune_failure_dirs(tmp.to_str().unwrap(), 1);

        // Only directories should be counted/pruned, not files
        let remaining_dirs: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect();
        assert_eq!(remaining_dirs.len(), 1, "Should prune down to 1 directory");

        // Files should be untouched
        assert!(tmp.join("file_a.txt").exists());
        assert!(tmp.join("file_b.txt").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // =========================================================================
    // write_failure_dump — full integration test with temp directory
    // =========================================================================

    #[test]
    #[serial]
    fn write_failure_dump_creates_file_with_correct_format() {
        // write_failure_dump creates "failures/<job_dir>" relative to CWD.
        // We test with config "1" and session_id to verify the format.
        let result = write_failure_dump(FailureDumpParams {
            kind: "parse_error",
            agent_name: "test-agent",
            model_name: "gpt-4",
            provider_id: "openai",
            error: "JSON parse failed: unexpected token",
            session_id: Some("abc12345"),
            phase: Some("propose"),
            round: Some(2),
            attempt: Some(3),
            finish_reason: Some("stop"),
            input_tokens: Some(500),
            output_tokens: Some(200),
            response_content: Some("This is the raw LLM response"),
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("1"),
        });

        // Should have created a file
        assert!(result.is_some(), "Dump should be created with config '1'");
        let filename = result.unwrap();
        assert!(
            filename.contains("abc12345"),
            "Filename should contain session ID prefix"
        );
        assert!(
            filename.contains("parse_error_r2.md"),
            "Filename should contain kind and round"
        );

        // Read and verify content
        let content = std::fs::read_to_string(&filename).unwrap();
        assert!(
            content.contains("# parse_error (attempt 3)"),
            "Should contain header"
        );
        assert!(
            content.contains("| agent | test-agent |"),
            "Should contain agent name"
        );
        assert!(
            content.contains("| model | gpt-4 |"),
            "Should contain model name"
        );
        assert!(
            content.contains("| phase | propose |"),
            "Should contain phase"
        );
        assert!(content.contains("| round | 2 |"), "Should contain round");
        assert!(content.contains("## Error"), "Should contain error section");
        assert!(
            content.contains("JSON parse failed"),
            "Should contain error text"
        );
        assert!(
            content.contains("## LLM Response"),
            "Should contain response section"
        );
        assert!(
            content.contains("This is the raw LLM response"),
            "Should contain response content"
        );
        // Without "full" mode, should NOT contain system prompt section
        assert!(
            content.contains("NSED_FAILURE_DUMPS=full"),
            "Should contain hint about full mode"
        );

        // Cleanup
        let _ = std::fs::remove_dir_all("failures");
    }

    #[test]
    #[serial]
    fn write_failure_dump_full_mode_includes_system_prompt() {
        let result = write_failure_dump(FailureDumpParams {
            kind: "api_error",
            agent_name: "full-test-agent",
            model_name: "model",
            provider_id: "provider",
            error: "timeout",
            session_id: Some("fulltest1"),
            phase: None,
            round: Some(1),
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: Some("You are a helpful assistant."),
            request_body: Some("{\"model\": \"test\"}"),
            messages: None,
            failure_dumps_config: Some("full"),
        });

        assert!(result.is_some(), "Full mode dump should be created");
        let filename = result.unwrap();
        let content = std::fs::read_to_string(&filename).unwrap();
        assert!(
            content.contains("## System Prompt"),
            "Full mode should include system prompt"
        );
        assert!(content.contains("You are a helpful assistant."));
        assert!(
            content.contains("## Request Body"),
            "Full mode should include request body"
        );

        // Cleanup
        let _ = std::fs::remove_dir_all("failures");
    }

    #[test]
    fn write_failure_dump_invalid_mode_returns_none() {
        let result = write_failure_dump(FailureDumpParams {
            kind: "test",
            agent_name: "agent",
            model_name: "model",
            provider_id: "prov",
            error: "err",
            session_id: None,
            phase: None,
            round: None,
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("maybe"), // Not "1", "on", or "full"
        });
        assert!(result.is_none(), "Invalid mode 'maybe' should return None");
    }

    // =========================================================================
    // truncate_for_dump — additional edge cases
    // =========================================================================

    #[test]
    fn test_truncate_for_dump_empty() {
        use super::truncate_for_dump;
        assert_eq!(truncate_for_dump("", 100), "");
    }

    #[test]
    fn test_truncate_for_dump_unicode() {
        use super::truncate_for_dump;
        // Unicode string: each char may be multi-byte but we count chars not bytes
        let input = "日本語テストデータ"; // 9 chars
        let result = truncate_for_dump(input, 5);
        assert!(result.contains("... (truncated at 5 chars)"));
        // Should contain exactly 5 chars from the original
        let prefix = result.split("\n... (truncated at 5 chars)").next().unwrap();
        assert_eq!(prefix.chars().count(), 5);
        assert_eq!(prefix, "日本語テス");
    }

    #[test]
    fn test_truncate_for_dump_one_char_over() {
        use super::truncate_for_dump;
        let input = "abcdef"; // 6 chars
        let result = truncate_for_dump(input, 5);
        assert!(result.contains("... (truncated at 5 chars)"));
    }

    // =========================================================================
    // strip_thinking_prefix — additional branch coverage
    // =========================================================================

    #[test]
    fn strip_thinking_prefix_commentary_with_json_brace() {
        // "commentary" followed by "json{" path
        let input = "commentaryto=functions.submit_proposal json{\"key\": 1}";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "{\"key\": 1}");
    }

    #[test]
    fn strip_thinking_prefix_assistant_followed_by_final_then_json() {
        let input = "random tokensassistantfinal{\"data\": true}";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "{\"data\": true}");
    }

    // =========================================================================
    // Transport error detection — validate retry eligibility
    // =========================================================================

    /// Helper that mirrors the production classifier in
    /// `generate_structured_output`'s Err arm. Tests use this to drive
    /// the same downcast path the retry loop uses, so renaming or
    /// reshaping `LlmError::Transport` is caught at compile time.
    fn is_transport_anyhow(err: &anyhow::Error) -> bool {
        use crate::telemetry::LlmError;
        matches!(err.downcast_ref::<LlmError>(), Some(LlmError::Transport(_)))
    }

    /// `LlmError::Transport` flowing up through `anyhow::Error::from`
    /// must classify as retryable on the inner error path. A bare
    /// `anyhow!("plain string")` (no LlmError chain) must not.
    #[test]
    fn transport_error_detection_covers_known_patterns() {
        use crate::telemetry::LlmError;
        let causes: Vec<Box<dyn std::error::Error + Send + Sync>> = vec![
            Box::new(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected EOF during chunk size line",
            )),
            Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            )),
            Box::new(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "broken pipe",
            )),
            Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request timed out after 30s",
            )),
        ];
        for cause in causes {
            let display = cause.to_string();
            let err: anyhow::Error = LlmError::Transport(cause).into();
            assert!(
                is_transport_anyhow(&err),
                "LlmError::Transport({display:?}) must classify as retryable transport"
            );
        }
    }

    /// Non-transport `LlmError` variants and bare `anyhow` errors must
    /// not classify as retryable transport.
    #[test]
    fn non_transport_errors_not_retryable() {
        use crate::telemetry::LlmError;
        let non_transport: Vec<anyhow::Error> = vec![
            // Other LlmError variants
            LlmError::ServerError { status: 500 }.into(),
            LlmError::PaymentRequired { status: 402 }.into(),
            LlmError::ContextOverflow {
                tokens: 5000,
                limit: 4096,
            }
            .into(),
            LlmError::Parse(Box::new(std::io::Error::other("parse failed"))).into(),
            LlmError::RateLimit {
                retry_after_ms: None,
                status: 429,
            }
            .into(),
            // Plain anyhow chain — no LlmError in the source path
            anyhow::anyhow!("Failed to parse structured output after 4 attempts"),
            anyhow::anyhow!("No choice in LLM response"),
        ];
        for err in non_transport {
            assert!(
                !is_transport_anyhow(&err),
                "Error should NOT classify as retryable transport: {err:#}"
            );
        }
    }

    /// Empty response parsing should fail with EOF error — this is the exact
    /// error that gpt-oss-120b triggers when it returns empty content with
    /// finish_reason=Stop (reasoning tokens consumed, no visible output).
    #[test]
    fn empty_response_triggers_eof_parse_error() {
        let result = serde_json::from_str::<StructuredProposalResponse>("");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.is_eof(),
            "Empty string should trigger EOF error, got: {}",
            err
        );
    }

    /// Empty response should also fail for batch evaluation parsing.
    #[test]
    fn empty_response_triggers_eof_parse_error_evaluation() {
        let result = serde_json::from_str::<StructuredBatchEvaluationResponse>("");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.is_eof(),
            "Empty string should trigger EOF error for evaluation, got: {}",
            err
        );
    }

    /// Verify the empty-response retry message tells the model to USE the tool.
    /// This is the specific feedback sent when gpt-oss-120b returns empty content.
    #[test]
    fn empty_response_retry_message_is_actionable() {
        let cleaned_json = "";
        let error_msg = if cleaned_json.trim().is_empty() {
            "Your response was EMPTY — no content was returned. You MUST call the tool with a JSON argument. Do NOT just think about the answer — you must actually output the tool call with the required fields.".to_string()
        } else {
            "other".to_string()
        };
        assert!(
            error_msg.contains("EMPTY"),
            "Error message should clearly indicate empty response"
        );
        assert!(
            error_msg.contains("MUST call the tool"),
            "Error message should instruct the model to use the tool"
        );
    }

    // =========================================================================
    // deserialize_string_or_array_or_object — float value
    // =========================================================================

    #[test]
    fn deser_string_or_array_float_value() {
        let val = 7.89_f64;
        let json = serde_json::json!({
            "thought_process": val,
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "7.89");
    }

    #[test]
    fn deser_string_or_array_array_with_numbers() {
        // Array containing non-string values should be converted via to_string()
        let json = serde_json::json!({
            "thought_process": [1, 2.5, "text"],
            "solution_content": "y"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        let parts: Vec<&str> = result.thought_process.split('\n').collect();
        assert_eq!(parts[0], "1");
        assert_eq!(parts[1], "2.5");
        assert_eq!(parts[2], "text");
    }

    // =========================================================================
    // truncate_for_dump — boundary and edge cases
    // =========================================================================

    #[test]
    fn truncate_for_dump_exact_limit() {
        use super::truncate_for_dump;
        // String with exactly max_chars — no truncation
        let input = "abcde"; // 5 chars
        let result = truncate_for_dump(input, 5);
        assert_eq!(result, "abcde");
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn truncate_for_dump_max_one_char() {
        use super::truncate_for_dump;
        let result = truncate_for_dump("hello", 1);
        assert!(result.starts_with('h'));
        assert!(result.contains("... (truncated at 1 chars)"));
    }

    #[test]
    fn truncate_for_dump_max_zero() {
        use super::truncate_for_dump;
        // max_chars = 0 should produce an empty prefix with truncation note
        let result = truncate_for_dump("hello", 0);
        assert!(result.contains("... (truncated at 0 chars)"));
    }

    #[test]
    fn truncate_for_dump_multibyte_boundary() {
        use super::truncate_for_dump;
        // 4 emoji chars, each is 4 bytes
        let input = "\u{1F600}\u{1F601}\u{1F602}\u{1F603}"; // 4 chars
        let result = truncate_for_dump(input, 2);
        assert!(result.contains("... (truncated at 2 chars)"));
        // The prefix should have exactly 2 emoji chars
        let prefix = result.split("\n... (truncated at 2 chars)").next().unwrap();
        assert_eq!(prefix.chars().count(), 2);
    }

    #[test]
    fn truncate_for_dump_newlines_preserved() {
        use super::truncate_for_dump;
        let input = "line1\nline2\nline3\nline4";
        let result = truncate_for_dump(input, 12);
        // 12 chars = "line1\nline2\n" (12 chars)
        assert!(result.contains("line1\nline2\n"));
        assert!(result.contains("... (truncated at 12 chars)"));
    }

    // =========================================================================
    // write_failure_dump — additional branch coverage
    // =========================================================================

    #[test]
    #[serial]
    fn write_failure_dump_off_config_returns_none() {
        // Explicit "off" config should short-circuit and return None
        let result = write_failure_dump(FailureDumpParams {
            kind: "test",
            agent_name: "agent",
            model_name: "model",
            provider_id: "prov",
            error: "err",
            session_id: None,
            phase: None,
            round: None,
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("off"),
        });
        assert!(result.is_none(), "'off' config should return None");
    }

    #[test]
    #[serial]
    fn write_failure_dump_empty_config_returns_none() {
        // Empty config string should return None
        let result = write_failure_dump(FailureDumpParams {
            kind: "test",
            agent_name: "agent",
            model_name: "model",
            provider_id: "prov",
            error: "err",
            session_id: None,
            phase: None,
            round: None,
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some(""),
        });
        assert!(result.is_none(), "Empty config should return None");
    }

    #[test]
    #[serial]
    fn write_failure_dump_on_config_creates_file() {
        let result = write_failure_dump(FailureDumpParams {
            kind: "api_error",
            agent_name: "on-test-agent",
            model_name: "test-model",
            provider_id: "test-provider",
            error: "connection timeout",
            session_id: Some("ontest123"),
            phase: Some("evaluate"),
            round: Some(3),
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("on"),
        });

        assert!(result.is_some(), "'on' config should create a dump");
        let filename = result.unwrap();
        let content = std::fs::read_to_string(&filename).unwrap();
        assert!(content.contains("# api_error"));
        assert!(content.contains("| agent | on-test-agent |"));
        assert!(content.contains("| phase | evaluate |"));
        assert!(content.contains("connection timeout"));

        let _ = std::fs::remove_dir_all("failures");
    }

    #[test]
    #[serial]
    fn write_failure_dump_append_mode_adds_separator() {
        // First write
        let result1 = write_failure_dump(FailureDumpParams {
            kind: "parse_error",
            agent_name: "append-agent",
            model_name: "model",
            provider_id: "prov",
            error: "first error",
            session_id: Some("appendtst"),
            phase: None,
            round: Some(1),
            attempt: Some(1),
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("1"),
        });
        assert!(result1.is_some());

        // Second write to same file (same session, kind, round)
        let result2 = write_failure_dump(FailureDumpParams {
            kind: "parse_error",
            agent_name: "append-agent",
            model_name: "model",
            provider_id: "prov",
            error: "second error",
            session_id: Some("appendtst"),
            phase: None,
            round: Some(1),
            attempt: Some(2),
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("1"),
        });
        assert!(result2.is_some());

        let filename = result2.unwrap();
        let content = std::fs::read_to_string(&filename).unwrap();
        // Append mode should add "---" separator
        assert!(content.contains("---"), "Append should add separator");
        assert!(
            content.contains("first error"),
            "Should contain first entry"
        );
        assert!(
            content.contains("second error"),
            "Should contain second entry"
        );
        assert!(
            content.contains("(attempt 1)"),
            "First attempt label should be present"
        );
        assert!(
            content.contains("(attempt 2)"),
            "Second attempt label should be present"
        );

        let _ = std::fs::remove_dir_all("failures");
    }

    #[test]
    #[serial]
    fn write_failure_dump_no_session_id_uses_timestamp_dir() {
        let result = write_failure_dump(FailureDumpParams {
            kind: "api_error",
            agent_name: "no-session-agent",
            model_name: "model",
            provider_id: "prov",
            error: "err",
            session_id: None, // No session ID
            phase: None,
            round: None,
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("1"),
        });
        assert!(result.is_some());
        let filename = result.unwrap();
        // Without session_id, directory name uses timestamp_agentname pattern
        assert!(filename.contains("no-session-agent"));
        // Round defaults to 0
        assert!(filename.contains("_r0.md"));

        let _ = std::fs::remove_dir_all("failures");
    }

    #[test]
    #[serial]
    fn write_failure_dump_full_mode_with_messages() {
        use async_openai::types::{ChatCompletionRequestMessage, ChatCompletionRequestUserMessage};

        let user_msg =
            ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage::from("Hello!"));
        let messages = vec![user_msg];

        let result = write_failure_dump(FailureDumpParams {
            kind: "parse_error",
            agent_name: "msg-agent",
            model_name: "gpt-4",
            provider_id: "openai",
            error: "parse failed",
            session_id: Some("msgtest12"),
            phase: None,
            round: Some(1),
            attempt: None,
            finish_reason: Some("length"),
            input_tokens: Some(1000),
            output_tokens: Some(500),
            response_content: Some("partial response"),
            system_prompt: Some("You are helpful."),
            request_body: Some("{\"model\": \"gpt-4\"}"),
            messages: Some(&messages),
            failure_dumps_config: Some("full"),
        });
        assert!(result.is_some());
        let filename = result.unwrap();
        let content = std::fs::read_to_string(&filename).unwrap();

        // Full mode: system prompt, request body, AND messages
        assert!(content.contains("## System Prompt"));
        assert!(content.contains("You are helpful."));
        assert!(content.contains("## Request Body"));
        assert!(content.contains("## Messages"));
        // Metadata
        assert!(content.contains("| finish_reason | length |"));
        assert!(content.contains("| input_tokens | 1000 |"));
        assert!(content.contains("| output_tokens | 500 |"));
        // LLM Response section
        assert!(content.contains("## LLM Response"));
        assert!(content.contains("partial response"));

        let _ = std::fs::remove_dir_all("failures");
    }

    #[test]
    #[serial]
    fn write_failure_dump_full_mode_append_skips_context() {
        // First write in full mode — should include system prompt
        let _r1 = write_failure_dump(FailureDumpParams {
            kind: "parse_error",
            agent_name: "ctx-agent",
            model_name: "model",
            provider_id: "prov",
            error: "first",
            session_id: Some("ctxtest12"),
            phase: None,
            round: Some(1),
            attempt: Some(1),
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: Some("System prompt content here"),
            request_body: Some("{\"body\": true}"),
            messages: None,
            failure_dumps_config: Some("full"),
        });

        // Second write (append) — should NOT repeat system prompt/request body
        let r2 = write_failure_dump(FailureDumpParams {
            kind: "parse_error",
            agent_name: "ctx-agent",
            model_name: "model",
            provider_id: "prov",
            error: "second",
            session_id: Some("ctxtest12"),
            phase: None,
            round: Some(1),
            attempt: Some(2),
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: Some("System prompt content here"),
            request_body: Some("{\"body\": true}"),
            messages: None,
            failure_dumps_config: Some("full"),
        });

        let filename = r2.unwrap();
        let content = std::fs::read_to_string(&filename).unwrap();

        // System prompt should appear exactly once (from first write only)
        let sys_count = content.matches("## System Prompt").count();
        assert_eq!(
            sys_count, 1,
            "System Prompt should appear only once (first write), found {}",
            sys_count
        );

        let _ = std::fs::remove_dir_all("failures");
    }

    #[test]
    #[serial]
    fn write_failure_dump_special_chars_in_agent_name() {
        let result = write_failure_dump(FailureDumpParams {
            kind: "test",
            agent_name: "agent/with (special) chars\\here",
            model_name: "model",
            provider_id: "prov",
            error: "err",
            session_id: Some("special1"),
            phase: None,
            round: Some(1),
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: None,
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("1"),
        });
        assert!(result.is_some());
        let filename = result.unwrap();
        // Special chars should be sanitized to underscores
        assert!(
            !filename.contains('/') || filename.starts_with("failures/"),
            "Agent name special chars should be sanitized"
        );
        assert!(!filename.contains('('));
        assert!(!filename.contains(')'));

        let _ = std::fs::remove_dir_all("failures");
    }

    #[test]
    #[serial]
    fn write_failure_dump_response_truncation() {
        // Verify that long response content is truncated at 4000 chars
        let long_response = "X".repeat(5000);
        let result = write_failure_dump(FailureDumpParams {
            kind: "parse_error",
            agent_name: "trunc-agent",
            model_name: "model",
            provider_id: "prov",
            error: "parse error",
            session_id: Some("trunctest"),
            phase: None,
            round: Some(1),
            attempt: None,
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
            response_content: Some(&long_response),
            system_prompt: None,
            request_body: None,
            messages: None,
            failure_dumps_config: Some("1"),
        });
        assert!(result.is_some());
        let filename = result.unwrap();
        let content = std::fs::read_to_string(&filename).unwrap();
        // Response should be truncated
        assert!(content.contains("truncated at 4000 chars"));
        // The full 5000-char response should NOT appear
        assert!(!content.contains(&long_response));

        let _ = std::fs::remove_dir_all("failures");
    }

    // =========================================================================
    // prune_failure_dirs — edge case: mixed files and empty subdirs
    // =========================================================================

    #[test]
    fn prune_failure_dirs_with_nested_content() {
        use super::prune_failure_dirs;
        let tmp =
            std::env::temp_dir().join(format!("nsed_prune_test_nested_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create 4 directories, each with files inside
        for i in 0..4 {
            let dir = tmp.join(format!("job_{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("parse_error_r1.md"), format!("error {i}")).unwrap();
            // Stagger mtimes slightly
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        prune_failure_dirs(tmp.to_str().unwrap(), 2);

        let remaining: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect();
        assert_eq!(remaining.len(), 2, "Should prune down to 2 directories");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // =========================================================================
    // StructuredProposalResponse — additional deserialization edge cases
    // =========================================================================

    #[test]
    fn deser_proposal_object_solution_content() {
        // When solution_content is an object (not a string), it should be serialized
        let json = serde_json::json!({
            "thought_process": "thinking",
            "solution_content": {"code": "fn main() {}", "language": "rust"}
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert!(result.solution_content.contains("code"));
        assert!(result.solution_content.contains("fn main()"));
    }

    #[test]
    fn deser_proposal_array_solution_content() {
        let json = serde_json::json!({
            "thought_process": "thinking",
            "solution_content": ["Part 1", "Part 2", "Part 3"]
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert!(result.solution_content.contains("Part 1"));
        assert!(result.solution_content.contains("Part 2"));
        assert!(result.solution_content.contains("Part 3"));
        // Parts should be joined with newlines
        let parts: Vec<&str> = result.solution_content.split('\n').collect();
        assert_eq!(parts.len(), 3);
    }

    // =========================================================================
    // StructuredBatchEvaluationResponse — alias coverage
    // =========================================================================

    #[test]
    fn deser_batch_eval_candidate_evaluations_alias() {
        // "candidate_evaluations" alias should work
        let json = serde_json::json!({
            "candidate_evaluations": [
                {"agent_id": "a1", "endorsement_weight": 80.0}
            ]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.evaluations.len(), 1);
        assert_eq!(result.evaluations[0].agent_id, "a1");
        assert_eq!(result.evaluations[0].endorsement_weight, 80.0);
    }

    #[test]
    fn deser_batch_eval_candidates_alias() {
        // "candidates" alias should work
        let json = serde_json::json!({
            "candidates": [
                {"candidate_id": "c1", "score": 65.0, "justification": "Good work"}
            ]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.evaluations.len(), 1);
        assert_eq!(result.evaluations[0].agent_id, "c1");
        assert_eq!(result.evaluations[0].endorsement_weight, 65.0);
        assert_eq!(
            result.evaluations[0].justification,
            Some("Good work".to_string())
        );
    }

    #[test]
    fn deser_batch_eval_with_all_optional_fields() {
        let json = serde_json::json!({
            "evaluations": [{
                "agent_id": "agent_1",
                "endorsement_weight": 75.0,
                "justification": "Solid approach",
                "is_final_solution": true,
                "stance": "agree",
                "claim_assessments": [{
                    "claim": "Memory safety is guaranteed",
                    "verdict": "verified"
                }],
                "disagreements": [{
                    "what_they_claim": "Always faster",
                    "what_you_believe": "Not always",
                    "confidence": "high"
                }],
                "category_scores": {
                    "correctness": 80.0,
                    "completeness": 70.0,
                    "novelty": 60.0,
                    "feasibility": 90.0,
                    "evidence_quality": 75.0
                }
            }]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.evaluations.len(), 1);
        assert!(result.evaluations[0].is_final_solution);
        assert!(result.evaluations[0].category_scores.is_some());
    }

    // =========================================================================
    // strip_thinking_prefix — additional branches
    // =========================================================================

    #[test]
    fn strip_thinking_prefix_commentary_with_json_space() {
        // "commentary...json {..." path
        let input = "commentaryto=functions.submit_proposal json {\"key\": 1}";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "{\"key\": 1}");
    }

    #[test]
    fn strip_thinking_prefix_commentary_no_json_unchanged() {
        // "commentary" prefix without "json" keyword — falls through, returns trimmed
        let input = "commentary some random text without a special keyword";
        let result = strip_thinking_prefix(input);
        // No "json{" or "json " found, so the full string (trimmed) is returned
        assert_eq!(result, input);
    }

    #[test]
    fn strip_thinking_prefix_assistant_then_hash() {
        // "assistant" followed by '#' should strip
        let input = "analysis tokens here assistant# Solution Header";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "# Solution Header");
    }

    // =========================================================================
    // strip_scratchpad — additional edge cases for coverage
    // =========================================================================

    #[test]
    fn strip_scratchpad_only_text_before() {
        // Text only before scratchpad — should trim trailing whitespace from before
        let input = "Important preamble text\n<scratchpad>scratch content</scratchpad>";
        let result = strip_scratchpad(input);
        assert_eq!(result, "Important preamble text");
        assert!(!result.contains("scratch content"));
    }

    #[test]
    fn strip_scratchpad_only_text_after() {
        // No text before, only text after
        let input = "<scratchpad>notes</scratchpad>\nAfter the scratchpad";
        let result = strip_scratchpad(input);
        assert_eq!(result, "After the scratchpad");
    }

    #[test]
    fn strip_scratchpad_whitespace_between() {
        // Whitespace around scratchpad block — should be trimmed
        let input = "Before   \n\n   <scratchpad>internal</scratchpad>   \n\n   After";
        let result = strip_scratchpad(input);
        assert_eq!(result, "Before\n\nAfter");
    }

    #[test]
    fn strip_scratchpad_empty_tags() {
        let input = "Text before <scratchpad></scratchpad> text after";
        let result = strip_scratchpad(input);
        assert_eq!(result, "Text before\n\ntext after");
    }

    #[test]
    fn strip_scratchpad_closing_tag_only() {
        // Closing tag without opening tag — no match, returns as-is
        let input = "</scratchpad>Content after closing tag";
        let result = strip_scratchpad(input);
        assert_eq!(result, input);
    }

    // =========================================================================
    // strip_working_memory — additional edge cases
    // =========================================================================

    #[test]
    fn strip_working_memory_empty_tags() {
        let input = "Before <working_memory></working_memory> After";
        let result = strip_working_memory(input);
        assert_eq!(result, "Before\n\nAfter");
    }

    #[test]
    fn strip_working_memory_only_working_memory() {
        let input = "<working_memory>all content is ephemeral</working_memory>";
        let result = strip_working_memory(input);
        assert_eq!(result, "");
    }

    #[test]
    fn strip_working_memory_three_blocks() {
        let input = "<working_memory>a</working_memory>\n\ntext1\n\n<working_memory>b</working_memory>\n\ntext2\n\n<working_memory>c</working_memory>";
        let result = strip_working_memory(input);
        assert!(!result.contains("<working_memory>"));
        assert!(result.contains("text1"));
        assert!(result.contains("text2"));
    }

    #[test]
    fn strip_working_memory_with_whitespace_only_content() {
        let input = "<working_memory>   \n\t  </working_memory>";
        let result = strip_working_memory(input);
        assert_eq!(result, "");
    }

    // =========================================================================
    // extract_evaluation_sections — additional edge cases
    // =========================================================================

    #[test]
    fn extract_evaluation_sections_one_closed_one_unclosed() {
        // key_findings is properly closed, strategy is unclosed
        let input = "<key_findings>data here</key_findings>\n<strategy>unclosed";
        let result = extract_evaluation_sections(input);
        // Only key_findings should be extracted (strategy is unclosed, so not captured)
        assert_eq!(result, "<key_findings>data here</key_findings>");
        assert!(!result.contains("unclosed"));
    }

    #[test]
    fn extract_evaluation_sections_only_closing_tags() {
        // Closing tags without opening tags — no sections found
        let input = "</key_findings></strategy>some text";
        let result = extract_evaluation_sections(input);
        assert_eq!(result, input, "Should fall back to full text");
    }

    #[test]
    fn extract_evaluation_sections_interleaved_with_working_memory() {
        // Test realistic scenario with all three section types
        let input = "<key_findings>Important findings</key_findings>\n<working_memory>ephemeral</working_memory>\n<strategy>Long-term plan</strategy>";
        let result = extract_evaluation_sections(input);
        assert!(result.contains("<key_findings>Important findings</key_findings>"));
        assert!(result.contains("<strategy>Long-term plan</strategy>"));
        // working_memory should NOT appear in extracted sections
        assert!(!result.contains("working_memory"));
        assert!(!result.contains("ephemeral"));
    }

    // =========================================================================
    // strip_thinking_prefix — remaining uncovered branches
    // =========================================================================

    #[test]
    fn strip_thinking_prefix_commentary_only() {
        // "commentary" without any "json" keyword at all
        let input = "commentary";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "commentary");
    }

    #[test]
    fn strip_thinking_prefix_assistant_mid_content_not_at_start() {
        // "assistant" found but followed by something that doesn't match the pattern
        let input = "My assistant model works well";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "My assistant model works well");
    }

    #[test]
    fn strip_thinking_prefix_final_with_leading_whitespace() {
        let input = "  final  **Bold**  ";
        let result = strip_thinking_prefix(input);
        // Leading whitespace is trimmed first, then "final" prefix is detected
        // and stripped, then trailing trim produces the final result.
        assert_eq!(result, "**Bold**");
    }

    #[test]
    fn strip_thinking_prefix_multiple_assistant_occurrences() {
        // find() returns the FIRST occurrence of "assistant"
        let input = "analysisassistantfinal**Content**assistantmore";
        let result = strip_thinking_prefix(input);
        // First "assistant" is found, followed by "final" → strip to "final**Content**assistantmore"
        // Then "final" prefix is stripped → "**Content**assistantmore"
        assert_eq!(result, "**Content**assistantmore");
    }

    // =========================================================================
    // prune_failure_dirs — single directory
    // =========================================================================

    #[test]
    fn prune_failure_dirs_single_dir_over_limit() {
        use super::prune_failure_dirs;
        let tmp =
            std::env::temp_dir().join(format!("nsed_prune_test_single_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        std::fs::create_dir_all(tmp.join("only_dir")).unwrap();

        // Max 0 means remove all
        prune_failure_dirs(tmp.to_str().unwrap(), 0);

        let remaining: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect();
        assert_eq!(
            remaining.len(),
            0,
            "Single dir should be removed when max=0"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prune_failure_dirs_empty_dir() {
        use super::prune_failure_dirs;
        let tmp =
            std::env::temp_dir().join(format!("nsed_prune_test_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // No directories inside — should be a no-op
        prune_failure_dirs(tmp.to_str().unwrap(), 5);
        // Just verify no panic occurred

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // =========================================================================
    // deserialize_string_or_array_or_object — additional edge cases
    // =========================================================================

    #[test]
    fn deser_string_or_array_empty_string() {
        let json = serde_json::json!({
            "thought_process": "",
            "solution_content": ""
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "");
        assert_eq!(result.solution_content, "");
    }

    #[test]
    fn deser_string_or_array_whitespace_only_string() {
        let json = serde_json::json!({
            "thought_process": "   \n\t  ",
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "   \n\t  ");
    }

    #[test]
    fn deser_string_or_array_array_of_empty_strings() {
        let json = serde_json::json!({
            "thought_process": ["", "", ""],
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "\n\n");
    }

    // =========================================================================
    // StructuredBatchEvaluationResponse — edge cases
    // =========================================================================

    #[test]
    fn batch_eval_empty_evaluations_array() {
        let json = serde_json::json!({ "evaluations": [] });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert!(result.evaluations.is_empty());
    }

    #[test]
    fn batch_eval_minimal_item() {
        // Minimum required fields
        let json = serde_json::json!({
            "evaluations": [{
                "agent_id": "a",
                "endorsement_weight": 50.0
            }]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.evaluations.len(), 1);
        assert_eq!(result.evaluations[0].agent_id, "a");
        assert_eq!(result.evaluations[0].endorsement_weight, 50.0);
        assert!(result.evaluations[0].justification.is_none());
        assert!(!result.evaluations[0].is_final_solution);
        assert!(result.evaluations[0].stance.is_none());
        assert!(result.evaluations[0].claim_assessments.is_empty());
        assert!(result.evaluations[0].disagreements.is_empty());
        assert!(result.evaluations[0].category_scores.is_none());
    }

    #[test]
    fn batch_eval_candidate_id_alias_for_agent_id() {
        let json = serde_json::json!({
            "evaluations": [{
                "candidate_id": "candidate_x",
                "endorsement_weight": 70.0
            }]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.evaluations[0].agent_id, "candidate_x");
    }

    #[test]
    fn batch_eval_score_alias_for_endorsement_weight() {
        let json = serde_json::json!({
            "evaluations": [{
                "agent_id": "b",
                "score": 85.0
            }]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.evaluations[0].endorsement_weight, 85.0);
    }

    // =========================================================================
    // truncate_for_dump — zero-length input
    // =========================================================================

    #[test]
    fn truncate_for_dump_max_exceeds_length() {
        use super::truncate_for_dump;
        let input = "short";
        let result = truncate_for_dump(input, 1000);
        assert_eq!(result, "short");
    }

    // =========================================================================
    // truncate_for_dump — additional edge cases
    // =========================================================================

    #[test]
    fn truncate_for_dump_empty_input() {
        use super::truncate_for_dump;
        let result = truncate_for_dump("", 100);
        assert_eq!(result, "");
    }

    #[test]
    fn truncate_for_dump_exact_length() {
        use super::truncate_for_dump;
        let input = "12345";
        let result = truncate_for_dump(input, 5);
        assert_eq!(result, "12345");
    }

    #[test]
    fn truncate_for_dump_one_over() {
        use super::truncate_for_dump;
        let input = "123456";
        let result = truncate_for_dump(input, 5);
        assert!(result.starts_with("12345"));
        assert!(result.contains("truncated at 5 chars"));
    }

    #[test]
    fn truncate_for_dump_unicode() {
        use super::truncate_for_dump;
        // Each emoji is 1 char but multiple bytes
        let input = "\u{1F600}\u{1F601}\u{1F602}\u{1F603}\u{1F604}";
        assert_eq!(input.chars().count(), 5);
        let result = truncate_for_dump(input, 3);
        assert!(result.starts_with("\u{1F600}\u{1F601}\u{1F602}"));
        assert!(result.contains("truncated at 3 chars"));
    }

    #[test]
    fn truncate_for_dump_zero_max() {
        use super::truncate_for_dump;
        let input = "something";
        let result = truncate_for_dump(input, 0);
        assert!(result.contains("truncated at 0 chars"));
        assert!(!result.starts_with('s'));
    }

    // =========================================================================
    // strip_thinking_prefix — commentary branches
    // =========================================================================

    #[test]
    fn strip_thinking_prefix_commentary_json_brace() {
        let input = r#"commentaryto=functions.submit_proposal json{"solution": "42"}"#;
        let result = strip_thinking_prefix(input);
        assert_eq!(result, r#"{"solution": "42"}"#);
    }

    #[test]
    fn strip_thinking_prefix_commentary_json_space() {
        let input = r#"commentaryto=functions.submit_proposal json {"solution": "42"}"#;
        let result = strip_thinking_prefix(input);
        assert_eq!(result, r#"{"solution": "42"}"#);
    }

    #[test]
    fn strip_thinking_prefix_final_bold() {
        let input = "final**My bold answer**";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "**My bold answer**");
    }

    #[test]
    fn strip_thinking_prefix_final_heading() {
        let input = "final# Heading";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, "# Heading");
    }

    #[test]
    fn strip_thinking_prefix_assistant_final_json() {
        let input = r#"analysisThinking about itassistantfinal{"key": "val"}"#;
        let result = strip_thinking_prefix(input);
        assert_eq!(result, r#"{"key": "val"}"#);
    }

    #[test]
    fn strip_thinking_prefix_assistant_json_direct() {
        let input = r#"analysisassistant{"tool": true}"#;
        let result = strip_thinking_prefix(input);
        assert_eq!(result, r#"{"tool": true}"#);
    }

    #[test]
    fn strip_thinking_prefix_plain_text() {
        let input = "Just regular content with no prefix tokens";
        let result = strip_thinking_prefix(input);
        assert_eq!(result, input);
    }

    // =========================================================================
    // StructuredBatchEvaluationResponse — more alias & field tests
    // =========================================================================

    #[test]
    fn batch_eval_with_justification() {
        let json = serde_json::json!({
            "evaluations": [{
                "agent_id": "a1",
                "endorsement_weight": 90.0,
                "justification": "Strong argument with evidence"
            }]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert_eq!(
            result.evaluations[0].justification.as_deref(),
            Some("Strong argument with evidence")
        );
    }

    #[test]
    fn batch_eval_with_is_final_solution_true() {
        let json = serde_json::json!({
            "evaluations": [{
                "agent_id": "a1",
                "endorsement_weight": 95.0,
                "is_final_solution": true
            }]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert!(result.evaluations[0].is_final_solution);
    }

    #[test]
    fn batch_eval_multiple_items() {
        let json = serde_json::json!({
            "evaluations": [
                {"agent_id": "a", "endorsement_weight": 80.0},
                {"agent_id": "b", "endorsement_weight": 60.0},
                {"agent_id": "c", "endorsement_weight": 40.0}
            ]
        });
        let result: StructuredBatchEvaluationResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.evaluations.len(), 3);
        assert_eq!(result.evaluations[2].endorsement_weight, 40.0);
    }

    // =========================================================================
    // deserialize_string_or_array_or_object — object input
    // =========================================================================

    #[test]
    fn deser_string_or_array_nested_object_with_multiple_keys() {
        let json = serde_json::json!({
            "thought_process": {"step1": "analyze", "step2": "conclude"},
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        // Object should be serialized to JSON string
        assert!(result.thought_process.contains("step1"));
        assert!(result.thought_process.contains("analyze"));
    }

    #[test]
    fn deser_string_or_array_number() {
        let json = serde_json::json!({
            "thought_process": 42,
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "42");
    }

    #[test]
    fn deser_string_or_array_bool() {
        let json = serde_json::json!({
            "thought_process": true,
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "true");
    }

    #[test]
    fn deser_string_or_array_null_becomes_string() {
        let json = serde_json::json!({
            "thought_process": null,
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        assert_eq!(result.thought_process, "null");
    }

    #[test]
    fn deser_string_or_array_mixed_types_in_array() {
        let json = serde_json::json!({
            "thought_process": ["text", 123, true, null, {"k": "v"}],
            "solution_content": "x"
        });
        let result: StructuredProposalResponse = serde_json::from_value(json).unwrap();
        // Strings stay as-is, non-strings get serialized
        assert!(result.thought_process.contains("text"));
        assert!(result.thought_process.contains("123"));
        assert!(result.thought_process.contains("true"));
    }

    // =========================================================================
    // strip_scratchpad — nested and multiline edge cases
    // =========================================================================

    #[test]
    fn strip_scratchpad_multiline_content() {
        let input = "Start\n<scratchpad>\nLine 1\nLine 2\nLine 3\n</scratchpad>\nEnd";
        let result = strip_scratchpad(input);
        assert!(!result.contains("Line 1"));
        assert!(!result.contains("Line 2"));
        assert!(!result.contains("Line 3"));
        assert!(result.contains("Start"));
        assert!(result.contains("End"));
    }

    #[test]
    fn strip_scratchpad_no_tags() {
        let input = "Content without any scratchpad tags";
        let result = strip_scratchpad(input);
        assert_eq!(result, input);
    }

    // =========================================================================
    // strip_working_memory — no tags
    // =========================================================================

    #[test]
    fn strip_working_memory_no_tags() {
        let input = "Content without any working_memory tags";
        let result = strip_working_memory(input);
        assert_eq!(result, input);
    }

    // ── native-path citation grounding (shared seam, Reject policy) ──────────

    use super::BatchEvaluationItem;
    use crate::agents::{AgentContext, ClaimAssessment, ClaimVerdict};

    /// The native runtime's grounding call, matching `evaluate`'s policy.
    fn ground_native(
        ctx: &AgentContext,
        items: &mut [BatchEvaluationItem],
    ) -> Vec<crate::agents::cite::UnresolvedCite> {
        crate::agents::cite::ground_all(
            &ctx.candidates,
            ctx.round_number,
            items,
            crate::agents::cite::GroundingPolicy::Reject,
        )
    }

    /// A store that holds nothing — these tests care about which tools get
    /// offered, which is decided before any read happens.
    #[derive(Debug)]
    struct EmptyStore;

    #[async_trait::async_trait]
    impl crate::agents::PersistenceStore for EmptyStore {
        async fn get(&self, _key: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn append(&self, _key: &str, _content: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set(&self, _key: &str, _content: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_round_history(
            &self,
            _round: u32,
        ) -> anyhow::Result<Option<Vec<crate::agents::ProposalRecord>>> {
            Ok(None)
        }
    }

    fn tool_names_for(round: u32, candidates: usize) -> Vec<String> {
        tool_names_for_config(
            AgentConfig {
                name: "ALPHA".to_string(),
                ..Default::default()
            },
            round,
            candidates,
        )
    }

    fn tool_names_for_config(config: AgentConfig, round: u32, candidates: usize) -> Vec<String> {
        let agent = super::ProposerEvaluatorAgent::new(
            config,
            Box::new(CannedSummaryModel {
                text: String::new(),
            }),
            Box::new(crate::prompts::defaults::DefaultPromptSet::default()),
            vec![],
            vec![],
        );
        let context = AgentContext {
            round_number: round,
            store: Some(std::sync::Arc::new(EmptyStore)),
            candidates: (0..candidates)
                .map(|i| crate::agents::CandidateProposal {
                    id: format!("Candidate_{i}"),
                    proposal: crate::agents::Proposal::default(),
                })
                .collect(),
            ..Default::default()
        };
        let mut names: Vec<String> = agent
            .aggregate_tools(&context)
            .iter()
            .map(|t| t.name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_agent_configured_for_delegated_search_is_given_the_tool() {
        // The setting existed, was documented, and was read by nothing: an agent
        // whose backend refuses to take its search tool alongside ours was left
        // with no way to search at all, which reads as configured-and-working.
        let names = tool_names_for_config(
            AgentConfig {
                name: "SEARCHER".to_string(),
                delegated_search: Some("web_search".to_string()),
                ..Default::default()
            },
            1,
            0,
        );
        assert!(
            names.iter().any(|n| n == "web_search"),
            "a configured search tool must reach the agent: {names:?}"
        );
    }

    #[test]
    fn an_agent_without_delegated_search_is_given_no_search_tool() {
        let names = tool_names_for(1, 0);
        assert!(
            !names.iter().any(|n| n == "web_search"),
            "search is opt-in: {names:?}"
        );
    }

    #[test]
    fn the_first_round_is_not_offered_history_readers() {
        // Round 1 proposing: nothing has been proposed or critiqued yet. Offering
        // a reader here advertises data that does not exist — the model spends a
        // turn discovering that, and every request of the round pays for the tool
        // definitions.
        let names = tool_names_for(1, 0);
        for absent in ["read_proposal", "read_critiques", "read_own_proposal"] {
            assert!(
                !names.contains(&absent.to_string()),
                "{absent} should not be offered in round 1 with no candidates: {names:?}"
            );
        }
    }

    #[test]
    fn the_first_round_evaluation_can_still_read_its_candidates() {
        // Same round, but candidates are in hand: `read_proposal` serves those, so
        // withholding it would remove the evaluator's only way to read past the
        // truncation of an inlined candidate.
        let names = tool_names_for(1, 2);
        assert!(names.contains(&"read_proposal".to_string()), "{names:?}");
        assert!(
            !names.contains(&"read_critiques".to_string()),
            "no critiques exist in the first round: {names:?}"
        );
    }

    #[test]
    fn later_rounds_are_offered_the_full_set() {
        let names = tool_names_for(2, 0);
        for present in ["read_proposal", "read_critiques", "read_own_proposal"] {
            assert!(
                names.contains(&present.to_string()),
                "{present} missing once history exists: {names:?}"
            );
        }
    }

    /// An event store over a throwaway JetStream, or `None` when NATS is absent.
    async fn test_event_store() -> Option<crate::status::agent_events::AgentEventStore> {
        let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
        let client = async_nats::connect(&url).await.ok()?;
        Some(crate::status::agent_events::AgentEventStore::new(
            async_nats::jetstream::new(client),
            "ALPHA".to_string(),
        ))
    }

    /// The guard's contract is which of the two outcomes it reports on drop.
    /// Publishing needs a live JetStream, so these assert the arming decision —
    /// the branch that decides whether a cancellation is claimed at all.
    #[tokio::test]
    async fn a_disarmed_tool_call_guard_reports_nothing() {
        let Some(store) = test_event_store().await else {
            eprintln!("Skipping: NATS unavailable");
            return;
        };
        let mut guard = super::ToolCallGuard::new(
            store,
            "ALPHA".to_string(),
            "call-1".to_string(),
            "user_dm_user".to_string(),
            Some("job-1".to_string()),
            1,
        );
        assert!(guard.armed, "a live call is armed");
        guard.disarm();
        assert!(
            !guard.armed,
            "a call that published its own outcome must not also report a cancellation"
        );
    }

    #[tokio::test]
    async fn an_abandoned_tool_call_guard_stays_armed() {
        let Some(store) = test_event_store().await else {
            eprintln!("Skipping: NATS unavailable");
            return;
        };
        let guard = super::ToolCallGuard::new(
            store,
            "ALPHA".to_string(),
            "call-2".to_string(),
            "user_dm_user".to_string(),
            Some("job-1".to_string()),
            1,
        );
        // Dropped without disarming — the case a question expiring produces.
        assert!(guard.armed, "an unfinished call reports its cancellation");
    }

    fn ctx_with_candidate(content: &str, thoughts: &str) -> AgentContext {
        AgentContext {
            round_number: 2,
            candidates: vec![crate::agents::CandidateProposal {
                id: "Candidate_A".to_string(),
                proposal: crate::agents::Proposal {
                    content: content.to_string(),
                    thought_process: thoughts.to_string(),
                    ..Default::default()
                },
            }],
            ..Default::default()
        }
    }

    fn batch(cites: &[&str]) -> Vec<BatchEvaluationItem> {
        vec![BatchEvaluationItem {
            agent_id: "Candidate_A".to_string(),
            endorsement_weight: 0.5,
            justification: Some("j".to_string()),
            is_final_solution: false,
            stance: None,
            claim_assessments: cites
                .iter()
                .map(|c| ClaimAssessment {
                    claim: (*c).to_string(),
                    verdict: ClaimVerdict::Verified,
                    ..Default::default()
                })
                .collect(),
            disagreements: vec![],
            category_scores: None,
        }]
    }

    #[test]
    fn native_grounding_substitutes_the_exact_span() {
        let ctx = ctx_with_candidate("The system sorts in O(n log n) time.", "t");
        let mut items = batch(&["\"sorts in O(n log n) time\""]);

        let unresolved = ground_native(&ctx, &mut items);

        assert!(unresolved.is_empty());
        assert_eq!(
            items[0].claim_assessments[0].claim, "sorts in O(n log n) time",
            "a decorated cite is replaced by the exact proposal span"
        );
    }

    /// Reproduces what this runtime published before it grounded anything: a
    /// cite matching no span of its target, accepted in silence. It is now
    /// reported so the caller can ask for a re-quote, and pruned per-claim.
    #[test]
    fn native_grounding_reports_and_prunes_only_the_bad_cite() {
        let ctx = ctx_with_candidate("The system sorts in O(n log n) time.", "t");
        let mut items = batch(&["sorts in O(n log n) time", "runs in constant time"]);

        let unresolved = ground_native(&ctx, &mut items);

        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].target_id, "Candidate_A");
        assert_eq!(unresolved[0].cite, "runs in constant time");
        assert_eq!(
            items[0].claim_assessments.len(),
            1,
            "the sibling claim survives"
        );
    }

    #[test]
    fn requote_feedback_names_every_failed_cite() {
        let unresolved = vec![
            crate::agents::cite::UnresolvedCite {
                target_id: "Candidate_A".to_string(),
                cite: "runs in constant time".to_string(),
            },
            crate::agents::cite::UnresolvedCite {
                target_id: "Candidate_B".to_string(),
                cite: "is provably optimal".to_string(),
            },
        ];
        let msg = super::requote_feedback(&unresolved);
        assert!(msg.contains("VERBATIM"), "must carry the instruction");
        assert!(
            msg.contains("submit_batch_evaluation"),
            "names the tool to retry"
        );
        assert!(msg.contains("runs in constant time"));
        assert!(msg.contains("is provably optimal"));
        assert!(msg.contains("Candidate_B"));
    }
}
