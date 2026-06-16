use std::collections::HashSet;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use thiserror::Error;

use serde::Serialize;

use crate::cli::request::DeliberationRequest;
use crate::cli::workspace::PolicyConfig;

/// Minimal projection of a remote policy entry returned by
/// `GET /policies` — just what discovery needs (id + display name +
/// tags). Avoids pulling the full TUI-only `PolicyInfo` shape.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscoveredPolicy {
    pub policy_id: String,
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Minimal projection of a room from `GET /rooms` (grant-filtered
/// server-side). Reads the discovery fields and ignores the rest.
#[derive(Debug, Clone, Deserialize)]
pub struct DiscoveredRoom {
    pub id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub eligible_agent_count: usize,
}

// ── Response structs for status / trace commands ──────────────────────

/// Health-check response from `GET /health`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthResponse {
    pub status: String,
    #[serde(default)]
    pub nats_connection: String,
    #[serde(default)]
    pub timestamp: String,
}

/// Agent info from `GET /agents`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentInfo {
    pub agent_id: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub is_online: bool,
    pub estimated_cost_per_round: Option<f64>,
    #[serde(default)]
    pub capability_tags: Vec<String>,
    /// Principal who registered the agent.
    #[serde(default)]
    pub operator: Option<String>,
    /// Display name of the operator.
    #[serde(default)]
    pub operator_display_name: Option<String>,
    /// Short description of the agent's specialization.
    #[serde(default)]
    pub description: Option<String>,
    /// Current job the agent is working on.
    #[serde(default)]
    pub current_job: Option<String>,
    /// USD per million input tokens.
    #[serde(default)]
    pub input_price_per_mtok: Option<f64>,
    /// USD per million output tokens.
    #[serde(default)]
    pub output_price_per_mtok: Option<f64>,
    /// Uptime in seconds.
    #[serde(default)]
    pub uptime_secs: Option<u64>,
    /// Last seen ISO 8601 timestamp.
    #[serde(default)]
    pub last_seen: Option<String>,
}

/// Job result from `GET /deliberation/{id}/result`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobResult {
    pub job_id: String,
    pub status: String,
    pub result: Option<String>,
}

/// Full deliberation trace from `GET /deliberation/{id}/details`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobDetails {
    pub job_id: String,
    #[serde(default)]
    pub history: Vec<TraceRecord>,
    pub final_result: Option<TraceRecord>,
}

/// One proposal with its evaluations (within a round).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraceRecord {
    pub round: u32,
    pub author_agent_id: String,
    pub proposal: TraceProposal,
    #[serde(default)]
    pub evaluations: Vec<TraceEvaluation>,
    #[serde(default)]
    pub aggregated_score: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraceProposal {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub thought_process: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraceEvaluation {
    pub evaluator_agent_id: String,
    pub evaluation: TraceEvalDetail,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraceEvalDetail {
    #[serde(default)]
    pub score: f32,
    #[serde(default)]
    pub justification: String,
}

/// Budget status from `GET /deliberation/{id}/budget`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BudgetResponse {
    pub job_id: String,
    pub budget: BudgetInfo,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BudgetInfo {
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub elapsed_secs: f64,
    #[serde(default)]
    pub remaining_secs: f64,
    #[serde(default)]
    pub rounds_completed: u32,
    #[serde(default)]
    pub total_rounds: u32,
    #[serde(default)]
    pub total_input_tokens: u32,
    #[serde(default)]
    pub total_output_tokens: u32,
    #[serde(default)]
    pub estimated_cost_usd: f64,
}

/// Result of pushing a policy to a remote orchestrator.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PushPolicyResult {
    pub policy_id: String,
    pub name: String,
    #[serde(default)]
    pub created: bool,
}

/// Build the JSON body for `POST /policies`, converting CLI's `PolicyConfig`
/// to the orchestrator's format (strips CLI-only fields like `context` on roles).
fn build_policy_push_body(name: &str, config: &PolicyConfig) -> serde_json::Value {
    use crate::cli::workspace::PolicyMode;

    // Minimal role struct matching orchestrator's PolicyRole
    #[derive(Serialize)]
    struct OrchestratorRole {
        role: String,
        count: u8,
        capabilities: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pinned_agents: Option<Vec<String>>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        moderator: bool,
    }

    let roles = config.roles.as_ref().map(|rs| {
        rs.iter()
            .map(|r| OrchestratorRole {
                role: r.role.clone(),
                count: r.count,
                capabilities: r.capabilities.clone(),
                pinned_agents: r.pinned_agents.clone(),
                moderator: r.moderator,
            })
            .collect::<Vec<_>>()
    });

    let mut policy = serde_json::json!({
        "max_rounds": config.max_rounds,
        "effort": config.effort,
    });

    if let Some(agents) = &config.agents {
        policy["agents"] = serde_json::json!(agents);
    }
    if let Some(roles) = roles {
        policy["roles"] = serde_json::to_value(roles).unwrap();
    }
    if let Some(sla) = &config.sla {
        policy["sla"] = serde_json::to_value(sla).unwrap();
    }
    if let Some(caps) = &config.capabilities {
        policy["capabilities"] = serde_json::json!(caps);
    }
    if let Some(tags) = &config.tags {
        policy["tags"] = serde_json::json!(tags);
    }
    // Include mode only for non-Deliberation so existing policy hashes are preserved.
    if config.mode != PolicyMode::Deliberation
        && let Ok(mode_val) = serde_json::to_value(config.mode)
    {
        policy["mode"] = mode_val;
    }

    serde_json::json!({
        "name": name,
        "config": policy,
    })
}

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("server returned {status}: {body}")]
    ApiError { status: u16, body: String },

    #[error("SSE handshake timed out (30s)")]
    HandshakeTimeout,

    #[error("SSE stream ended without job_complete event")]
    IncompleteStream,

    #[error("failed to parse SSE event: {0}")]
    ParseError(String),
}

#[derive(Debug, Deserialize)]
struct SubmitResponse {
    job_id: String,
}

/// Response from `POST /deliberation/{job_id}/inject`.
#[derive(Debug, Clone, Deserialize)]
pub struct InjectResponse {
    pub sequence: usize,
    pub injected_at_round: u32,
}

/// Payload of the `job_complete` SSE event.
#[derive(Debug, Clone, Deserialize)]
pub struct JobCompletePayload {
    pub status: String,
    pub job_id: String,
    pub rounds_completed: u32,
    pub best_proposal_content: String,
    pub best_proposal_score: f32,
    pub best_proposal_author: String,
}

/// Outcome of streaming a deliberation.
#[derive(Debug)]
pub enum JobOutcome {
    Success(JobCompletePayload),
    Failed(String),
}

/// HTTP client for a remote NSED orchestrator.
#[derive(Clone)]
pub struct RemoteOrchestrator {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl RemoteOrchestrator {
    /// Build from a workspace orchestrator config entry, resolving env tokens.
    pub fn from_config(
        name: &str,
        orch: &crate::cli::workspace::OrchestratorConfig,
    ) -> Result<Self, String> {
        use crate::cli::workspace::OrchestratorMode;

        if orch.mode.as_ref() != Some(&OrchestratorMode::Remote) {
            return Err(format!(
                "orchestrator '{name}' is not a remote orchestrator"
            ));
        }

        let address = match &orch.address {
            Some(a) => {
                let resolved = crate::config::resolve_env_token("address", a);
                if resolved.trim().is_empty() {
                    return Err(format!(
                        "orchestrator '{name}' has empty address after env expansion of '{a}'"
                    ));
                }
                resolved
            }
            None => return Err(format!("orchestrator '{name}' is missing address")),
        };

        let token = match &orch.token {
            Some(t) => {
                let resolved = crate::config::resolve_env_token("token", t);
                if resolved.trim().is_empty() {
                    return Err(format!(
                        "orchestrator '{name}' has empty token after env expansion of '{t}'"
                    ));
                }
                resolved
            }
            None => return Err(format!("orchestrator '{name}' is missing token")),
        };

        Self::new(&address, &token)
            .map_err(|e| format!("failed to create client for '{name}': {e}"))
    }

    pub fn new(address: &str, token: &str) -> Result<Self, RemoteError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()?;

        let base_url = address.trim_end_matches('/').to_string();

        Ok(Self {
            client,
            base_url,
            token: token.to_string(),
        })
    }

    /// Resolve the orchestrator's agent-facing NATS URL via
    /// `GET /api/runtime/nats`.
    ///
    /// Returns the same value `CredentialManager` would hand out at
    /// mint time on `/credentials/register` or `/redeem`. The SDK
    /// uses this on `quorum serve` startup to learn the URL once
    /// without re-running the registration round-trip.
    ///
    /// Bearer-authenticated. 503 on orchestrators where credential
    /// issuance is disabled (operator must pass `--nats-url`).
    pub async fn runtime_nats(&self) -> Result<String, RemoteError> {
        #[derive(serde::Deserialize)]
        struct RuntimeNats {
            nats_url: String,
        }
        let url = format!("{}/api/runtime/nats", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }
        let parsed: RuntimeNats = resp
            .json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("runtime/nats: {e}")))?;
        Ok(parsed.nats_url)
    }

    /// Health check — `GET /health` (public, no auth).
    pub async fn health(&self) -> Result<HealthResponse, RemoteError> {
        let url = format!("{}/health", self.base_url);
        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("health: {e}")))
    }

    /// List registered agents — `GET /agents`.
    pub async fn agents(&self) -> Result<Vec<AgentInfo>, RemoteError> {
        let url = format!("{}/agents", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("agents: {e}")))
    }

    /// Job status — `GET /deliberation/{id}/result`.
    pub async fn result(&self, job_id: &str) -> Result<JobResult, RemoteError> {
        let url = format!("{}/deliberation/{}/result", self.base_url, job_id);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("result: {e}")))
    }

    /// Full deliberation trace — `GET /deliberation/{id}/details`.
    pub async fn details(&self, job_id: &str) -> Result<JobDetails, RemoteError> {
        let url = format!("{}/deliberation/{}/details", self.base_url, job_id);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("details: {e}")))
    }

    /// Budget status — `GET /deliberation/{id}/budget`.
    pub async fn budget(&self, job_id: &str) -> Result<BudgetResponse, RemoteError> {
        let url = format!("{}/deliberation/{}/budget", self.base_url, job_id);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("budget: {e}")))
    }

    /// Register a policy on the remote orchestrator — `POST /policies`.
    /// Returns the policy_id on success.
    pub async fn push_policy(
        &self,
        name: &str,
        config: &PolicyConfig,
    ) -> Result<PushPolicyResult, RemoteError> {
        let url = format!("{}/policies", self.base_url);

        // Build orchestrator-compatible body: strip CLI-only fields (context on roles).
        let body = build_policy_push_body(name, config);

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: PushPolicyResult = resp.json().await.map_err(|e| {
            RemoteError::ParseError(format!("failed to parse policy push response: {e}"))
        })?;

        Ok(parsed)
    }

    /// Submit a deliberation job. Returns the job ID on success.
    pub async fn submit(&self, req: &DeliberationRequest) -> Result<String, RemoteError> {
        let url = format!("{}/deliberation", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(req)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: SubmitResponse = resp.json().await.map_err(|e| {
            RemoteError::ParseError(format!("failed to parse submit response: {e}"))
        })?;

        Ok(parsed.job_id)
    }

    /// Inject a message into a running deliberation — `POST /deliberation/{job_id}/inject`.
    pub async fn inject_message(
        &self,
        job_id: &str,
        message: &str,
        priority: Option<&str>,
    ) -> Result<InjectResponse, RemoteError> {
        let url = format!("{}/deliberation/{}/inject", self.base_url, job_id);
        let mut body = serde_json::json!({ "message": message });
        if let Some(p) = priority {
            body["priority"] = serde_json::Value::String(p.to_string());
        }
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("inject: {e}")))
    }

    /// Get the base URL of this orchestrator.
    pub fn address(&self) -> &str {
        &self.base_url
    }

    /// Discover the policies the authenticated caller can dispatch
    /// into. Returns a minimal projection (id + name + tags) — the
    /// full `PolicyInfo` lives behind `feature = "tui"` and pulls
    /// in shapes the wizard / non-TUI code doesn't need.
    ///
    /// Server-side filtering: `/policies` already calls
    /// `filter_policies_by_tenancy`, so the response only includes
    /// policies whose tags glob-match the caller's grants (admins
    /// see everything).
    pub async fn discover_policies(&self) -> Result<Vec<DiscoveredPolicy>, RemoteError> {
        let url = format!("{}/policies", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }
        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("policies: {e}")))
    }

    /// List rooms the caller can use — `GET /rooms`. Grant-filtered
    /// server-side (admins see all; others see public rooms plus rooms
    /// whose tags glob-match their grants).
    pub async fn discover_rooms(&self) -> Result<Vec<DiscoveredRoom>, RemoteError> {
        let url = format!("{}/rooms", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }
        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("rooms: {e}")))
    }

    /// List policies — `GET /policies` with optional tag filter.
    #[cfg(feature = "tui")]
    pub async fn policies(
        &self,
        tag: Option<&str>,
    ) -> Result<Vec<crate::cli::tui::event::PolicyInfo>, RemoteError> {
        let url = format!("{}/policies", self.base_url);
        let mut request = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(10));
        if let Some(tag) = tag {
            request = request.query(&[("tag", tag)]);
        }
        let resp = request.send().await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        resp.json()
            .await
            .map_err(|e| RemoteError::ParseError(format!("policies: {e}")))
    }

    /// Open an SSE event stream for a job, returning parsed events via channel.
    #[cfg(feature = "tui")]
    pub async fn open_sse_stream(
        &self,
        job_id: &str,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<crate::cli::tui::event::SseEvent>, RemoteError>
    {
        use crate::cli::tui::event::{EvaluationEntry, ProposalScore, SseEvent};

        let url = format!("{}/job/{}/stream", self.base_url, job_id);

        let handshake = self
            .client
            .get(&url)
            .query(&[("token", &self.token)])
            .send();
        let resp = tokio::time::timeout(Duration::from_secs(30), handshake)
            .await
            .map_err(|_| RemoteError::HandshakeTimeout)??;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut stream = resp.bytes_stream();

        tokio::spawn(async move {
            let mut buffer = Vec::<u8>::new();
            while let Some(Ok(bytes)) = stream.next().await {
                buffer.extend_from_slice(&bytes);

                while let Some(pos) = buffer.windows(2).position(|w| w == b"\n\n") {
                    let frame_bytes = buffer[..pos].to_vec();
                    buffer = buffer[pos + 2..].to_vec();
                    let Ok(frame) = String::from_utf8(frame_bytes) else {
                        continue;
                    };

                    let mut event_type = String::new();
                    let mut data = String::new();
                    for line in frame.lines() {
                        if let Some(val) = line.strip_prefix("event: ") {
                            event_type = val.trim().to_string();
                        } else if let Some(val) = line.strip_prefix("data: ") {
                            if !data.is_empty() {
                                data.push('\n');
                            }
                            data.push_str(val);
                        }
                    }
                    if event_type.is_empty() && data.is_empty() {
                        continue;
                    }

                    let sse_event = match event_type.as_str() {
                        "connected" => SseEvent::Connected,
                        "round_start" => {
                            if let Ok(ev) = serde_json::from_str::<RoundStartData>(&data) {
                                SseEvent::RoundStart {
                                    round: ev.round,
                                    total_rounds: ev.total_rounds,
                                }
                            } else {
                                continue;
                            }
                        }
                        "agent_working" => {
                            if let Ok(ev) = serde_json::from_str::<AgentWorkingData>(&data) {
                                SseEvent::AgentWorking {
                                    agent_id: ev.agent_id,
                                    action: ev.action,
                                }
                            } else {
                                continue;
                            }
                        }
                        "proposal_submitted" => {
                            if let Ok(ev) = serde_json::from_str::<SseProposalData>(&data) {
                                SseEvent::ProposalSubmitted {
                                    round: ev.round,
                                    agent_id: ev.agent_id,
                                    content: ev.content,
                                    thought_process: ev.thought_process,
                                }
                            } else {
                                continue;
                            }
                        }
                        "evaluation_submitted" => {
                            if let Ok(ev) = serde_json::from_str::<SseEvaluationData>(&data) {
                                SseEvent::EvaluationSubmitted {
                                    round: ev.round,
                                    evaluator_id: ev.evaluator_id,
                                    evaluations: ev
                                        .evaluations
                                        .into_iter()
                                        .map(|e| EvaluationEntry {
                                            target_id: e.target_id,
                                            score: e.score,
                                            justification: e.justification,
                                        })
                                        .collect(),
                                }
                            } else {
                                continue;
                            }
                        }
                        "budget_phase_complete" => {
                            if let Ok(ev) = serde_json::from_str::<BudgetPhaseCompleteData>(&data) {
                                SseEvent::BudgetPhaseComplete {
                                    round: ev.round,
                                    phase: ev.phase,
                                    budgeted_secs: ev.budgeted_secs,
                                    actual_secs: ev.actual_secs,
                                    under_budget: ev.under_budget,
                                }
                            } else {
                                continue;
                            }
                        }
                        "round_summary" => {
                            if let Ok(ev) = serde_json::from_str::<SseRoundSummaryData>(&data) {
                                SseEvent::RoundSummary {
                                    round: ev.round,
                                    convergence_score: ev.convergence_score,
                                    proposal_scores: ev
                                        .proposal_scores
                                        .into_iter()
                                        .map(|s| ProposalScore {
                                            agent_id: s.agent_id,
                                            aggregated_score: s.aggregated_score,
                                        })
                                        .collect(),
                                }
                            } else {
                                continue;
                            }
                        }
                        "round_complete" => {
                            if let Ok(ev) = serde_json::from_str::<RoundCompleteData>(&data) {
                                SseEvent::RoundComplete {
                                    round: ev.round,
                                    total_rounds: ev.total_rounds,
                                }
                            } else {
                                continue;
                            }
                        }
                        "job_complete" => {
                            if let Ok(ev) = serde_json::from_str::<JobCompletePayload>(&data) {
                                SseEvent::JobComplete {
                                    status: ev.status,
                                    job_id: ev.job_id,
                                    rounds_completed: ev.rounds_completed,
                                    best_proposal_content: ev.best_proposal_content,
                                    best_proposal_score: ev.best_proposal_score,
                                    best_proposal_author: ev.best_proposal_author,
                                }
                            } else {
                                continue;
                            }
                        }
                        "timeout" => SseEvent::Timeout(data),
                        "keepalive" => continue, // server heartbeat — skip
                        other => SseEvent::Unknown {
                            event_type: other.to_string(),
                        },
                    };

                    if tx.send(sse_event).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(rx)
    }

    /// Stream SSE events for a job, rendering progress to stderr.
    /// Returns `JobOutcome` when the stream completes.
    pub async fn stream_events(&self, job_id: &str) -> Result<JobOutcome, RemoteError> {
        let url = format!("{}/job/{}/stream", self.base_url, job_id);

        let handshake = self
            .client
            .get(&url)
            .query(&[("token", &self.token)])
            .send();
        let resp = tokio::time::timeout(Duration::from_secs(30), handshake)
            .await
            .map_err(|_| RemoteError::HandshakeTimeout)??;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RemoteError::ApiError {
                status: status.as_u16(),
                body,
            });
        }

        let mut stream = resp.bytes_stream();
        let mut buffer = Vec::<u8>::new();
        let mut tracker = PhaseTracker::default();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            buffer.extend_from_slice(&bytes);

            while let Some(pos) = buffer.windows(2).position(|w| w == b"\n\n") {
                let frame_bytes = buffer[..pos].to_vec();
                buffer = buffer[pos + 2..].to_vec();
                let frame = String::from_utf8(frame_bytes).map_err(|e| {
                    RemoteError::ParseError(format!("invalid UTF-8 in SSE frame: {e}"))
                })?;

                let mut event_type = String::new();
                let mut data = String::new();

                for line in frame.lines() {
                    if let Some(val) = line.strip_prefix("event: ") {
                        event_type = val.trim().to_string();
                    } else if let Some(val) = line.strip_prefix("data: ") {
                        data = val.to_string();
                    } else if line.starts_with(':') {
                        // SSE comment / keep-alive, ignore
                    }
                }

                if event_type.is_empty() && data.is_empty() {
                    continue;
                }

                match event_type.as_str() {
                    "connected" => {
                        eprintln!("Connected to orchestrator");
                    }
                    "round_start" => {
                        if let Ok(ev) = serde_json::from_str::<RoundStartData>(&data) {
                            tracker.reset();
                            eprintln!(
                                "[{}/{}] Round {} started",
                                ev.round, ev.total_rounds, ev.round
                            );
                        }
                    }
                    "agent_working" => {
                        if let Ok(ev) = serde_json::from_str::<AgentWorkingData>(&data) {
                            let is_new = match ev.action.as_str() {
                                "propose" => tracker.proposing.insert(ev.agent_id.clone()),
                                "evaluate" => tracker.evaluating.insert(ev.agent_id.clone()),
                                _ => true,
                            };
                            // Deduplicate: only print the first dispatch per agent per phase
                            if is_new {
                                let verb = match ev.action.as_str() {
                                    "propose" => "proposing",
                                    "evaluate" => "evaluating",
                                    _ => &ev.action,
                                };
                                eprintln!("  -> {} {}...", ev.agent_id, verb);
                            }
                        }
                    }
                    "proposal_submitted" => {
                        if let Ok(ev) = serde_json::from_str::<ProposalData>(&data) {
                            tracker.proposed.insert(ev.agent_id.clone());
                            eprintln!("  <- {} proposed", ev.agent_id);
                        }
                    }
                    "evaluation_submitted" => {
                        if let Ok(ev) = serde_json::from_str::<EvaluationData>(&data) {
                            tracker.evaluated.insert(ev.evaluator_id.clone());
                            eprintln!("  <- {} evaluated", ev.evaluator_id);
                        }
                    }
                    "budget_phase_complete" => {
                        if let Ok(ev) = serde_json::from_str::<BudgetPhaseCompleteData>(&data) {
                            let remaining = ev.budgeted_secs - ev.actual_secs;
                            let status = if ev.under_budget {
                                format!("{:.0}s remaining", remaining)
                            } else {
                                format!("{:.0}s over budget!", -remaining)
                            };
                            eprintln!(
                                "  {} phase: {:.1}s / {:.1}s budget ({})",
                                ev.phase, ev.actual_secs, ev.budgeted_secs, status
                            );

                            let stragglers = tracker.stragglers(&ev.phase);
                            if !stragglers.is_empty() {
                                eprintln!("  !! timed out: {}", stragglers.join(", "));
                            }
                        }
                    }
                    "round_summary" => {
                        if let Ok(ev) = serde_json::from_str::<RoundSummaryData>(&data) {
                            let best = ev.proposal_scores.iter().max_by(|a, b| {
                                a.aggregated_score
                                    .partial_cmp(&b.aggregated_score)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                            if let Some(top) = best {
                                eprintln!(
                                    "  Convergence: {:.2} | Best: {} ({:.2})",
                                    ev.convergence_score, top.agent_id, top.aggregated_score
                                );
                            }
                        }
                    }
                    "round_complete" => {
                        if let Ok(ev) = serde_json::from_str::<RoundCompleteData>(&data) {
                            eprintln!(
                                "[{}/{}] Round {} complete",
                                ev.round, ev.total_rounds, ev.round
                            );
                        }
                    }
                    "timeout" => {
                        eprintln!("  !! Stream timeout: {data}");
                    }
                    "keepalive" => {} // server heartbeat — ignore
                    "job_complete" => {
                        let payload: JobCompletePayload = serde_json::from_str(&data)
                            .map_err(|e| RemoteError::ParseError(format!("job_complete: {e}")))?;

                        if payload.status == "Success" {
                            return Ok(JobOutcome::Success(payload));
                        } else {
                            return Ok(JobOutcome::Failed(payload.status));
                        }
                    }
                    _ => {
                        // Forward-compatible: ignore unknown event types
                    }
                }
            }
        }

        Err(RemoteError::IncompleteStream)
    }
}

// ── SSE data structs for TUI (richer parsing) ─────────────────────────

#[cfg(feature = "tui")]
#[derive(Deserialize)]
struct SseProposalData {
    round: u32,
    agent_id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    thought_process: String,
}

#[cfg(feature = "tui")]
#[derive(Deserialize)]
struct SseEvaluationData {
    round: u32,
    evaluator_id: String,
    #[serde(default)]
    evaluations: Vec<SseEvalEntry>,
}

#[cfg(feature = "tui")]
#[derive(Deserialize)]
struct SseEvalEntry {
    target_id: String,
    score: f32,
    #[serde(default)]
    justification: String,
}

#[cfg(feature = "tui")]
#[derive(Deserialize)]
struct SseRoundSummaryData {
    round: u32,
    #[serde(default)]
    convergence_score: f32,
    #[serde(default)]
    proposal_scores: Vec<ScoreEntry>,
}

// ── Internal SSE event data structs (Deserialize only what we need for rendering) ──

#[derive(Deserialize)]
struct RoundStartData {
    round: u32,
    total_rounds: u32,
}

#[derive(Deserialize)]
struct ProposalData {
    agent_id: String,
}

#[derive(Deserialize)]
struct EvaluationData {
    evaluator_id: String,
}

#[derive(Deserialize)]
struct RoundSummaryData {
    #[serde(default)]
    convergence_score: f32,
    proposal_scores: Vec<ScoreEntry>,
}

#[derive(Deserialize)]
struct ScoreEntry {
    agent_id: String,
    aggregated_score: f32,
}

#[derive(Deserialize)]
struct RoundCompleteData {
    round: u32,
    total_rounds: u32,
}

#[derive(Deserialize)]
struct BudgetPhaseCompleteData {
    #[serde(default)]
    round: Option<u32>,
    phase: String,
    budgeted_secs: f64,
    actual_secs: f64,
    under_budget: bool,
}

#[derive(Deserialize)]
struct AgentWorkingData {
    agent_id: String,
    action: String,
}

/// Tracks which agents were dispatched vs delivered in the current phase.
#[derive(Default)]
struct PhaseTracker {
    /// Agents dispatched for "propose"
    proposing: HashSet<String>,
    /// Agents that delivered a proposal
    proposed: HashSet<String>,
    /// Agents dispatched for "evaluate"
    evaluating: HashSet<String>,
    /// Agents that delivered evaluations
    evaluated: HashSet<String>,
}

impl PhaseTracker {
    fn reset(&mut self) {
        self.proposing.clear();
        self.proposed.clear();
        self.evaluating.clear();
        self.evaluated.clear();
    }

    /// Returns agents that were dispatched but did not deliver for the given phase.
    fn stragglers(&self, phase: &str) -> Vec<String> {
        let (dispatched, delivered) = match phase {
            "propose" => (&self.proposing, &self.proposed),
            "evaluate" => (&self.evaluating, &self.evaluated),
            _ => return vec![],
        };
        let mut missing: Vec<String> = dispatched.difference(delivered).cloned().collect();
        missing.sort();
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn phase_tracker_identifies_stragglers() {
        let mut tracker = PhaseTracker::default();
        tracker.proposing.insert("alpha".into());
        tracker.proposing.insert("beta".into());
        tracker.proposing.insert("gamma".into());
        tracker.proposed.insert("alpha".into());
        // beta and gamma did not deliver
        let mut stragglers = tracker.stragglers("propose");
        stragglers.sort();
        assert_eq!(stragglers, vec!["beta", "gamma"]);

        // No stragglers when all delivered
        tracker.proposed.insert("beta".into());
        tracker.proposed.insert("gamma".into());
        assert!(tracker.stragglers("propose").is_empty());
    }

    #[test]
    fn phase_tracker_reset_clears_all() {
        let mut tracker = PhaseTracker::default();
        tracker.proposing.insert("a".into());
        tracker.proposed.insert("a".into());
        tracker.evaluating.insert("a".into());
        tracker.evaluated.insert("a".into());
        tracker.reset();
        assert!(tracker.proposing.is_empty());
        assert!(tracker.proposed.is_empty());
        assert!(tracker.evaluating.is_empty());
        assert!(tracker.evaluated.is_empty());
    }

    #[tokio::test]
    async fn submit_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/deliberation"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(202).set_body_json(serde_json::json!({"job_id": "job-123"})),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "test-token").unwrap();
        let req = DeliberationRequest {
            room_id: "room".into(),
            user_query: "test".into(),
            deliberation_rounds: 3,
            agent_names: Some(vec!["a".into(), "b".into()]),
            policy_id: None,
            effort: None,
            scope: None,
            timeout_seconds: None,
        };

        let job_id = client.submit(&req).await.unwrap();
        assert_eq!(job_id, "job-123");
    }

    #[tokio::test]
    async fn submit_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/deliberation"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"unauthorized"}"#))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "bad-token").unwrap();
        let req = DeliberationRequest {
            room_id: "room".into(),
            user_query: "test".into(),
            deliberation_rounds: 3,
            agent_names: Some(vec!["a".into(), "b".into()]),
            policy_id: None,
            effort: None,
            scope: None,
            timeout_seconds: None,
        };

        let err = client.submit(&req).await.unwrap_err();
        match err {
            RemoteError::ApiError { status, .. } => assert_eq!(status, 401),
            other => panic!("expected ApiError, got: {other}"),
        }
    }

    #[tokio::test]
    async fn submit_conflict() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/deliberation"))
            .respond_with(
                ResponseTemplate::new(409).set_body_string(r#"{"error":"job already running"}"#),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "token").unwrap();
        let req = DeliberationRequest {
            room_id: "room".into(),
            user_query: "test".into(),
            deliberation_rounds: 1,
            agent_names: Some(vec!["a".into(), "b".into()]),
            policy_id: None,
            effort: None,
            scope: None,
            timeout_seconds: None,
        };

        let err = client.submit(&req).await.unwrap_err();
        match err {
            RemoteError::ApiError { status, .. } => assert_eq!(status, 409),
            other => panic!("expected ApiError 409, got: {other}"),
        }
    }

    #[tokio::test]
    async fn submit_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/deliberation"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(r#"{"error":"invalid input"}"#),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "token").unwrap();
        let req = DeliberationRequest {
            room_id: "room".into(),
            user_query: "test".into(),
            deliberation_rounds: 1,
            agent_names: Some(vec!["a".into(), "b".into()]),
            policy_id: None,
            effort: None,
            scope: None,
            timeout_seconds: None,
        };

        let err = client.submit(&req).await.unwrap_err();
        match err {
            RemoteError::ApiError { status, .. } => assert_eq!(status, 400),
            other => panic!("expected ApiError 400, got: {other}"),
        }
    }

    fn sse_body(events: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (event_type, data) in events {
            body.push_str(&format!("event: {event_type}\ndata: {data}\n\n"));
        }
        body
    }

    #[tokio::test]
    async fn stream_job_complete() {
        let server = MockServer::start().await;
        let body = sse_body(&[(
            "job_complete",
            r#"{"status":"Success","job_id":"j1","rounds_completed":2,"best_proposal_content":"answer","best_proposal_score":0.95,"best_proposal_author":"agent-a"}"#,
        )]);

        Mock::given(method("GET"))
            .and(path("/job/j1/stream"))
            .and(query_param("token", "tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let outcome = client.stream_events("j1").await.unwrap();

        match outcome {
            JobOutcome::Success(payload) => {
                assert_eq!(payload.best_proposal_content, "answer");
                assert_eq!(payload.best_proposal_author, "agent-a");
                assert_eq!(payload.rounds_completed, 2);
            }
            JobOutcome::Failed(s) => panic!("expected success, got failed: {s}"),
        }
    }

    #[tokio::test]
    async fn stream_full_lifecycle() {
        let server = MockServer::start().await;
        let body = sse_body(&[
            ("connected", r#""ok""#),
            (
                "round_start",
                r#"{"round":1,"total_rounds":2,"is_resuming":false}"#,
            ),
            (
                "proposal_submitted",
                r#"{"round":1,"agent_id":"alpha","content":"p","thought_process":"t"}"#,
            ),
            (
                "proposal_submitted",
                r#"{"round":1,"agent_id":"beta","content":"q","thought_process":"t"}"#,
            ),
            (
                "evaluation_submitted",
                r#"{"round":1,"evaluator_id":"alpha","evaluations":[]}"#,
            ),
            (
                "round_summary",
                r#"{"round":1,"convergence_score":0.72,"proposal_scores":[{"agent_id":"alpha","aggregated_score":0.89},{"agent_id":"beta","aggregated_score":0.71}]}"#,
            ),
            (
                "round_complete",
                r#"{"round":1,"total_rounds":2,"proposals_count":2}"#,
            ),
            (
                "job_complete",
                r#"{"status":"Success","job_id":"j2","rounds_completed":1,"best_proposal_content":"final answer","best_proposal_score":0.89,"best_proposal_author":"alpha"}"#,
            ),
        ]);

        Mock::given(method("GET"))
            .and(path("/job/j2/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let outcome = client.stream_events("j2").await.unwrap();

        match outcome {
            JobOutcome::Success(p) => assert_eq!(p.best_proposal_content, "final answer"),
            other => panic!("expected success, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_unknown_event_ignored() {
        let server = MockServer::start().await;
        let body = sse_body(&[
            ("future_event_v2", r#"{"some":"data"}"#),
            (
                "job_complete",
                r#"{"status":"Success","job_id":"j3","rounds_completed":1,"best_proposal_content":"ok","best_proposal_score":0.9,"best_proposal_author":"a"}"#,
            ),
        ]);

        Mock::given(method("GET"))
            .and(path("/job/j3/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let outcome = client.stream_events("j3").await.unwrap();
        assert!(matches!(outcome, JobOutcome::Success(_)));
    }

    #[tokio::test]
    async fn stream_incomplete_returns_error() {
        let server = MockServer::start().await;
        let body = sse_body(&[
            ("connected", r#""ok""#),
            (
                "round_start",
                r#"{"round":1,"total_rounds":3,"is_resuming":false}"#,
            ),
        ]);

        Mock::given(method("GET"))
            .and(path("/job/j4/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let err = client.stream_events("j4").await.unwrap_err();
        assert!(matches!(err, RemoteError::IncompleteStream));
    }

    #[tokio::test]
    async fn stream_budget_and_timeout_events() {
        let server = MockServer::start().await;
        let body = sse_body(&[
            ("connected", r#""ok""#),
            (
                "round_start",
                r#"{"round":1,"total_rounds":2,"is_resuming":false}"#,
            ),
            (
                "agent_working",
                r#"{"round":1,"agent_id":"alpha","action":"propose"}"#,
            ),
            (
                "agent_working",
                r#"{"round":1,"agent_id":"beta","action":"propose"}"#,
            ),
            (
                "proposal_submitted",
                r#"{"round":1,"agent_id":"alpha","content":"p","thought_process":"t"}"#,
            ),
            // beta did NOT submit a proposal — it timed out
            (
                "budget_phase_complete",
                r#"{"round":1,"phase":"propose","budgeted_secs":30.0,"actual_secs":30.0,"under_budget":false}"#,
            ),
            (
                "job_complete",
                r#"{"status":"Success","job_id":"j6","rounds_completed":1,"best_proposal_content":"ok","best_proposal_score":0.8,"best_proposal_author":"alpha"}"#,
            ),
        ]);

        Mock::given(method("GET"))
            .and(path("/job/j6/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let outcome = client.stream_events("j6").await.unwrap();
        assert!(matches!(outcome, JobOutcome::Success(_)));
    }

    #[tokio::test]
    async fn stream_timeout_event_then_incomplete() {
        let server = MockServer::start().await;
        let body = sse_body(&[
            ("connected", r#""ok""#),
            (
                "timeout",
                r#""No events received — stream closed due to inactivity""#,
            ),
        ]);

        Mock::given(method("GET"))
            .and(path("/job/j7/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let err = client.stream_events("j7").await.unwrap_err();
        assert!(matches!(err, RemoteError::IncompleteStream));
    }

    // ── health / agents / result / details / budget tests ──────────

    #[tokio::test]
    async fn health_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "healthy",
                "nats_connection": "Connected",
                "timestamp": "2025-01-01T00:00:00Z"
            })))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let resp = client.health().await.unwrap();
        assert_eq!(resp.status, "healthy");
        assert_eq!(resp.nats_connection, "Connected");
    }

    #[tokio::test]
    async fn health_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let err = client.health().await.unwrap_err();
        match err {
            RemoteError::ApiError { status, .. } => assert_eq!(status, 500),
            other => panic!("expected ApiError, got: {other}"),
        }
    }

    #[tokio::test]
    async fn agents_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agents"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "agent_id": "alpha",
                    "model_name": "gpt-4o",
                    "provider_id": "openai",
                    "is_online": true,
                    "estimated_cost_per_round": 0.12,
                    "capability_tags": ["lang:rust"],
                    "operator": "acme-corp",
                    "operator_display_name": "Acme Corp"
                },
                {
                    "agent_id": "beta",
                    "model_name": "claude-3.5",
                    "provider_id": "anthropic",
                    "is_online": false,
                    "capability_tags": []
                }
            ])))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let agents = client.agents().await.unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].agent_id, "alpha");
        assert!(agents[0].is_online);
        assert_eq!(agents[0].operator.as_deref(), Some("acme-corp"));
        assert_eq!(
            agents[0].operator_display_name.as_deref(),
            Some("Acme Corp")
        );
        assert!(!agents[1].is_online);
        assert_eq!(agents[1].estimated_cost_per_round, None);
        assert_eq!(agents[1].operator, None);
    }

    #[tokio::test]
    async fn agents_empty_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/agents"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let agents = client.agents().await.unwrap();
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn inject_message_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/deliberation/job-123/inject"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
                "sequence": 1,
                "injected_at_round": 2
            })))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let resp = client
            .inject_message("job-123", "Please focus on security", Some("urgent"))
            .await
            .unwrap();
        assert_eq!(resp.sequence, 1);
        assert_eq!(resp.injected_at_round, 2);
    }

    #[tokio::test]
    async fn inject_message_job_not_running() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/deliberation/job-456/inject"))
            .respond_with(ResponseTemplate::new(409).set_body_string("job not in running state"))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let err = client
            .inject_message("job-456", "test", None)
            .await
            .unwrap_err();
        match err {
            RemoteError::ApiError { status, .. } => assert_eq!(status, 409),
            other => panic!("expected ApiError, got: {other}"),
        }
    }

    #[tokio::test]
    async fn result_completed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/deliberation/j1/result"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "job_id": "j1",
                "status": "completed",
                "result": "The best proposal content"
            })))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let r = client.result("j1").await.unwrap();
        assert_eq!(r.status, "completed");
        assert_eq!(r.result.as_deref(), Some("The best proposal content"));
    }

    #[tokio::test]
    async fn result_pending() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/deliberation/j2/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "job_id": "j2",
                "status": "pending",
                "result": null
            })))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let r = client.result("j2").await.unwrap();
        assert_eq!(r.status, "pending");
        assert!(r.result.is_none());
    }

    #[tokio::test]
    async fn result_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/deliberation/missing/result"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let err = client.result("missing").await.unwrap_err();
        match err {
            RemoteError::ApiError { status, .. } => assert_eq!(status, 404),
            other => panic!("expected 404, got: {other}"),
        }
    }

    #[tokio::test]
    async fn details_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/deliberation/j1/details"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "job_id": "j1",
                "history": [
                    {
                        "round": 1,
                        "author_agent_id": "alpha",
                        "proposal": { "content": "My proposal", "thought_process": "thinking" },
                        "evaluations": [
                            {
                                "evaluator_agent_id": "beta",
                                "evaluation": { "score": 0.85, "justification": "good work" }
                            }
                        ],
                        "aggregated_score": 0.85
                    }
                ],
                "final_result": {
                    "round": 1,
                    "author_agent_id": "alpha",
                    "proposal": { "content": "My proposal", "thought_process": "thinking" },
                    "evaluations": [],
                    "aggregated_score": 0.85
                }
            })))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let d = client.details("j1").await.unwrap();
        assert_eq!(d.history.len(), 1);
        assert_eq!(d.history[0].author_agent_id, "alpha");
        assert_eq!(d.history[0].evaluations[0].evaluation.score, 0.85);
        assert!(d.final_result.is_some());
    }

    #[tokio::test]
    async fn details_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/deliberation/missing/details"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let err = client.details("missing").await.unwrap_err();
        match err {
            RemoteError::ApiError { status, .. } => assert_eq!(status, 404),
            other => panic!("expected 404, got: {other}"),
        }
    }

    #[tokio::test]
    async fn budget_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/deliberation/j1/budget"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "job_id": "j1",
                "budget": {
                    "scope": "standard",
                    "elapsed_secs": 25.6,
                    "remaining_secs": 1534.4,
                    "rounds_completed": 1,
                    "total_rounds": 3,
                    "total_input_tokens": 1200,
                    "total_output_tokens": 800,
                    "estimated_cost_usd": 0.003
                }
            })))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let b = client.budget("j1").await.unwrap();
        assert_eq!(b.budget.scope, "standard");
        assert_eq!(b.budget.rounds_completed, 1);
        assert_eq!(b.budget.total_input_tokens, 1200);
    }

    #[tokio::test]
    async fn stream_failed_job() {
        let server = MockServer::start().await;
        let body = sse_body(&[(
            "job_complete",
            r#"{"status":"Failed: timeout","job_id":"j5","rounds_completed":0,"best_proposal_content":"","best_proposal_score":0.0,"best_proposal_author":""}"#,
        )]);

        Mock::given(method("GET"))
            .and(path("/job/j5/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let outcome = client.stream_events("j5").await.unwrap();
        match outcome {
            JobOutcome::Failed(s) => assert!(s.contains("timeout")),
            other => panic!("expected Failed, got: {other:?}"),
        }
    }

    // ── push_policy tests ────────────────────────────────────────────

    fn sample_cli_policy() -> crate::cli::workspace::PolicyConfig {
        crate::cli::workspace::PolicyConfig {
            agents: Some(vec!["alice".into(), "bob".into()]),
            roles: None,
            max_rounds: 2,
            effort: 0.7,
            sla: None,
            capabilities: None,
            tags: Some(vec!["test".into()]),
            mode: Default::default(),
        }
    }

    #[tokio::test]
    async fn push_policy_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/policies"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "policy_id": "abc123",
                "name": "test-policy",
                "created": true
            })))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let result = client
            .push_policy("test-policy", &sample_cli_policy())
            .await
            .unwrap();
        assert_eq!(result.policy_id, "abc123");
        assert_eq!(result.name, "test-policy");
        assert!(result.created);
    }

    #[tokio::test]
    async fn push_policy_idempotent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/policies"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "policy_id": "abc123",
                "name": "test-policy",
                "created": false
            })))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let result = client
            .push_policy("test-policy", &sample_cli_policy())
            .await
            .unwrap();
        assert!(!result.created);
    }

    #[tokio::test]
    async fn push_policy_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/policies"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"unauthorized"}"#))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "bad").unwrap();
        let err = client
            .push_policy("p", &sample_cli_policy())
            .await
            .unwrap_err();
        match err {
            RemoteError::ApiError { status, .. } => assert_eq!(status, 401),
            other => panic!("expected ApiError 401, got: {other}"),
        }
    }

    /// Contract test: `GET /policies` returns the orchestrator's full
    /// `PolicyInfo` shape (policy_id/name/tags + max_rounds/effort/
    /// is_role_based/sla/…). `discover_policies` must pull the three
    /// discovery fields and ignore the rest. Pins the cross-crate wire
    /// contract that `--policy <label>` resolution relies on.
    #[tokio::test]
    async fn discover_policies_parses_full_policy_info_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/policies"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "policy_id": "abc123",
                    "name": "noosphera:0v1",
                    "tags": ["noosphera:0v1"],
                    "max_rounds": 3,
                    "effort": 0.6,
                    "is_role_based": true,
                    "sla": { "job_timeout_secs": 1200 }
                },
                {
                    "policy_id": "def456",
                    "name": "nsed:mid:0v1",
                    "tags": ["mid:0v1"],
                    "max_rounds": 6,
                    "effort": 0.6,
                    "is_role_based": true
                }
            ])))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let policies = client.discover_policies().await.unwrap();
        assert_eq!(policies.len(), 2);
        let noosphera_policy = policies
            .iter()
            .find(|p| p.name == "noosphera:0v1")
            .expect("noosphera policy present");
        assert_eq!(noosphera_policy.policy_id, "abc123");
        assert_eq!(noosphera_policy.tags, vec!["noosphera:0v1".to_string()]);
    }

    /// Contract test: `GET /rooms` returns the orchestrator's full
    /// `RoomInfo` shape (id/tags/visibility/eligible_agent_count +
    /// eligible_agent_ids). `discover_rooms` pulls the discovery fields
    /// and ignores the rest.
    #[tokio::test]
    async fn discover_rooms_parses_full_room_info_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rooms"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "id": "noosphera",
                    "tags": ["noosphera:0v1"],
                    "visibility": "private",
                    "eligible_agent_count": 3,
                    "eligible_agent_ids": ["NoospheraEpic", "CortexB"]
                },
                {
                    "id": "public-room",
                    "tags": [],
                    "visibility": "public",
                    "eligible_agent_count": 0
                }
            ])))
            .mount(&server)
            .await;

        let client = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        let rooms = client.discover_rooms().await.unwrap();
        assert_eq!(rooms.len(), 2);
        let noosphera_room = rooms
            .iter()
            .find(|r| r.id == "noosphera")
            .expect("noosphera room present");
        assert_eq!(noosphera_room.tags, vec!["noosphera:0v1".to_string()]);
        assert_eq!(noosphera_room.visibility, "private");
        assert_eq!(noosphera_room.eligible_agent_count, 3);
    }

    #[test]
    fn build_policy_push_body_strips_context() {
        use crate::cli::workspace::RoleConfig;
        let policy = crate::cli::workspace::PolicyConfig {
            agents: None,
            roles: Some(vec![RoleConfig {
                role: "reviewer".into(),
                count: 2,
                capabilities: vec!["lang:rust".into()],
                context: Some(vec![crate::cli::workspace::ContextRef {
                    name: "docs".into(),
                    path: "docs/".into(),
                }]),
                pinned_agents: None,
                moderator: false,
            }]),
            max_rounds: 3,
            effort: 0.85,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        };

        let body = build_policy_push_body("test", &policy);
        let config = &body["config"];
        let role = &config["roles"][0];
        // context should NOT be present
        assert!(role.get("context").is_none(), "context should be stripped");
        assert_eq!(role["role"], "reviewer");
        assert_eq!(role["count"], 2);
    }
}
