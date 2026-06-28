//! `quorum smoke-test <agent_id>` — run real NSED deliberations with ONLY the
//! one specified agent (which must be the operator's own, from `quorum.yml`),
//! then report how often it actually participated. The full protocol runs
//! server-side (so it exercises whichever API the agent uses — chat-completions
//! or responses). Submits ad-hoc deliberations the same way `quorum run` does
//! (an ad-hoc `room_id` + `agent_names`), so it needs only the operator token —
//! no `manage_rooms`. It never pulls in other operators' / remote agents.

use std::path::Path;
use std::process::ExitCode;

use crate::cli::remote::{AgentInfo, JobDetails, JobOutcome, RemoteOrchestrator, TraceRecord};
use crate::cli::request::DeliberationRequest;

const SMOKE_TASK: &str =
    "Smoke test: reply briefly with your role and confirm you are operational.";
const SMOKE_ROUNDS: u32 = 1;

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

/// The final report line, e.g. `smoke alice: 4/5 participated (80%)`.
fn report_line(agent_id: &str, passed: u32, runs: u32) -> String {
    let pct = (passed * 100).checked_div(runs).unwrap_or(0);
    format!("smoke {agent_id}: {passed}/{runs} participated ({pct}%)")
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
        "\u{26a0} smoke-test runs REAL deliberations on {address} using your agents — \
         real LLM calls (cost + latency)."
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

    // The target must be the operator's OWN agent (from quorum.yml), not just
    // any agent visible at the orchestrator — otherwise the smoke would run
    // against strangers' remote agents.
    let local_agents: Vec<String> = match super::serve::load_fleet_unified(config_path) {
        Ok(fleet) => fleet.agents.into_iter().map(|a| a.name).collect(),
        Err(e) => {
            eprintln!(
                "error: could not load your fleet from {}: {e}",
                config_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
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
    // ONLY the specified agent participates.
    let chosen = vec![agent_id.to_string()];
    eprintln!("smoke: 1 agent → {agent_id}");

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
                eprintln!("run {k}/{runs} \u{2717} submit failed: {e}");
                continue;
            }
        };
        match client.stream_events(&job_id).await {
            Ok(JobOutcome::Success(payload)) => match client.details(&job_id).await {
                Ok(details) if participated(&details, agent_id) => {
                    passed += 1;
                    eprintln!(
                        "run {k}/{runs} \u{2713} {agent_id} participated (score {:.2})",
                        payload.best_proposal_score
                    );
                }
                Ok(_) => eprintln!("run {k}/{runs} \u{2717} {agent_id} absent from trace"),
                Err(e) => eprintln!("run {k}/{runs} \u{2717} could not fetch trace: {e}"),
            },
            Ok(JobOutcome::Failed(status)) => {
                eprintln!("run {k}/{runs} \u{2717} deliberation failed: {status}")
            }
            Err(e) => eprintln!("run {k}/{runs} \u{2717} stream failed: {e}"),
        }
    }

    eprintln!("{}", report_line(agent_id, passed, runs));
    if passed == runs {
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
        // cortex-a is online at the orchestrator but NOT one of the operator's
        // own agents → must be rejected (never smoke strangers' remote agents).
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
        // Empty fleet → the "(none)" message branch; always an error.
        let online = [agent("alice", true)];
        let err = validate_target(&[], &online, "alice").unwrap_err();
        assert!(err.contains("(none)"));
    }
}
