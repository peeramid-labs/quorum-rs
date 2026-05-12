use super::{GlobalConfig, GlobalConfigUpdate, MultiAppState};
use crate::orchestrator_registry::AddOrchestratorRequest;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use std::sync::atomic::Ordering;
use tracing::info;

// ---------------------------------------------------------------------------
// Orchestrator management
// ---------------------------------------------------------------------------

/// `GET /api/orchestrators` — list active orchestrator connections.
#[utoipa::path(
    get,
    path = "/api/orchestrators",
    responses(
        (status = 200, description = "List of active orchestrator connections", body = Vec<crate::orchestrator_registry::ActiveOrchestrator>)
    ),
    tag = "Registry"
)]
pub(super) async fn list_orchestrators(State(state): State<MultiAppState>) -> impl IntoResponse {
    match &state.orchestrator_registry {
        Some(registry) => {
            let orchestrators = registry.list().await;
            match serde_json::to_value(&orchestrators) {
                Ok(v) => (StatusCode::OK, Json(v)).into_response(),
                Err(e) => {
                    tracing::error!(error = %e, "Failed to serialize orchestrators list");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        None => (StatusCode::OK, Json(serde_json::json!([]))).into_response(),
    }
}

/// `POST /api/orchestrators` — request adding a new orchestrator at runtime.
///
/// The request is forwarded to the runner via a channel. The actual
/// registration (JWT ceremony) and worker spawning happens asynchronously.
#[utoipa::path(
    post,
    path = "/api/orchestrators",
    request_body = crate::orchestrator_registry::AddOrchestratorRequest,
    responses(
        (status = 202, description = "Orchestrator registration queued"),
        (status = 400, description = "Invalid request"),
        (status = 503, description = "Orchestrator registry not available")
    ),
    tag = "Registry"
)]
pub(super) async fn add_orchestrator(
    State(state): State<MultiAppState>,
    Json(req): Json<AddOrchestratorRequest>,
) -> impl IntoResponse {
    let Some(registry) = &state.orchestrator_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Orchestrator registry not available. Runtime orchestrator management requires multi-agent mode."
            })),
        )
            .into_response();
    };

    if req.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "url is required"})),
        )
            .into_response();
    }

    match registry.request_add(req) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "accepted",
                "message": "Orchestrator registration queued. Workers will be spawned asynchronously."
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Orchestrator budget proxy
// ---------------------------------------------------------------------------

/// Per-orchestrator budget info returned by the budget proxy.
#[derive(serde::Serialize)]
#[cfg_attr(feature = "status-server", derive(utoipa::ToSchema))]
pub struct OrchestratorBudget {
    /// Orchestrator ID.
    pub id: String,
    /// Orchestrator HTTP URL.
    pub url: String,
    /// Budget data from orchestrator (null if fetch failed).
    pub budget: Option<serde_json::Value>,
    /// Error message if fetch failed.
    pub error: Option<String>,
}

/// `GET /api/orchestrators/budgets` — fetch budget from each connected orchestrator.
///
/// For each orchestrator, proxies `GET /api/operators/budget` using the stored
/// bearer token. Returns an array of results (one per orchestrator).
#[utoipa::path(
    get,
    path = "/api/orchestrators/budgets",
    responses(
        (status = 200, description = "Budget info per orchestrator", body = Vec<OrchestratorBudget>)
    ),
    tag = "Registry"
)]
pub(super) async fn get_orchestrator_budgets(
    State(state): State<MultiAppState>,
) -> impl IntoResponse {
    let Some(registry) = &state.orchestrator_registry else {
        return (StatusCode::OK, Json(serde_json::json!([]))).into_response();
    };

    let orchestrators = registry.list().await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    let mut results = Vec::new();
    for orch in &orchestrators {
        let token = registry.get_bearer_token(&orch.id).await;
        let url = format!("{}/api/operators/budget", orch.url.trim_end_matches('/'));

        let result = match token {
            Some(t) => client.get(&url).bearer_auth(&t).send().await,
            None => {
                results.push(OrchestratorBudget {
                    id: orch.id.clone(),
                    url: orch.url.clone(),
                    budget: None,
                    error: Some("No bearer token available".into()),
                });
                continue;
            }
        };

        match result {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                results.push(OrchestratorBudget {
                    id: orch.id.clone(),
                    url: orch.url.clone(),
                    budget: Some(body),
                    error: None,
                });
            }
            Ok(resp) => {
                results.push(OrchestratorBudget {
                    id: orch.id.clone(),
                    url: orch.url.clone(),
                    budget: None,
                    error: Some(format!("HTTP {}", resp.status())),
                });
            }
            Err(e) => {
                results.push(OrchestratorBudget {
                    id: orch.id.clone(),
                    url: orch.url.clone(),
                    budget: None,
                    error: Some(format!("{e}")),
                });
            }
        }
    }

    (StatusCode::OK, Json(serde_json::json!(results))).into_response()
}

// ---------------------------------------------------------------------------
// Orchestrator API proxy
// ---------------------------------------------------------------------------

/// Proxy a request to a connected orchestrator's API.
///
/// `GET /api/orchestrators/:id/proxy/*path` — forwards GET requests.
/// `POST /api/orchestrators/:id/proxy/*path` — forwards POST requests with body.
///
/// Uses the stored bearer token for authentication.
pub(super) async fn proxy_orchestrator_get(
    State(state): State<MultiAppState>,
    axum::extract::Path((orch_id, path)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> impl IntoResponse {
    proxy_orchestrator_request(&state, &orch_id, &path, "GET", None, query).await
}

pub(super) async fn proxy_orchestrator_post(
    State(state): State<MultiAppState>,
    axum::extract::Path((orch_id, path)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    proxy_orchestrator_request(&state, &orch_id, &path, "POST", Some(body), query).await
}

async fn proxy_orchestrator_request(
    state: &MultiAppState,
    orch_id: &str,
    path: &str,
    method: &str,
    body: Option<axum::body::Bytes>,
    query: Option<String>,
) -> axum::response::Response {
    let Some(registry) = &state.orchestrator_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "No orchestrator registry"})),
        )
            .into_response();
    };

    let orchestrators = registry.list().await;
    let Some(orch) = orchestrators.iter().find(|o| o.id == orch_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Orchestrator '{}' not found", orch_id)})),
        )
            .into_response();
    };

    let token = registry.get_bearer_token(orch_id).await;
    let mut url = format!(
        "{}/{}",
        orch.url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    if let Some(q) = &query {
        if !q.is_empty() {
            url = format!("{}?{}", url, q);
        }
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let mut req = match method {
        "POST" => client.post(&url),
        _ => client.get(&url),
    };

    if let Some(t) = &token {
        req = req.bearer_auth(t);
    }
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(b);
    }

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!(null));
            (status, Json(body)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("Proxy error: {e}")})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// SSE stream proxy
// ---------------------------------------------------------------------------

/// Proxy an SSE stream from an orchestrator.
///
/// `GET /api/orchestrators/:id/stream/:job_id` — forwards the SSE stream
/// from the orchestrator's `/job/{job_id}/stream?token=<bearer>`.
pub(super) async fn proxy_orchestrator_sse(
    State(state): State<MultiAppState>,
    axum::extract::Path((orch_id, job_id)): axum::extract::Path<(String, String)>,
) -> impl IntoResponse {
    let Some(registry) = &state.orchestrator_registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "No orchestrator registry"})),
        )
            .into_response();
    };

    let orchestrators = registry.list().await;
    let Some(orch) = orchestrators.iter().find(|o| o.id == orch_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Orchestrator '{}' not found", orch_id)})),
        )
            .into_response();
    };

    let token = registry.get_bearer_token(&orch_id).await;
    // Build URL with proper encoding via reqwest::Url
    let base_str = format!("{}/job/{}/stream", orch.url.trim_end_matches('/'), job_id);
    let mut parsed_url = match reqwest::Url::parse(&base_str) {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(orch_id = %orch_id, error = %e, "Invalid orchestrator URL for SSE");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "Invalid orchestrator URL"})),
            )
                .into_response();
        }
    };
    if let Some(ref t) = token {
        parsed_url.query_pairs_mut().append_pair("token", t);
    }
    let url = parsed_url.to_string();

    // Use connect_timeout only — no total request timeout for SSE streams
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(orch_id = %orch_id, error = %e, "SSE proxy connection failed");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "Failed to connect to orchestrator"})),
            )
                .into_response();
        }
    };

    if !resp.status().is_success() {
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let body = resp.text().await.unwrap_or_default();
        return (status, body).into_response();
    }

    // Stream the response body as SSE
    let stream = resp.bytes_stream();
    let body = axum::body::Body::from_stream(stream);

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap()
        .into_response()
}

// ---------------------------------------------------------------------------
// Global config endpoints
// ---------------------------------------------------------------------------

/// `GET /api/config` — return global configuration (base hold duration).
#[utoipa::path(
    get,
    path = "/api/config",
    responses(
        (status = 200, description = "Current global configuration", body = super::GlobalConfig)
    ),
    tag = "Config"
)]
pub(super) async fn get_global_config(State(state): State<MultiAppState>) -> Json<GlobalConfig> {
    Json(GlobalConfig {
        base_hold_secs: state.base_hold_secs.load(Ordering::Relaxed),
        response_sla_secs: state.response_sla_secs.load(Ordering::Relaxed),
        buffer_floor_pct: state.buffer_floor_pct.load(Ordering::Relaxed),
    })
}

/// `PUT /api/config` — update global configuration.
///
/// When `base_hold_secs` changes, all agent buffers' base hold duration
/// is updated to the new value.
#[utoipa::path(
    put,
    path = "/api/config",
    request_body = super::GlobalConfigUpdate,
    responses(
        (status = 200, description = "Updated global configuration", body = super::GlobalConfig)
    ),
    tag = "Config"
)]
pub(super) async fn update_global_config(
    State(state): State<MultiAppState>,
    Json(req): Json<GlobalConfigUpdate>,
) -> impl IntoResponse {
    if let Some(secs) = req.base_hold_secs {
        state.base_hold_secs.store(secs, Ordering::Relaxed);
        let new_base = std::time::Duration::from_secs(secs);
        // Update all agent buffers
        for (name, buf) in &state.buffers {
            buf.set_hold_duration(new_base);
            info!(
                "Updated buffer hold duration for agent '{}' to {}s",
                name, secs
            );
        }
    }
    if let Some(secs) = req.response_sla_secs {
        state.response_sla_secs.store(secs, Ordering::Relaxed);
        // Propagate SLA to all buffers for deadline-based release
        let sla_duration = std::time::Duration::from_secs(secs);
        for (name, buf) in &state.buffers {
            buf.set_response_sla(sla_duration);
            info!(
                "Updated response SLA for agent '{}' buffer to {}s",
                name, secs
            );
        }
        info!("Updated global response SLA to {}s", secs);
    }
    if let Some(pct) = req.buffer_floor_pct {
        let clamped = pct.min(100);
        state.buffer_floor_pct.store(clamped, Ordering::Relaxed);
        info!("Updated buffer floor to {}%", clamped);
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "base_hold_secs": state.base_hold_secs.load(Ordering::Relaxed),
            "response_sla_secs": state.response_sla_secs.load(Ordering::Relaxed),
            "buffer_floor_pct": state.buffer_floor_pct.load(Ordering::Relaxed)
        })),
    )
}
