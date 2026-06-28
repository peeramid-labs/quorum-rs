//! `quorum smoke-test <agent_id>` — run real NSED deliberations filled with the
//! operator's own online agents, then report how often the target agent actually
//! participated. The full protocol runs server-side (so it exercises whichever
//! API each agent uses — chat-completions or responses). Submits ad-hoc
//! deliberations the same way `quorum run` does (an ad-hoc `room_id` +
//! `agent_names`), so it needs only the operator token — no `manage_rooms`.

use std::path::Path;
use std::process::ExitCode;

use crate::cli::remote::{AgentInfo, JobDetails, JobOutcome, RemoteOrchestrator, TraceRecord};
use crate::cli::request::DeliberationRequest;

const SMOKE_TASK: &str =
    "Smoke test: reply briefly with your role and confirm you are operational.";
const SMOKE_ROUNDS: u32 = 2;

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

/// Pick agents for the smoke deliberation: the target must be present AND online;
/// fill with the other online agents the caller can see to reach the 2-agent
/// deliberation minimum. Errors (no panics) when the target is offline or there
/// aren't enough online agents.
fn choose_smoke_agents(all: &[AgentInfo], target: &str) -> Result<Vec<String>, String> {
    if !all.iter().any(|a| a.agent_id == target && a.is_online) {
        return Err(format!(
            "agent `{target}` is not online — run `quorum serve` first"
        ));
    }
    let mut chosen = vec![target.to_string()];
    for a in all {
        if a.is_online && a.agent_id != target {
            chosen.push(a.agent_id.clone());
        }
    }
    if chosen.len() < 2 {
        return Err(format!(
            "smoke needs at least 2 online agents; only {} available",
            chosen.len()
        ));
    }
    Ok(chosen)
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

    let agents = match client.agents().await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: could not list agents (GET /agents): {e}");
            return ExitCode::FAILURE;
        }
    };
    let chosen = match choose_smoke_agents(&agents, agent_id) {
        Ok(c) => c,
        Err(m) => {
            eprintln!("error: {m}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("smoke: {} agent(s) → {}", chosen.len(), chosen.join(", "));

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
    fn choose_requires_target_online() {
        let all = [
            agent("alice", false),
            agent("bob", true),
            agent("carol", true),
        ];
        assert!(choose_smoke_agents(&all, "alice").is_err());
    }

    #[test]
    fn choose_includes_target_and_others() {
        let all = [
            agent("alice", true),
            agent("bob", true),
            agent("carol", false),
        ];
        let chosen = choose_smoke_agents(&all, "alice").unwrap();
        assert_eq!(chosen[0], "alice", "target first");
        assert!(chosen.contains(&"bob".to_string()), "online other included");
        assert!(
            !chosen.contains(&"carol".to_string()),
            "offline other excluded"
        );
    }

    #[test]
    fn choose_errors_when_too_few_online() {
        let all = [agent("alice", true), agent("bob", false)];
        assert!(choose_smoke_agents(&all, "alice").is_err());
    }
}
