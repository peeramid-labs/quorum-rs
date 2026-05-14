use std::path::Path;
use std::process::ExitCode;

use crate::remote::{RemoteError, TraceRecord};
use crate::workspace::WorkspaceConfig;

use super::common::{build_remote, resolve_single_orchestrator};

pub async fn run(
    config_path: &Path,
    job_id: &str,
    orchestrator: Option<&str>,
    verbose: bool,
) -> ExitCode {
    let config = match WorkspaceConfig::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (name, orch) = match resolve_single_orchestrator(&config, orchestrator) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let client = match build_remote(name, orch) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Fetch job status first
    let status = match client.result(job_id).await {
        Ok(r) => r,
        Err(RemoteError::ApiError { status: 404, .. }) => {
            eprintln!("error: job '{job_id}' not found on orchestrator '{name}'");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("error: failed to fetch job status: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("Job: {}", status.job_id);
    println!("Status: {}", status.status);

    // For non-completed, non-running jobs there may be no details
    if status.status == "pending" {
        println!("\nJob is queued — no rounds executed yet.");
        return ExitCode::SUCCESS;
    }

    // Fetch full details — 404 is OK for in-flight jobs (no history yet)
    let details = match client.details(job_id).await {
        Ok(d) => d,
        Err(RemoteError::ApiError { status: 404, .. }) => {
            println!("\nNo history available yet.");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: failed to fetch details: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Budget info (best effort) — fetch early to get total_rounds
    let budget = client.budget(job_id).await.ok();

    // Prefer budget.total_rounds over history max
    let max_round = details.history.iter().map(|r| r.round).max().unwrap_or(0);
    let total_rounds = budget
        .as_ref()
        .map(|b| b.budget.total_rounds)
        .filter(|&t| t > 0)
        .unwrap_or(max_round);

    for round in 1..=max_round {
        let proposals: Vec<&TraceRecord> = details
            .history
            .iter()
            .filter(|r| r.round == round)
            .collect();

        if proposals.is_empty() {
            continue;
        }

        println!("\nRound {round} / {total_rounds}:");
        println!("  Proposals:");

        for p in &proposals {
            let content = truncate(&p.proposal.content, 80);
            println!(
                "    {:<20} {:.2}  \"{}\"",
                p.author_agent_id, p.aggregated_score, content
            );
        }

        // Show evaluations — compact by default, full justification with --verbose
        if proposals.iter().any(|p| !p.evaluations.is_empty()) {
            println!("  Evaluations:");
            if verbose {
                for p in &proposals {
                    for e in &p.evaluations {
                        println!(
                            "    {} \u{2192} {} ({:.2}):",
                            e.evaluator_agent_id, p.author_agent_id, e.evaluation.score
                        );
                        if !e.evaluation.justification.is_empty() {
                            for line in e.evaluation.justification.lines() {
                                println!("      {line}");
                            }
                        }
                    }
                }
            } else {
                for p in &proposals {
                    if p.evaluations.is_empty() {
                        continue;
                    }
                    let evals: Vec<String> = p
                        .evaluations
                        .iter()
                        .map(|e| format!("{}({:.2})", e.evaluator_agent_id, e.evaluation.score))
                        .collect();
                    println!("    {} \u{2190} {}", p.author_agent_id, evals.join(" "));
                }
            }
        }
    }

    if let Some(ref b) = budget {
        println!(
            "\nBudget: {:.1}s elapsed | {:.0}s remaining | {} in + {} out tokens | ${:.4}",
            b.budget.elapsed_secs,
            b.budget.remaining_secs,
            b.budget.total_input_tokens,
            b.budget.total_output_tokens,
            b.budget.estimated_cost_usd,
        );
    }

    // Final result
    if let Some(ref best) = details.final_result {
        println!(
            "\nResult (round {}, score {:.2}, by {}):",
            best.round, best.aggregated_score, best.author_agent_id,
        );
        println!("{}", best.proposal.content);
    } else if let Some(result_text) = &status.result {
        println!("\nResult:");
        println!("{result_text}");
    }

    ExitCode::SUCCESS
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let truncated: String = s.chars().take(max - 3).collect();
        format!("{}...", truncated.replace('\n', " "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 80), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let long = "a".repeat(100);
        let result = truncate(&long, 80);
        assert_eq!(result.chars().count(), 80);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_replaces_newlines() {
        assert_eq!(truncate("line1\nline2", 80), "line1 line2");
    }

    #[test]
    fn truncate_exact_length_no_ellipsis() {
        let s = "a".repeat(80);
        assert_eq!(truncate(&s, 80), s);
    }
}
