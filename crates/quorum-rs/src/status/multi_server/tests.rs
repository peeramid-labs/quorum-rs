use super::*;
use crate::agents::AgentConfig;
use crate::status::new_shared_status;
use crate::workers::buffer::AckHandle;
use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt;
use std::sync::atomic::Ordering;
use tower::ServiceExt;

/// Shared no-op ack handle for test buffer entries.
struct NoopAck;
#[async_trait::async_trait]
impl AckHandle for NoopAck {
    async fn ack(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Build a test app state with two agents.
fn test_state() -> MultiAppState {
    let mut statuses = HashMap::new();
    let mut configs = HashMap::new();

    // Agent ALPHA
    let alpha_status =
        new_shared_status("ALPHA".into(), "MiniMax-M2.5".into(), "together_ai".into());
    statuses.insert("ALPHA".to_string(), alpha_status);
    configs.insert(
        "ALPHA".to_string(),
        Arc::new(RwLock::new(AgentConfig {
            name: "ALPHA".into(),
            provider_id: "together_ai".into(),
            model_name: "MiniMax-M2.5".into(),
            temperature: 0.7,
            max_tokens: 8192,
            ..Default::default()
        })),
    );

    // Agent BETA
    let beta_status = new_shared_status("BETA".into(), "llama3.2".into(), "ollama_local".into());
    statuses.insert("BETA".to_string(), beta_status);
    configs.insert(
        "BETA".to_string(),
        Arc::new(RwLock::new(AgentConfig {
            name: "BETA".into(),
            provider_id: "ollama_local".into(),
            model_name: "llama3.2".into(),
            temperature: 0.5,
            max_tokens: 4096,
            ..Default::default()
        })),
    );

    let pause_handles = configs
        .keys()
        .map(|k| (k.clone(), Arc::new(AtomicBool::new(false))))
        .collect();

    MultiAppState {
        statuses,
        chat_agents: HashMap::new(),
        configs,
        buffers: HashMap::new(),
        pause_handles,
        orchestrator_registry: None,
        base_hold_secs: Arc::new(AtomicU64::new(10)),
        response_sla_secs: Arc::new(AtomicU64::new(10)),
        buffer_floor_pct: Arc::new(AtomicU64::new(0)),
        before_release_middleware: None,
    }
}

/// Build a test state with HITL buffer enabled for ALPHA.
fn test_state_with_buffer() -> MultiAppState {
    let mut state = test_state();
    let buf = Arc::new(ResponseBuffer::new(std::time::Duration::from_secs(30)));
    state.buffers.insert("ALPHA".to_string(), buf);
    state.base_hold_secs = Arc::new(AtomicU64::new(30));
    state
}

/// Helper: perform a GET request and return (status_code, body_string).
async fn get_request(app: Router, uri: &str) -> (StatusCode, String) {
    let req = axum::http::Request::builder()
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// Helper: perform a POST request with JSON body.
async fn post_json(app: Router, uri: &str, json: &str) -> (StatusCode, String) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(json.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// Helper: perform a PUT request with JSON body.
async fn put_json(app: Router, uri: &str, json: &str) -> (StatusCode, String) {
    let req = axum::http::Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(json.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

// -----------------------------------------------------------------------
// Existing tests (updated for new state shape)
// -----------------------------------------------------------------------

#[tokio::test]
async fn dashboard_returns_html() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.to_lowercase().contains("<!doctype html>"));
}

#[tokio::test]
async fn list_agents_returns_all_agents_sorted() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/agents").await;
    assert_eq!(status, StatusCode::OK);

    let agents: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(agents.len(), 2);
    // Sorted alphabetically: ALPHA before BETA
    assert_eq!(agents[0]["name"], "ALPHA");
    assert_eq!(agents[1]["name"], "BETA");
    assert_eq!(agents[0]["model_name"], "MiniMax-M2.5");
    assert_eq!(agents[1]["provider_id"], "ollama_local");
}

#[tokio::test]
async fn list_agents_shows_nats_disconnected_by_default() {
    let app = build_router(test_state());
    let (_, body) = get_request(app, "/api/agents").await;
    let agents: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    // New status snapshots default to nats_connected=false
    assert_eq!(agents[0]["nats_connected"], false);
    assert_eq!(agents[1]["nats_connected"], false);
}

#[tokio::test]
async fn list_agents_shows_has_chat_false_when_no_chat_agents() {
    let app = build_router(test_state());
    let (_, body) = get_request(app, "/api/agents").await;
    let agents: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(agents[0]["has_chat"], false);
    assert_eq!(agents[1]["has_chat"], false);
}

#[tokio::test]
async fn list_agents_includes_hitl_fields() {
    let app = build_router(test_state());
    let (_, body) = get_request(app, "/api/agents").await;
    let agents: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(agents[0]["is_paused"], false);
    assert_eq!(agents[0]["buffered_count"], 0);
    assert_eq!(agents[0]["error_rate"], 0.0);
    assert!(agents[0]["mean_score"].is_null());
}

#[tokio::test]
async fn agent_status_returns_snapshot() {
    let state = test_state();
    // Set a field on ALPHA's status to verify it's returned
    {
        let mut snap = state.statuses["ALPHA"].write().await;
        snap.nats_connected = true;
        snap.current_job = Some("job-123".to_string());
    }
    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/status").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["nats_connected"], true);
    assert_eq!(json["current_job"], "job-123");
}

#[tokio::test]
async fn agent_status_returns_404_for_unknown_agent() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/agents/UNKNOWN/status").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn agent_config_returns_config_view() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/agents/ALPHA/config").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["name"], "ALPHA");
    assert_eq!(json["model_name"], "MiniMax-M2.5");
    assert_eq!(json["provider_id"], "together_ai");
    // f32 → JSON serialization may introduce floating point drift
    let temp = json["temperature"].as_f64().unwrap();
    assert!((temp - 0.7).abs() < 0.001, "temperature was {}", temp);
}

#[tokio::test]
async fn agent_config_returns_404_for_unknown_agent() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/agents/GHOST/config").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn agent_chat_returns_404_when_no_chat_agent() {
    let app = build_router(test_state());
    let (status, body) = post_json(
        app,
        "/api/agents/ALPHA/chat",
        r#"{"messages":[{"role":"user","content":"hello"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found") || body.contains("does not support chat"));
}

#[tokio::test]
async fn agent_chat_returns_404_for_unknown_agent() {
    let app = build_router(test_state());
    let (status, _) = post_json(
        app,
        "/api/agents/NOBODY/chat",
        r#"{"messages":[{"role":"user","content":"hello"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_agents_empty_state() {
    let state = MultiAppState {
        statuses: HashMap::new(),
        chat_agents: HashMap::new(),
        configs: HashMap::new(),
        buffers: HashMap::new(),
        pause_handles: HashMap::new(),
        orchestrator_registry: None,
        base_hold_secs: Arc::new(AtomicU64::new(10)),
        response_sla_secs: Arc::new(AtomicU64::new(10)),
        buffer_floor_pct: Arc::new(AtomicU64::new(0)),
        before_release_middleware: None,
    };
    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents").await;
    assert_eq!(status, StatusCode::OK);
    let agents: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert!(agents.is_empty());
}

#[tokio::test]
async fn agent_chat_rejects_unsupported_role() {
    let app = build_router(test_state());
    // "system" role is not accepted — ChatRole enum only allows "user" and "assistant".
    // Handler catches JsonRejection and maps it to 400 with ChatResponse body.
    let (status, body) = post_json(
        app,
        "/api/agents/ALPHA/chat",
        r#"{"messages":[{"role":"system","content":"You are a helper"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("Invalid request"),
        "Expected rejection error, got: {}",
        body
    );
}

#[tokio::test]
async fn agent_chat_accepts_user_and_assistant_roles() {
    // This hits the 404 path (no chat agent configured), but importantly
    // it does NOT hit the 400 path — the roles are valid.
    let app = build_router(test_state());
    let (status, _) = post_json(
        app,
        "/api/agents/ALPHA/chat",
        r#"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"how are you"}]}"#,
    )
    .await;
    // ALPHA has no chat agent, so we get 404 (not found), not 400 (bad request)
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn agent_chat_rejects_malformed_json() {
    let app = build_router(test_state());
    // Completely invalid JSON body — handler catches JsonRejection → 400.
    let (status, body) = post_json(app, "/api/agents/ALPHA/chat", "not json at all").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("Invalid request"),
        "Expected rejection error, got: {}",
        body
    );
}

#[tokio::test]
async fn agent_chat_rejects_empty_messages() {
    let app = build_router(test_state());
    let (status, body) = post_json(app, "/api/agents/ALPHA/chat", r#"{"messages":[]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("must not be empty"),
        "Expected empty-messages error, got: {}",
        body
    );
}

// -----------------------------------------------------------------------
// HITL Control Plane tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn pause_agent_returns_ok() {
    let app = build_router(test_state_with_buffer());
    let (status, body) = put_json(app, "/api/agents/ALPHA/pause", r#"{"paused": true}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["paused"], true);
}

#[tokio::test]
async fn pause_agent_actually_pauses_buffer() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    assert!(!buf.is_paused());

    let app = build_router(state);
    let (status, _) = put_json(app, "/api/agents/ALPHA/pause", r#"{"paused": true}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert!(buf.is_paused());
}

#[tokio::test]
async fn pause_unknown_agent_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, _) = put_json(app, "/api/agents/GHOST/pause", r#"{"paused": true}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pause_agent_without_buffer_succeeds() {
    let app = build_router(test_state()); // no buffers, but pause handles exist
    let (status, body) = put_json(app, "/api/agents/ALPHA/pause", r#"{"paused": true}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["paused"], true);
}

#[tokio::test]
async fn pause_agent_without_buffer_toggles_handle() {
    let state = test_state();
    let handle = state.pause_handles["ALPHA"].clone();
    assert!(!handle.load(Ordering::Relaxed));

    let app = build_router(state);
    let (status, _) = put_json(app, "/api/agents/ALPHA/pause", r#"{"paused": true}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert!(handle.load(Ordering::Relaxed));
}

#[tokio::test]
async fn pause_all_pauses_all_agents() {
    let state = test_state();
    let alpha_handle = state.pause_handles["ALPHA"].clone();
    let beta_handle = state.pause_handles["BETA"].clone();
    assert!(!alpha_handle.load(Ordering::Relaxed));
    assert!(!beta_handle.load(Ordering::Relaxed));

    let app = build_router(state);
    let (status, body) = put_json(app, "/api/agents/pause-all", r#"{"paused": true}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["paused"], true);
    assert_eq!(json["count"], 2);
    assert!(alpha_handle.load(Ordering::Relaxed));
    assert!(beta_handle.load(Ordering::Relaxed));
}

#[tokio::test]
async fn pause_all_resumes_all_agents() {
    let state = test_state();
    let alpha_handle = state.pause_handles["ALPHA"].clone();
    let beta_handle = state.pause_handles["BETA"].clone();
    // Pre-pause both
    alpha_handle.store(true, Ordering::Relaxed);
    beta_handle.store(true, Ordering::Relaxed);

    let app = build_router(state);
    let (status, body) = put_json(app, "/api/agents/pause-all", r#"{"paused": false}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["paused"], false);
    assert!(!alpha_handle.load(Ordering::Relaxed));
    assert!(!beta_handle.load(Ordering::Relaxed));
}

#[tokio::test]
async fn update_config_applies_temperature() {
    let state = test_state();
    let alpha_config = state.configs["ALPHA"].clone();
    let app = build_router(state);
    let (status, _) = put_json(app, "/api/agents/ALPHA/config", r#"{"temperature": 1.2}"#).await;
    assert_eq!(status, StatusCode::OK);
    let config = alpha_config.read().await;
    assert!((config.temperature - 1.2).abs() < 0.001);
}

#[tokio::test]
async fn update_config_partial_patch() {
    let state = test_state();
    let alpha_config = state.configs["ALPHA"].clone();
    let app = build_router(state);
    let (status, _) = put_json(
        app,
        "/api/agents/ALPHA/config",
        r#"{"persona": "aggressive debater"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let config = alpha_config.read().await;
    assert_eq!(config.persona, Some("aggressive debater".into()));
    // Other fields unchanged
    assert!((config.temperature - 0.7).abs() < 0.001);
}

#[tokio::test]
async fn update_config_unknown_agent_404() {
    let app = build_router(test_state());
    let (status, _) = put_json(app, "/api/agents/GHOST/config", r#"{"temperature": 0.5}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_buffer_empty() {
    let app = build_router(test_state_with_buffer());
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer").await;
    assert_eq!(status, StatusCode::OK);
    let entries: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
async fn list_buffer_no_buffer_returns_empty() {
    // Agent without a buffer configured
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer").await;
    assert_eq!(status, StatusCode::OK);
    let entries: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
async fn list_buffer_unknown_agent_404() {
    let app = build_router(test_state());
    let (status, _) = get_request(app, "/api/agents/GHOST/buffer").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Helper: push a test entry into a buffer.
async fn push_test_entry(buf: &ResponseBuffer, id: &str, action: &str, payload: &[u8]) {
    use crate::workers::buffer::BufferedResponse;
    use std::time::{Duration, Instant};

    let now = Instant::now();
    buf.push(BufferedResponse {
        id: id.to_string(),
        action: action.to_string(),
        job_id: "job-test".to_string(),
        round: 1,
        reply_subject: format!("nsed.job-test.result.1.agent.{}", action),
        payload: payload.to_vec(),
        created_at: now,
        release_at: now + Duration::from_secs(60),
        ack_handle: Box::new(NoopAck),
        msg_id: format!("msg-{}", id),
        annotations: Vec::new(),
        edited: false,
        stopped: false,
    })
    .await;
}

// -----------------------------------------------------------------------
// Buffer detail + edit endpoint tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn buffer_detail_returns_content() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    let payload = serde_json::json!({"title": "Test Proposal", "content": "Hello"});
    push_test_entry(
        &buf,
        "entry-1",
        "propose",
        &serde_json::to_vec(&payload).unwrap(),
    )
    .await;

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer/entry-1").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["id"], "entry-1");
    assert_eq!(json["action"], "propose");
    assert_eq!(json["content"]["title"], "Test Proposal");
    assert!(json["release_in_ms"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn buffer_detail_unknown_entry_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn buffer_detail_unknown_agent_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, _) = get_request(app, "/api/agents/GHOST/buffer/entry-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn buffer_detail_agent_without_buffer_returns_404() {
    let app = build_router(test_state()); // no buffer for ALPHA
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer/entry-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("No buffer configured"));
}

#[tokio::test]
async fn buffer_edit_updates_content() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    let original = serde_json::json!({"title": "Original", "content": "v1"});
    push_test_entry(
        &buf,
        "edit-1",
        "propose",
        &serde_json::to_vec(&original).unwrap(),
    )
    .await;

    let app = build_router(state);
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/buffer/edit-1",
        r#"{"content": {"title": "Modified", "content": "v2"}, "operator_comment": "Fixed wording"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "edited");

    // Verify content was actually changed
    let detail = buf.get_detail("edit-1").await.unwrap();
    assert_eq!(detail.content["title"], "Modified");
    assert_eq!(detail.content["content"], "v2");
}

#[tokio::test]
async fn buffer_edit_unknown_entry_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/buffer/nonexistent",
        r#"{"content": {"x": 1}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn buffer_edit_unknown_agent_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, _) = put_json(
        app,
        "/api/agents/GHOST/buffer/entry-1",
        r#"{"content": {"x": 1}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn buffer_edit_agent_without_buffer_returns_404() {
    let app = build_router(test_state());
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/buffer/entry-1",
        r#"{"content": {"x": 1}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("No buffer configured"));
}

#[tokio::test]
async fn buffer_comment_only_adds_annotation() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    let payload = serde_json::json!({"title": "Original"});
    push_test_entry(
        &buf,
        "comment-1",
        "propose",
        &serde_json::to_vec(&payload).unwrap(),
    )
    .await;

    let app = build_router(state);
    // No content field — only operator_comment
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/buffer/comment-1",
        r#"{"operator_comment": "Looks good, proceed"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "edited");

    // Content should be unchanged
    let detail = buf.get_detail("comment-1").await.unwrap();
    assert_eq!(detail.content["title"], "Original");
}

#[tokio::test]
async fn buffer_edit_no_content_no_comment_returns_400() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    let payload = serde_json::json!({"title": "Test"});
    push_test_entry(
        &buf,
        "empty-1",
        "propose",
        &serde_json::to_vec(&payload).unwrap(),
    )
    .await;

    let app = build_router(state);
    let (status, body) = put_json(app, "/api/agents/ALPHA/buffer/empty-1", r#"{}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("Must provide either"));
}

#[tokio::test]
async fn release_unknown_entry_404() {
    let app = build_router(test_state_with_buffer());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/nonexistent/release", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn reject_unknown_entry_404() {
    let app = build_router(test_state_with_buffer());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/nonexistent/reject", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

// -- SLA / Config endpoint tests --

#[tokio::test]
async fn get_config_returns_response_sla() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    // test_state() sets response_sla_secs to 10 (hardcoded in test helper)
    assert_eq!(json["response_sla_secs"], 10);
}

#[tokio::test]
async fn put_config_updates_response_sla() {
    let state = test_state();
    let app = build_router(state.clone());
    let (status, body) = put_json(app, "/api/config", r#"{"response_sla_secs": 300}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response_sla_secs"], 300);
    // Verify the atomic was actually updated
    assert_eq!(state.response_sla_secs.load(Ordering::Relaxed), 300);
}

#[tokio::test]
async fn put_config_sla_propagates_to_buffers() {
    let state = test_state_with_buffer();
    let buf = state.buffers.get("ALPHA").unwrap().clone();
    // SLA initializes to the hold_duration (30s) — no floor applied.
    assert_eq!(buf.response_sla().unwrap().as_secs(), 30);

    let app = build_router(state);
    // Set SLA to 600s — should propagate to buffer
    let (status, _) = put_json(app, "/api/config", r#"{"response_sla_secs": 600}"#).await;
    assert_eq!(status, StatusCode::OK);
    // Buffer should now have 600s SLA
    assert_eq!(buf.response_sla().unwrap().as_secs(), 600);
}

#[tokio::test]
async fn default_sla_is_zero_when_no_buffers() {
    // Production default: no buffers → pass-through (0)
    let state = MultiAppState {
        statuses: HashMap::new(),
        chat_agents: HashMap::new(),
        configs: HashMap::new(),
        buffers: HashMap::new(),
        pause_handles: HashMap::new(),
        orchestrator_registry: None,
        base_hold_secs: Arc::new(AtomicU64::new(0)),
        response_sla_secs: Arc::new(AtomicU64::new(0)),
        buffer_floor_pct: Arc::new(AtomicU64::new(0)),
        before_release_middleware: None,
    };
    let app = build_router(state);
    let (status, body) = get_request(app, "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response_sla_secs"], 0);
}

#[tokio::test]
async fn sla_matches_configured_value_with_short_buffer_hold() {
    // When a buffer exists with a short base_hold (10s), the response_sla_secs
    // reflects the configured value — no floor applied.
    let mut state = test_state();
    let buf = Arc::new(ResponseBuffer::new(std::time::Duration::from_secs(10)));
    state.buffers.insert("ALPHA".to_string(), buf.clone());
    state.base_hold_secs = Arc::new(AtomicU64::new(10));
    state.response_sla_secs = Arc::new(AtomicU64::new(10));

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/config").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response_sla_secs"], 10);
    assert_eq!(json["base_hold_secs"], 10);
    assert_eq!(buf.response_sla().unwrap().as_secs(), 10);
}

#[tokio::test]
async fn put_config_partial_update_preserves_other_fields() {
    let state = test_state();
    // Set known initial values
    state.base_hold_secs.store(30, Ordering::Relaxed);
    state.response_sla_secs.store(120, Ordering::Relaxed);

    let app = build_router(state.clone());
    // Only update response_sla_secs, not base_hold_secs
    let (status, body) = put_json(app, "/api/config", r#"{"response_sla_secs": 300}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response_sla_secs"], 300);
    // base_hold_secs should be untouched
    assert_eq!(json["base_hold_secs"], 30);
    assert_eq!(state.base_hold_secs.load(Ordering::Relaxed), 30);
}

// -----------------------------------------------------------------------
// Stop / Unstop endpoint e2e tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn stop_entry_returns_ok() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "stop-1", "propose", b"{}").await;

    let app = build_router(state);
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/stop-1/stop", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["id"], "stop-1");
}

#[tokio::test]
async fn stop_unknown_entry_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/nonexistent/stop", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn stop_unknown_agent_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, _) = post_json(app, "/api/agents/GHOST/buffer/entry-1/stop", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stop_agent_without_buffer_returns_404() {
    let app = build_router(test_state());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/entry-1/stop", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("No buffer"));
}

#[tokio::test]
async fn unstop_entry_returns_ok() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "unstop-1", "evaluate", b"{}").await;
    buf.stop("unstop-1").await;

    let app = build_router(state);
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/unstop-1/unstop", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "unstopped");
    assert_eq!(json["id"], "unstop-1");
}

#[tokio::test]
async fn unstop_unknown_entry_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/nonexistent/unstop", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn unstop_unknown_agent_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, _) = post_json(app, "/api/agents/GHOST/buffer/entry-1/unstop", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unstop_agent_without_buffer_returns_404() {
    let app = build_router(test_state());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/entry-1/unstop", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("No buffer"));
}

// -----------------------------------------------------------------------
// Stop / Unstop integration with list + detail
// -----------------------------------------------------------------------

#[tokio::test]
async fn stopped_entry_visible_in_list_with_flag() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "vis-1", "propose", b"{}").await;
    buf.stop("vis-1").await;

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer").await;
    assert_eq!(status, StatusCode::OK);
    let entries: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["stopped"], true);
}

#[tokio::test]
async fn stopped_entry_visible_in_detail_with_flag() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "vis-d-1", "propose", b"{}").await;
    buf.stop("vis-d-1").await;

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer/vis-d-1").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["stopped"], true);
}

#[tokio::test]
async fn unstopped_entry_shows_stopped_false_in_list() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "unsf-1", "propose", b"{}").await;
    buf.stop("unsf-1").await;
    buf.unstop("unsf-1").await;

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer").await;
    assert_eq!(status, StatusCode::OK);
    let entries: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["stopped"], false);
}

// -----------------------------------------------------------------------
// Release endpoint e2e tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn release_entry_returns_ok() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "rel-1", "propose", b"{}").await;

    let app = build_router(state);
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/rel-1/release", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "releasing");
    assert_eq!(json["id"], "rel-1");
}

#[tokio::test]
async fn release_marks_entry_for_immediate_drain() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "rel-drain-1", "propose", b"{}").await;

    // Entry has release_at 60s in the future — should NOT drain yet
    assert!(buf.drain_ready().await.is_empty());

    // Release via API
    let app = build_router(state);
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/rel-drain-1/release", "").await;
    assert_eq!(status, StatusCode::OK);

    // After mark_for_release, drain_ready should pick it up
    let drained = buf.drain_ready().await;
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].id, "rel-drain-1");
}

#[tokio::test]
async fn release_unknown_agent_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, _) = post_json(app, "/api/agents/GHOST/buffer/entry-1/release", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn release_agent_without_buffer_returns_404() {
    let app = build_router(test_state());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/entry-1/release", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("No buffer"));
}

// -----------------------------------------------------------------------
// Reject endpoint e2e tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn reject_entry_returns_ok() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "rej-1", "propose", b"{}").await;

    let app = build_router(state);
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/rej-1/reject", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "rejected");
    assert_eq!(json["id"], "rej-1");
}

#[tokio::test]
async fn reject_removes_entry_from_buffer() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "rej-rem-1", "propose", b"{}").await;
    assert_eq!(buf.len().await, 1);

    let app = build_router(state);
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/rej-rem-1/reject", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(buf.len().await, 0, "rejected entry should be removed");
}

#[tokio::test]
async fn reject_unknown_agent_returns_404() {
    let app = build_router(test_state_with_buffer());
    let (status, _) = post_json(app, "/api/agents/GHOST/buffer/entry-1/reject", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reject_agent_without_buffer_returns_404() {
    let app = build_router(test_state());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/entry-1/reject", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("No buffer"));
}

// -----------------------------------------------------------------------
// Combined flows: stop → edit → unstop → release
// -----------------------------------------------------------------------

#[tokio::test]
async fn stop_then_edit_then_unstop_flow() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    let payload = serde_json::json!({"content": "original"});
    push_test_entry(
        &buf,
        "flow-1",
        "propose",
        &serde_json::to_vec(&payload).unwrap(),
    )
    .await;

    // Step 1: Stop
    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/flow-1/stop", "").await;
    assert_eq!(status, StatusCode::OK);

    // Verify stopped in detail
    let detail = buf.get_detail("flow-1").await.unwrap();
    assert!(detail.summary.stopped);

    // Step 2: Edit while stopped
    let app = build_router(state.clone());
    let (status, _) = put_json(
        app,
        "/api/agents/ALPHA/buffer/flow-1",
        r#"{"content": {"content": "edited while stopped"}, "operator_comment": "Regen"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify content changed but still stopped
    let detail = buf.get_detail("flow-1").await.unwrap();
    assert_eq!(detail.content["content"], "edited while stopped");
    assert!(detail.summary.stopped);

    // Step 3: Unstop
    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/flow-1/unstop", "").await;
    assert_eq!(status, StatusCode::OK);

    // Verify not stopped
    let detail = buf.get_detail("flow-1").await.unwrap();
    assert!(!detail.summary.stopped);
}

#[tokio::test]
async fn stop_prevents_raw_mark_for_release_drain() {
    // Low-level mark_for_release preserves the stopped flag — the SDK
    // primitive does not auto-unstop.
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "stop-rel-1", "propose", b"{}").await;

    buf.stop("stop-rel-1").await;

    // Raw mark_for_release (NOT via API) preserves stopped
    buf.mark_for_release("stop-rel-1").await;
    let drained = buf.drain_ready().await;
    assert!(
        drained.is_empty(),
        "stopped entry should not drain after raw mark_for_release"
    );

    // Explicit unstop → now it should drain
    buf.unstop("stop-rel-1").await;
    let drained = buf.drain_ready().await;
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].id, "stop-rel-1");
}

#[tokio::test]
async fn api_release_force_releases_stopped_entry() {
    // The API release handler is a force-release — it unstops then marks
    // for release, so the operator can always get an entry out.
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "force-rel-1", "propose", b"{}").await;

    buf.stop("force-rel-1").await;

    // Force-release via API should unstop + mark
    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/force-rel-1/release", "").await;
    assert_eq!(status, StatusCode::OK);

    // Entry should drain immediately — API handler unstopped it
    let drained = buf.drain_ready().await;
    assert_eq!(drained.len(), 1, "force-released entry should drain");
    assert_eq!(drained[0].id, "force-rel-1");
}

#[tokio::test]
async fn edit_preserves_reply_subject_via_api() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    let payload = serde_json::json!({"content": "v1"});
    push_test_entry(
        &buf,
        "edit-rs-1",
        "propose",
        &serde_json::to_vec(&payload).unwrap(),
    )
    .await;

    // Capture original reply_subject from buffer (via known test helper format)
    let original_reply_subject = "nsed.job-test.result.1.agent.propose".to_string();
    let detail = buf.get_detail("edit-rs-1").await.unwrap();
    assert_eq!(detail.summary.action, "propose");

    // Edit via API
    let app = build_router(state.clone());
    let (status, _) = put_json(
        app,
        "/api/agents/ALPHA/buffer/edit-rs-1",
        r#"{"content": {"content": "v2"}, "operator_comment": "Better"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify reply_subject unchanged — release to get the full BufferedResponse
    let released = buf
        .release("edit-rs-1")
        .await
        .expect("entry should still exist");
    assert_eq!(released.id, "edit-rs-1");
    assert_eq!(released.action, "propose"); // action preserved
    assert_eq!(
        released.reply_subject, original_reply_subject,
        "reply_subject must survive API edits"
    );
}

#[tokio::test]
async fn list_buffer_with_multiple_entries() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "m-1", "propose", b"{}").await;
    push_test_entry(&buf, "m-2", "evaluate", b"{}").await;
    push_test_entry(&buf, "m-3", "propose", b"{}").await;

    // Stop one entry
    buf.stop("m-2").await;

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer").await;
    assert_eq!(status, StatusCode::OK);
    let entries: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(entries.len(), 3);

    // Find the stopped one
    let stopped: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["stopped"].as_bool() == Some(true))
        .collect();
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0]["id"], "m-2");
}

// -----------------------------------------------------------------------
// Event logging verification
// -----------------------------------------------------------------------

#[tokio::test]
async fn stop_logs_event() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "evt-stop-1", "propose", b"{}").await;

    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/evt-stop-1/stop", "").await;
    assert_eq!(status, StatusCode::OK);

    // Check event was logged
    let snap = state.statuses["ALPHA"].read().await;
    let stop_events: Vec<_> = snap
        .event_log
        .iter()
        .filter(|e| e.event_type == "buffer_stopped")
        .collect();
    assert_eq!(stop_events.len(), 1);
    assert!(stop_events[0].detail.contains("stopped"));
}

#[tokio::test]
async fn unstop_logs_event() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "evt-unstop-1", "propose", b"{}").await;
    buf.stop("evt-unstop-1").await;

    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/evt-unstop-1/unstop", "").await;
    assert_eq!(status, StatusCode::OK);

    let snap = state.statuses["ALPHA"].read().await;
    let unstop_events: Vec<_> = snap
        .event_log
        .iter()
        .filter(|e| e.event_type == "buffer_unstopped")
        .collect();
    assert_eq!(unstop_events.len(), 1);
    assert!(unstop_events[0].detail.contains("eligible"));
}

#[tokio::test]
async fn release_logs_event() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "evt-rel-1", "propose", b"{}").await;

    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/evt-rel-1/release", "").await;
    assert_eq!(status, StatusCode::OK);

    let snap = state.statuses["ALPHA"].read().await;
    let release_events: Vec<_> = snap
        .event_log
        .iter()
        .filter(|e| e.event_type == "buffer_released")
        .collect();
    assert_eq!(release_events.len(), 1);
    assert!(release_events[0].detail.contains("release"));
}

#[tokio::test]
async fn reject_logs_event() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "evt-rej-1", "propose", b"{}").await;

    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/evt-rej-1/reject", "").await;
    assert_eq!(status, StatusCode::OK);

    let snap = state.statuses["ALPHA"].read().await;
    let reject_events: Vec<_> = snap
        .event_log
        .iter()
        .filter(|e| e.event_type == "buffer_rejected")
        .collect();
    assert_eq!(reject_events.len(), 1);
    assert!(reject_events[0].detail.contains("rejected"));
}

// -----------------------------------------------------------------------
// Regen / edit buffer preservation tests (API-level)
// -----------------------------------------------------------------------

/// Full regen flow via API: stop → edit → unstop → verify reply_subject preserved
#[tokio::test]
async fn regen_flow_preserves_reply_subject() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    let original_payload = serde_json::json!({"content": "original proposal"});
    push_test_entry(
        &buf,
        "regen-1",
        "propose",
        &serde_json::to_vec(&original_payload).unwrap(),
    )
    .await;

    // Capture original reply_subject from buffer
    let detail_before = buf.get_detail("regen-1").await.unwrap();
    assert_eq!(detail_before.content["content"], "original proposal");

    // Step 1: Stop the entry (simulates operator clicking "Re-generate")
    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/regen-1/stop", "").await;
    assert_eq!(status, StatusCode::OK);

    // Step 2: Edit with new content (simulates regen result being PUT)
    let app = build_router(state.clone());
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/buffer/regen-1",
        r#"{"content": {"content": "regenerated proposal v2"}, "operator_comment": "Regenerated by operator"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "edited");

    // Step 3: Unstop (makes it eligible for drain again)
    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/regen-1/unstop", "").await;
    assert_eq!(status, StatusCode::OK);

    // Verify: content changed, entry still in buffer, not stopped
    let detail_after = buf.get_detail("regen-1").await.unwrap();
    assert_eq!(detail_after.content["content"], "regenerated proposal v2");
    assert!(!detail_after.summary.stopped);
    assert_eq!(detail_after.summary.id, "regen-1");
    assert_eq!(detail_after.summary.action, "propose");
    assert_eq!(detail_after.summary.job_id, "job-test");
}

/// Verify multiple edits via API preserve entry identity
#[tokio::test]
async fn multiple_edits_preserve_entry_identity() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    let payload = serde_json::json!({"content": "v1"});
    push_test_entry(
        &buf,
        "multi-edit-1",
        "propose",
        &serde_json::to_vec(&payload).unwrap(),
    )
    .await;

    // Edit 1
    let app = build_router(state.clone());
    let (status, _) = put_json(
        app,
        "/api/agents/ALPHA/buffer/multi-edit-1",
        r#"{"content": {"content": "v2"}, "operator_comment": "First edit"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Edit 2
    let app = build_router(state.clone());
    let (status, _) = put_json(
        app,
        "/api/agents/ALPHA/buffer/multi-edit-1",
        r#"{"content": {"content": "v3"}, "operator_comment": "Second edit"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Comment only
    let app = build_router(state.clone());
    let (status, _) = put_json(
        app,
        "/api/agents/ALPHA/buffer/multi-edit-1",
        r#"{"operator_comment": "LGTM"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify final state
    let detail = buf.get_detail("multi-edit-1").await.unwrap();
    assert_eq!(detail.content["content"], "v3");
    assert_eq!(detail.summary.id, "multi-edit-1");
    assert_eq!(detail.summary.action, "propose");
    assert_eq!(detail.summary.job_id, "job-test");
}

/// Regen flow then release — verify the entry can be successfully released
#[tokio::test]
async fn regen_then_release_succeeds() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "regen-rel-1", "propose", b"{}").await;

    // Stop → edit → unstop (regen flow)
    buf.stop("regen-rel-1").await;
    let app = build_router(state.clone());
    let (status, _) = put_json(
        app,
        "/api/agents/ALPHA/buffer/regen-rel-1",
        r#"{"content": {"content": "regenerated"}, "operator_comment": "Regen"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    buf.unstop("regen-rel-1").await;

    // Release
    let app = build_router(state.clone());
    let (status, body) = post_json(app, "/api/agents/ALPHA/buffer/regen-rel-1/release", "").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "releasing");

    // Buffer should be drainable
    let drained = buf.drain_ready().await;
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].id, "regen-rel-1");
    assert_eq!(
        drained[0].reply_subject, "nsed.job-test.result.1.agent.propose",
        "reply_subject must be preserved through regen flow"
    );
}

/// Edit after reject should fail (entry was removed)
#[tokio::test]
async fn edit_after_reject_returns_404() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "rej-edit-1", "propose", b"{}").await;

    // Reject the entry
    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/rej-edit-1/reject", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(buf.len().await, 0);

    // Try to edit — should fail
    let app = build_router(state.clone());
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/buffer/rej-edit-1",
        r#"{"content": {"content": "too late"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

/// Stop after release should fail (entry was marked for release)
#[tokio::test]
async fn stop_after_release_drain_returns_404() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "sr-1", "propose", b"{}").await;

    // Release then drain (entry removed from buffer)
    buf.mark_for_release("sr-1").await;
    buf.drain_ready().await;

    // Try to stop — should fail (entry gone)
    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/sr-1/stop", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Event log ordering after full regen flow via API
#[tokio::test]
async fn regen_flow_event_ordering() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "evt-regen-1", "propose", b"{}").await;

    // Stop
    let app = build_router(state.clone());
    post_json(app, "/api/agents/ALPHA/buffer/evt-regen-1/stop", "").await;

    // Unstop
    let app = build_router(state.clone());
    post_json(app, "/api/agents/ALPHA/buffer/evt-regen-1/unstop", "").await;

    // Release
    let app = build_router(state.clone());
    post_json(app, "/api/agents/ALPHA/buffer/evt-regen-1/release", "").await;

    // Verify event ordering
    let snap = state.statuses["ALPHA"].read().await;
    let types: Vec<&str> = snap
        .event_log
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();
    assert_eq!(
        types,
        vec!["buffer_stopped", "buffer_unstopped", "buffer_released"],
        "events should be in chronological order"
    );
}

// -----------------------------------------------------------------------
// CodeRabbit fix: stop/unstop events use job_id, not entry_id
// -----------------------------------------------------------------------

#[tokio::test]
async fn stop_event_uses_job_id_not_entry_id() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "stop-jid-1", "propose", b"{}").await;

    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/stop-jid-1/stop", "").await;
    assert_eq!(status, StatusCode::OK);

    let snap = state.statuses["ALPHA"].read().await;
    let stop_event = snap
        .event_log
        .iter()
        .find(|e| e.event_type == "buffer_stopped")
        .expect("should have buffer_stopped event");
    // job_id should be "job-test" (from push_test_entry), NOT "stop-jid-1" (entry id)
    assert_eq!(
        stop_event.job_id.as_deref(),
        Some("job-test"),
        "stop event should use job_id not entry_id"
    );
}

#[tokio::test]
async fn unstop_event_uses_job_id_not_entry_id() {
    let state = test_state_with_buffer();
    let buf = state.buffers["ALPHA"].clone();
    push_test_entry(&buf, "unstop-jid-1", "propose", b"{}").await;
    buf.stop("unstop-jid-1").await;

    let app = build_router(state.clone());
    let (status, _) = post_json(app, "/api/agents/ALPHA/buffer/unstop-jid-1/unstop", "").await;
    assert_eq!(status, StatusCode::OK);

    let snap = state.statuses["ALPHA"].read().await;
    let unstop_event = snap
        .event_log
        .iter()
        .find(|e| e.event_type == "buffer_unstopped")
        .expect("should have buffer_unstopped event");
    assert_eq!(
        unstop_event.job_id.as_deref(),
        Some("job-test"),
        "unstop event should use job_id not entry_id"
    );
}

// -----------------------------------------------------------------------
// response_sla_secs accepts any value (no floor)
// -----------------------------------------------------------------------

#[tokio::test]
async fn put_config_sla_accepts_any_value() {
    let state = test_state_with_buffer();
    let buf = state.buffers.get("ALPHA").unwrap().clone();

    let app = build_router(state.clone());
    // Set SLA to 10s — accepted as-is (no floor)
    let (status, body) = put_json(app, "/api/config", r#"{"response_sla_secs": 10}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response_sla_secs"], 10);
    assert_eq!(state.response_sla_secs.load(Ordering::Relaxed), 10);
    assert_eq!(buf.response_sla().unwrap().as_secs(), 10);
}

#[tokio::test]
async fn put_config_sla_zero_is_passthrough() {
    let state = test_state_with_buffer();
    let _buf = state.buffers.get("ALPHA").unwrap().clone();

    let app = build_router(state.clone());
    // Setting SLA to 0 means pass-through (no buffering)
    let (status, body) = put_json(app, "/api/config", r#"{"response_sla_secs": 0}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response_sla_secs"], 0);
    assert_eq!(state.response_sla_secs.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn put_config_sla_above_minimum_preserved() {
    let state = test_state_with_buffer();

    let app = build_router(state.clone());
    // 600s > 300s minimum — should be preserved exactly
    let (status, body) = put_json(app, "/api/config", r#"{"response_sla_secs": 600}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response_sla_secs"], 600);
    assert_eq!(state.response_sla_secs.load(Ordering::Relaxed), 600);
}

// -----------------------------------------------------------------------
// CodeRabbit fix: config update rejects invalid values
// -----------------------------------------------------------------------

#[tokio::test]
async fn config_update_rejects_invalid_temperature() {
    let state = test_state_with_buffer();
    let app = build_router(state);
    let (status, body) = put_json(app, "/api/agents/ALPHA/config", r#"{"temperature": 5.0}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("temperature"));
}

#[tokio::test]
async fn config_update_rejects_negative_retries() {
    let state = test_state_with_buffer();
    let app = build_router(state);
    let (status, body) = put_json(app, "/api/agents/ALPHA/config", r#"{"max_retries": -1}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("max_retries"));
}

#[tokio::test]
async fn config_update_accepts_valid_values() {
    let state = test_state_with_buffer();
    let app = build_router(state);
    let (status, _) = put_json(
        app,
        "/api/agents/ALPHA/config",
        r#"{"temperature": 1.2, "max_retries": 3}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// -------------------------------------------------------------------
// Auto-approve endpoint tests
// -------------------------------------------------------------------

#[tokio::test]
async fn auto_approve_enable_returns_200() {
    let state = test_state_with_buffer();
    let app = build_router(state);
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/auto",
        r#"{"enabled": true, "threshold": 0.6}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["auto_approve"], true);
    assert!((json["threshold"].as_f64().unwrap() - 0.6).abs() < 0.01);
}

#[tokio::test]
async fn auto_approve_disable_returns_200() {
    let state = test_state_with_buffer();
    // First enable, then disable
    let buf = state.buffers.get("ALPHA").unwrap();
    buf.set_auto_approve(true);
    buf.set_auto_approve_threshold(0.7);

    let app = build_router(state);
    let (status, body) = put_json(app, "/api/agents/ALPHA/auto", r#"{"enabled": false}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["auto_approve"], false);
    // Threshold unchanged when not provided
    assert!((json["threshold"].as_f64().unwrap() - 0.7).abs() < 0.01);
}

#[tokio::test]
async fn auto_approve_unknown_agent_returns_404() {
    let app = build_router(test_state());
    let (status, body) = put_json(app, "/api/agents/UNKNOWN/auto", r#"{"enabled": true}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
}

#[tokio::test]
async fn auto_approve_invalid_threshold_returns_400() {
    let state = test_state_with_buffer();
    let app = build_router(state);
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/auto",
        r#"{"enabled": true, "threshold": 1.5}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("threshold"));
}

#[tokio::test]
async fn auto_approve_negative_threshold_returns_400() {
    let state = test_state_with_buffer();
    let app = build_router(state);
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/auto",
        r#"{"enabled": true, "threshold": -0.1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("threshold"));
}

#[tokio::test]
async fn list_agents_includes_auto_approve_fields() {
    let state = test_state_with_buffer();
    let buf = state.buffers.get("ALPHA").unwrap();
    buf.set_auto_approve(true);
    buf.set_auto_approve_threshold(0.3);

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents").await;
    assert_eq!(status, StatusCode::OK);
    let agents: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let alpha = agents.iter().find(|a| a["name"] == "ALPHA").unwrap();
    assert_eq!(alpha["auto_approve"], true);
    assert!((alpha["auto_approve_threshold"].as_f64().unwrap() - 0.3).abs() < 0.01);

    // BETA has no buffer → pass-through defaults surfaced as
    // auto_approve=true, threshold=1.0 (an agent with no buffer IS a
    // pass-through, semantically identical to a buffered agent under
    // the new ResponseBuffer defaults).
    let beta = agents.iter().find(|a| a["name"] == "BETA").unwrap();
    assert_eq!(beta["auto_approve"], true);
    assert!((beta["auto_approve_threshold"].as_f64().unwrap() - 1.0).abs() < 0.01);
}

#[tokio::test]
async fn auto_approve_emits_status_event() {
    let state = test_state_with_buffer();
    let app = build_router(state.clone());
    let (_status, _body) = put_json(
        app,
        "/api/agents/ALPHA/auto",
        r#"{"enabled": true, "threshold": 0.4}"#,
    )
    .await;

    let snap = state.statuses["ALPHA"].read().await;
    let last_event = snap.event_log.back().unwrap();
    assert_eq!(last_event.event_type, "auto_approve_enabled");
    assert!(last_event.detail.contains("threshold: 40%"));
}

// -------------------------------------------------------------------
// Auto-approve ALL endpoint tests
// -------------------------------------------------------------------

/// Build a test state with HITL buffers for BOTH agents.
fn test_state_with_both_buffers() -> MultiAppState {
    let mut state = test_state();
    let alpha_buf = Arc::new(ResponseBuffer::new(std::time::Duration::from_secs(30)));
    let beta_buf = Arc::new(ResponseBuffer::new(std::time::Duration::from_secs(30)));
    state.buffers.insert("ALPHA".to_string(), alpha_buf);
    state.buffers.insert("BETA".to_string(), beta_buf);
    state
}

#[tokio::test]
async fn auto_all_enable_returns_200() {
    let state = test_state_with_both_buffers();
    let app = build_router(state);
    let (status, body) = put_json(
        app,
        "/api/agents/auto-all",
        r#"{"enabled": true, "threshold": 0.6}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["auto_approve"], true);
    assert!((json["threshold"].as_f64().unwrap() - 0.6).abs() < 0.01);
    assert_eq!(json["count"], 2);
}

#[tokio::test]
async fn auto_all_disable_returns_200() {
    let state = test_state_with_both_buffers();
    // Pre-enable both
    for buf in state.buffers.values() {
        buf.set_auto_approve(true);
    }

    let app = build_router(state);
    let (status, body) = put_json(app, "/api/agents/auto-all", r#"{"enabled": false}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["auto_approve"], false);
    assert_eq!(json["count"], 2);
}

#[tokio::test]
async fn auto_all_propagates_to_all_buffers() {
    let state = test_state_with_both_buffers();
    let alpha_buf = state.buffers["ALPHA"].clone();
    let beta_buf = state.buffers["BETA"].clone();

    // The ResponseBuffer default is now auto_approve=true, so we must
    // explicitly disable first to prove the /auto-all PUT flips the
    // flag back on (otherwise the post-condition assertions would be
    // satisfied vacuously by the default state).
    alpha_buf.set_auto_approve(false);
    beta_buf.set_auto_approve(false);
    assert!(!alpha_buf.is_auto_approve());
    assert!(!beta_buf.is_auto_approve());

    let app = build_router(state);
    let (status, _) = put_json(
        app,
        "/api/agents/auto-all",
        r#"{"enabled": true, "threshold": 0.35}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Both should be on with the same threshold
    assert!(alpha_buf.is_auto_approve());
    assert!(beta_buf.is_auto_approve());
    assert!((alpha_buf.auto_approve_threshold() - 0.35).abs() < 0.01);
    assert!((beta_buf.auto_approve_threshold() - 0.35).abs() < 0.01);
}

#[tokio::test]
async fn auto_all_invalid_threshold_returns_400() {
    let state = test_state_with_both_buffers();
    let app = build_router(state);
    let (status, body) = put_json(
        app,
        "/api/agents/auto-all",
        r#"{"enabled": true, "threshold": 2.0}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("threshold"));
}

#[tokio::test]
async fn auto_all_negative_threshold_returns_400() {
    let state = test_state_with_both_buffers();
    let app = build_router(state);
    let (status, body) = put_json(
        app,
        "/api/agents/auto-all",
        r#"{"enabled": true, "threshold": -0.5}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("threshold"));
}

#[tokio::test]
async fn auto_all_emits_events_for_all_agents() {
    let state = test_state_with_both_buffers();
    let app = build_router(state.clone());
    let (status, _) = put_json(
        app,
        "/api/agents/auto-all",
        r#"{"enabled": true, "threshold": 0.5}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Both agents should have auto_approve_enabled events
    for name in &["ALPHA", "BETA"] {
        let snap = state.statuses[*name].read().await;
        let events: Vec<_> = snap
            .event_log
            .iter()
            .filter(|e| e.event_type == "auto_approve_enabled")
            .collect();
        assert_eq!(
            events.len(),
            1,
            "Agent {} should have exactly one auto_approve_enabled event",
            name
        );
        assert!(events[0].detail.contains("master control"));
    }
}

#[tokio::test]
async fn auto_all_empty_state_returns_zero_count() {
    let state = MultiAppState {
        statuses: HashMap::new(),
        chat_agents: HashMap::new(),
        configs: HashMap::new(),
        buffers: HashMap::new(),
        pause_handles: HashMap::new(),
        orchestrator_registry: None,
        base_hold_secs: Arc::new(AtomicU64::new(10)),
        response_sla_secs: Arc::new(AtomicU64::new(10)),
        buffer_floor_pct: Arc::new(AtomicU64::new(0)),
        before_release_middleware: None,
    };
    let app = build_router(state);
    let (status, body) = put_json(app, "/api/agents/auto-all", r#"{"enabled": true}"#).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["count"], 0);
}

#[tokio::test]
async fn auto_all_without_threshold_preserves_existing() {
    let state = test_state_with_both_buffers();
    let alpha_buf = state.buffers["ALPHA"].clone();
    // Set a custom threshold on ALPHA first
    alpha_buf.set_auto_approve_threshold(0.8);

    let app = build_router(state);
    // Enable without providing threshold
    let (status, _) = put_json(app, "/api/agents/auto-all", r#"{"enabled": true}"#).await;
    assert_eq!(status, StatusCode::OK);

    // ALPHA's custom threshold should be preserved (no threshold in request = no update)
    assert!(alpha_buf.is_auto_approve());
    assert!(
        (alpha_buf.auto_approve_threshold() - 0.8).abs() < 0.01,
        "threshold should be preserved when not specified in request"
    );
}

// =========================================================================
// E2E data contract tests — typed deserialization of API responses
// =========================================================================
//
// These tests validate that API responses match the exact data contracts
// the dashboard frontend depends on. Every field the JS code reads is
// verified via strongly-typed Rust deserialization.

/// Typed mirror of the AgentSummary struct for deserialization.
#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct AgentSummaryContract {
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
    score_std_dev: Option<f32>,
    avg_response_ms: Option<u64>,
    is_flagged: bool,
    flag_reason: Option<String>,
    auto_approve: bool,
    auto_approve_threshold: f32,
}

/// Typed mirror of AgentStatusSnapshot for deserialization.
#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct StatusContract {
    agent_id: String,
    model_name: String,
    provider_id: String,
    nats_connected: bool,
    current_job: Option<String>,
    current_round: Option<u32>,
    current_phase: Option<String>,
    uptime_secs: u64,
    tasks_completed: u64,
    tasks_failed: u64,
    recent_tasks: Vec<TaskLogContract>,
    scratchpad_keys: u64,
    event_log: Vec<EventLogContract>,
    is_paused: bool,
    buffered_count: u32,
    error_rate: f32,
    recent_scores: Vec<ScoreContract>,
    mean_score: Option<f32>,
    score_std_dev: Option<f32>,
    is_flagged: bool,
    flag_reason: Option<String>,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct TaskLogContract {
    timestamp: String,
    action: String,
    job_id: String,
    round: u32,
    status: String,
    duration_ms: u64,
    content_preview: Option<String>,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct EventLogContract {
    timestamp: String,
    event_type: String,
    job_id: Option<String>,
    detail: String,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct ScoreContract {
    timestamp: String,
    job_id: String,
    round: u32,
    evaluator: String,
    score: f32,
}

/// Typed mirror of BufferEntrySummary.
#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct BufferSummaryContract {
    id: String,
    action: String,
    job_id: String,
    round: u32,
    age_ms: u64,
    release_in_ms: i64,
    stopped: bool,
}

/// Typed mirror of BufferEntryDetail (flattened).
#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct BufferDetailContract {
    id: String,
    action: String,
    job_id: String,
    round: u32,
    age_ms: u64,
    release_in_ms: i64,
    stopped: bool,
    content: serde_json::Value,
}

// ── OpenAPI spec generation test ──

#[test]
fn openapi_spec_generates_without_panic() {
    use utoipa::OpenApi;
    let spec = super::api_docs::ApiDoc::openapi();
    let json = spec.to_json().unwrap();
    assert!(json.contains("\"openapi\":\"3.1.0\""));
    assert!(json.contains("AgentStatusSnapshot"));
    assert!(json.contains("BufferEntrySummary"));
    assert!(json.contains("TaskLogEntry"));
    assert!(json.contains("/api/agents"));
    assert!(json.contains("/api/agents/{name}/status"));
    assert!(json.contains("/api/agents/{name}/buffer"));
}

// ── Swagger UI endpoint test ──

#[tokio::test]
async fn swagger_ui_endpoint_returns_html() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/swagger-ui/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("swagger") || body.contains("Swagger"),
        "swagger-ui response should contain Swagger references"
    );
}

#[tokio::test]
async fn openapi_json_endpoint_returns_spec() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api-docs/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    let spec: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(spec["openapi"], "3.1.0");
    assert!(spec["paths"]["/api/agents"].is_object());
    assert!(spec["paths"]["/api/agents/{name}/status"].is_object());
}

// ── /api/agents typed contract ──

#[tokio::test]
async fn contract_list_agents_typed_deserialization() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/agents").await;
    assert_eq!(status, StatusCode::OK);

    // Must deserialize into typed struct — validates all field names and types
    let agents: Vec<AgentSummaryContract> =
        serde_json::from_str(&body).expect("Failed to deserialize /api/agents into typed contract");
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0].name, "ALPHA");
    assert_eq!(agents[1].name, "BETA");
    assert!(!agents[0].is_paused);
    assert_eq!(agents[0].buffered_count, 0);
    assert!(agents[0].mean_score.is_none());
}

// ── /api/agents/{name}/status typed contract ──

#[tokio::test]
async fn contract_status_snapshot_typed_deserialization() {
    use crate::status::TaskLogEntry;

    let state = test_state();
    {
        let mut snap = state.statuses["ALPHA"].write().await;
        snap.nats_connected = true;
        snap.current_job = Some("job-xyz".into());
        snap.current_round = Some(3);
        snap.current_phase = Some("evaluate".into());
        snap.push_task(TaskLogEntry {
            timestamp: "2025-01-01T00:00:00Z".into(),
            action: "propose".into(),
            job_id: "job-xyz".into(),
            round: 2,
            status: "ok".into(),
            duration_ms: 1500,
            content_preview: Some(r#"{"t":"p","c":"Hello world"}"#.into()),
        });
        snap.push_event("agent_working", Some("job-xyz"), "Round 3 evaluate");
    }
    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/status").await;
    assert_eq!(status, StatusCode::OK);

    let snap: StatusContract = serde_json::from_str(&body)
        .expect("Failed to deserialize /api/agents/ALPHA/status into typed contract");

    assert_eq!(snap.agent_id, "ALPHA");
    assert!(snap.nats_connected);
    assert_eq!(snap.current_job.as_deref(), Some("job-xyz"));
    assert_eq!(snap.current_round, Some(3));
    assert_eq!(snap.current_phase.as_deref(), Some("evaluate"));
    assert_eq!(snap.tasks_completed, 1);
    assert_eq!(snap.tasks_failed, 0);

    // recent_tasks: LIFO — most recent at index 0
    assert_eq!(snap.recent_tasks.len(), 1);
    let task = &snap.recent_tasks[0];
    assert_eq!(task.action, "propose");
    assert_eq!(task.job_id, "job-xyz");
    assert_eq!(task.round, 2);
    assert_eq!(task.status, "ok");
    assert_eq!(task.duration_ms, 1500);
    assert!(task.content_preview.is_some());

    // content_preview is a JSON string — the frontend parses it
    let preview: serde_json::Value =
        serde_json::from_str(task.content_preview.as_ref().unwrap()).unwrap();
    assert_eq!(preview["t"], "p");
    assert_eq!(preview["c"], "Hello world");

    // event_log
    assert!(!snap.event_log.is_empty());
    assert_eq!(snap.event_log.last().unwrap().event_type, "agent_working");
}

// ── /api/agents/{name}/status — recent_tasks LIFO ordering ──

#[tokio::test]
async fn contract_status_recent_tasks_lifo_ordering() {
    use crate::status::TaskLogEntry;

    let state = test_state();
    {
        let mut snap = state.statuses["ALPHA"].write().await;
        for i in 1..=5 {
            snap.push_task(TaskLogEntry {
                timestamp: format!("2025-01-01T00:00:{:02}Z", i),
                action: if i % 2 == 0 { "evaluate" } else { "propose" }.into(),
                job_id: "job-lifo".into(),
                round: i,
                status: "ok".into(),
                duration_ms: 100 * i as u64,
                content_preview: None,
            });
        }
    }
    let app = build_router(state);
    let (_, body) = get_request(app, "/api/agents/ALPHA/status").await;
    let snap: StatusContract = serde_json::from_str(&body).unwrap();

    assert_eq!(snap.recent_tasks.len(), 5);
    // Most recent (round 5) should be at index 0
    assert_eq!(snap.recent_tasks[0].round, 5);
    assert_eq!(snap.recent_tasks[4].round, 1);
    // Verify ordering is strictly newest-first
    for i in 0..4 {
        assert!(
            snap.recent_tasks[i].round > snap.recent_tasks[i + 1].round,
            "recent_tasks[{}].round={} should be > recent_tasks[{}].round={}",
            i,
            snap.recent_tasks[i].round,
            i + 1,
            snap.recent_tasks[i + 1].round
        );
    }
}

// ── /api/agents/{name}/status — scores and flagging ──

#[tokio::test]
async fn contract_status_scores_and_flagging() {
    use crate::status::ScoreEntry;

    let state = test_state();
    {
        let mut snap = state.statuses["ALPHA"].write().await;
        snap.push_score(ScoreEntry {
            timestamp: "t1".into(),
            job_id: "j1".into(),
            round: 1,
            evaluator: "BETA".into(),
            score: -0.5,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t2".into(),
            job_id: "j1".into(),
            round: 1,
            evaluator: "GAMMA".into(),
            score: -0.6,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t3".into(),
            job_id: "j1".into(),
            round: 2,
            evaluator: "BETA".into(),
            score: -0.4,
        });
    }
    let app = build_router(state);
    let (_, body) = get_request(app, "/api/agents/ALPHA/status").await;
    let snap: StatusContract = serde_json::from_str(&body).unwrap();

    assert_eq!(snap.recent_scores.len(), 3);
    assert!(snap.mean_score.is_some());
    assert!(snap.is_flagged, "low scores should trigger flagging");
    assert!(snap.flag_reason.is_some());
    assert!(
        snap.flag_reason.as_ref().unwrap().contains("Low scores"),
        "flag_reason: {:?}",
        snap.flag_reason
    );
}

// ── /api/agents/{name}/buffer — typed contract ──

#[tokio::test]
async fn contract_buffer_list_typed_deserialization() {
    use crate::workers::buffer::BufferedResponse;

    let state = test_state_with_buffer();
    let buf = state.buffers.get("ALPHA").unwrap();
    let now = std::time::Instant::now();
    let payload = serde_json::json!({"content": "test proposal", "thought_process": "thinking..."});
    buf.push(BufferedResponse {
        id: "buf-001".into(),
        action: "propose".into(),
        job_id: "job-abc".into(),
        round: 2,
        reply_subject: "nsed.job-abc.result.2.ALPHA.propose".into(),
        payload: serde_json::to_vec(&payload).unwrap(),
        created_at: now,
        release_at: now + std::time::Duration::from_secs(300),
        ack_handle: Box::new(NoopAck),
        msg_id: "msg-001".into(),
        annotations: vec![],
        edited: false,
        stopped: false,
    })
    .await;

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer").await;
    assert_eq!(status, StatusCode::OK);

    let entries: Vec<BufferSummaryContract> = serde_json::from_str(&body)
        .expect("Failed to deserialize /api/agents/ALPHA/buffer into typed contract");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.id, "buf-001");
    assert_eq!(entry.action, "propose");
    assert_eq!(entry.job_id, "job-abc");
    assert_eq!(entry.round, 2);
    assert!(!entry.stopped);
    assert!(
        entry.release_in_ms > 0,
        "should have positive release_in_ms"
    );
}

// ── /api/agents/{name}/buffer/{id} — typed detail contract ──

#[tokio::test]
async fn contract_buffer_detail_typed_deserialization() {
    use crate::workers::buffer::BufferedResponse;

    let state = test_state_with_buffer();
    let buf = state.buffers.get("ALPHA").unwrap();
    let now = std::time::Instant::now();
    // Evaluation payload: [[target_agent, eval_obj], ...]
    let eval_payload = serde_json::json!([
        ["BETA", {"score": 7.5, "justification": "Good proposal"}],
        ["GAMMA", {"score": 4.0, "justification": "Needs work"}]
    ]);
    buf.push(BufferedResponse {
        id: "buf-eval-001".into(),
        action: "evaluate".into(),
        job_id: "job-eval".into(),
        round: 3,
        reply_subject: "nsed.job-eval.result.3.ALPHA.evaluate".into(),
        payload: serde_json::to_vec(&eval_payload).unwrap(),
        created_at: now,
        release_at: now + std::time::Duration::from_secs(300),
        ack_handle: Box::new(NoopAck),
        msg_id: "msg-eval-001".into(),
        annotations: vec![],
        edited: false,
        stopped: false,
    })
    .await;

    let app = build_router(state);
    let (status, body) = get_request(app, "/api/agents/ALPHA/buffer/buf-eval-001").await;
    assert_eq!(status, StatusCode::OK);

    let detail: BufferDetailContract = serde_json::from_str(&body)
        .expect("Failed to deserialize buffer detail into typed contract");
    assert_eq!(detail.id, "buf-eval-001");
    assert_eq!(detail.action, "evaluate");
    assert_eq!(detail.job_id, "job-eval");
    assert_eq!(detail.round, 3);
    assert!(!detail.stopped);

    // content is the raw evaluation payload — frontend extracts target agents from this
    let content = &detail.content;
    assert!(content.is_array(), "evaluation content should be array");
    let evals = content.as_array().unwrap();
    assert_eq!(evals.len(), 2);
    assert_eq!(evals[0][0], "BETA");
    assert_eq!(evals[1][0], "GAMMA");
    assert_eq!(evals[0][1]["score"], 7.5);
}

// ── /api/agents/{name}/status — content_preview formats ──

#[tokio::test]
async fn contract_status_content_preview_proposal_format() {
    use crate::status::TaskLogEntry;

    let state = test_state();
    {
        let mut snap = state.statuses["ALPHA"].write().await;
        snap.push_task(TaskLogEntry {
            timestamp: "2025-01-01T00:00:00Z".into(),
            action: "propose".into(),
            job_id: "job-cp".into(),
            round: 1,
            status: "ok".into(),
            duration_ms: 500,
            content_preview: Some(
                serde_json::json!({"t": "p", "c": "My proposal content", "tp": "My reasoning"})
                    .to_string(),
            ),
        });
    }
    let app = build_router(state);
    let (_, body) = get_request(app, "/api/agents/ALPHA/status").await;
    let snap: StatusContract = serde_json::from_str(&body).unwrap();

    let preview_str = snap.recent_tasks[0].content_preview.as_ref().unwrap();
    let preview: serde_json::Value = serde_json::from_str(preview_str).unwrap();
    assert_eq!(preview["t"], "p", "type marker should be 'p' for proposal");
    assert_eq!(preview["c"], "My proposal content");
    assert_eq!(preview["tp"], "My reasoning");
}

#[tokio::test]
async fn contract_status_content_preview_evaluation_format() {
    use crate::status::TaskLogEntry;

    let state = test_state();
    {
        let mut snap = state.statuses["ALPHA"].write().await;
        snap.push_task(TaskLogEntry {
            timestamp: "2025-01-01T00:00:00Z".into(),
            action: "evaluate".into(),
            job_id: "job-cp".into(),
            round: 1,
            status: "ok".into(),
            duration_ms: 800,
            content_preview: Some(
                serde_json::json!({
                    "t": "e",
                    "evals": [
                        {"target": "BETA", "s": 7.5, "j": "Good work"},
                        {"target": "GAMMA", "s": 3.0, "j": "Needs improvement"}
                    ]
                })
                .to_string(),
            ),
        });
    }
    let app = build_router(state);
    let (_, body) = get_request(app, "/api/agents/ALPHA/status").await;
    let snap: StatusContract = serde_json::from_str(&body).unwrap();

    let preview_str = snap.recent_tasks[0].content_preview.as_ref().unwrap();
    let preview: serde_json::Value = serde_json::from_str(preview_str).unwrap();
    assert_eq!(
        preview["t"], "e",
        "type marker should be 'e' for evaluation"
    );
    let evals = preview["evals"].as_array().unwrap();
    assert_eq!(evals.len(), 2);
    assert_eq!(evals[0]["target"], "BETA");
    assert_eq!(evals[0]["s"], 7.5);
    assert_eq!(evals[1]["target"], "GAMMA");
}

// ── /api/config typed contract ──

#[tokio::test]
async fn contract_global_config_typed_deserialization() {
    #[derive(serde::Deserialize, Debug)]
    #[allow(dead_code)]
    struct GlobalConfigContract {
        base_hold_secs: u64,
        response_sla_secs: u64,
        buffer_floor_pct: u64,
    }

    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/config").await;
    assert_eq!(status, StatusCode::OK);

    let config: GlobalConfigContract =
        serde_json::from_str(&body).expect("Failed to deserialize /api/config into typed contract");
    assert_eq!(config.base_hold_secs, 10);
    assert_eq!(config.response_sla_secs, 10);
    assert_eq!(config.buffer_floor_pct, 0);
}

// -----------------------------------------------------------------------
// Agent Config Management API (registration_handlers) integration tests
// -----------------------------------------------------------------------

/// Helper: perform a PATCH request with JSON body.
async fn patch_json(app: Router, uri: &str, json: &str) -> (StatusCode, String) {
    let req = axum::http::Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(json.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// Helper: perform a DELETE request.
async fn delete_request(app: Router, uri: &str) -> (StatusCode, String) {
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn register_agent_returns_501_pending_manager() {
    let app = build_router(test_state());
    let (status, body) = post_json(
        app,
        "/api/agents/register",
        r#"{"name":"NEW_AGENT","provider_id":"test","model_name":"gpt-4"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["name"], "NEW_AGENT");
    assert!(json["validated"].as_bool().unwrap());
}

#[tokio::test]
async fn register_agent_rejects_empty_name() {
    let app = build_router(test_state());
    let (status, body) = post_json(
        app,
        "/api/agents/register",
        r#"{"name":"","provider_id":"test","model_name":"gpt-4"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("1-64 characters"));
}

#[tokio::test]
async fn register_agent_rejects_duplicate() {
    let app = build_router(test_state());
    let (status, _) = post_json(
        app,
        "/api/agents/register",
        r#"{"name":"ALPHA","provider_id":"test","model_name":"gpt-4"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn replace_agent_updates_config() {
    let state = test_state();
    let app = build_router(state.clone());
    let (status, body) = put_json(
        app,
        "/api/agents/ALPHA/manage",
        r#"{"name":"ALPHA","provider_id":"new_provider","model_name":"new-model","persona":"new persona"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["name"], "ALPHA");

    // Verify the config was actually updated
    let config = state.configs.get("ALPHA").unwrap().read().await;
    assert_eq!(config.provider_id, "new_provider");
    assert_eq!(config.model_name, "new-model");
}

#[tokio::test]
async fn replace_agent_404_for_unknown() {
    let app = build_router(test_state());
    let (status, _) = put_json(
        app,
        "/api/agents/NONEXISTENT/manage",
        r#"{"name":"NONEXISTENT","provider_id":"p","model_name":"m"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_agent_partial_update() {
    let state = test_state();
    let app = build_router(state.clone());
    let (status, _) = patch_json(
        app,
        "/api/agents/ALPHA/manage",
        r#"{"persona":"updated persona","capability_tags":["legal","audit"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify only the patched fields changed
    let config = state.configs.get("ALPHA").unwrap().read().await;
    assert_eq!(config.persona.as_deref(), Some("updated persona"));
    assert_eq!(config.capability_tags, vec!["legal", "audit"]);
    // Original fields should be unchanged
    assert_eq!(config.provider_id, "together_ai");
    assert_eq!(config.model_name, "MiniMax-M2.5");
}

#[tokio::test]
async fn patch_agent_404_for_unknown() {
    let app = build_router(test_state());
    let (status, _) = patch_json(app, "/api/agents/GHOST/manage", r#"{"persona":"nope"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_agent_returns_501() {
    let state = test_state();
    assert!(state.configs.contains_key("BETA"));
    let app = build_router(state.clone());
    let (status, body) = delete_request(app, "/api/agents/BETA/manage").await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["id"], "BETA");
    assert!(json["validated"].as_bool().unwrap());
}

#[tokio::test]
async fn delete_agent_404_for_unknown() {
    let app = build_router(test_state());
    let (status, _) = delete_request(app, "/api/agents/GHOST/manage").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bulk_register_validates_and_returns_501() {
    let app = build_router(test_state());
    let (status, body) = post_json(
        app,
        "/api/agents/bulk",
        r#"{"agents":[
            {"name":"BULK_A","provider_id":"p","model_name":"m"},
            {"name":"BULK_B","provider_id":"p","model_name":"m"}
        ]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let validated = json["validated"].as_array().unwrap();
    assert_eq!(validated.len(), 2);
}

#[tokio::test]
async fn bulk_register_reports_duplicates() {
    let app = build_router(test_state());
    let (status, body) = post_json(
        app,
        "/api/agents/bulk",
        r#"{"agents":[
            {"name":"ALPHA","provider_id":"p","model_name":"m"},
            {"name":"NEW_ONE","provider_id":"p","model_name":"m"}
        ]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let errors = json["validation_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1); // ALPHA is duplicate
    let validated = json["validated"].as_array().unwrap();
    assert_eq!(validated.len(), 1); // NEW_ONE validated
}

#[tokio::test]
async fn bulk_register_rejects_in_request_duplicates() {
    let app = build_router(test_state());
    let (status, body) = post_json(
        app,
        "/api/agents/bulk",
        r#"{"agents":[
            {"name":"DUP","provider_id":"p","model_name":"m"},
            {"name":"DUP","provider_id":"p","model_name":"m"}
        ]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let errors = json["validation_errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().unwrap().contains("duplicate"))
    );
}

#[tokio::test]
async fn register_rejects_nats_unsafe_name() {
    let app = build_router(test_state());
    let (status, _) = post_json(
        app,
        "/api/agents/register",
        r#"{"name":"bad.name","provider_id":"p","model_name":"m"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_agents_includes_all() {
    let app = build_router(test_state());
    let (status, body) = get_request(app, "/api/agents").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let agents = json.as_array().unwrap();
    // test_state has ALPHA and BETA
    assert!(agents.len() >= 2);
}

#[test]
fn resolve_dashboard_bind_falls_back_to_loopback_on_none() {
    assert_eq!(
        super::resolve_dashboard_bind(None),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[test]
fn resolve_dashboard_bind_parses_lan_address() {
    assert_eq!(
        super::resolve_dashboard_bind(Some("0.0.0.0")),
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
    );
}

#[test]
fn resolve_dashboard_bind_parses_specific_iface() {
    assert_eq!(
        super::resolve_dashboard_bind(Some("192.168.1.42")),
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 42))
    );
}

/// Malformed env-var input must NOT panic — silent fallback to
/// loopback so a typo doesn't take the dashboard offline (operator
/// will see the loopback bind in the info-log + correct from there).
#[test]
fn resolve_dashboard_bind_falls_back_on_garbage() {
    assert_eq!(
        super::resolve_dashboard_bind(Some("not-an-ip")),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[test]
fn diagnostics_from_snapshot_surfaces_errors_and_metrics() {
    use crate::status::{AgentStatusSnapshot, EventLogEntry, TaskLogEntry};
    let mut snap = AgentStatusSnapshot::new("Corepunk18".into(), "gpt".into(), "openrouter".into());
    snap.tasks_completed = 3;
    snap.tasks_failed = 5;
    snap.error_rate = 0.625;
    snap.is_paused = true;
    snap.is_flagged = true;
    snap.flag_reason = Some("score divergence".into());
    snap.event_log.push_back(EventLogEntry {
        timestamp: "t1".into(),
        event_type: "agent_error".into(),
        job_id: Some("j".into()),
        detail: "API request failed with status 404".into(),
    });
    snap.event_log.push_back(EventLogEntry {
        timestamp: "t2".into(),
        event_type: "task_complete".into(),
        job_id: None,
        detail: "ok".into(),
    });
    snap.recent_tasks.push_back(TaskLogEntry {
        timestamp: "t".into(),
        action: "evaluate".into(),
        job_id: "j".into(),
        round: 4,
        status: "error".into(),
        duration_ms: 10,
        content_preview: None,
    });
    snap.recent_tasks.push_back(TaskLogEntry {
        timestamp: "t".into(),
        action: "propose".into(),
        job_id: "j".into(),
        round: 4,
        status: "ok".into(),
        duration_ms: 10,
        content_preview: None,
    });

    let d = super::diagnostics_from_snapshot("Corepunk18", &snap);
    assert_eq!(d.tasks_failed, 5);
    assert_eq!(d.tasks_completed, 3);
    assert!(d.is_paused, "paused state surfaced");
    assert!(d.is_flagged);
    assert_eq!(d.flag_reason.as_deref(), Some("score divergence"));
    // Only agent_error events surface, carrying their detail.
    assert_eq!(d.recent_errors.len(), 1);
    assert_eq!(
        d.recent_errors[0].detail,
        "API request failed with status 404"
    );
    // Only status=="error" tasks surface.
    assert_eq!(d.recent_failed_tasks.len(), 1);
    assert_eq!(d.recent_failed_tasks[0].action, "evaluate");
}
