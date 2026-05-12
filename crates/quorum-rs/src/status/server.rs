//! Embedded HTTP server for agent status monitoring.
//!
//! Requires the `status-server` feature flag. Serves a lightweight dashboard
//! on a configurable port with real-time event log and direct chat.

use super::SharedAgentStatus;
use crate::agents::{AgentConfig, ChatCapable};
use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Json},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info};

/// Combined application state for the status server.
#[derive(Clone)]
struct AppState {
    status: SharedAgentStatus,
    chat_agent: Option<Arc<dyn ChatCapable>>,
    agent_config: AgentConfig,
}

/// Embedded HTTP server that exposes dashboard, status JSON, and chat API.
pub struct StatusServer;

impl StatusServer {
    /// Start the status server on the given port.
    ///
    /// `chat_agent` is optional — if `None`, the chat endpoint returns 503.
    /// `agent_config` is used for the config endpoint.
    /// `api_error_telemetry` is optional — when present the
    /// `api_error_telemetry_middleware` emits an `ApiError` event for
    /// every 4xx/5xx response; when absent the layer is a transparent
    /// pass-through.
    ///
    /// This function runs indefinitely — spawn it in a background task.
    pub async fn run(
        port: u16,
        status: SharedAgentStatus,
        chat_agent: Option<Arc<dyn ChatCapable>>,
        agent_config: AgentConfig,
        api_error_telemetry: Option<Arc<crate::api_error_middleware::ApiErrorTelemetry>>,
    ) {
        let state = AppState {
            status,
            chat_agent,
            agent_config,
        };

        let app = Router::new()
            .route("/", get(dashboard_page))
            .route("/api/status", get(status_json))
            .route("/api/config", get(config_json))
            .route("/api/chat", post(chat_handler))
            .with_state(state)
            .layer(axum::middleware::from_fn_with_state(
                api_error_telemetry,
                crate::api_error_middleware::api_error_telemetry_middleware,
            ));

        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        info!("Agent status dashboard → http://{}/", addr);

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("Failed to bind status server on port {}: {}", port, e);
                return;
            }
        };
        if let Err(e) = axum::serve(listener, app).await {
            error!("Status server error: {}", e);
        }
    }
}

/// `GET /` — serves the embedded status dashboard HTML.
///
/// Returns `Cache-Control: no-store` so the browser always fetches the latest
/// version after an agent rebuild (HTML is embedded at compile time via
/// `include_str!`).
async fn dashboard_page() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("status.html")),
    )
}

/// `GET /api/status` — returns agent status as JSON.
async fn status_json(State(state): State<AppState>) -> Json<super::AgentStatusSnapshot> {
    let snap = state.status.read().await;
    Json(snap.clone())
}

/// `GET /api/config` — returns agent configuration as JSON.
///
/// Serializes the full `AgentConfig` directly. The `orchestrators` field is
/// excluded automatically via `#[serde(skip_serializing)]` on the struct.
async fn config_json(State(state): State<AppState>) -> Json<crate::agents::AgentConfig> {
    Json(state.agent_config.clone())
}

/// A single message in a chat conversation.
#[derive(Deserialize)]
struct ChatMessage {
    role: String, // "user" or "assistant"
    content: String,
}

/// Request body for the chat endpoint.
#[derive(Deserialize)]
struct ChatRequest {
    messages: Vec<ChatMessage>,
}

/// Response body for the chat endpoint.
#[derive(Serialize)]
struct ChatResponse {
    response: String,
}

/// `POST /api/chat` — direct conversation with the agent's LLM.
async fn chat_handler(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    use async_openai::types::{
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
        ChatCompletionRequestUserMessage,
    };

    let Some(ref chat_agent) = state.chat_agent else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ChatResponse {
                response: "Chat not available — agent does not implement ChatCapable.".into(),
            }),
        )
            .into_response();
    };

    // Convert ChatMessage → ChatCompletionRequestMessage
    let messages: Vec<ChatCompletionRequestMessage> = req
        .messages
        .into_iter()
        .map(|m| match m.role.as_str() {
            "assistant" => ChatCompletionRequestAssistantMessage {
                content: Some(
                    async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(
                        m.content,
                    ),
                ),
                ..Default::default()
            }
            .into(),
            _ => ChatCompletionRequestUserMessage {
                content: async_openai::types::ChatCompletionRequestUserMessageContent::Text(
                    m.content,
                ),
                ..Default::default()
            }
            .into(),
        })
        .collect();

    match chat_agent.chat(messages).await {
        Ok(response) => (StatusCode::OK, Json(ChatResponse { response })).into_response(),
        Err(e) => {
            error!("Chat error: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ChatResponse {
                    response: format!("Error: {}", e),
                }),
            )
                .into_response()
        }
    }
}
