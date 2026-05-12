//! Agent registration REST handlers — add, remove, update, and bulk-register agents.
//!
//! These endpoints validate requests and update in-memory configuration.
//! Full worker lifecycle (spawn/stop/persist) requires AgentManager integration.

use super::MultiAppState;
use crate::agents::AgentConfig;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

/// Request body for registering a new agent.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterAgentRequest {
    /// Unique agent name (alphanumeric + underscore, max 64 chars).
    pub name: String,
    /// Provider ID — must reference a configured provider.
    pub provider_id: String,
    /// Model name (optional for stub provider).
    #[serde(default)]
    pub model_name: Option<String>,
    /// Agent persona / system prompt.
    #[serde(default)]
    pub persona: Option<String>,
    /// Response SLA in seconds.
    #[serde(default)]
    pub response_sla_secs: Option<u64>,
    /// Capability tags for directory filtering.
    #[serde(default)]
    pub capability_tags: Vec<String>,
    /// Agent description.
    #[serde(default)]
    pub description: Option<String>,
    /// Signing schemes supported.
    #[serde(default)]
    pub signing_schemes: Vec<String>,
}

/// Response after registering an agent.
#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterAgentResponse {
    pub name: String,
    pub status: String,
}

/// Request for bulk agent registration.
#[derive(Debug, Deserialize, ToSchema)]
pub struct BulkRegisterRequest {
    pub agents: Vec<RegisterAgentRequest>,
}

/// Response for bulk registration.
#[derive(Debug, Serialize, ToSchema)]
pub struct BulkRegisterResponse {
    pub registered: Vec<String>,
    pub failed: u32,
    pub errors: Vec<String>,
}

/// Query params for force-delete.
#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub force: bool,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err("Agent name must be 1-64 characters".to_string());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("Agent name must be alphanumeric + underscore only".to_string());
    }
    Ok(())
}

fn request_to_config(req: &RegisterAgentRequest) -> Result<AgentConfig, String> {
    let model_name = match &req.model_name {
        Some(m) => m.clone(),
        None if req.provider_id.contains("stub") => "stub".to_string(),
        None => {
            return Err(format!(
                "model_name is required for provider '{}'",
                req.provider_id
            ));
        }
    };
    Ok(AgentConfig {
        name: req.name.clone(),
        provider_id: req.provider_id.clone(),
        model_name,
        persona: req.persona.clone(),
        response_sla_secs: req.response_sla_secs.unwrap_or(300),
        capability_tags: req.capability_tags.clone(),
        description: req.description.clone(),
        signing_schemes: req.signing_schemes.clone(),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/agents/register` — Register a new agent.
#[utoipa::path(
    post,
    path = "/api/agents/register",
    tag = "Agent Management",
    summary = "Register new agent",
    description = "Add a new agent to the config and start it. Returns 409 if the agent already exists.",
    request_body = RegisterAgentRequest,
    responses(
        (status = 202, description = "Agent registration accepted (pending hot-reload)", body = RegisterAgentResponse),
        (status = 400, description = "Invalid agent name or config"),
        (status = 409, description = "Agent already exists"),
    )
)]
pub(super) async fn register_agent(
    State(state): State<MultiAppState>,
    Json(req): Json<RegisterAgentRequest>,
) -> impl IntoResponse {
    if let Err(msg) = validate_agent_name(&req.name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response();
    }

    // Check for duplicate
    if state.configs.contains_key(&req.name) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("Agent '{}' already exists", req.name)})),
        )
            .into_response();
    }

    // Validate the config can be built (catches bad field combos early)
    if let Err(msg) = request_to_config(&req) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response();
    }

    // AgentManager is not yet wired into MultiAppState — config validated
    // but worker lifecycle (spawn/stop/persist) requires full integration.
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "Agent registration validated but not yet executable — AgentManager integration pending",
            "name": req.name,
            "validated": true
        })),
    )
        .into_response()
}

/// `DELETE /api/agents/{id}/manage` — Remove an agent.
#[utoipa::path(
    delete,
    path = "/api/agents/{id}/manage",
    tag = "Agent Management",
    summary = "Remove agent",
    description = "Stop and remove an agent. Use ?force=true to remove agents with pending tasks.",
    params(
        ("id" = String, Path, description = "Agent ID (currently name; future: pubkey fingerprint)"),
        ("force" = Option<bool>, Query, description = "Set to true to force removal of agents with pending tasks"),
    ),
    responses(
        (status = 202, description = "Agent removal accepted (pending hot-reload)"),
        (status = 404, description = "Agent not found"),
        (status = 409, description = "Agent has pending tasks (use ?force=true)"),
    )
)]
pub(super) async fn delete_agent(
    State(state): State<MultiAppState>,
    Path(id): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> impl IntoResponse {
    if !state.configs.contains_key(&id) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent '{}' not found", id)})),
        )
            .into_response();
    }

    // Check for pending tasks (buffered entries)
    if !query.force {
        if let Some(buf) = state.buffers.get(&id) {
            let pending = buf.list().await;
            if !pending.is_empty() {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": format!("Agent '{}' has {} pending buffer entries. Use ?force=true to remove.", id, pending.len())
                    })),
                )
                    .into_response();
            }
        }
    }

    // AgentManager not wired — can't stop worker or persist removal
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "Agent removal validated but not yet executable — AgentManager integration pending",
            "id": id,
            "validated": true
        })),
    )
        .into_response()
}

/// `PUT /api/agents/{id}/manage` — Full replace of agent config.
#[utoipa::path(
    put,
    path = "/api/agents/{id}/manage",
    tag = "Agent Management",
    summary = "Replace agent config",
    description = "Full replace of agent configuration in memory. Config changes take effect on the worker's next task cycle. Full restart requires AgentManager integration.",
    params(
        ("id" = String, Path, description = "Agent ID (currently name; future: pubkey fingerprint)"),
    ),
    request_body = RegisterAgentRequest,
    responses(
        (status = 200, description = "Agent updated", body = RegisterAgentResponse),
        (status = 404, description = "Agent not found"),
        (status = 400, description = "Invalid config"),
    )
)]
pub(super) async fn replace_agent(
    State(state): State<MultiAppState>,
    Path(id): Path<String>,
    Json(req): Json<RegisterAgentRequest>,
) -> impl IntoResponse {
    // Body name must match path ID (or be omitted — use path ID)
    if req.name != id {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("Body name '{}' does not match path id '{}'", req.name, id)})),
        )
            .into_response();
    }

    let Some(config_lock) = state.configs.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent '{}' not found", id)})),
        )
            .into_response();
    };

    let new_config = match request_to_config(&req) {
        Ok(c) => c,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }
    };

    // Update the in-memory config
    {
        let mut config = config_lock.write().await;
        config.provider_id = new_config.provider_id;
        config.model_name = new_config.model_name;
        config.persona = new_config.persona;
        config.response_sla_secs = new_config.response_sla_secs;
        config.capability_tags = new_config.capability_tags;
        config.description = new_config.description;
        config.signing_schemes = new_config.signing_schemes;
    }

    let response = RegisterAgentResponse {
        name: id.clone(),
        status: "config_updated".to_string(),
    };

    tracing::info!(agent = %id, "Agent config replaced in-memory (worker restart requires AgentManager)");
    (StatusCode::OK, Json(response)).into_response()
}

/// `PATCH /api/agents/{id}/manage` — Partial update of agent config.
#[utoipa::path(
    patch,
    path = "/api/agents/{id}/manage",
    tag = "Agent Management",
    summary = "Patch agent config",
    description = "Partial update — only provided fields are changed.",
    params(
        ("id" = String, Path, description = "Agent ID (currently name; future: pubkey fingerprint)"),
    ),
    request_body = PatchAgentRequest,
    responses(
        (status = 200, description = "Agent patched", body = RegisterAgentResponse),
        (status = 404, description = "Agent not found"),
    )
)]
pub(super) async fn patch_agent(
    State(state): State<MultiAppState>,
    Path(id): Path<String>,
    Json(patch): Json<PatchAgentRequest>,
) -> impl IntoResponse {
    let Some(config_lock) = state.configs.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent '{}' not found", id)})),
        )
            .into_response();
    };

    {
        let mut config = config_lock.write().await;
        if let Some(ref provider_id) = patch.provider_id {
            config.provider_id = provider_id.clone();
        }
        if let Some(ref model_name) = patch.model_name {
            config.model_name = model_name.clone();
        }
        if let Some(ref persona) = patch.persona {
            config.persona = Some(persona.clone());
        }
        if let Some(sla) = patch.response_sla_secs {
            config.response_sla_secs = sla;
        }
        if let Some(ref tags) = patch.capability_tags {
            config.capability_tags = tags.clone();
        }
        if let Some(ref desc) = patch.description {
            config.description = Some(desc.clone());
        }
        if let Some(ref schemes) = patch.signing_schemes {
            config.signing_schemes = schemes.clone();
        }
    }

    let response = RegisterAgentResponse {
        name: id.clone(),
        status: "patched".to_string(),
    };

    tracing::info!(agent = %id, "Agent config patched via API");
    (StatusCode::OK, Json(response)).into_response()
}

/// Partial update fields for PATCH.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchAgentRequest {
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub response_sla_secs: Option<u64>,
    #[serde(default)]
    pub capability_tags: Option<Vec<String>>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub signing_schemes: Option<Vec<String>>,
}

/// `POST /api/agents/bulk` — Register multiple agents at once.
#[utoipa::path(
    post,
    path = "/api/agents/bulk",
    tag = "Agent Management",
    summary = "Bulk register agents",
    description = "Register multiple agents in one request.",
    request_body = BulkRegisterRequest,
    responses(
        (status = 202, description = "Agent registrations accepted (pending hot-reload)", body = BulkRegisterResponse),
    )
)]
pub(super) async fn bulk_register(
    State(state): State<MultiAppState>,
    Json(req): Json<BulkRegisterRequest>,
) -> impl IntoResponse {
    let mut registered = Vec::new();
    let mut errors = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for agent_req in &req.agents {
        if let Err(msg) = validate_agent_name(&agent_req.name) {
            errors.push(format!("{}: {}", agent_req.name, msg));
            continue;
        }
        if state.configs.contains_key(&agent_req.name) {
            errors.push(format!("{}: already exists", agent_req.name));
            continue;
        }
        if !seen.insert(agent_req.name.clone()) {
            errors.push(format!("{}: duplicate in request", agent_req.name));
            continue;
        }
        // Validate config can be built
        if let Err(msg) = request_to_config(agent_req) {
            errors.push(format!("{}: {}", agent_req.name, msg));
            continue;
        }
        registered.push(agent_req.name.clone());
        tracing::info!(agent = %agent_req.name, "Agent bulk-registered via API (pending hot-reload)");
    }

    // Return validation results but indicate that actual registration
    // (worker spawn + persistence) is not yet executable.
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "Bulk registration validated but not yet executable — AgentManager integration pending",
            "validated": registered,
            "validation_errors": errors,
        })),
    )
        .into_response()
}
