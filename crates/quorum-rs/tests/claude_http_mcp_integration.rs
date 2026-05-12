//! Integration tests for the in-process HTTP MCP server used by ClaudeAgent.
//!
//! These tests start a real HTTP MCP server, connect as an rmcp client, and
//! verify tool listing, tool calls, and result delivery — the same path that
//! Claude CLI takes when connected via `"type": "http"` in `--mcp-config`.

use quorum_rs::agents::mcp_agent::{ActivePhase, ClaudeAgent};
use quorum_rs::agents::{AgentContext, CandidateProposal, DeliberationPhase, Proposal};

fn minimal_context() -> AgentContext {
    AgentContext {
        task_description: "Test deliberation task".to_string(),
        round_number: 1,
        total_rounds: 3,
        phase: DeliberationPhase::Proposing,
        phase_budget_remaining_secs: 60.0,
        session_id: Some("test-session".into()),
        ..Default::default()
    }
}

fn eval_context() -> AgentContext {
    AgentContext {
        task_description: "Test eval task".to_string(),
        round_number: 1,
        total_rounds: 3,
        phase: DeliberationPhase::Evaluating,
        phase_budget_remaining_secs: 60.0,
        candidates: vec![CandidateProposal {
            id: "AGENT_A".to_string(),
            proposal: Proposal {
                content: "Proposal A content".to_string(),
                thought_process: "Reasoning A".to_string(),
                ..Default::default()
            },
        }],
        ..Default::default()
    }
}

// ─── HTTP server lifecycle ───────────────────────────────────────────────────

#[tokio::test]
async fn http_server_binds_and_responds() {
    let ctx = minimal_context();
    let (port, ct, _rx) = ClaudeAgent::start_http_mcp_server(&ctx, ActivePhase::Proposing)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    ct.cancel();
}

#[tokio::test]
async fn http_server_shuts_down_on_cancel() {
    let ctx = minimal_context();
    let (port, ct, _rx) = ClaudeAgent::start_http_mcp_server(&ctx, ActivePhase::Proposing)
        .await
        .unwrap();

    ct.cancel();

    // Server should become unreachable within a reasonable timeout
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap();
    let check = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        // Retry briefly — server may need a moment to unbind
        loop {
            let result = client
                .post(format!("http://127.0.0.1:{port}/mcp"))
                .header("Content-Type", "application/json")
                .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#)
                .send()
                .await;
            if result.is_err() {
                return; // Server is down
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        check.is_ok(),
        "server should become unreachable after cancel within 2s"
    );
}

// ─── MCP config generation ──────────────────────────────────────────────────

#[test]
fn mcp_config_uses_http_transport() {
    let f = ClaudeAgent::write_mcp_config_http(8080).unwrap();
    let content = std::fs::read_to_string(f.path()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(parsed["mcpServers"]["nsed"]["type"], "http");
    assert_eq!(
        parsed["mcpServers"]["nsed"]["url"],
        "http://127.0.0.1:8080/mcp"
    );
    // Must NOT contain stdio-related fields
    assert!(parsed["mcpServers"]["nsed"]["command"].is_null());
    assert!(parsed["mcpServers"]["nsed"]["args"].is_null());
    assert!(parsed["mcpServers"]["nsed"]["env"].is_null());
}

// ─── Tool listing via raw HTTP ──────────────────────────────────────────────

#[tokio::test]
async fn tools_list_returns_nsed_tools_propose_phase() {
    let ctx = minimal_context();
    let (port, ct, _rx) = ClaudeAgent::start_http_mcp_server(&ctx, ActivePhase::Proposing)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}/mcp");
    let session_id = init_session(&client, &base).await;

    // List tools
    let mut req = client
        .post(&base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    if let Some(ref sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }
    let tools_resp = req.send().await.unwrap();
    let tools_body = tools_resp.text().await.unwrap();

    // During propose phase, nsed_propose should be available
    assert!(
        tools_body.contains("nsed_propose"),
        "propose tool should be listed in propose phase, got: {tools_body}"
    );
    assert!(
        tools_body.contains("nsed_get_context"),
        "get_context tool should always be listed"
    );

    ct.cancel();
}

#[tokio::test]
async fn tools_list_returns_nsed_tools_evaluate_phase() {
    let ctx = eval_context();
    let (port, ct, _rx) = ClaudeAgent::start_http_mcp_server(&ctx, ActivePhase::Evaluating)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}/mcp");
    let session_id = init_session(&client, &base).await;

    // List tools
    let mut req = client
        .post(&base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    if let Some(ref sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }
    let tools_resp = req.send().await.unwrap();
    let tools_body = tools_resp.text().await.unwrap();

    assert!(
        tools_body.contains("nsed_evaluate"),
        "evaluate tool should be listed in evaluate phase, got: {tools_body}"
    );
    assert!(
        tools_body.contains("nsed_get_context"),
        "get_context tool should always be listed"
    );

    ct.cancel();
}

// ─── Tool call: nsed_propose delivers result via channel ─────────────────────

#[tokio::test]
async fn propose_tool_delivers_result_via_channel() {
    let ctx = minimal_context();
    let (port, ct, result_rx) = ClaudeAgent::start_http_mcp_server(&ctx, ActivePhase::Proposing)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}/mcp");

    let session_id = init_session(&client, &base).await;

    // Call nsed_propose tool
    let propose_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "nsed_propose",
            "arguments": {
                "thought_process": "My analysis of the task",
                "content": "My proposal content"
            }
        }
    });
    let mut req = client
        .post(&base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(propose_body.to_string());
    if let Some(ref sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }
    let _ = req.send().await.unwrap();

    // Wait for result with timeout instead of fixed sleep
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), result_rx)
        .await
        .expect("result should arrive within 2s")
        .expect("result channel should not be dropped");
    match result {
        quorum_rs::agents::mcp_tools::McpResult::Proposal {
            thought_process,
            content,
        } => {
            assert_eq!(thought_process, "My analysis of the task");
            assert_eq!(content, "My proposal content");
        }
        _ => panic!("expected Proposal result"),
    }

    ct.cancel();
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Extract Mcp-Session-Id from HTTP response headers.
fn extract_session_id_from_resp(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Initialize an MCP session and return the session ID.
async fn init_session(client: &reqwest::Client, base: &str) -> Option<String> {
    let resp = client
        .post(base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#)
        .send()
        .await
        .unwrap();
    let session_id = extract_session_id_from_resp(&resp);
    // Consume body
    let _ = resp.text().await;

    // Send initialized notification
    let mut req = client
        .post(base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    if let Some(ref sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }
    let _ = req.send().await.unwrap();

    session_id
}
