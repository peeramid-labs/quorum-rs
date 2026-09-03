//! Unified HTTP server for multi-agent status monitoring and HITL control plane.
//!
//! Aggregates multiple agents' status, configuration, and chat endpoints
//! behind a single HTTP port. Includes pause/resume, live config patching,
//! and response buffer management for the HITL control plane.
//!
//! Requires the `status-server` feature flag.

pub mod api_docs;
mod chat_handlers;
pub mod content_handlers;
mod hitl_handlers;
mod registration_handlers;
mod registry_handlers;
mod status_handlers;

use super::SharedAgentStatus;
use super::agent_events::AgentEventStore;
use crate::agents::{AgentConfig, ChatCapable};
use crate::orchestrator_registry::OrchestratorRegistry;
use crate::workers::buffer::ResponseBuffer;
use axum::{
    Router,
    extract::{FromRef, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
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
    /// Per-agent NATS event-log read handles, backing the 24h error feed and the
    /// per-agent tasks / tool-calls views. Empty when the process has no NATS
    /// (e.g. tests) — the views then report no history.
    event_stores: HashMap<String, AgentEventStore>,
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
    /// Bearer token guarding the `/api/*` control plane. `None` = auth disabled
    /// (the loopback-default / dev behaviour); `Some` = every `/api/*` request
    /// must carry `Authorization: Bearer <token>`.
    auth_token: Option<Arc<str>>,
    /// File uploads. `None` where the agent was given no bucket to write to,
    /// which is the default — the routes then answer 503 rather than 404, so
    /// "not configured" is distinguishable from "no such file".
    content: Option<content_handlers::ContentUploads>,
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

/// Resolve the dashboard bind address from an env-var-shaped string.
///
/// `Some("0.0.0.0")` → `0.0.0.0` (LAN-visible). `Some("malformed")`
/// silently falls back to loopback so a typo doesn't take the
/// dashboard offline — operators see the loopback bind in the
/// `info!` log and can correct. `None` → loopback.
fn resolve_dashboard_bind(raw: Option<&str>) -> std::net::IpAddr {
    raw.and_then(|s| s.parse().ok())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

/// Resolve the dashboard bearer token from an env-var-shaped string.
///
/// A present-but-empty value is treated as absent so an exported-but-blank
/// `QUORUM_DASHBOARD_TOKEN=` does not silently enable an unmatchable guard.
fn resolve_dashboard_token(raw: Option<String>) -> Option<Arc<str>> {
    raw.filter(|s| !s.is_empty()).map(Arc::from)
}

/// Fail-closed guard: `true` when the server must refuse to start because a
/// non-loopback bind carries no auth token (which would expose the control
/// plane unauthenticated).
fn refuse_unauthenticated_exposure(ip: &std::net::IpAddr, auth_enabled: bool) -> bool {
    !ip.is_loopback() && !auth_enabled
}

/// Constant-time byte comparison so token validation does not leak length or
/// prefix information through response timing.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Cheap, cloneable view of the control-plane guard extracted from
/// [`MultiAppState`] — carries only the token so per-request middleware
/// extraction never clones the state's agent maps.
#[derive(Clone)]
struct DashAuth {
    token: Option<Arc<str>>,
}

impl FromRef<MultiAppState> for DashAuth {
    fn from_ref(state: &MultiAppState) -> Self {
        DashAuth {
            token: state.auth_token.clone(),
        }
    }
}

impl DashAuth {
    /// `true` when the request may proceed: auth disabled (no token configured)
    /// or the `Authorization: Bearer <token>` header matches the configured one.
    fn is_authorized(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = &self.token else {
            return true;
        };
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(|token| ct_eq(token.as_bytes(), expected.as_bytes()))
            .unwrap_or(false)
    }
}

/// Middleware guarding the `/api/*` control plane. Rejects with `401` +
/// `WWW-Authenticate: Bearer` when a token is configured and the request's
/// bearer credential is missing or wrong; passes through when auth is disabled.
async fn require_bearer(State(auth): State<DashAuth>, req: Request, next: Next) -> Response {
    if auth.is_authorized(req.headers()) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized",
        )
            .into_response()
    }
}

/// Reports whether the dashboard requires a token and whether the caller's
/// current credential satisfies it. Public (no auth) so the frontend can decide
/// whether to show the token entry field before the user has connected.
#[derive(Serialize)]
struct AuthStatus {
    auth_required: bool,
    authenticated: bool,
}

async fn auth_status(State(auth): State<DashAuth>, headers: HeaderMap) -> Json<AuthStatus> {
    Json(AuthStatus {
        auth_required: auth.token.is_some(),
        authenticated: auth.is_authorized(&headers),
    })
}

#[cfg(test)]
impl MultiAppState {
    /// A state carrying nothing but the upload configuration, for tests that
    /// exercise the content routes and no agent.
    pub(super) fn for_content(content: Option<content_handlers::ContentUploads>) -> Self {
        Self {
            statuses: HashMap::new(),
            chat_agents: HashMap::new(),
            configs: HashMap::new(),
            buffers: HashMap::new(),
            pause_handles: HashMap::new(),
            event_stores: HashMap::new(),
            orchestrator_registry: None,
            base_hold_secs: Arc::new(AtomicU64::new(0)),
            response_sla_secs: Arc::new(AtomicU64::new(0)),
            buffer_floor_pct: Arc::new(AtomicU64::new(0)),
            before_release_middleware: None,
            auth_token: None,
            content,
        }
    }
}

/// Build the upload configuration from this process's environment.
///
/// Exposed so the runner can construct it while it still holds a worker's NATS
/// connection; the server itself is handed the result.
pub async fn content_uploads_from_env(
    js: &async_nats::jetstream::Context,
    uploaded_by: String,
) -> Option<content_handlers::ContentUploads> {
    content_handlers::ContentUploads::from_env(js, uploaded_by).await
}

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
    /// Unchanged nine-argument form, kept so adding uploads is not a breaking
    /// change for anyone already calling this on a published version.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_control_plane(
        port: u16,
        statuses: HashMap<String, SharedAgentStatus>,
        chat_agents: HashMap<String, Arc<dyn ChatCapable>>,
        configs: HashMap<String, Arc<RwLock<AgentConfig>>>,
        buffers: HashMap<String, Arc<ResponseBuffer>>,
        pause_handles: HashMap<String, Arc<AtomicBool>>,
        event_stores: HashMap<String, AgentEventStore>,
        registry: Option<OrchestratorRegistry>,
        middleware: Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
    ) {
        Self::run_control_plane_with_uploads(
            port,
            statuses,
            chat_agents,
            configs,
            buffers,
            pause_handles,
            event_stores,
            registry,
            middleware,
            None,
        )
        .await
    }

    /// As [`Self::run_control_plane`], plus the file-upload routes.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_control_plane_with_uploads(
        port: u16,
        statuses: HashMap<String, SharedAgentStatus>,
        chat_agents: HashMap<String, Arc<dyn ChatCapable>>,
        configs: HashMap<String, Arc<RwLock<AgentConfig>>>,
        buffers: HashMap<String, Arc<ResponseBuffer>>,
        pause_handles: HashMap<String, Arc<AtomicBool>>,
        event_stores: HashMap<String, AgentEventStore>,
        registry: Option<OrchestratorRegistry>,
        middleware: Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
        content: Option<content_handlers::ContentUploads>,
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
            event_stores,
            orchestrator_registry: registry,
            base_hold_secs: Arc::new(AtomicU64::new(base_secs)),
            response_sla_secs: Arc::new(AtomicU64::new(sla_secs)),
            buffer_floor_pct: Arc::new(AtomicU64::new(0)), // deprecated — SLA-based release replaces buffer floor
            before_release_middleware: middleware,
            auth_token: resolve_dashboard_token(std::env::var("QUORUM_DASHBOARD_TOKEN").ok()),
            content,
        };

        // Propagate initial response SLA to all buffers for deadline-based release.
        // Uses full-precision Duration (not truncated seconds) so sub-second holds
        // are preserved.
        for buf in state.buffers.values() {
            buf.set_response_sla(base_hold);
        }

        let auth_enabled = state.auth_token.is_some();
        let app = build_router(state);

        // Bind address resolution: `QUORUM_DASHBOARD_BIND` env var
        // (any address `IpAddr::from_str` accepts — `0.0.0.0` for
        // LAN-visible, `::` for dual-stack, specific iface IP for
        // pinned binding) falls back to `127.0.0.1` for the
        // historical loopback-only behaviour. CLI surface in
        // `quorum serve --dashboard-bind` sets the env var before
        // dispatching into the runner.
        let ip = resolve_dashboard_bind(std::env::var("QUORUM_DASHBOARD_BIND").ok().as_deref());
        let addr = SocketAddr::from((ip, port));
        // Fail-closed: a non-loopback bind with no token would expose the whole
        // control plane (status, chat-capture, buffer inspection, live config,
        // pause/auto-approve) to the network unauthenticated. Refuse to start
        // rather than silently opening it — a missing QUORUM_DASHBOARD_TOKEN
        // must never become an open door. Loopback stays open (local dev);
        // non-loopback requires a token.
        if refuse_unauthenticated_exposure(&ip, auth_enabled) {
            error!(
                bind = %ip,
                "refusing to start the dashboard: bound to a non-loopback address with no \
                 QUORUM_DASHBOARD_TOKEN — that would expose the control plane unauthenticated. \
                 Set QUORUM_DASHBOARD_TOKEN, or bind to loopback (QUORUM_DASHBOARD_BIND)."
            );
            return;
        }
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

/// Per-agent operator diagnostics — metrics + latest errors, pullable via API so
/// operators can catch agent-side problems from the dashboard without going
/// through the orchestrator.
#[derive(Serialize, ToSchema)]
pub(super) struct AgentDiagnostics {
    name: String,
    model_name: String,
    uptime_secs: u64,
    tasks_completed: u64,
    tasks_failed: u64,
    error_rate: f32,
    /// Whether the agent is paused (e.g. auto-paused on a 402/billing error) —
    /// it pulls no new tasks while paused.
    is_paused: bool,
    /// Whether the agent is flagged for operator attention (e.g. score
    /// divergence from peers).
    is_flagged: bool,
    /// Why it's flagged, if flagged.
    flag_reason: Option<String>,
    /// Most recent `agent_error` events (newest first), with their detail.
    #[schema(value_type = Vec<crate::status::EventLogEntry>)]
    recent_errors: Vec<crate::status::EventLogEntry>,
    /// Most recent tasks that ended in `"error"` (newest first).
    #[schema(value_type = Vec<crate::status::TaskLogEntry>)]
    recent_failed_tasks: Vec<crate::status::TaskLogEntry>,
}

/// Build diagnostics from an agent's status snapshot. Pure — testable without
/// the dashboard app state. Surfaces the reliability metrics plus the latest
/// error events and failed tasks an operator needs to catch an agent-side
/// problem (e.g. a model 404-ing every round).
fn diagnostics_from_snapshot(
    name: &str,
    snap: &crate::status::AgentStatusSnapshot,
) -> AgentDiagnostics {
    const MAX: usize = 20;
    let recent_errors: Vec<_> = snap
        .event_log
        .iter()
        .rev()
        .filter(|e| e.event_type == "agent_error")
        .take(MAX)
        .cloned()
        .collect();
    let recent_failed_tasks: Vec<_> = snap
        .recent_tasks
        .iter()
        .rev()
        .filter(|t| t.status == "error")
        .take(MAX)
        .cloned()
        .collect();
    AgentDiagnostics {
        name: name.to_string(),
        model_name: snap.model_name.clone(),
        uptime_secs: snap.uptime_secs,
        tasks_completed: snap.tasks_completed,
        tasks_failed: snap.tasks_failed,
        error_rate: snap.error_rate,
        is_paused: snap.is_paused,
        is_flagged: snap.is_flagged,
        flag_reason: snap.flag_reason.clone(),
        recent_errors,
        recent_failed_tasks,
    }
}

/// `GET /api/agents/{name}/diagnostics` — metrics + latest errors for one agent.
#[utoipa::path(
    get,
    path = "/api/agents/{name}/diagnostics",
    params(("name" = String, Path, description = "Agent name")),
    responses(
        (status = 200, description = "Agent metrics + latest errors", body = AgentDiagnostics),
        (status = 404, description = "Unknown agent")
    ),
    tag = "Agents"
)]
pub(super) async fn agent_diagnostics(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
) -> Result<Json<AgentDiagnostics>, StatusCode> {
    let status = state.statuses.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    let snap = status.read().await;
    Ok(Json(diagnostics_from_snapshot(&name, &snap)))
}

use super::agent_events::{AgentEvent, TasksView, ToolCallsView};

const ERROR_WINDOW_HOURS: i64 = 24;

/// One `agent_error` event, tagged with the agent it came from.
#[derive(Serialize, ToSchema)]
pub(super) struct AgentErrorEntry {
    agent: String,
    /// Model the agent runs — lets an operator spot a failing provider/model.
    model_name: String,
    /// RFC3339 timestamp of the error.
    timestamp: String,
    /// Job / session ID the error occurred under, if any.
    job_id: Option<String>,
    /// Human-readable detail, e.g. `"evaluate failed: API request failed with status 404"`.
    detail: String,
}

/// Fleet-wide API error feed over a rolling time window.
#[derive(Serialize, ToSchema)]
pub(super) struct AgentErrorsReport {
    /// Rolling window, in hours.
    window_hours: i64,
    /// Per-agent hard cap on retained events. The 24h window is bounded by this:
    /// an agent emitting more than this many events inside the window loses its
    /// oldest events to eviction.
    stream_cap: i64,
    /// Number of errors in `errors`.
    total: usize,
    /// Errors across all agents, newest first.
    errors: Vec<AgentErrorEntry>,
}

/// One agent's raw error events together with the model it runs.
struct AgentErrorSource {
    agent: String,
    model_name: String,
    events: Vec<AgentEvent>,
}

/// Flatten per-agent error events into one newest-first feed. Pure — testable
/// without NATS. Input events are assumed already error-kind and in-window
/// (as produced by the store read + [`super::agent_events::collect_errors`]).
fn flatten_error_feed(sources: &[AgentErrorSource]) -> Vec<AgentErrorEntry> {
    let mut dated: Vec<(chrono::DateTime<chrono::Utc>, AgentErrorEntry)> = sources
        .iter()
        .flat_map(|source| {
            source.events.iter().filter_map(move |event| {
                let at = event.parsed_time()?;
                Some((
                    at,
                    AgentErrorEntry {
                        agent: source.agent.clone(),
                        model_name: source.model_name.clone(),
                        timestamp: event.timestamp.clone(),
                        job_id: event.job_id.clone(),
                        detail: event.detail.clone(),
                    },
                ))
            })
        })
        .collect();
    dated.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    dated.into_iter().map(|(_, entry)| entry).collect()
}

/// Model name the agent runs, from its live config (empty if unknown).
async fn model_name_for(state: &MultiAppState, agent: &str) -> String {
    match state.configs.get(agent) {
        Some(config) => config.read().await.model_name.clone(),
        None => String::new(),
    }
}

/// `GET /api/agents/errors` — fleet-wide API errors over the last 24h.
///
/// Reads each agent's NATS-persisted event log (24h retention) and aggregates
/// the `agent_error` events into one operator view, so infra can be watched at a
/// glance without pulling each agent's diagnostics individually.
#[utoipa::path(
    get,
    path = "/api/agents/errors",
    responses(
        (status = 200, description = "Fleet-wide API errors over the last 24h", body = AgentErrorsReport)
    ),
    tag = "Agents"
)]
pub(super) async fn agents_errors(State(state): State<MultiAppState>) -> Json<AgentErrorsReport> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(ERROR_WINDOW_HOURS);
    let mut sources = Vec::with_capacity(state.event_stores.len());
    for (agent, store) in &state.event_stores {
        let events = match store.read_since(cutoff).await {
            Ok(events) => super::agent_events::collect_errors(&events),
            Err(e) => {
                error!(agent = %agent, error = %e, "failed to read agent error log");
                continue;
            }
        };
        sources.push(AgentErrorSource {
            agent: agent.clone(),
            model_name: model_name_for(&state, agent).await,
            events,
        });
    }
    let errors = flatten_error_feed(&sources);
    Json(AgentErrorsReport {
        window_hours: ERROR_WINDOW_HOURS,
        stream_cap: super::agent_events::STREAM_MAX_MESSAGES,
        total: errors.len(),
        errors,
    })
}

/// Read one agent's events from the last 24h, or `None` if it has no event log.
async fn read_agent_window(
    state: &MultiAppState,
    name: &str,
) -> Option<Result<Vec<AgentEvent>, StatusCode>> {
    let store = state.event_stores.get(name)?;
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(ERROR_WINDOW_HOURS);
    Some(match store.read_since(cutoff).await {
        Ok(events) => Ok(events),
        Err(e) => {
            error!(agent = %name, error = %e, "failed to read agent event log");
            Err(StatusCode::BAD_GATEWAY)
        }
    })
}

/// `GET /api/agents/{name}/tasks` — the agent's in-flight and finished
/// tasks/queries over the last 24h, from its NATS event log.
#[utoipa::path(
    get,
    path = "/api/agents/{name}/tasks",
    params(("name" = String, Path, description = "Agent name")),
    responses(
        (status = 200, description = "In-flight and finished tasks", body = TasksView),
        (status = 404, description = "Unknown agent")
    ),
    tag = "Agents"
)]
pub(super) async fn agent_tasks(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
) -> Result<Json<TasksView>, StatusCode> {
    if !state.configs.contains_key(&name) && !state.statuses.contains_key(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let events = match read_agent_window(&state, &name).await {
        Some(result) => result?,
        None => return Ok(Json(TasksView::default())),
    };
    Ok(Json(super::agent_events::reconcile_tasks(&events)))
}

/// `GET /api/agents/{name}/tool-calls` — the agent's pending and finished tool
/// invocations over the last 24h, from its NATS event log.
#[utoipa::path(
    get,
    path = "/api/agents/{name}/tool-calls",
    params(("name" = String, Path, description = "Agent name")),
    responses(
        (status = 200, description = "Pending and finished tool calls", body = ToolCallsView),
        (status = 404, description = "Unknown agent")
    ),
    tag = "Agents"
)]
pub(super) async fn agent_tool_calls(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
) -> Result<Json<ToolCallsView>, StatusCode> {
    if !state.configs.contains_key(&name) && !state.statuses.contains_key(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let events = match read_agent_window(&state, &name).await {
        Some(result) => result?,
        None => return Ok(Json(ToolCallsView::default())),
    };
    Ok(Json(super::agent_events::reconcile_tool_calls(&events)))
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the multi-agent router (exposed for testing).
///
/// The `/api/*` control plane is guarded by [`require_bearer`]; the guard is a
/// no-op unless `state.auth_token` is set. The dashboard page, Swagger UI and
/// `/auth/status` stay public so the frontend can load and probe auth first.
fn build_router(state: MultiAppState) -> Router {
    use utoipa::OpenApi;
    let swagger_ui = utoipa_swagger_ui::SwaggerUi::new("/swagger-ui")
        .url("/api-docs/openapi.json", api_docs::ApiDoc::openapi());

    let protected = Router::new()
        .route(
            "/api/config",
            get(registry_handlers::get_global_config).put(registry_handlers::update_global_config),
        )
        .route(
            "/api/content",
            post(content_handlers::upload)
                // The upload's own ceiling is enforced while the body streams;
                // axum's default 2 MiB limit would reject a video long before
                // that and with a less useful answer.
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/api/content/usage", get(content_handlers::usage))
        .route(
            "/api/content/{digest}/status",
            get(content_handlers::status),
        )
        .route(
            "/api/content/{digest}",
            get(content_handlers::fetch).delete(content_handlers::remove),
        )
        .route("/api/agents", get(list_agents))
        .route("/api/agents/errors", get(agents_errors))
        .route("/api/agents/{name}/diagnostics", get(agent_diagnostics))
        .route("/api/agents/{name}/tasks", get(agent_tasks))
        .route("/api/agents/{name}/tool-calls", get(agent_tool_calls))
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
        .route_layer(middleware::from_fn_with_state(
            DashAuth::from_ref(&state),
            require_bearer,
        ));

    Router::new()
        .merge(swagger_ui)
        .route("/", get(dashboard_page))
        .route("/auth/status", get(auth_status))
        .merge(protected)
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
