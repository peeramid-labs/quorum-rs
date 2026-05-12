use super::MultiAppState;
use axum::{
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use tracing::error;
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

/// Accepted chat message roles.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub(super) enum ChatRole {
    User,
    Assistant,
}

/// Chat request body.
#[derive(Deserialize, ToSchema)]
pub(super) struct ChatMessage {
    role: ChatRole,
    content: String,
}

#[derive(Deserialize, ToSchema)]
pub(super) struct ChatRequest {
    messages: Vec<ChatMessage>,
}

#[derive(Serialize, ToSchema)]
pub(super) struct ChatResponse {
    response: String,
}

/// `POST /api/agents/{name}/chat` — chat with a specific agent.
#[utoipa::path(
    post,
    path = "/api/agents/{name}/chat",
    params(("name" = String, Path, description = "Agent name")),
    request_body = ChatRequest,
    responses(
        (status = 200, description = "Chat response from the agent", body = ChatResponse),
        (status = 400, description = "Invalid chat request payload", body = ChatResponse),
        (status = 404, description = "Agent not found", body = ChatResponse),
        (status = 500, description = "LLM call failed", body = ChatResponse)
    ),
    tag = "Chat"
)]
pub(super) async fn agent_chat(
    State(state): State<MultiAppState>,
    Path(name): Path<String>,
    payload: Result<Json<ChatRequest>, JsonRejection>,
) -> impl IntoResponse {
    use async_openai::types::{
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
        ChatCompletionRequestUserMessage,
    };

    // Surface deserialization errors (e.g. unsupported role) as 400 with
    // a ChatResponse body — keeps the OpenAPI annotation accurate.
    // Log the full rejection for debugging; return a stable client message.
    let Json(req) = match payload {
        Ok(json) => json,
        Err(rejection) => {
            tracing::warn!("Chat request rejected: {}", rejection);
            return (
                StatusCode::BAD_REQUEST,
                Json(ChatResponse {
                    response: "Invalid request payload".to_string(),
                }),
            )
                .into_response();
        }
    };

    if req.messages.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ChatResponse {
                response: "messages must not be empty".to_string(),
            }),
        )
            .into_response();
    }

    let Some(chat_agent) = state.chat_agents.get(&name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ChatResponse {
                response: format!("Agent '{}' not found or does not support chat.", name),
            }),
        )
            .into_response();
    };

    let messages: Vec<ChatCompletionRequestMessage> = req
        .messages
        .into_iter()
        .map(|m| match m.role {
            ChatRole::Assistant => ChatCompletionRequestAssistantMessage {
                content: Some(
                    async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(
                        m.content,
                    ),
                ),
                ..Default::default()
            }
            .into(),
            ChatRole::User => ChatCompletionRequestUserMessage {
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
            error!("Chat error for agent '{}': {}", name, e);
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
