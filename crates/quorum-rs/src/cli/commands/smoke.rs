//! `quorum smoke-test <agent_id>` — staged escalation for ONE of the operator's
//! own agents, run entirely IN-PROCESS (no orchestrator, no NATS, no peers):
//!
//! 1. **chat** — call the agent's model directly N times with a trivial prompt;
//!    report success / avg-latency / error-rate.
//! 2. **tool-calling** — same model, N times, expecting a tool call back.
//! 3. **NSED** — build the agent and call `propose()` directly with a synthetic
//!    context. `propose()` is the full NSED wrapper (ReAct loop + tool-calling),
//!    so this exercises the deliberation path for a single agent — no second
//!    participant and no orchestrator required.
//!
//! Each stage gates the next (a stage that fails every sample stops the run).
//! Stages 1-2 are direct model calls; a subprocess provider (`exec`/`claude`/
//! `mcp`) has no directly-callable model, so those are skipped — but the NSED
//! stage still runs (those agents implement `propose`). `quorum serve` need not
//! be running.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use async_openai::types::{
    ChatCompletionRequestUserMessage, ChatCompletionTool, ChatCompletionToolChoiceOption,
    ChatCompletionToolType, CreateChatCompletionResponse, FunctionObject,
};

use crate::agents::config::AgentConfig;
use crate::agents::{AgentContext, CandidateProposal, DeliberationPhase, NsedAgent, Proposal};
use crate::config::{ProviderEntry, load_agent_from_config_with_registry};
use crate::llms::openai_compatible::OpenAICompatibleModel;
use crate::llms::simulated::SimulatedModel;
use crate::llms::{AiModel, RequestConfig};
use crate::providers::ProviderRegistry;

/// Samples per direct stage (chat, tool-calling).
const SMOKE_SAMPLES: u32 = 10;
const SMOKE_TASK: &str =
    "Smoke test: reply briefly with your role and confirm you are operational.";

/// Tally for one stage: how many of `runs` succeeded, their latency, and the
/// most recent failure reason. Everything runs in this process, so the reason
/// has to surface here — there is no server log to consult.
struct StageStats {
    ok: u32,
    runs: u32,
    total_latency_ms: u128,
    last_error: Option<String>,
}

impl StageStats {
    fn new(runs: u32) -> Self {
        StageStats {
            ok: 0,
            runs,
            total_latency_ms: 0,
            last_error: None,
        }
    }

    fn avg_latency_ms(&self) -> u64 {
        if self.ok == 0 {
            0
        } else {
            (self.total_latency_ms / self.ok as u128) as u64
        }
    }

    fn error_rate_pct(&self) -> u32 {
        ((self.runs - self.ok) * 100)
            .checked_div(self.runs)
            .unwrap_or(0)
    }

    fn line(&self, label: &str) -> String {
        let mut s = format!(
            "{label}: {}/{} ok \u{b7} avg {}ms \u{b7} errors {}%",
            self.ok,
            self.runs,
            self.avg_latency_ms(),
            self.error_rate_pct()
        );
        if self.ok < self.runs
            && let Some(err) = &self.last_error
        {
            s.push_str(&format!(" \u{2014} last error: {err}"));
        }
        s
    }
}

/// Flatten an error + its `source()` chain into one line. `LlmError`'s terse
/// variants (`#[error("other")]`, `"transport"`, `"parse"`) hide the real cause
/// in `#[source]`, so `to_string()` alone shows a useless "other".
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        s.push_str(&format!(": {cause}"));
        src = cause.source();
    }
    s
}

/// Did the model return at least one tool call?
fn has_tool_call(resp: &CreateChatCompletionResponse) -> bool {
    resp.choices
        .first()
        .and_then(|c| c.message.tool_calls.as_ref())
        .is_some_and(|calls| !calls.is_empty())
}

fn chat_request() -> RequestConfig {
    RequestConfig {
        messages: vec![
            ChatCompletionRequestUserMessage {
                content: "Reply briefly: OK".into(),
                ..Default::default()
            }
            .into(),
        ],
        tools: None,
        tool_choice: None,
        presence_penalty: None,
    }
}

fn tool_request() -> RequestConfig {
    let echo = ChatCompletionTool {
        r#type: ChatCompletionToolType::Function,
        function: FunctionObject {
            name: "echo".to_string(),
            description: Some("Echo the given text back to the caller.".to_string()),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            })),
            strict: None,
        },
    };
    RequestConfig {
        messages: vec![
            ChatCompletionRequestUserMessage {
                content: "Use the echo tool to echo the text \"ok\".".into(),
                ..Default::default()
            }
            .into(),
        ],
        tools: Some(vec![echo]),
        tool_choice: Some(ChatCompletionToolChoiceOption::Auto),
        presence_penalty: None,
    }
}

/// Build a directly-callable model for the agent's provider, or `None` for
/// subprocess providers (`exec`/`claude`/`mcp`) and misconfigured openai-types
/// with no base_url. Mirrors `providers::builtins` base-url resolution.
fn build_model(agent: &AgentConfig, provider: &ProviderEntry) -> Option<Box<dyn AiModel>> {
    match provider.provider_type.as_str() {
        "simulated" => Some(Box::new(SimulatedModel::new(
            agent.model_name.clone(),
            provider.latency_ms,
        ))),
        "exec" | "claude" | "mcp" => None,
        _ => {
            let base_url = crate::providers::builtins::resolve_openai_base_url(
                &provider.provider_type,
                &provider.base_url,
            )?;
            Some(Box::new(OpenAICompatibleModel::new(
                base_url,
                provider.api_key.clone(),
                provider.engine.clone(),
            )))
        }
    }
}

/// A minimal first-round proposing context for the NSED stage. `session_id` and
/// `agent_id` are required by the agent's telemetry path even with no endpoints.
fn smoke_context(agent_id: &str) -> AgentContext {
    AgentContext {
        task_description: SMOKE_TASK.to_string(),
        round_number: 1,
        total_rounds: 1,
        phase: DeliberationPhase::Proposing,
        session_id: Some(format!("smoke-{agent_id}")),
        agent_id: agent_id.to_string(),
        ..Default::default()
    }
}

/// Run one direct stage: `samples` model calls; a sample is "ok" when the call
/// succeeds (and, for the tool stage, returns a tool call). Latency is summed
/// over ok samples only.
async fn run_direct_stage(
    model: &dyn AiModel,
    agent: &AgentConfig,
    samples: u32,
    with_tools: bool,
) -> StageStats {
    let mut stats = StageStats::new(samples);
    for _ in 0..samples {
        let req = if with_tools {
            tool_request()
        } else {
            chat_request()
        };
        let started = Instant::now();
        match model.chat_completion(agent, req).await {
            Ok(res) => {
                if !with_tools || has_tool_call(&res.response) {
                    stats.ok += 1;
                    stats.total_latency_ms += started.elapsed().as_millis();
                } else {
                    stats.last_error = Some("model returned no tool call".to_string());
                }
            }
            Err(e) => stats.last_error = Some(error_chain(&e)),
        }
    }
    stats
}

/// Per-round observability for one deliberation: what prior context was fed back
/// into `propose()` (so we can show the agent had past proposals/evals/critiques
/// to inspect) and whether the agent wrote its scratchpad. Printed as "full
/// details" so the operator sees the NSED loop actually exercised cross-round
/// state, not just isolated one-shot calls.
struct RoundDetail {
    round: u32,
    proposal_chars: usize,
    scratchpad_chars: usize,
    prior_proposal_fed: bool,
    prior_score: Option<f32>,
    prior_critiques: usize,
    candidates_evaluated: usize,
    eval_score: Option<f32>,
}

impl RoundDetail {
    fn line(&self) -> String {
        let scratchpad = if self.scratchpad_chars > 0 {
            format!("written {}c", self.scratchpad_chars)
        } else {
            "none".to_string()
        };
        let prior = if self.prior_proposal_fed {
            format!(
                "proposal\u{2713} score {} critiques {}",
                self.prior_score
                    .map(|s| format!("{s:.2}"))
                    .unwrap_or_else(|| "n/a".to_string()),
                self.prior_critiques,
            )
        } else {
            "none (first round)".to_string()
        };
        let eval = self
            .eval_score
            .map(|s| format!("\u{2192} score {s:.2}"))
            .unwrap_or_else(|| "\u{2192} no score".to_string());
        format!(
            "  round {}: proposal {}c \u{b7} scratchpad {} \u{b7} prior: {} \u{b7} evaluated {} candidate(s) {}",
            self.round, self.proposal_chars, scratchpad, prior, self.candidates_evaluated, eval,
        )
    }
}

/// Run the NSED stage in-process: `deliberations` full deliberations, each of
/// `rounds` rounds running BOTH NSED phases. Returns the tally plus the per-round
/// detail of the first successful deliberation (for the "full details" report).
async fn run_nsed_stage(
    agent: &dyn NsedAgent,
    deliberations: u32,
    rounds: u32,
) -> (StageStats, Vec<RoundDetail>) {
    let mut stats = StageStats::new(deliberations);
    let mut first_detail: Vec<RoundDetail> = Vec::new();
    for _ in 0..deliberations {
        let started = Instant::now();
        match run_one_deliberation(agent, rounds).await {
            Ok(details) => {
                if first_detail.is_empty() {
                    first_detail = details;
                }
                stats.ok += 1;
                stats.total_latency_ms += started.elapsed().as_millis();
            }
            Err(detail) => stats.last_error = Some(detail),
        }
    }
    (stats, first_detail)
}

/// One full deliberation: `rounds` rounds, each running BOTH NSED phases — the
/// agent proposes, then evaluates its own proposal (the candidate). The
/// evaluation's score + justification are threaded back into the next round's
/// proposing context (`previous_own_score`, `previous_critiques`), so the agent
/// inspects its own past proposals and evals exactly as in real deliberation.
/// Exercises the real ReAct loop + tool-calling in propose AND evaluate. Returns
/// a detailed `round N <phase>: <error chain>` string on the first failure.
async fn run_one_deliberation(
    agent: &dyn NsedAgent,
    rounds: u32,
) -> Result<Vec<RoundDetail>, String> {
    let mut details = Vec::new();
    let mut previous: Option<Proposal> = None;
    let mut previous_score: Option<f32> = None;
    let mut previous_critiques: Vec<String> = Vec::new();
    for round in 1..=rounds {
        let mut pctx = smoke_context(&agent.name());
        pctx.round_number = round;
        pctx.total_rounds = rounds;
        pctx.phase = DeliberationPhase::Proposing;
        pctx.previous_own_proposal = previous.clone();
        pctx.previous_own_score = previous_score;
        pctx.previous_critiques = previous_critiques.clone();
        let proposal = agent
            .propose(&pctx)
            .await
            .map_err(|e| format!("round {round} propose: {e:#}"))?;
        if proposal.content.trim().is_empty() {
            return Err(format!("round {round} propose: empty content"));
        }

        let cand_id = format!("smoke-cand-{round}");
        let mut ectx = smoke_context(&agent.name());
        ectx.round_number = round;
        ectx.total_rounds = rounds;
        ectx.phase = DeliberationPhase::Evaluating;
        ectx.candidates = vec![CandidateProposal {
            id: cand_id.clone(),
            proposal: proposal.clone(),
        }];
        let evals = agent
            .evaluate(&ectx)
            .await
            .map_err(|e| format!("round {round} evaluate: {e:#}"))?;
        let eval = evals.iter().find(|(id, _)| *id == cand_id).map(|(_, e)| e);

        details.push(RoundDetail {
            round,
            proposal_chars: proposal.content.chars().count(),
            scratchpad_chars: proposal
                .final_scratchpad
                .as_deref()
                .map(|s| s.chars().count())
                .unwrap_or(0),
            prior_proposal_fed: previous.is_some(),
            prior_score: previous_score,
            prior_critiques: previous_critiques.len(),
            candidates_evaluated: ectx.candidates.len(),
            eval_score: eval.map(|e| e.score),
        });

        previous_score = eval.map(|e| e.score);
        previous_critiques = eval
            .map(|e| vec![e.justification.clone()])
            .unwrap_or_default();
        previous = Some(proposal);
    }
    Ok(details)
}

/// The target must be one of the operator's OWN agents (declared in
/// `quorum.yml`'s `agents:`). It is built and driven locally, so it need not be
/// serving. Never accepts a stranger's / remote agent id.
fn validate_target(local_agents: &[String], target: &str) -> Result<(), String> {
    if local_agents.iter().any(|n| n == target) {
        Ok(())
    } else {
        Err(format!(
            "`{target}` is not one of your agents in quorum.yml. Your agents: {}",
            if local_agents.is_empty() {
                "(none)".to_string()
            } else {
                local_agents.join(", ")
            }
        ))
    }
}

pub async fn run(
    config_path: &Path,
    agent_id: &str,
    runs: u32,
    rounds: u32,
    assume_yes: bool,
) -> ExitCode {
    let fleet = match super::serve::load_fleet_unified(config_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "error: could not load your fleet from {}: {e}",
                config_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let local_agents: Vec<String> = fleet.agents.iter().map(|a| a.name.clone()).collect();
    if let Err(m) = validate_target(&local_agents, agent_id) {
        eprintln!("error: {m}");
        return ExitCode::FAILURE;
    }

    eprintln!(
        "\u{26a0} smoke-test makes REAL LLM calls (chat, tool-calling, and NSED propose for \
         `{agent_id}`) — cost + latency."
    );
    if !assume_yes && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        match inquire::Confirm::new("Proceed?")
            .with_default(false)
            .prompt()
        {
            Ok(true) => {}
            _ => {
                eprintln!("Aborted.");
                return ExitCode::SUCCESS;
            }
        }
    }

    let registry = ProviderRegistry::with_builtins();
    let (agent_config, provider) =
        match load_agent_from_config_with_registry(&fleet, agent_id, &registry) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: could not load agent `{agent_id}` from your fleet: {e}");
                return ExitCode::FAILURE;
            }
        };

    // Make it unmistakable this hits the operator's REAL backend.
    eprintln!(
        "smoke `{agent_id}` \u{2192} provider `{}`, model `{}`{}{}",
        provider.provider_type,
        agent_config.model_name,
        if provider.base_url.is_empty() {
            String::new()
        } else {
            format!(" @ {}", provider.base_url)
        },
        provider
            .engine
            .as_ref()
            .map(|e| format!(", engine `{e}`"))
            .unwrap_or_default(),
    );

    let mut all_ok = true;

    // ── Stages 1-2: direct model calls (built from the agent's provider) ──
    match build_model(&agent_config, &provider) {
        Some(model) => {
            let chat = run_direct_stage(&*model, &agent_config, SMOKE_SAMPLES, false).await;
            eprintln!("{}", chat.line("chat"));
            if chat.ok == 0 {
                eprintln!("\u{2717} chat stage failed for every sample — stopping.");
                return ExitCode::FAILURE;
            }
            all_ok &= chat.ok == chat.runs;

            let tools = run_direct_stage(&*model, &agent_config, SMOKE_SAMPLES, true).await;
            eprintln!("{}", tools.line("tools"));
            if tools.ok == 0 {
                eprintln!(
                    "\u{2717} tool-calling stage failed for every sample — stopping before NSED."
                );
                return ExitCode::FAILURE;
            }
            all_ok &= tools.ok == tools.runs;
        }
        None => eprintln!(
            "\u{2139} chat/tool stages skipped — `{}` is a subprocess agent (no direct model)",
            provider.provider_type
        ),
    }

    // ── Stage 3: NSED in-process via the agent's own propose() ──
    let agent = match registry.build_agent(&provider.provider_type, &agent_config, &provider) {
        Ok(Some(a)) => a,
        Ok(None) => {
            eprintln!(
                "\u{2717} no NSED agent implementation for provider type `{}`",
                provider.provider_type
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("error: could not build agent `{agent_id}`: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (nsed, details) = run_nsed_stage(&*agent, runs, rounds).await;
    eprintln!("{}", nsed.line("nsed"));
    if !details.is_empty() {
        eprintln!(
            "  full details (first deliberation, {} rounds):",
            details.len()
        );
        for d in &details {
            eprintln!("{}", d.line());
        }
    }
    all_ok &= nsed.ok == nsed.runs;

    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_stats_math_and_line() {
        let mut s = StageStats::new(10);
        s.ok = 9;
        s.total_latency_ms = 9 * 400;
        s.last_error = Some("401 Unauthorized".to_string());
        assert_eq!(s.avg_latency_ms(), 400);
        assert_eq!(s.error_rate_pct(), 10);
        let line = s.line("chat");
        assert!(line.contains("9/10 ok"));
        assert!(line.contains("avg 400ms"));
        assert!(line.contains("errors 10%"));
        assert!(line.contains("last error: 401 Unauthorized"));
    }

    #[test]
    fn stage_stats_line_omits_error_when_all_ok() {
        let mut s = StageStats::new(10);
        s.ok = 10;
        s.total_latency_ms = 10 * 100;
        assert!(!s.line("chat").contains("last error"));
    }

    #[test]
    fn stage_stats_all_failed_surfaces_error() {
        let mut s = StageStats::new(10);
        s.last_error = Some("Connection refused".to_string());
        assert_eq!(s.avg_latency_ms(), 0);
        assert_eq!(s.error_rate_pct(), 100);
        assert!(s.line("nsed").contains("last error: Connection refused"));
    }

    #[test]
    fn stage_stats_zero_runs_no_divide_by_zero() {
        let s = StageStats::new(0);
        assert_eq!(s.error_rate_pct(), 0);
        assert_eq!(s.avg_latency_ms(), 0);
    }

    #[test]
    fn has_tool_call_detects_presence_and_absence() {
        let with = serde_json::json!({
            "id": "x", "created": 0, "model": "m", "object": "chat.completion",
            "choices": [{
                "index": 0, "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{
                        "id": "c1", "type": "function",
                        "function": { "name": "echo", "arguments": "{}" }
                    }]
                }
            }]
        });
        let without = serde_json::json!({
            "id": "x", "created": 0, "model": "m", "object": "chat.completion",
            "choices": [{
                "index": 0, "finish_reason": "stop",
                "message": { "role": "assistant", "content": "hello" }
            }]
        });
        let with: CreateChatCompletionResponse = serde_json::from_value(with).unwrap();
        let without: CreateChatCompletionResponse = serde_json::from_value(without).unwrap();
        assert!(has_tool_call(&with));
        assert!(!has_tool_call(&without));
    }

    fn provider(provider_type: &str) -> ProviderEntry {
        ProviderEntry {
            provider_type: provider_type.to_string(),
            base_url: String::new(),
            api_key: String::new(),
            engine: None,
            latency_ms: 0,
            models: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn build_model_skips_subprocess_providers() {
        let agent = AgentConfig {
            model_name: "m".to_string(),
            ..Default::default()
        };
        for t in ["exec", "claude", "mcp"] {
            assert!(
                build_model(&agent, &provider(t)).is_none(),
                "{t} must have no direct model"
            );
        }
        assert!(build_model(&agent, &provider("openai")).is_some());
        assert!(build_model(&agent, &provider("simulated")).is_some());
    }

    #[test]
    fn build_model_none_for_non_openai_without_base_url() {
        let agent = AgentConfig {
            model_name: "m".to_string(),
            ..Default::default()
        };
        // Non-openai type with no base_url → no model (won't leak the key to OpenAI).
        assert!(build_model(&agent, &provider("groq")).is_none());
    }

    #[test]
    fn validate_accepts_local_target() {
        let local = vec!["alice".to_string(), "bob".to_string()];
        assert!(validate_target(&local, "alice").is_ok());
    }

    #[test]
    fn validate_rejects_non_local_target() {
        let local = vec!["alice".to_string()];
        // A stranger's / remote agent id is refused — smoke only runs YOUR agent.
        assert!(validate_target(&local, "cortex-a").is_err());
    }

    #[test]
    fn validate_rejects_when_no_local_agents() {
        let err = validate_target(&[], "alice").unwrap_err();
        assert!(err.contains("(none)"));
    }

    /// True local end-to-end of the NSED stage: a real `ProposerEvaluatorAgent`
    /// backed by the offline `SimulatedModel`, driven through `propose()` with
    /// no orchestrator or network.
    #[tokio::test]
    async fn nsed_stage_runs_against_simulated_agent() {
        use crate::agents::ProposerEvaluatorAgent;
        use crate::prompts::defaults::DefaultPromptSet;

        let agent_config = AgentConfig {
            name: "sim".to_string(),
            provider_id: "sim".to_string(),
            model_name: "sim-model".to_string(),
            ..Default::default()
        };
        let agent = ProposerEvaluatorAgent::new(
            agent_config,
            Box::new(SimulatedModel::new("sim-model".to_string(), 0)),
            Box::new(DefaultPromptSet::new()),
            vec![],
            vec![],
        );
        let (stats, details) = run_nsed_stage(&agent, 3, 2).await;
        assert_eq!(stats.ok, 3, "simulated propose should succeed every run");
        assert_eq!(stats.error_rate_pct(), 0);
        assert_eq!(details.len(), 2);
        assert!(!details[0].prior_proposal_fed);
        assert!(details[1].prior_proposal_fed);
        assert_eq!(details[0].candidates_evaluated, 1);
    }

    #[test]
    fn round_detail_line_shows_prior_context_and_scratchpad() {
        let first = RoundDetail {
            round: 1,
            proposal_chars: 240,
            scratchpad_chars: 0,
            prior_proposal_fed: false,
            prior_score: None,
            prior_critiques: 0,
            candidates_evaluated: 1,
            eval_score: Some(0.5),
        }
        .line();
        assert!(first.contains("round 1"));
        assert!(first.contains("scratchpad none"));
        assert!(first.contains("none (first round)"));
        assert!(first.contains("score 0.50"));

        let second = RoundDetail {
            round: 2,
            proposal_chars: 310,
            scratchpad_chars: 412,
            prior_proposal_fed: true,
            prior_score: Some(0.5),
            prior_critiques: 1,
            candidates_evaluated: 1,
            eval_score: Some(0.62),
        }
        .line();
        assert!(second.contains("scratchpad written 412c"));
        assert!(second.contains("proposal\u{2713} score 0.50 critiques 1"));
    }
}
