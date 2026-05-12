//! Unified HTTP server for multi-agent status monitoring and HITL control plane.
//!
//! Aggregates multiple agents' status, configuration, and chat endpoints
//! behind a single HTTP port. Includes pause/resume, live config patching,
//! and response buffer management for the HITL control plane.
//!
//! Requires the `status-server` feature flag.

pub mod api_docs;
mod chat_handlers;
mod hitl_handlers;
mod registration_handlers;
mod registry_handlers;
mod status_handlers;

use super::SharedAgentStatus;
use crate::agents::{AgentConfig, ChatCapable};
use crate::orchestrator_registry::OrchestratorRegistry;
use crate::workers::buffer::ResponseBuffer;
use axum::{
    Router,
    extract::State,
    http::header,
    response::{Html, IntoResponse, Json},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use tokio::sync::RwLock;
use tracing::{error, info};
use utoipa::ToSchema;

/// Combined application state for the multi-agent status server.
#[derive(Clone)]
pub(crate) struct MultiAppState {
    statuses: HashMap<String, SharedAgentStatus>,
    chat_agents: HashMap<String, Arc<dyn ChatCapable>>,
    configs: HashMap<String, Arc<RwLock<AgentConfig>>>,
    buffers: HashMap<String, Arc<ResponseBuffer>>,
    /// Pause handles for each agent — toggles the worker's AtomicBool directly.
    pause_handles: HashMap<String, Arc<AtomicBool>>,
    /// Optional orchestrator registry for runtime orchestrator management.
    orchestrator_registry: Option<OrchestratorRegistry>,
    /// Global base hold duration in seconds (shared by all agent buffers).
    base_hold_secs: Arc<AtomicU64>,
    /// Global response SLA in seconds — agents with avg_response_ms exceeding
    /// this value are considered out-of-SLA. Defaults to base_hold_secs.
    response_sla_secs: Arc<AtomicU64>,
    /// Buffer floor as percentage of total SLA (0-100). This is the minimum
    /// buffer hold time before divergence amplification kicks in.
    buffer_floor_pct: Arc<AtomicU64>,
    /// Middleware pipeline for `before_release` hook point (edit + release stages).
    /// None = no middleware configured.
    before_release_middleware: Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
}

/// Global configuration visible via the dashboard.
#[derive(Serialize, ToSchema)]
pub(super) struct GlobalConfig {
    base_hold_secs: u64,
    /// Global response SLA in seconds — agents exceeding this are flagged.
    response_sla_secs: u64,
    /// Buffer floor as % of total SLA — minimum hold before divergence boost.
    buffer_floor_pct: u64,
}

/// Request body for updating global configuration.
#[derive(Deserialize, ToSchema)]
pub(super) struct GlobalConfigUpdate {
    base_hold_secs: Option<u64>,
    response_sla_secs: Option<u64>,
    buffer_floor_pct: Option<u64>,
}

/// Unified HTTP server serving all agents on a single port.
pub struct MultiAgentStatusServer;

impl MultiAgentStatusServer {
    /// Start the multi-agent status server on the given port.
    ///
    /// This function runs indefinitely — spawn it in a background task.
    pub async fn run(
        port: u16,
        statuses: HashMap<String, SharedAgentStatus>,
        chat_agents: HashMap<String, Arc<dyn ChatCapable>>,
        configs: HashMap<String, AgentConfig>,
    ) {
        Self::run_with_registry(port, statuses, chat_agents, configs, None).await;
    }

    /// Start the multi-agent status server with an optional orchestrator registry.
    ///
    /// When a registry is provided, `GET /api/orchestrators` and
    /// `POST /api/orchestrators` endpoints are active, allowing runtime
    /// orchestrator management via the dashboard API.
    pub async fn run_with_registry(
        port: u16,
        statuses: HashMap<String, SharedAgentStatus>,
        chat_agents: HashMap<String, Arc<dyn ChatCapable>>,
        configs: HashMap<String, AgentConfig>,
        registry: Option<OrchestratorRegistry>,
    ) {
        let rw_configs = configs
            .into_iter()
            .map(|(k, v)| (k, Arc::new(RwLock::new(v))))
            .collect();
        Self::run_control_plane(
            port,
            statuses,
            chat_agents,
            rw_configs,
            HashMap::new(),
            HashMap::new(),
            registry,
            None, // No middleware in basic mode
        )
        .await;
    }

    /// Start the multi-agent status server with full HITL control plane support.
    ///
    /// This is the primary entry point when response buffers are available.
    /// The `pause_handles` map provides direct access to each worker's pause
    /// `AtomicBool`, enabling pause/resume even when no response buffer is
    /// configured.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_control_plane(
        port: u16,
        statuses: HashMap<String, SharedAgentStatus>,
        chat_agents: HashMap<String, Arc<dyn ChatCapable>>,
        configs: HashMap<String, Arc<RwLock<AgentConfig>>>,
        buffers: HashMap<String, Arc<ResponseBuffer>>,
        pause_handles: HashMap<String, Arc<AtomicBool>>,
        registry: Option<OrchestratorRegistry>,
        middleware: Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
    ) {
        // Compute the maximum base_hold_duration() across all buffers (full precision).
        // Defaults to Duration::ZERO (pass-through) when no buffers are configured.
        let base_hold = buffers
            .values()
            .map(|b| b.base_hold_duration())
            .max()
            .unwrap_or(std::time::Duration::ZERO);
        // Display-only seconds (for GET /api/config); runtime uses full Duration.
        let base_secs = base_hold.as_secs();
        let sla_secs = base_secs;
        let agent_count = configs.len();
        let state = MultiAppState {
            statuses,
            chat_agents,
            configs,
            buffers,
            pause_handles,
            orchestrator_registry: registry,
            base_hold_secs: Arc::new(AtomicU64::new(base_secs)),
            response_sla_secs: Arc::new(AtomicU64::new(sla_secs)),
            buffer_floor_pct: Arc::new(AtomicU64::new(0)), // deprecated — SLA-based release replaces buffer floor
            before_release_middleware: middleware,
        };

        // Propagate initial response SLA to all buffers for deadline-based release.
        // Uses full-precision Duration (not truncated seconds) so sub-second holds
        // are preserved.
        for buf in state.buffers.values() {
            buf.set_response_sla(base_hold);
        }

        let app = build_router(state);

        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        info!(
            "Multi-agent dashboard → http://{}/  ({} agents)  Swagger UI → http://{}/swagger-ui/",
            addr, agent_count, addr
        );

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("Failed to bind multi-agent server on port {}: {}", port, e);
                return;
            }
        };
        if let Err(e) = axum::serve(listener, app).await {
            error!("Multi-agent server error: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// Agent listing
// ---------------------------------------------------------------------------

/// Summary of a single agent for the listing endpoint.
#[derive(Serialize, ToSchema)]
pub(super) struct AgentSummary {
    name: String,
    model_name: String,
    provider_id: String,
    nats_connected: bool,
    current_job: Option<String>,
    current_phase: Option<String>,
    has_chat: bool,
    is_paused: bool,
    buffered_count: u32,
    error_rate: f32,
    mean_score: Option<f32>,
    /// Standard deviation of recent scores — divergence metric.
    score_std_dev: Option<f32>,
    /// Average response time in milliseconds across recent tasks.
    avg_response_ms: Option<u64>,
    /// Whether the agent is flagged for operator attention.
    is_flagged: bool,
    /// Human-readable reason for the flag.
    flag_reason: Option<String>,
    /// Whether auto-approve mode is enabled for this agent.
    auto_approve: bool,
    /// Current auto-approve divergence threshold (0.0-1.0).
    auto_approve_threshold: f32,
}

/// `GET /api/agents` — list all agents with summary status.
#[utoipa::path(
    get,
    path = "/api/agents",
    responses(
        (status = 200, description = "List of all agents with summary status", body = Vec<AgentSummary>)
    ),
    tag = "Agents"
)]
pub(super) async fn list_agents(State(state): State<MultiAppState>) -> Json<Vec<AgentSummary>> {
    let mut agents = Vec::new();
    for (name, config) in &state.configs {
        let config = config.read().await;
        let (
            nats_connected,
            current_job,
            current_phase,
            is_paused,
            buffered_count,
            error_rate,
            mean_score,
            score_std_dev,
            avg_response_ms,
            is_flagged,
            flag_reason,
        ) = if let Some(status) = state.statuses.get(name) {
            let snap = status.read().await;
            let avg_ms = if snap.recent_tasks.is_empty() {
                None
            } else {
                let total: u64 = snap.recent_tasks.iter().map(|t| t.duration_ms).sum();
                Some(total / snap.recent_tasks.len() as u64)
            };
            (
                snap.nats_connected,
                snap.current_job.clone(),
                snap.current_phase.clone(),
                snap.is_paused,
                snap.buffered_count,
                snap.error_rate,
                snap.mean_score,
                snap.score_std_dev,
                avg_ms,
                snap.is_flagged,
                snap.flag_reason.clone(),
            )
        } else {
            (
                false, None, None, false, 0, 0.0, None, None, None, false, None,
            )
        };
        // Read live buffer length (not stale snapshot) — auto-approve
        // can drain entries between snapshot updates.
        let buffered_count = if let Some(buf) = state.buffers.get(name) {
            buf.len().await as u32
        } else {
            buffered_count
        };
        // When the agent has no ResponseBuffer at all, it is effectively
        // in pass-through mode — every response goes straight out. Surface
        // that as `auto_approve=true, threshold=1.0` so the dashboard shows
        // the same pass-through state as a buffered agent running the new
        // defaults. (Prior to the buffer default change, the fallback was
        // `(false, 0.5)` to match the old buffer defaults.)
        let (auto_approve, auto_approve_threshold) = state
            .buffers
            .get(name)
            .map(|b| (b.is_auto_approve(), b.auto_approve_threshold()))
            .unwrap_or((true, 1.0));
        agents.push(AgentSummary {
            name: name.clone(),
            model_name: config.model_name.clone(),
            provider_id: config.provider_id.clone(),
            nats_connected,
            current_job,
            current_phase,
            has_chat: state.chat_agents.contains_key(name),
            is_paused,
            buffered_count,
            error_rate,
            mean_score,
            score_std_dev,
            avg_response_ms,
            is_flagged,
            flag_reason,
            auto_approve,
            auto_approve_threshold,
        });
    }
    // Sort by name for stable ordering
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    Json(agents)
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the multi-agent router (exposed for testing).
fn build_router(state: MultiAppState) -> Router {
    use utoipa::OpenApi;
    let swagger_ui = utoipa_swagger_ui::SwaggerUi::new("/swagger-ui")
        .url("/api-docs/openapi.json", api_docs::ApiDoc::openapi());

    Router::new()
        .merge(swagger_ui)
        .route("/", get(dashboard_page))
        .route(
            "/api/config",
            get(registry_handlers::get_global_config).put(registry_handlers::update_global_config),
        )
        .route("/api/agents", get(list_agents))
        .route(
            "/api/agents/register",
            post(registration_handlers::register_agent),
        )
        .route(
            "/api/agents/bulk",
            post(registration_handlers::bulk_register),
        )
        .route(
            "/api/agents/pause-all",
            put(hitl_handlers::pause_all_agents),
        )
        .route("/api/agents/auto-all", put(hitl_handlers::auto_all_agents))
        .route(
            "/api/agents/{name}/status",
            get(status_handlers::agent_status),
        )
        .route(
            "/api/agents/{name}/config",
            get(status_handlers::agent_config).put(hitl_handlers::agent_config_update),
        )
        .route("/api/agents/{name}/chat", post(chat_handlers::agent_chat))
        .route("/api/agents/{name}/pause", put(hitl_handlers::agent_pause))
        .route(
            "/api/agents/{name}/auto",
            put(hitl_handlers::agent_auto_approve),
        )
        .route(
            "/api/agents/{name}/buffer",
            get(hitl_handlers::agent_buffer_list),
        )
        .route(
            "/api/agents/{name}/buffer/{id}",
            get(hitl_handlers::agent_buffer_detail).put(hitl_handlers::agent_buffer_edit),
        )
        .route(
            "/api/agents/{name}/buffer/{id}/release",
            post(hitl_handlers::agent_buffer_release),
        )
        .route(
            "/api/agents/{name}/buffer/{id}/reject",
            post(hitl_handlers::agent_buffer_reject),
        )
        .route(
            "/api/agents/{name}/buffer/{id}/stop",
            post(hitl_handlers::agent_buffer_stop),
        )
        .route(
            "/api/agents/{name}/buffer/{id}/unstop",
            post(hitl_handlers::agent_buffer_unstop),
        )
        // Agent CRUD (PUT/PATCH/DELETE) — must come after all /api/agents/{name}/* sub-routes
        .route(
            "/api/agents/{id}/manage",
            put(registration_handlers::replace_agent)
                .patch(registration_handlers::patch_agent)
                .delete(registration_handlers::delete_agent),
        )
        .route(
            "/api/orchestrators",
            get(registry_handlers::list_orchestrators).post(registry_handlers::add_orchestrator),
        )
        .route(
            "/api/orchestrators/budgets",
            get(registry_handlers::get_orchestrator_budgets),
        )
        .route(
            "/api/orchestrators/{orch_id}/proxy/{*path}",
            get(registry_handlers::proxy_orchestrator_get)
                .post(registry_handlers::proxy_orchestrator_post),
        )
        .route(
            "/api/orchestrators/{orch_id}/stream/{job_id}",
            get(registry_handlers::proxy_orchestrator_sse),
        )
        .with_state(state)
}

/// `GET /` — multi-agent dashboard HTML.
///
/// Returns `Cache-Control: no-store` so the browser always fetches the latest
/// version after a rebuild (HTML is embedded at compile time via `include_str!`).
#[utoipa::path(
    get,
    path = "/",
    responses(
        (status = 200, description = "Multi-agent dashboard HTML page", content_type = "text/html")
    ),
    tag = "Dashboard"
)]
async fn dashboard_page() -> impl IntoResponse {
    let html = include_str!("../multi_status.html");
    ([(header::CACHE_CONTROL, "no-store")], Html(html))
}

#[cfg(test)]
mod tests;
