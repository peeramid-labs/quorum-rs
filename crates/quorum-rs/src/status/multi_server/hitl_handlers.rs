use super::MultiAppState;
use crate::control_plane::ConfigPatch;
use crate::workers::buffer::compute_divergence;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::Deserialize;
use std::sync::atomic::Ordering;
use tracing::{info, warn};
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// HITL Control Plane — Pause / Config / Buffer
// ---------------------------------------------------------------------------

/// Request body for `PUT /api/agents/{name}/pause`.
#[derive(Deserialize, ToSchema)]
pub(super) struct PauseRequest {
    paused: bool,
}

/// `PUT /api/agents/{name}/pause` — pause or resume an agent.
///
/// Toggles the worker's pause flag via its `AtomicBool` handle. This works
/// regardless of whether a response buffer is configured. When a buffer is
/// also present, its pause flag is toggled in tandem.
#[utoipa::path(
    put,
    path = "/api/agents/{name}/pause",
    params(("name" = String, Path, description = "Agent name")),
    request_body = PauseRequest,
    responses(
        (status = 200, description = "Pause state updated"),
        (status = 404, description = "Agent not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_pause(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
    Json(req): Json<PauseRequest>,
) -> impl IntoResponse {
    // Check if agent exists at all
    if !state.configs.contains_key(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent '{}' not found", name)})),
        )
            .into_response();
    }

    // Toggle the worker's pause handle (always available for known agents)
    if let Some(handle) = state.pause_handles.get(&name) {
        handle.store(req.paused, Ordering::Relaxed);
    }

    // Also toggle the buffer's pause flag if present
    if let Some(buf) = state.buffers.get(&name) {
        if req.paused {
            buf.pause();
        } else {
            buf.resume();
        }
    }

    // Update status snapshot
    if let Some(status) = state.statuses.get(&name) {
        let mut snap = status.write().await;
        snap.is_paused = req.paused;
        snap.push_event(
            if req.paused {
                "agent_paused"
            } else {
                "agent_resumed"
            },
            None,
            &format!(
                "Agent {} by operator",
                if req.paused { "paused" } else { "resumed" }
            ),
        );
    }

    info!(
        "Agent '{}' {} by operator",
        name,
        if req.paused { "PAUSED" } else { "RESUMED" }
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({"paused": req.paused})),
    )
        .into_response()
}

/// Request body for `PUT /api/agents/{name}/auto`.
#[derive(Deserialize, ToSchema)]
pub(super) struct AutoApproveRequest {
    enabled: bool,
    /// Divergence threshold (0.0 to 1.0). Optional — if absent, only the
    /// enabled flag is updated.
    threshold: Option<f32>,
}

/// `PUT /api/agents/{name}/auto` — set auto-approve mode for an agent.
///
/// When auto-approve is enabled and the agent's effective divergence score
/// falls below the configured threshold, buffered responses are auto-released
/// immediately instead of waiting for the hold timer.
#[utoipa::path(
    put,
    path = "/api/agents/{name}/auto",
    params(("name" = String, Path, description = "Agent name")),
    request_body = AutoApproveRequest,
    responses(
        (status = 200, description = "Auto-approve state updated"),
        (status = 400, description = "Invalid threshold value"),
        (status = 404, description = "Agent or buffer not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_auto_approve(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
    Json(req): Json<AutoApproveRequest>,
) -> impl IntoResponse {
    if !state.configs.contains_key(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent '{}' not found", name)})),
        )
            .into_response();
    }

    // Validate threshold if provided
    if let Some(t) = req.threshold {
        if !t.is_finite() || !(0.0..=1.0).contains(&t) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!(
                    "threshold must be in [0.0, 1.0], got {}", t
                )})),
            )
                .into_response();
        }
    }

    let Some(buf) = state.buffers.get(&name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({"error": format!("No buffer configured for agent '{}'", name)}),
            ),
        )
            .into_response();
    };
    buf.set_auto_approve(req.enabled);
    if let Some(t) = req.threshold {
        buf.set_auto_approve_threshold(t);
    }

    // When auto-approve is enabled, immediately sweep buffered entries that
    // now qualify for release (divergence < threshold). This gives instant
    // feedback instead of waiting for the next drain cycle (~500ms).
    let mut swept = 0u32;
    if req.enabled {
        if let Some(status) = state.statuses.get(&name) {
            let snap = status.read().await;
            let divergence = compute_divergence(snap.mean_score, snap.score_std_dev);
            swept = buf.auto_release_if_eligible(divergence).await as u32;
        }
    }

    // Update status snapshot event log
    if let Some(status) = state.statuses.get(&name) {
        let mut snap = status.write().await;
        let threshold_str = req
            .threshold
            .map(|t| format!(" (threshold: {:.0}%)", t * 100.0))
            .unwrap_or_default();
        let swept_str = if swept > 0 {
            format!(" — swept {} buffered entries", swept)
        } else {
            String::new()
        };
        snap.push_event(
            if req.enabled {
                "auto_approve_enabled"
            } else {
                "auto_approve_disabled"
            },
            None,
            &format!(
                "Auto-approve {} by operator{}{}",
                if req.enabled { "enabled" } else { "disabled" },
                threshold_str,
                swept_str
            ),
        );
    }

    info!(
        "Agent '{}' auto-approve {} by operator (threshold: {:?}, swept: {})",
        name,
        if req.enabled { "ENABLED" } else { "DISABLED" },
        req.threshold,
        swept
    );

    let current_threshold = buf.auto_approve_threshold();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "auto_approve": req.enabled,
            "threshold": current_threshold,
            "swept": swept
        })),
    )
        .into_response()
}

/// `PUT /api/agents/pause-all` — pause or resume all agents at once.
#[utoipa::path(
    put,
    path = "/api/agents/pause-all",
    request_body = PauseRequest,
    responses(
        (status = 200, description = "All agents paused/resumed")
    ),
    tag = "HITL"
)]
pub(super) async fn pause_all_agents(
    State(state): State<MultiAppState>,
    Json(req): Json<PauseRequest>,
) -> impl IntoResponse {
    let mut count = 0u32;
    for (name, handle) in &state.pause_handles {
        handle.store(req.paused, Ordering::Relaxed);
        if let Some(buf) = state.buffers.get(name) {
            if req.paused {
                buf.pause();
            } else {
                buf.resume();
            }
        }
        if let Some(status) = state.statuses.get(name) {
            let mut snap = status.write().await;
            snap.is_paused = req.paused;
            snap.push_event(
                if req.paused {
                    "agent_paused"
                } else {
                    "agent_resumed"
                },
                None,
                &format!(
                    "Agent {} by master control",
                    if req.paused { "paused" } else { "resumed" }
                ),
            );
        }
        count += 1;
    }

    info!(
        "Master control: {} {} agent(s)",
        if req.paused { "PAUSED" } else { "RESUMED" },
        count
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({"paused": req.paused, "count": count})),
    )
        .into_response()
}

/// `PUT /api/agents/auto-all` — enable or disable auto-approve for all agents.
#[utoipa::path(
    put,
    path = "/api/agents/auto-all",
    request_body = AutoApproveRequest,
    responses(
        (status = 200, description = "Auto-approve updated for all agents"),
        (status = 400, description = "Invalid threshold value")
    ),
    tag = "HITL"
)]
pub(super) async fn auto_all_agents(
    State(state): State<MultiAppState>,
    Json(req): Json<AutoApproveRequest>,
) -> impl IntoResponse {
    // Validate threshold if provided
    if let Some(t) = req.threshold {
        if !t.is_finite() || !(0.0..=1.0).contains(&t) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!(
                    "threshold must be in [0.0, 1.0], got {}", t
                )})),
            )
                .into_response();
        }
    }

    let mut count = 0u32;
    let mut swept = 0u32;
    for (name, buf) in &state.buffers {
        buf.set_auto_approve(req.enabled);
        if let Some(t) = req.threshold {
            buf.set_auto_approve_threshold(t);
        }

        // Immediate sweep when enabling — mark eligible entries for release
        if req.enabled {
            if let Some(status) = state.statuses.get(name) {
                let snap = status.read().await;
                let divergence = compute_divergence(snap.mean_score, snap.score_std_dev);
                swept += buf.auto_release_if_eligible(divergence).await as u32;
            }
        }

        if let Some(status) = state.statuses.get(name) {
            let mut snap = status.write().await;
            snap.push_event(
                if req.enabled {
                    "auto_approve_enabled"
                } else {
                    "auto_approve_disabled"
                },
                None,
                &format!(
                    "Auto-approve {} by master control",
                    if req.enabled { "enabled" } else { "disabled" }
                ),
            );
        }
        count += 1;
    }

    info!(
        "Master control: auto-approve {} for {} agent(s) (threshold: {}, swept: {})",
        if req.enabled { "ENABLED" } else { "DISABLED" },
        count,
        req.threshold
            .map(|t| format!("{:.0}%", t * 100.0))
            .unwrap_or_else(|| "preserved".into()),
        swept
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "auto_approve": req.enabled,
            "threshold": req.threshold,
            "swept": swept,
            "count": count
        })),
    )
        .into_response()
}

/// `PUT /api/agents/{name}/config` — live-update tunable parameters.
///
/// Applies a [`ConfigPatch`] — only non-null fields are updated.
/// Changes are in-memory only (lost on restart).
#[utoipa::path(
    put,
    path = "/api/agents/{name}/config",
    params(("name" = String, Path, description = "Agent name")),
    request_body = crate::control_plane::ConfigPatch,
    responses(
        (status = 200, description = "Config patched successfully"),
        (status = 400, description = "Invalid config patch"),
        (status = 404, description = "Agent not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_config_update(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
    Json(patch): Json<ConfigPatch>,
) -> impl IntoResponse {
    let Some(config) = state.configs.get(&name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Agent '{}' not found", name)})),
        )
            .into_response();
    };

    let mut config = config.write().await;
    if let Err(msg) = patch.apply(&mut config) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response();
    }

    let changed_fields: Vec<&str> = [
        patch.temperature.as_ref().map(|_| "temperature"),
        patch
            .frequency_penalty
            .as_ref()
            .map(|_| "frequency_penalty"),
        patch.presence_penalty.as_ref().map(|_| "presence_penalty"),
        patch.persona.as_ref().map(|_| "persona"),
        patch.textual_feedback.as_ref().map(|_| "textual_feedback"),
        patch
            .max_react_iterations
            .as_ref()
            .map(|_| "max_react_iterations"),
        patch.max_retries.as_ref().map(|_| "max_retries"),
    ]
    .into_iter()
    .flatten()
    .collect();
    info!(
        "Agent '{}' config patched by operator (fields: {:?})",
        name, changed_fields
    );

    if let Some(status) = state.statuses.get(&name) {
        let mut snap = status.write().await;
        snap.push_event("config_updated", None, "Config patched by operator");
    }

    (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))).into_response()
}

/// `GET /api/agents/{name}/buffer` — list buffered responses.
///
/// Automatically drains entries from previous jobs when the agent has moved
/// on to a new job, preventing stale evaluations from cluttering the review queue.
#[utoipa::path(
    get,
    path = "/api/agents/{name}/buffer",
    params(("name" = String, Path, description = "Agent name")),
    responses(
        (status = 200, description = "List of buffered entries", body = Vec<crate::workers::buffer::BufferEntrySummary>),
        (status = 404, description = "Agent not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_buffer_list(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(buf) = state.buffers.get(&name) else {
        if !state.configs.contains_key(&name) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Agent '{}' not found", name)})),
            )
                .into_response();
        }
        return (StatusCode::OK, Json(serde_json::json!([]))).into_response();
    };

    let entries = buf.list().await;
    match serde_json::to_value(&entries) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize buffer entries");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Path parameters for buffer entry operations.
#[derive(Deserialize)]
pub(super) struct BufferEntryPath {
    name: String,
    id: String,
}

/// `GET /api/agents/{name}/buffer/{id}` — get full detail of a buffer entry.
///
/// Returns the deserialized response content (Proposal or Evaluation JSON)
/// along with summary metadata. Used by the dashboard for operator inspection
/// and editing before release.
#[utoipa::path(
    get,
    path = "/api/agents/{name}/buffer/{id}",
    params(
        ("name" = String, Path, description = "Agent name"),
        ("id" = String, Path, description = "Buffer entry ID")
    ),
    responses(
        (status = 200, description = "Buffer entry detail", body = crate::workers::buffer::BufferEntryDetail),
        (status = 404, description = "Agent or entry not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_buffer_detail(
    State(state): State<MultiAppState>,
    Path(path): Path<BufferEntryPath>,
) -> impl IntoResponse {
    let Some(buf) = state.buffers.get(&path.name) else {
        if !state.configs.contains_key(&path.name) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Agent '{}' not found", path.name)})),
            )
                .into_response();
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No buffer configured for this agent"})),
        )
            .into_response();
    };

    match buf.get_detail(&path.id).await {
        Some(detail) => match serde_json::to_value(&detail) {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => {
                tracing::error!(error = %e, "Failed to serialize buffer entry detail");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Buffer entry '{}' not found", path.id)})),
        )
            .into_response(),
    }
}

/// Request body for editing a buffered response.
#[derive(Deserialize, ToSchema)]
pub(super) struct BufferEditRequest {
    /// Modified response content (Proposal or Evaluation JSON).
    /// If `None`, only the operator comment is recorded (no content change).
    content: Option<serde_json::Value>,
    /// Optional operator commentary.
    #[serde(default)]
    operator_comment: Option<String>,
}

/// `PUT /api/agents/{name}/buffer/{id}` — edit a buffered response's content.
///
/// Creates an `OperatorAnnotation` for audit traceability. If `content` is
/// provided, the payload is replaced and the annotation type is `Edit`.
/// If only `operator_comment` is provided, the annotation type is `Comment`.
///
/// When the operator edits the response, they become the "owner" of that output
/// (in the future this will replace the agent's digital signature with the
/// operator's higher-order key).
#[utoipa::path(
    put,
    path = "/api/agents/{name}/buffer/{id}",
    params(
        ("name" = String, Path, description = "Agent name"),
        ("id" = String, Path, description = "Buffer entry ID")
    ),
    request_body = BufferEditRequest,
    responses(
        (status = 200, description = "Buffer entry edited"),
        (status = 400, description = "Invalid content or missing fields"),
        (status = 404, description = "Agent or entry not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_buffer_edit(
    State(state): State<MultiAppState>,
    Path(path): Path<BufferEntryPath>,
    Json(req): Json<BufferEditRequest>,
) -> impl IntoResponse {
    use crate::agents::{AnnotationType, OperatorAnnotation};

    let Some(buf) = state.buffers.get(&path.name) else {
        if !state.configs.contains_key(&path.name) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Agent '{}' not found", path.name)})),
            )
                .into_response();
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No buffer configured for this agent"})),
        )
            .into_response();
    };

    let comment = req.operator_comment.clone().unwrap_or_default();
    let timestamp = chrono::Utc::now().to_rfc3339();

    let success = if let Some(content) = req.content {
        // Content edit — verify entry exists first, compute hash for audit trail
        let Some(detail) = buf.get_detail(&path.id).await else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Buffer entry '{}' not found", path.id)})),
            )
                .into_response();
        };
        let original_hash = {
            let json_str = serde_json::to_string(&detail.content).unwrap_or_default();
            Some(crate::sha256_hex(&json_str))
        };

        let new_payload = match serde_json::to_vec(&content) {
            Ok(p) => p,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": format!("Invalid content: {}", e)})),
                )
                    .into_response();
            }
        };

        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: comment.clone(),
            timestamp,
            original_content_hash: original_hash,
            signatures: Vec::new(),
        };

        // Enforce: Edit annotations must carry a non-empty original_content_hash.
        if let Err(msg) = annotation.validate() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }

        // Run before_release middleware at Edit stage (if configured)
        let mut new_payload = new_payload;
        if let Some(ref pipeline) = state.before_release_middleware {
            let content_val =
                serde_json::from_slice::<serde_json::Value>(&new_payload).unwrap_or_default();
            // Extract action and job_id from the buffered entry content if available
            let entry_action = content_val
                .get("phase")
                .and_then(|v| v.as_str())
                .unwrap_or("edit")
                .to_string();
            let entry_job_id = content_val
                .get("session_id")
                .or_else(|| content_val.get("job_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let entry_round = content_val
                .get("round")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let mut ctx = crate::middleware::MiddlewareContext {
                content: content_val.clone(),
                action: entry_action,
                agent_id: path.name.clone(),
                job_id: entry_job_id,
                round: entry_round,
                stage: crate::middleware::MiddlewareStage::Edit,
                metadata: serde_json::json!({}),
                hook_state: std::collections::HashMap::new(),
            };
            match pipeline.run(&mut ctx).await {
                crate::middleware::pipeline::PipelineResult::Blocked {
                    category,
                    reason,
                    blocked_by,
                } => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(serde_json::json!({
                            "error": "Content rejected by middleware",
                            "category": category,
                            "reason": reason,
                            "middleware": blocked_by,
                        })),
                    )
                        .into_response();
                }
                crate::middleware::pipeline::PipelineResult::Passed { .. } => {
                    // Persist middleware content transformations (if any)
                    if ctx.content != content_val {
                        if let Ok(transformed) = serde_json::to_vec(&ctx.content) {
                            new_payload = transformed;
                        }
                    }
                }
            }
        }

        buf.update_payload_with_annotation(&path.id, new_payload, annotation)
            .await
    } else if !comment.is_empty() {
        // Comment only — no content change
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Comment,
            comment: comment.clone(),
            timestamp,
            original_content_hash: None,
            signatures: Vec::new(),
        };
        buf.add_comment(&path.id, annotation).await
    } else {
        // Neither content nor comment — nothing to do
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Must provide either content or operator_comment"})),
        )
            .into_response();
    };

    if success {
        if let Some(status) = state.statuses.get(&path.name) {
            let mut snap = status.write().await;
            snap.push_event(
                "buffer_edited",
                None,
                &format!(
                    "Entry {} edited by operator{}",
                    &path.id[..8.min(path.id.len())],
                    if comment.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", comment)
                    }
                ),
            );
        }

        info!(
            "Buffer entry '{}' for agent '{}' edited by operator",
            path.id, path.name
        );

        (
            StatusCode::OK,
            Json(serde_json::json!({"status": "edited", "id": path.id})),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Buffer entry '{}' not found", path.id)})),
        )
            .into_response()
    }
}

/// `POST /api/agents/{name}/buffer/{id}/release` — force-release a buffer entry.
#[utoipa::path(
    post,
    path = "/api/agents/{name}/buffer/{id}/release",
    params(
        ("name" = String, Path, description = "Agent name"),
        ("id" = String, Path, description = "Buffer entry ID")
    ),
    responses(
        (status = 200, description = "Entry marked for release"),
        (status = 404, description = "Agent or entry not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_buffer_release(
    State(state): State<MultiAppState>,
    Path(path): Path<BufferEntryPath>,
) -> impl IntoResponse {
    let Some(buf) = state.buffers.get(&path.name) else {
        if !state.configs.contains_key(&path.name) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Agent '{}' not found", path.name)})),
            )
                .into_response();
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No buffer entries"})),
        )
            .into_response();
    };

    // Run before_release middleware at Release stage (if configured)
    if let Some(ref pipeline) = state.before_release_middleware {
        if let Some(detail) = buf.get_detail(&path.id).await {
            let original_content = detail.content.clone();
            let release_action = detail
                .content
                .get("phase")
                .and_then(|v| v.as_str())
                .unwrap_or("release")
                .to_string();
            let release_job_id = detail
                .content
                .get("session_id")
                .or_else(|| detail.content.get("job_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let release_round = detail
                .content
                .get("round")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let mut ctx = crate::middleware::MiddlewareContext {
                content: detail.content,
                action: release_action,
                agent_id: path.name.clone(),
                job_id: release_job_id,
                round: release_round,
                stage: crate::middleware::MiddlewareStage::Release,
                metadata: serde_json::json!({}),
                hook_state: std::collections::HashMap::new(),
            };
            match pipeline.run(&mut ctx).await {
                crate::middleware::pipeline::PipelineResult::Blocked {
                    category,
                    reason,
                    blocked_by,
                } => {
                    tracing::warn!(
                        entry_id = %path.id,
                        agent = %path.name,
                        middleware = %blocked_by,
                        category = %category,
                        "Release blocked by middleware"
                    );
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(serde_json::json!({
                            "error": "Content rejected by middleware",
                            "category": category,
                            "reason": reason,
                            "middleware": blocked_by,
                        })),
                    )
                        .into_response();
                }
                crate::middleware::pipeline::PipelineResult::Passed { warnings } => {
                    for w in &warnings {
                        tracing::info!(
                            entry_id = %path.id,
                            agent = %path.name,
                            middleware = %w.middleware,
                            "Middleware warning on release"
                        );
                    }
                    // Persist middleware content transformations (if any)
                    if ctx.content != original_content {
                        if let Ok(new_payload) = serde_json::to_vec(&ctx.content) {
                            buf.update_payload(&path.id, new_payload).await;
                        }
                    }
                }
            }
        }
    }

    // Atomically unstop + mark for immediate release in a single lock
    // acquisition.  This eliminates the race window where drain_ready()
    // could observe the entry as unstopped with a stale release_at between
    // two separate await points.
    if !buf.force_release(&path.id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Buffer entry '{}' not found", path.id)})),
        )
            .into_response();
    }

    info!(
        "Buffer entry '{}' for agent '{}' marked for immediate release by operator",
        path.id, path.name
    );

    if let Some(status) = state.statuses.get(&path.name) {
        let mut snap = status.write().await;
        snap.push_event(
            "buffer_released",
            None,
            &format!(
                "Entry {} marked for release by operator",
                &path.id[..8.min(path.id.len())]
            ),
        );
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "releasing", "id": path.id})),
    )
        .into_response()
}

/// `POST /api/agents/{name}/buffer/{id}/reject` — discard a buffer entry.
#[utoipa::path(
    post,
    path = "/api/agents/{name}/buffer/{id}/reject",
    params(
        ("name" = String, Path, description = "Agent name"),
        ("id" = String, Path, description = "Buffer entry ID")
    ),
    responses(
        (status = 200, description = "Entry rejected and discarded"),
        (status = 404, description = "Agent or entry not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_buffer_reject(
    State(state): State<MultiAppState>,
    Path(path): Path<BufferEntryPath>,
) -> impl IntoResponse {
    let Some(buf) = state.buffers.get(&path.name) else {
        if !state.configs.contains_key(&path.name) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Agent '{}' not found", path.name)})),
            )
                .into_response();
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No buffer entries"})),
        )
            .into_response();
    };

    let Some(entry) = buf.reject(&path.id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Buffer entry '{}' not found", path.id)})),
        )
            .into_response();
    };

    // Ack without publishing — the response is discarded.
    if let Err(e) = entry.ack_handle.ack().await {
        warn!("Failed to ack rejected buffer entry {}: {}", path.id, e);
    }

    info!(
        "Buffer entry '{}' for agent '{}' rejected by operator",
        path.id, path.name
    );

    if let Some(status) = state.statuses.get(&path.name) {
        let mut snap = status.write().await;
        snap.buffered_count = buf.len().await as u32;
        snap.push_event(
            "buffer_rejected",
            Some(&entry.job_id),
            &format!("{} rejected by operator", entry.action),
        );
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "rejected", "id": path.id})),
    )
        .into_response()
}

/// `POST /api/agents/{name}/buffer/{id}/stop` — reversibly stop a buffer entry.
///
/// Stopped entries remain in the buffer but are skipped by `drain_ready()`.
/// The operator can later un-stop the entry to make it eligible for release.
#[utoipa::path(
    post,
    path = "/api/agents/{name}/buffer/{id}/stop",
    params(
        ("name" = String, Path, description = "Agent name"),
        ("id" = String, Path, description = "Buffer entry ID")
    ),
    responses(
        (status = 200, description = "Entry stopped"),
        (status = 404, description = "Agent or entry not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_buffer_stop(
    State(state): State<MultiAppState>,
    Path(path): Path<BufferEntryPath>,
) -> impl IntoResponse {
    let Some(buf) = state.buffers.get(&path.name) else {
        if !state.configs.contains_key(&path.name) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Agent '{}' not found", path.name)})),
            )
                .into_response();
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No buffer entries"})),
        )
            .into_response();
    };

    // Look up job_id before stop() for correct event correlation
    let job_id = buf.get_detail(&path.id).await.map(|d| d.summary.job_id);

    if !buf.stop(&path.id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Buffer entry '{}' not found", path.id)})),
        )
            .into_response();
    }

    info!(
        "Buffer entry '{}' for agent '{}' stopped by operator",
        path.id, path.name
    );

    if let Some(status) = state.statuses.get(&path.name) {
        let mut snap = status.write().await;
        snap.push_event(
            "buffer_stopped",
            job_id.as_deref(),
            &format!(
                "Entry {} stopped by operator — will not auto-release",
                &path.id[..8.min(path.id.len())]
            ),
        );
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "stopped", "id": path.id})),
    )
        .into_response()
}

/// `POST /api/agents/{name}/buffer/{id}/unstop` — un-stop a previously stopped entry.
///
/// The entry becomes eligible for `drain_ready()` again. If its release_at
/// has already passed, it will drain on the next worker cycle (≤500ms).
#[utoipa::path(
    post,
    path = "/api/agents/{name}/buffer/{id}/unstop",
    params(
        ("name" = String, Path, description = "Agent name"),
        ("id" = String, Path, description = "Buffer entry ID")
    ),
    responses(
        (status = 200, description = "Entry un-stopped"),
        (status = 404, description = "Agent or entry not found")
    ),
    tag = "HITL"
)]
pub(super) async fn agent_buffer_unstop(
    State(state): State<MultiAppState>,
    Path(path): Path<BufferEntryPath>,
) -> impl IntoResponse {
    let Some(buf) = state.buffers.get(&path.name) else {
        if !state.configs.contains_key(&path.name) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Agent '{}' not found", path.name)})),
            )
                .into_response();
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No buffer entries"})),
        )
            .into_response();
    };

    // Look up job_id before unstop() for correct event correlation
    let job_id = buf.get_detail(&path.id).await.map(|d| d.summary.job_id);

    if !buf.unstop(&path.id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Buffer entry '{}' not found", path.id)})),
        )
            .into_response();
    }

    info!(
        "Buffer entry '{}' for agent '{}' un-stopped by operator",
        path.id, path.name
    );

    if let Some(status) = state.statuses.get(&path.name) {
        let mut snap = status.write().await;
        snap.push_event(
            "buffer_unstopped",
            job_id.as_deref(),
            &format!(
                "Entry {} un-stopped — eligible for release",
                &path.id[..8.min(path.id.len())]
            ),
        );
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "unstopped", "id": path.id})),
    )
        .into_response()
}
