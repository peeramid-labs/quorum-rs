use super::MultiAppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};

// ---------------------------------------------------------------------------
// Per-agent status & config
// ---------------------------------------------------------------------------

/// `GET /api/agents/{name}/status` — per-agent status.
#[utoipa::path(
    get,
    path = "/api/agents/{name}/status",
    params(("name" = String, Path, description = "Agent name")),
    responses(
        (status = 200, description = "Agent status snapshot", body = crate::status::AgentStatusSnapshot),
        (status = 404, description = "Agent not found")
    ),
    tag = "Status"
)]
pub(super) async fn agent_status(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.statuses.get(&name) {
        Some(status) => {
            let snap = status.read().await;
            match serde_json::to_value(&*snap) {
                Ok(v) => (StatusCode::OK, Json(v)).into_response(),
                Err(e) => {
                    tracing::error!(agent = %name, error = %e, "Failed to serialize agent status");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent '{}' not found", name)})),
        )
            .into_response(),
    }
}

/// `GET /api/agents/{name}/config` — per-agent configuration.
///
/// Serializes the full `AgentConfig` directly. The `orchestrators` field is
/// excluded automatically via `#[serde(skip_serializing)]` on the struct.
#[utoipa::path(
    get,
    path = "/api/agents/{name}/config",
    params(("name" = String, Path, description = "Agent name")),
    responses(
        (status = 200, description = "Agent configuration", body = crate::agents::AgentConfig),
        (status = 404, description = "Agent not found")
    ),
    tag = "Status"
)]
pub(super) async fn agent_config(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.configs.get(&name) {
        Some(config) => {
            let config = config.read().await;
            (StatusCode::OK, Json(config.clone())).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent '{}' not found", name)})),
        )
            .into_response(),
    }
}
