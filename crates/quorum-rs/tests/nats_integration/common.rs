use quorum_rs::agents::{AgentConfig, AgentContext, Evaluation, NsedAgent, Proposal, TokenUsage};
use quorum_rs::nats_utils::connect_nats;
use quorum_rs::workers::WorkerConfig;

use anyhow::Result;
use async_nats::jetstream::{self, stream};
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// NATS connection helpers
// ---------------------------------------------------------------------------

/// Returns the NATS URL from the environment, defaulting to localhost.
pub fn nats_url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string())
}

/// Attempts to connect to NATS. Returns `None` if the server is unavailable,
/// allowing tests to gracefully skip in local development.
///
/// When `CI=true` is set, a failed connection panics instead of skipping,
/// ensuring NATS integration tests never silently pass in CI without coverage.
pub async fn try_connect_nats() -> Option<async_nats::Client> {
    let url = nats_url();
    let is_ci = std::env::var("CI").map(|v| v == "true").unwrap_or(false);
    match tokio::time::timeout(std::time::Duration::from_secs(3), connect_nats(&url, None)).await {
        Ok(Ok(client)) => Some(client),
        other => {
            if is_ci {
                panic!(
                    "NATS not available at {} in CI — integration tests require a running NATS server. Error: {:?}",
                    url, other
                );
            }
            eprintln!("NATS not available at {} -- skipping test", url);
            None
        }
    }
}

/// Returns a UUID-based unique identifier for test isolation.
pub fn unique_id() -> String {
    uuid::Uuid::new_v4()
        .to_string()
        .replace('-', "")
        .chars()
        .take(12)
        .collect()
}

// ---------------------------------------------------------------------------
// Mock agents
// ---------------------------------------------------------------------------

/// A mock agent that returns deterministic propose/evaluate results.
#[derive(Debug, Clone)]
pub struct MockAgent {
    pub agent_name: String,
}

#[async_trait]
impl NsedAgent for MockAgent {
    async fn propose(&self, _context: &AgentContext) -> Result<Proposal> {
        Ok(Proposal {
            thought_process: "Mock thought process".to_string(),
            content: format!("Mock proposal from {}", self.agent_name),
            final_scratchpad: None,
            token_usage_stats: Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    async fn evaluate(&self, context: &AgentContext) -> Result<Vec<(String, Evaluation)>> {
        let mut results = Vec::new();
        for candidate in &context.candidates {
            results.push((
                candidate.id.clone(),
                Evaluation {
                    score: 0.85,
                    justification: format!(
                        "Mock evaluation of {} by {}",
                        candidate.id, self.agent_name
                    ),
                    token_usage: Some(TokenUsage {
                        input_tokens: 200,
                        output_tokens: 100,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ));
        }
        // Return at least one evaluation even if no candidates
        if results.is_empty() {
            results.push((
                "default-target".to_string(),
                Evaluation {
                    score: 0.75,
                    justification: format!("Default evaluation by {}", self.agent_name),
                    token_usage: None,
                    ..Default::default()
                },
            ));
        }
        Ok(results)
    }

    fn name(&self) -> String {
        self.agent_name.clone()
    }
}

/// A mock agent that always returns errors from propose/evaluate.
#[derive(Debug, Clone)]
pub struct FailingMockAgent {
    pub agent_name: String,
}

#[async_trait]
impl NsedAgent for FailingMockAgent {
    async fn propose(&self, _context: &AgentContext) -> Result<Proposal> {
        Err(anyhow::anyhow!(
            "FailingMockAgent: intentional propose failure"
        ))
    }

    async fn evaluate(&self, _context: &AgentContext) -> Result<Vec<(String, Evaluation)>> {
        Err(anyhow::anyhow!(
            "FailingMockAgent: intentional evaluate failure"
        ))
    }

    fn name(&self) -> String {
        self.agent_name.clone()
    }
}

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

/// Returns a minimal `AgentConfig` suitable for testing.
pub fn test_agent_config(name: &str) -> AgentConfig {
    AgentConfig {
        name: name.to_string(),
        provider_id: "test-provider".to_string(),
        model_name: "test-model".to_string(),
        temperature: 0.5,
        max_tokens: 1024,
        ..AgentConfig::default()
    }
}

/// Returns a `WorkerConfig` with UUID-scoped subject prefixes to avoid
/// JetStream subject collisions with existing streams or other test runs.
///
/// Subject prefixes:  `nsed_{uid}` and `sphera_{uid}`
/// Stream name:       `test_stream_{uid}`
/// Consumer name:     `test_consumer_{uid}_{safe_agent}`
pub fn test_worker_config(uid: &str, agent_name: &str) -> WorkerConfig {
    let safe = agent_name.replace(|c: char| !c.is_alphanumeric(), "_");
    WorkerConfig::new(
        nats_url(),
        format!("test_stream_{}", uid),
        format!("test_consumer_{}_{}", uid, safe),
    )
    .with_subject_prefix(format!("nsed_{}", uid))
    .with_api_prefix(format!("sphera_{}", uid))
}

/// Returns the subject prefix used by the worker for this test run.
pub fn subject_prefix(uid: &str) -> String {
    format!("nsed_{}", uid)
}

/// Returns the API prefix used by the worker for this test run.
pub fn api_prefix(uid: &str) -> String {
    format!("sphera_{}", uid)
}

// ---------------------------------------------------------------------------
// JetStream helpers
// ---------------------------------------------------------------------------

/// Creates a JetStream stream whose subjects cover both the scoped `nsed_{uid}`
/// protocol subjects and the scoped `sphera_{uid}` API subjects needed by the worker.
pub async fn create_test_stream(
    js: &jetstream::Context,
    stream_name: &str,
    subject_prefix: &str,
    api_prefix: &str,
) -> Result<()> {
    js.create_stream(stream::Config {
        name: stream_name.to_string(),
        subjects: vec![format!("{}.>", subject_prefix), format!("{}.>", api_prefix)],
        storage: stream::StorageType::Memory,
        ..Default::default()
    })
    .await
    .map_err(|e| anyhow::anyhow!("Failed to create test stream '{}': {}", stream_name, e))?;
    Ok(())
}

/// Cleans up all NATS resources (streams, KV buckets) created by a test.
pub async fn cleanup_nats_resources(js: &jetstream::Context, uid: &str, agent_name: &str) {
    let safe = agent_name.replace(|c: char| !c.is_alphanumeric(), "_");

    // Delete the test stream
    let stream_name = format!("test_stream_{}", uid);
    let _ = js.delete_stream(&stream_name).await;

    // Delete KV buckets created by the worker
    let proc_bucket = format!("nsed_proc_{}", safe);
    let mem_bucket = format!("nsed_local_mem_{}", safe);
    let _ = js.delete_key_value(&proc_bucket).await;
    let _ = js.delete_key_value(&mem_bucket).await;
}

/// Cleans up a single KV bucket by name.
pub async fn cleanup_kv_bucket(js: &jetstream::Context, bucket_name: &str) {
    let _ = js.delete_key_value(bucket_name).await;
}

/// Build a minimal `AgentContext` for testing propose tasks.
pub fn test_propose_context(session_id: &str) -> AgentContext {
    AgentContext {
        task_description: "Test deliberation task".to_string(),
        round_number: 1,
        total_rounds: 3,
        phase: quorum_rs::DeliberationPhase::Proposing,
        session_id: Some(session_id.to_string()),
        ..Default::default()
    }
}

/// Build an `AgentContext` for testing evaluate tasks (includes candidates).
pub fn test_evaluate_context(session_id: &str) -> AgentContext {
    use quorum_rs::agents::{CandidateProposal, Proposal};
    AgentContext {
        task_description: "Test deliberation task".to_string(),
        round_number: 1,
        total_rounds: 3,
        phase: quorum_rs::DeliberationPhase::Evaluating,
        session_id: Some(session_id.to_string()),
        candidates: vec![
            CandidateProposal {
                id: "candidate_1".to_string(),
                proposal: Proposal {
                    thought_process: "Some thinking".to_string(),
                    content: "Candidate 1 proposal content".to_string(),
                    final_scratchpad: None,
                    token_usage_stats: None,
                    ..Default::default()
                },
            },
            CandidateProposal {
                id: "candidate_2".to_string(),
                proposal: Proposal {
                    thought_process: "Other thinking".to_string(),
                    content: "Candidate 2 proposal content".to_string(),
                    final_scratchpad: None,
                    token_usage_stats: None,
                    ..Default::default()
                },
            },
        ],
        ..Default::default()
    }
}
