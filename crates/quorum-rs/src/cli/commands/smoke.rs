//! `quorum smoke-test <agent_id>` — staged escalation for ONE of the operator's
//! own online agents:
//!
//! 1. **chat** — call the agent's model directly N times with a trivial prompt;
//!    report success/avg-latency/error-rate.
//! 2. **tool-calling** — same model, N times, expecting a tool call back.
//! 3. **full NSED** — submit single-agent deliberations through the orchestrator
//!    and verify the agent participated.
//!
//! Each stage gates the next (a stage that fails every sample stops the run).
//! Stages 1-2 are direct model calls (built from the agent's provider in
//! `quorum.yml`); a subprocess provider (`exec`/`claude`/`mcp`) can't be called
//! directly, so those stages are skipped and only the NSED stage runs. It never
//! pulls in other operators' / remote agents.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use async_openai::types::{
    ChatCompletionRequestUserMessage, ChatCompletionTool, ChatCompletionToolChoiceOption,
    ChatCompletionToolType, CreateChatCompletionResponse, FunctionObject,
};

use crate::agents::config::AgentConfig;
use crate::cli::remote::{AgentInfo, JobDetails, JobOutcome, RemoteOrchestrator, TraceRecord};
use crate::cli::request::DeliberationRequest;
use crate::config::{ProviderEntry, load_agent_from_config_with_registry};
use crate::llms::openai_compatible::OpenAICompatibleModel;
use crate::llms::simulated::SimulatedModel;
use crate::llms::{AiModel, RequestConfig};
use crate::providers::ProviderRegistry;

/// Samples per direct stage (chat, tool-calling).
const SMOKE_SAMPLES: u32 = 10;
const SMOKE_TASK: &str =
    "Smoke test: reply briefly with your role and confirm you are operational.";
const SMOKE_ROUNDS: u32 = 1;

/// Tally for one direct stage: how many of `runs` succeeded, their latency, and
/// the most recent failure reason (these stages call the provider directly — so
/// `quorum serve` logs nothing; the reason has to surface here).
struct StageStats {
    ok: u32,
    runs: u32,
    total_latency_ms: u128,
    last_error: Option<String>,
}

impl StageStats {
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
        // Surface why it failed — these direct calls never touch `quorum serve`,
        // so without this the operator sees a 0% with no clue.
        if self.ok < self.runs
            && let Some(err) = &self.last_error
        {
            s.push_str(&format!(" \u{2014} last error: {err}"));
        }
        s
    }
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
            let base_url = if provider.base_url.is_empty() {
                if provider.provider_type.is_empty() || provider.provider_type == "openai" {
                    "https://api.openai.com/v1".to_string()
                } else {
                    return None;
                }
            } else {
                provider.base_url.clone()
            };
            Some(Box::new(OpenAICompatibleModel::new(
                base_url,
                provider.api_key.clone(),
                provider.engine.clone(),
            )))
        }
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
    let mut stats = StageStats {
        ok: 0,
        runs: samples,
        total_latency_ms: 0,
        last_error: None,
    };
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
            Err(e) => stats.last_error = Some(e.to_string()),
        }
    }
    stats
}

/// Did `agent_id` propose or evaluate anywhere in the deliberation trace?
fn participated(details: &JobDetails, agent_id: &str) -> bool {
    let touched = |r: &TraceRecord| {
        r.author_agent_id == agent_id
            || r.evaluations
                .iter()
                .any(|e| e.evaluator_agent_id == agent_id)
    };
    details.history.iter().any(touched) || details.final_result.as_ref().is_some_and(touched)
}

/// The NSED-stage report line, e.g. `nsed alice: 4/5 participated (80%)`.
fn report_line(agent_id: &str, passed: u32, runs: u32) -> String {
    let pct = (passed * 100).checked_div(runs).unwrap_or(0);
    format!("nsed {agent_id}: {passed}/{runs} participated ({pct}%)")
}

/// Validate the smoke target: it must be one of the operator's OWN agents
/// (declared in `quorum.yml`'s `agents:`) AND currently online at the
/// orchestrator. Returns errors (no panics); never selects any other agent —
/// the smoke runs exactly the specified agent, not strangers' remote agents.
fn validate_target(
    local_agents: &[String],
    online: &[AgentInfo],
    target: &str,
) -> Result<(), String> {
    if !local_agents.iter().any(|n| n == target) {
        return Err(format!(
            "`{target}` is not one of your agents in quorum.yml. Your agents: {}",
            if local_agents.is_empty() {
                "(none)".to_string()
            } else {
                local_agents.join(", ")
            }
        ));
    }
    if !online.iter().any(|a| a.agent_id == target && a.is_online) {
        return Err(format!(
            "agent `{target}` is not online — run `quorum serve` first"
        ));
    }
    Ok(())
}

pub async fn run(config_path: &Path, agent_id: &str, runs: u32, assume_yes: bool) -> ExitCode {
    let Some((address, token)) = super::serve::resolve_remote_orchestrator(config_path, None)
    else {
        eprintln!(
            "error: no remote orchestrator resolvable from the workspace ({}). \
             smoke-test needs a remote orchestrator entry with an address + token.",
            config_path.display()
        );
        return ExitCode::FAILURE;
    };
    let client = match RemoteOrchestrator::new(&address, &token) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    eprintln!(
        "\u{26a0} smoke-test makes REAL LLM calls (chat + tools direct to your provider, then \
         deliberations on {address}) — cost + latency."
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
    let agents = match client.agents().await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: could not list agents (GET /agents): {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(m) = validate_target(&local_agents, &agents, agent_id) {
        eprintln!("error: {m}");
        return ExitCode::FAILURE;
    }

    let mut all_ok = true;

    // ── Stages 1-2: direct model calls (built from the agent's provider) ──
    let registry = ProviderRegistry::with_builtins();
    match load_agent_from_config_with_registry(&fleet, agent_id, &registry) {
        Ok((agent_config, provider)) => match build_model(&agent_config, &provider) {
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
        },
        Err(e) => {
            eprintln!("error: could not load agent `{agent_id}` from your fleet: {e}");
            return ExitCode::FAILURE;
        }
    }

    // ── Stage 3: full NSED deliberation (only the specified agent) ──
    let chosen = vec![agent_id.to_string()];
    let mut passed = 0u32;
    for k in 1..=runs {
        let req = DeliberationRequest {
            room_id: format!("smoke-{}", uuid::Uuid::new_v4().simple()),
            user_query: SMOKE_TASK.to_string(),
            deliberation_rounds: SMOKE_ROUNDS,
            agent_names: Some(chosen.clone()),
            policy_id: None,
            effort: None,
            scope: None,
            timeout_seconds: None,
        };
        let job_id = match client.submit(&req).await {
            Ok(id) => id,
            Err(e) => {
                eprintln!("nsed {k}/{runs} \u{2717} submit failed: {e}");
                continue;
            }
        };
        match client.stream_events(&job_id).await {
            Ok(JobOutcome::Success(payload)) => match client.details(&job_id).await {
                Ok(details) if participated(&details, agent_id) => {
                    passed += 1;
                    eprintln!(
                        "nsed {k}/{runs} \u{2713} {agent_id} participated (score {:.2})",
                        payload.best_proposal_score
                    );
                }
                Ok(_) => eprintln!("nsed {k}/{runs} \u{2717} {agent_id} absent from trace"),
                Err(e) => eprintln!("nsed {k}/{runs} \u{2717} could not fetch trace: {e}"),
            },
            Ok(JobOutcome::Failed(status)) => {
                eprintln!("nsed {k}/{runs} \u{2717} deliberation failed: {status}")
            }
            Err(e) => eprintln!("nsed {k}/{runs} \u{2717} stream failed: {e}"),
        }
    }
    eprintln!("{}", report_line(agent_id, passed, runs));
    all_ok &= passed == runs;

    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::remote::{TraceEvalDetail, TraceEvaluation, TraceProposal};

    fn record(author: &str, evaluators: &[&str]) -> TraceRecord {
        TraceRecord {
            round: 1,
            author_agent_id: author.to_string(),
            proposal: TraceProposal {
                content: String::new(),
                thought_process: String::new(),
            },
            evaluations: evaluators
                .iter()
                .map(|e| TraceEvaluation {
                    evaluator_agent_id: e.to_string(),
                    evaluation: TraceEvalDetail {
                        score: 0.0,
                        justification: String::new(),
                    },
                })
                .collect(),
            aggregated_score: 0.0,
        }
    }

    fn details(history: Vec<TraceRecord>) -> JobDetails {
        JobDetails {
            job_id: "j".to_string(),
            history,
            final_result: None,
        }
    }

    fn agent(id: &str, online: bool) -> AgentInfo {
        AgentInfo {
            agent_id: id.to_string(),
            is_online: online,
            ..Default::default()
        }
    }

    #[test]
    fn stage_stats_math_and_line() {
        let s = StageStats {
            ok: 9,
            runs: 10,
            total_latency_ms: 9 * 400,
            last_error: Some("401 Unauthorized".to_string()),
        };
        assert_eq!(s.avg_latency_ms(), 400);
        assert_eq!(s.error_rate_pct(), 10);
        let line = s.line("chat");
        assert!(line.contains("9/10 ok"));
        assert!(line.contains("avg 400ms"));
        assert!(line.contains("errors 10%"));
        // Failures present → surface the reason.
        assert!(line.contains("last error: 401 Unauthorized"));
    }

    #[test]
    fn stage_stats_line_omits_error_when_all_ok() {
        let s = StageStats {
            ok: 10,
            runs: 10,
            total_latency_ms: 10 * 100,
            last_error: None,
        };
        assert!(!s.line("chat").contains("last error"));
    }

    #[test]
    fn stage_stats_all_failed_surfaces_error() {
        let s = StageStats {
            ok: 0,
            runs: 10,
            total_latency_ms: 0,
            last_error: Some("Connection refused".to_string()),
        };
        assert_eq!(s.avg_latency_ms(), 0);
        assert_eq!(s.error_rate_pct(), 100);
        assert!(s.line("chat").contains("last error: Connection refused"));
    }

    #[test]
    fn stage_stats_zero_runs_no_divide_by_zero() {
        let s = StageStats {
            ok: 0,
            runs: 0,
            total_latency_ms: 0,
            last_error: None,
        };
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
        // openai with no base_url → default openai endpoint, buildable.
        assert!(build_model(&agent, &provider("openai")).is_some());
        // simulated → buildable.
        assert!(build_model(&agent, &provider("simulated")).is_some());
    }

    #[test]
    fn participated_true_as_proposer_or_evaluator() {
        assert!(participated(
            &details(vec![record("alice", &["bob"])]),
            "alice"
        ));
        assert!(participated(
            &details(vec![record("bob", &["alice"])]),
            "alice"
        ));
    }

    #[test]
    fn participated_false_when_absent() {
        assert!(!participated(
            &details(vec![record("bob", &["carol"])]),
            "alice"
        ));
        assert!(!participated(&details(vec![]), "alice"));
    }

    #[test]
    fn report_line_percentages() {
        assert!(report_line("a", 3, 3).contains("3/3 participated (100%)"));
        assert!(report_line("a", 2, 3).contains("2/3 participated (66%)"));
        assert!(report_line("a", 0, 3).contains("0/3 participated (0%)"));
        assert!(report_line("a", 0, 0).contains("0/0 participated (0%)"));
    }

    #[test]
    fn validate_ok_when_local_and_online() {
        let local = vec!["alice".to_string(), "bob".to_string()];
        let online = [agent("alice", true), agent("cortex-a", true)];
        assert!(validate_target(&local, &online, "alice").is_ok());
    }

    #[test]
    fn validate_rejects_non_local_target() {
        let local = vec!["alice".to_string()];
        let online = [agent("cortex-a", true)];
        assert!(validate_target(&local, &online, "cortex-a").is_err());
    }

    #[test]
    fn validate_rejects_offline_local_target() {
        let local = vec!["alice".to_string()];
        let online = [agent("alice", false)];
        assert!(validate_target(&local, &online, "alice").is_err());
    }

    #[test]
    fn validate_rejects_when_no_local_agents() {
        let online = [agent("alice", true)];
        let err = validate_target(&[], &online, "alice").unwrap_err();
        assert!(err.contains("(none)"));
    }
}
