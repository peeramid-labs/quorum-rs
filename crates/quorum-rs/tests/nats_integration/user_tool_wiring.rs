//! TRUE end-to-end: the `quorum serve` fleet path must advertise a job's `ask_user`.
//!
//! Regression guard for the gap that survived weeks of tests: `serve::build_worker` built
//! workers WITHOUT a user-tool handler factory, so a job's `ask_user` arrived in the
//! AgentContext but no handler was constructed — `McpAgent::user_tools` stayed empty and the
//! claude agent never saw `mcp__nsed__user_ask_user`. The prior tests hand-wired a handler
//! onto the context, so they could not catch this: they skipped the exact seam that was
//! broken. This one drives the REAL `build_worker`, takes the factory it wired, and checks
//! the tool is actually advertised over MCP. Needs NATS.

use super::common::{nats_url, try_connect_nats};
use quorum_rs::agents::mcp_agent::{ActivePhase, ClaudeAgent};
use quorum_rs::agents::{AgentContext, DeliberationPhase, UserToolDefinition};
use quorum_rs::providers::ProviderRegistry;
use quorum_rs::serve::build_worker;

fn claude_fleet() -> quorum_rs::config::AgentFleetConfig {
    serde_yaml::from_str(
        "providers:\n  \
           claude_cli:\n    type: claude\n\
         agents:\n  \
           - name: TestBot\n    provider_id: claude_cli\n    model_name: haiku\n",
    )
    .expect("fleet yaml")
}

fn ask_user_def() -> UserToolDefinition {
    UserToolDefinition {
        name: "ask_user".to_string(),
        description: "Ask the operator and wait.".to_string(),
        parameters: None,
        strict: None,
    }
}

/// `tools/list` body from the MCP server the claude agent would connect to for `ctx`.
async fn advertised_tools_body(ctx: &AgentContext) -> String {
    let (port, ct, _rx) = ClaudeAgent::start_http_mcp_server(ctx, ActivePhase::Proposing)
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}/mcp");
    let init = client
        .post(&base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#)
        .send()
        .await
        .unwrap();
    let sid = init
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let _ = init.text().await;
    let mut n = client
        .post(&base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    if let Some(ref s) = sid {
        n = n.header("Mcp-Session-Id", s);
    }
    let _ = n.send().await.unwrap();
    let mut l = client
        .post(&base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    if let Some(ref s) = sid {
        l = l.header("Mcp-Session-Id", s);
    }
    let body = l.send().await.unwrap().text().await.unwrap();
    ct.cancel();
    body
}

#[tokio::test]
async fn serve_build_worker_advertises_ask_user_end_to_end() {
    let Some(nats) = try_connect_nats().await else {
        return;
    };

    // The REAL fleet path a `quorum serve` run takes for a claude agent.
    let (worker, _cfg) = build_worker(
        &claude_fleet(),
        "TestBot",
        &nats_url(),
        None,
        "sphera_jobs",
        "sphera",
        &ProviderRegistry::with_builtins(),
    )
    .await
    .expect("build_worker")
    .expect("claude agent must build");

    // THE regression: the serve path must wire a user-tool factory. Without it every
    // job-carried user tool (ask_user) is silently dropped — arrives, never advertised.
    let factory = worker.user_tool_factory().expect(
        "serve::build_worker must wire a user-tool factory (else ask_user is never advertised)",
    );

    // Drive that factory exactly as the worker does at job setup, then confirm the tool is
    // actually advertised over MCP as `user_ask_user`.
    let js = async_nats::jetstream::new(nats.clone());
    let handler = factory.create(
        nats.clone(),
        js,
        "sess-1".to_string(),
        "TestBot".to_string(),
        60.0,
        "sphera".to_string(),
    );
    let ctx = AgentContext {
        phase: DeliberationPhase::Proposing,
        phase_budget_remaining_secs: 60.0,
        session_id: Some("sess-1".into()),
        user_tools: vec![ask_user_def()],
        user_tool_handler: Some(handler),
        ..Default::default()
    };

    let body = advertised_tools_body(&ctx).await;
    assert!(
        body.contains("user_ask_user"),
        "ask_user must be advertised as `user_ask_user` through the real serve path; got: {body}"
    );
}
