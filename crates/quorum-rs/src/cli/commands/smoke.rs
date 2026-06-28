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
use crate::agents::{AgentContext, DeliberationPhase, NsedAgent};
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
            Err(e) => stats.last_error = Some(e.to_string()),
        }
    }
    stats
}

/// Run the NSED stage in-process: `runs` calls to `agent.propose()` with a
/// synthetic proposing context. A run is ok when it returns a proposal with
/// non-empty content. Exercises the full NSED wrapper (ReAct loop + tools).
async fn run_nsed_stage(agent: &dyn NsedAgent, runs: u32) -> StageStats {
    let ctx = smoke_context(&agent.name());
    let mut stats = StageStats::new(runs);
    for _ in 0..runs {
        let started = Instant::now();
        match agent.propose(&ctx).await {
            Ok(p) if !p.content.trim().is_empty() => {
                stats.ok += 1;
                stats.total_latency_ms += started.elapsed().as_millis();
            }
            Ok(_) => stats.last_error = Some("proposal had empty content".to_string()),
            Err(e) => stats.last_error = Some(e.to_string()),
        }
    }
    stats
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

pub async fn run(config_path: &Path, agent_id: &str, runs: u32, assume_yes: bool) -> ExitCode {
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
    let nsed = run_nsed_stage(&*agent, runs).await;
    eprintln!("{}", nsed.line("nsed"));
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
        let stats = run_nsed_stage(&agent, 3).await;
        assert_eq!(stats.ok, 3, "simulated propose should succeed every run");
        assert_eq!(stats.error_rate_pct(), 0);
    }
}
