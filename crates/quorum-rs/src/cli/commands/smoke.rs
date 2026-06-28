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

use indicatif::{ProgressBar, ProgressStyle};

use crate::agents::config::AgentConfig;
use crate::agents::{AgentContext, CandidateProposal, DeliberationPhase, NsedAgent, Proposal};
use crate::config::{ProviderEntry, load_agent_from_config_with_registry};
use crate::llms::error::LlmError;
use crate::llms::openai_compatible::OpenAICompatibleModel;
use crate::llms::simulated::SimulatedModel;
use crate::llms::{AiModel, RequestConfig};
use crate::providers::ProviderRegistry;

/// Samples per direct stage (chat, tool-calling).
const SMOKE_SAMPLES: u32 = 10;
const SMOKE_TASK: &str =
    "Smoke test: reply briefly with your role and confirm you are operational.";

/// One failed sample, with everything the harness knew at failure time so the
/// operator can triage without a server log. `context` is a stage-specific
/// breakdown (round/phase + prior-context for NSED, request shape for the direct
/// stages); `detail` is the provider's HTTP-400 body, which `LlmError::Display`
/// withholds but a 400 commonly fills with the real reason (token math, bad tool
/// schema). Smoke is operator-local, so surfacing the body here is safe.
struct Failure {
    seq: u32,
    context: String,
    latency_ms: u128,
    error: String,
    detail: Option<String>,
}

impl Failure {
    /// A direct-stage (chat / tool-calling) failure: we know the request shape.
    fn direct(seq: u32, with_tools: bool, latency_ms: u128, err: &LlmError) -> Self {
        let context = format!("req 1 msg, {} tool(s)", if with_tools { 1 } else { 0 });
        Failure {
            seq,
            context,
            latency_ms,
            error: err.display_chain().replace('\n', "; "),
            detail: err.detail().map(str::to_string),
        }
    }

    /// A direct-stage failure where the call SUCCEEDED but the model returned no
    /// tool call (tool stage only) — no `LlmError`, just a contract miss.
    fn direct_no_tool(seq: u32, latency_ms: u128) -> Self {
        Failure {
            seq,
            context: "req 1 msg, 1 tool(s)".to_string(),
            latency_ms,
            error: "model returned no tool call".to_string(),
            detail: None,
        }
    }

    /// An NSED-stage failure: `context` from [`nsed_context`] carries round/phase
    /// + prior context; the anyhow chain is mined for an `LlmError` 400 body.
    fn nsed(seq: u32, context: String, err: &anyhow::Error) -> Self {
        let detail = err
            .chain()
            .find_map(|c| c.downcast_ref::<LlmError>().and_then(LlmError::detail))
            .map(str::to_string);
        Failure {
            seq,
            context,
            latency_ms: 0,
            error: format!("{err:#}"),
            detail,
        }
    }

    fn nsed_empty(seq: u32, round: u32) -> Self {
        Failure {
            seq,
            context: format!("round {round}/propose"),
            latency_ms: 0,
            error: "empty proposal content".to_string(),
            detail: None,
        }
    }

    fn line(&self) -> String {
        let lat = if self.latency_ms > 0 {
            format!(" \u{b7} {}ms", self.latency_ms)
        } else {
            String::new()
        };
        let mut s = format!(
            "  #{} {}{}\n      {}",
            self.seq, self.context, lat, self.error
        );
        if let Some(d) = &self.detail {
            let d = d.trim();
            if !d.is_empty() {
                s.push_str(&format!("\n      reason: {d}"));
            }
        }
        s
    }
}

/// Tally for one stage: how many of `runs` succeeded, their latency, and every
/// failure with its breakdown. Everything runs in this process, so the reasons
/// have to surface here — there is no server log to consult.
struct StageStats {
    ok: u32,
    runs: u32,
    total_latency_ms: u128,
    failures: Vec<Failure>,
}

impl StageStats {
    fn new(runs: u32) -> Self {
        StageStats {
            ok: 0,
            runs,
            total_latency_ms: 0,
            failures: Vec::new(),
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
        format!(
            "{label}: {}/{} ok \u{b7} avg {}ms \u{b7} errors {}%",
            self.ok,
            self.runs,
            self.avg_latency_ms(),
            self.error_rate_pct()
        )
    }

    /// Build the per-failure breakdown lines: an aggregate count per distinct
    /// error headline, then each failure (capped) with its full context + 400
    /// body, then a "+N more" line when truncated. Returns the lines so the
    /// formatting is unit-testable; `print_failures` just emits them.
    fn report_lines(&self) -> Vec<String> {
        const CAP: usize = 8;
        let mut out = Vec::new();
        if self.failures.is_empty() {
            return out;
        }
        out.push("  failures by error:".to_string());
        for (err, n) in group_errors(&self.failures) {
            out.push(format!("    {n}\u{d7} {err}"));
        }
        for f in self.failures.iter().take(CAP) {
            out.push(f.line());
        }
        if self.failures.len() > CAP {
            out.push(format!("  \u{2026} and {} more", self.failures.len() - CAP));
        }
        out
    }

    fn print_failures(&self) {
        for line in self.report_lines() {
            eprintln!("{line}");
        }
    }
}

fn nsed_context(
    round: u32,
    phase: &str,
    prior_proposal: bool,
    prior_critiques: usize,
    candidates: usize,
) -> String {
    format!(
        "round {round}/{phase} \u{b7} prior {} critiques {} \u{b7} candidates {}",
        if prior_proposal {
            "proposal\u{2713}"
        } else {
            "none"
        },
        prior_critiques,
        candidates,
    )
}

/// Group failures by identical error headline, preserving first-seen order, so
/// the breakdown shows "3× bad request (status 400)" instead of three lines.
fn group_errors(failures: &[Failure]) -> Vec<(String, u32)> {
    let mut counts: Vec<(String, u32)> = Vec::new();
    for f in failures {
        match counts.iter_mut().find(|(k, _)| *k == f.error) {
            Some((_, n)) => *n += 1,
            None => counts.push((f.error.clone(), 1)),
        }
    }
    counts
}

/// Progress bar for a stage. Hidden when stderr isn't a TTY (CI/pipes) so logs
/// stay clean; `ProgressBar::hidden()` is also what tests pass.
fn stage_bar(label: &str, len: u32) -> ProgressBar {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return ProgressBar::hidden();
    }
    let bar = ProgressBar::new(len as u64);
    bar.set_style(
        ProgressStyle::with_template(
            "  {prefix} [{bar:24}] {pos}/{len} ok:{msg} {elapsed_precise}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> "),
    );
    bar.set_prefix(label.to_string());
    bar.set_message("0");
    bar
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
    bar: &ProgressBar,
) -> StageStats {
    let mut stats = StageStats::new(samples);
    for seq in 1..=samples {
        let req = if with_tools {
            tool_request()
        } else {
            chat_request()
        };
        let started = Instant::now();
        let latency = || started.elapsed().as_millis();
        match model.chat_completion(agent, req).await {
            Ok(res) => {
                if !with_tools || has_tool_call(&res.response) {
                    stats.ok += 1;
                    stats.total_latency_ms += latency();
                } else {
                    stats.failures.push(Failure::direct_no_tool(seq, latency()));
                }
            }
            Err(e) => stats
                .failures
                .push(Failure::direct(seq, with_tools, latency(), &e)),
        }
        bar.set_message(stats.ok.to_string());
        bar.inc(1);
    }
    bar.finish_and_clear();
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
    bar: &ProgressBar,
) -> (StageStats, Vec<RoundDetail>) {
    let mut stats = StageStats::new(deliberations);
    let mut first_detail: Vec<RoundDetail> = Vec::new();
    for seq in 1..=deliberations {
        let started = Instant::now();
        match run_one_deliberation(agent, rounds, seq).await {
            Ok(details) => {
                if first_detail.is_empty() {
                    first_detail = details;
                }
                stats.ok += 1;
                stats.total_latency_ms += started.elapsed().as_millis();
            }
            Err(mut failure) => {
                failure.latency_ms = started.elapsed().as_millis();
                stats.failures.push(failure);
            }
        }
        bar.set_message(stats.ok.to_string());
        bar.inc(1);
    }
    bar.finish_and_clear();
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
    seq: u32,
) -> Result<Vec<RoundDetail>, Failure> {
    let mut details = Vec::new();
    let mut previous: Option<Proposal> = None;
    let mut previous_score: Option<f32> = None;
    let mut previous_critiques: Vec<String> = Vec::new();
    for round in 1..=rounds {
        let prior_proposal = previous.is_some();
        let prior_critiques = previous_critiques.len();
        let mut pctx = smoke_context(&agent.name());
        pctx.round_number = round;
        pctx.total_rounds = rounds;
        pctx.phase = DeliberationPhase::Proposing;
        pctx.previous_own_proposal = previous.clone();
        pctx.previous_own_score = previous_score;
        pctx.previous_critiques = previous_critiques.clone();
        let proposal = agent.propose(&pctx).await.map_err(|e| {
            Failure::nsed(
                seq,
                nsed_context(round, "propose", prior_proposal, prior_critiques, 0),
                &e,
            )
        })?;
        if proposal.content.trim().is_empty() {
            return Err(Failure::nsed_empty(seq, round));
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
        let evals = agent.evaluate(&ectx).await.map_err(|e| {
            Failure::nsed(
                seq,
                nsed_context(round, "evaluate", prior_proposal, prior_critiques, 1),
                &e,
            )
        })?;
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
            let chat = run_direct_stage(
                &*model,
                &agent_config,
                SMOKE_SAMPLES,
                false,
                &stage_bar("chat ", SMOKE_SAMPLES),
            )
            .await;
            eprintln!("{}", chat.line("chat"));
            chat.print_failures();
            if chat.ok == 0 {
                eprintln!("\u{2717} chat stage failed for every sample — stopping.");
                return ExitCode::FAILURE;
            }
            all_ok &= chat.ok == chat.runs;

            let tools = run_direct_stage(
                &*model,
                &agent_config,
                SMOKE_SAMPLES,
                true,
                &stage_bar("tools", SMOKE_SAMPLES),
            )
            .await;
            eprintln!("{}", tools.line("tools"));
            tools.print_failures();
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
    let (nsed, details) = run_nsed_stage(&*agent, runs, rounds, &stage_bar("nsed ", runs)).await;
    eprintln!("{}", nsed.line("nsed"));
    nsed.print_failures();
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
        assert_eq!(s.avg_latency_ms(), 400);
        assert_eq!(s.error_rate_pct(), 10);
        let line = s.line("chat");
        assert!(line.contains("9/10 ok"));
        assert!(line.contains("avg 400ms"));
        assert!(line.contains("errors 10%"));
    }

    #[test]
    fn direct_failure_line_carries_400_body() {
        let err = LlmError::BadRequest {
            status: 400,
            body: "tools[0].function.parameters: invalid schema".to_string(),
        };
        let f = Failure::direct(4, true, 530, &err);
        let line = f.line();
        assert!(line.contains("#4"));
        assert!(line.contains("1 tool(s)"));
        assert!(line.contains("530ms"));
        assert!(line.contains("bad request (status 400)"));
        // The 400 body — withheld by Display — is surfaced as `reason:`.
        assert!(line.contains("reason: tools[0].function.parameters: invalid schema"));
    }

    #[test]
    fn nsed_failure_mines_llm_detail_from_anyhow_chain() {
        let err: anyhow::Error = LlmError::BadRequest {
            status: 400,
            body: "max_tokens 16000 > 21000 - 5395 input".to_string(),
        }
        .into();
        let err = err.context("round 2 evaluate");
        let f = Failure::nsed(2, nsed_context(2, "evaluate", true, 1, 1), &err);
        let line = f.line();
        assert!(line.contains("round 2/evaluate"));
        assert!(line.contains("proposal\u{2713} critiques 1"));
        assert!(line.contains("candidates 1"));
        assert!(line.contains("reason: max_tokens 16000 > 21000 - 5395 input"));
    }

    #[test]
    fn nsed_empty_failure_line_has_no_latency_or_reason() {
        let line = Failure::nsed_empty(3, 2).line();
        assert!(line.contains("#3"));
        assert!(line.contains("round 2/propose"));
        assert!(line.contains("empty proposal content"));
        assert!(!line.contains("ms"), "no latency segment: {line}");
        assert!(!line.contains("reason:"), "no reason segment: {line}");
    }

    #[test]
    fn group_errors_dedups_and_preserves_order() {
        let mk = |seq, msg: &str| Failure {
            seq,
            context: String::new(),
            latency_ms: 0,
            error: msg.to_string(),
            detail: None,
        };
        let fs = vec![
            mk(1, "bad request (status 400)"),
            mk(2, "transport"),
            mk(3, "bad request (status 400)"),
        ];
        let grouped = group_errors(&fs);
        assert_eq!(
            grouped,
            vec![
                ("bad request (status 400)".to_string(), 2),
                ("transport".to_string(), 1),
            ]
        );
    }

    #[test]
    fn report_lines_caps_failures_and_reports_remainder() {
        let mut s = StageStats::new(10);
        for seq in 1..=10 {
            s.failures.push(Failure {
                seq,
                context: format!("c{seq}"),
                latency_ms: 0,
                error: "boom".to_string(),
                detail: None,
            });
        }
        let lines = s.report_lines();
        assert_eq!(lines[0], "  failures by error:");
        assert!(lines.iter().any(|l| l.contains("10\u{d7} boom")));
        assert!(
            lines.iter().any(|l| l.contains("and 2 more")),
            "10 failures, cap 8 → 2 more: {lines:?}"
        );
    }

    #[test]
    fn report_lines_empty_when_no_failures() {
        assert!(StageStats::new(5).report_lines().is_empty());
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
        let (stats, details) = run_nsed_stage(&agent, 3, 2, &ProgressBar::hidden()).await;
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
