//! Does a `claude_cli` agent actually get the `ask_user` HITL tool?
//!
//! This pins the exact path Claude CLI takes: the in-process MCP server (`--mcp-config`,
//! `"type":"http"`) advertises user-tools, and Claude namespaces them.
//!
//! The fact that burned three debug rounds: a user-tool named `ask_user` is advertised as
//! **`user_ask_user`** (`UserToolDefinition.name` doc: "presented to the LLM with a `user_`
//! prefix"), which Claude Code then namespaces to **`mcp__nsed__user_ask_user`**. So an
//! agent searching the bare name `ask_user` — or a `ToolSearch` that doesn't index MCP
//! tools at all — will never find it. It IS there, under a different name.
//!
//! Two layers:
//! 1. deterministic (no claude, CI-safe): the server's `tools/list` includes
//!    `user_ask_user` iff a `user_tool_handler` is wired, and never the bare `ask_user`.
//! 2. real Claude CLI (env-gated `RUN_CLAUDE_CLI=1`): a live `claude --mcp-config` run
//!    reports `mcp__nsed__user_ask_user` among its tools.

use async_trait::async_trait;
use quorum_rs::agents::mcp_agent::{ActivePhase, ClaudeAgent};
use quorum_rs::agents::{
    AgentContext, DeliberationPhase, UserToolDefinition, UserToolHandlerTrait,
};
use std::sync::Arc;

/// A handler that echoes the call back so a test can prove the tool was actually invoked
/// (with the arguments the model sent), instead of blocking on a real operator answer.
#[derive(Debug)]
// TODO(slop): placeholder identifier — pick a name that says what this is
struct EchoingUserToolHandler;

#[async_trait]
impl UserToolHandlerTrait for EchoingUserToolHandler {
    // TODO(slop): placeholder identifier — pick a name that says what this is
    async fn handle_call(
        &self,
        tool_name: &str,
        arguments_json: &str,
        round: u32,
        phase: DeliberationPhase,
    ) -> String {
        format!("ok tool={tool_name} args={arguments_json} round={round} phase={phase:?}")
    }
}

/// The `ask_user` tool exactly as the client ships it (name `ask_user`).
fn ask_user_def() -> UserToolDefinition {
    UserToolDefinition {
        name: "ask_user".to_string(),
        description: "Ask the human operator a question and wait for their answer.".to_string(),
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        })),
        strict: None,
    }
}

fn ctx(with_handler: bool) -> AgentContext {
    AgentContext {
        task_description: "task".into(),
        round_number: 1,
        total_rounds: 3,
        phase: DeliberationPhase::Proposing,
        phase_budget_remaining_secs: 60.0,
        session_id: Some("test-session".into()),
        user_tools: vec![ask_user_def()],
        user_tool_handler: with_handler
            .then(|| Arc::new(EchoingUserToolHandler) as Arc<dyn UserToolHandlerTrait>),
        ..Default::default()
    }
}

/// Minimal MCP handshake: `initialize` (capture the session id) + `initialized`.
async fn init_session(client: &reqwest::Client, base: &str) -> Option<String> {
    let resp = client
        .post(base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#)
        .send()
        .await
        .unwrap();
    let sid = resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let _ = resp.text().await;
    let mut req = client
        .post(base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    if let Some(ref s) = sid {
        req = req.header("Mcp-Session-Id", s);
    }
    let _ = req.send().await.unwrap();
    sid
}

/// Start the server for `context`, return the raw `tools/list` response body.
async fn tools_list_body(context: &AgentContext) -> String {
    let (port, ct, _rx) = ClaudeAgent::start_http_mcp_server(context, ActivePhase::Proposing)
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}/mcp");
    let sid = init_session(&client, &base).await;
    let mut req = client
        .post(&base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
    if let Some(ref s) = sid {
        req = req.header("Mcp-Session-Id", s);
    }
    let body = req.send().await.unwrap().text().await.unwrap();
    ct.cancel();
    body
}

#[tokio::test]
async fn mcp_server_advertises_user_ask_user_only_with_handler() {
    // Handler wired → the tool IS advertised, under the name `user_ask_user`.
    let body = tools_list_body(&ctx(true)).await;
    assert!(
        body.contains("user_ask_user"),
        "ask_user must be advertised as `user_ask_user` when a handler is wired; got: {body}"
    );
    // And NOT under the bare name — this is why searching `ask_user` finds nothing.
    assert!(
        !body.contains("\"ask_user\""),
        "must NOT be advertised under the bare name `ask_user`; got: {body}"
    );
    // The standard nsed tools are still there.
    assert!(
        body.contains("nsed_propose"),
        "propose tool present; got: {body}"
    );

    // No handler → user tools are inert (not advertised at all).
    let body = tools_list_body(&ctx(false)).await;
    assert!(
        !body.contains("user_ask_user"),
        "no handler → ask_user must be absent; got: {body}"
    );
}

/// End to end through the REAL Claude CLI. Env-gated because it needs an authenticated
/// `claude` binary + network. Run with `RUN_CLAUDE_CLI=1 cargo test --test
/// claude_user_tools_exposure -- --nocapture`.
#[tokio::test]
async fn claude_cli_sees_namespaced_user_ask_user() {
    if std::env::var("RUN_CLAUDE_CLI").is_err() {
        eprintln!("skipped (set RUN_CLAUDE_CLI=1 to run against a live claude binary)");
        return;
    }
    let context = ctx(true);
    let (port, ct, _rx) = ClaudeAgent::start_http_mcp_server(&context, ActivePhase::Proposing)
        .await
        .unwrap();
    let cfg = ClaudeAgent::write_mcp_config_http(port).unwrap();

    // `--strict-mcp-config` ignores the operator's ambient MCP servers (so only `nsed`
    // is present). Forcing a CALL makes claude wait for the server to finish connecting —
    // `--print` otherwise answers before the async MCP handshake completes (which is
    // exactly the "nsed still connecting, tools not surfaced" state agents self-report).
    let out = std::process::Command::new("claude")
        .args([
            "--print",
            "--output-format",
            "text",
            "--strict-mcp-config",
            "--mcp-config",
        ])
        .arg(cfg.path())
        .args([
            "--dangerously-skip-permissions",
            "Call the MCP tool `mcp__nsed__user_ask_user` with arguments \
             {\"question\":\"ping\"}. Then reply with EXACTLY the raw string the tool \
             returned, nothing else.",
        ])
        .output()
        .expect("spawn claude");
    ct.cancel();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- claude stdout ---\n{stdout}\n--- claude stderr ---\n{stderr}");
    // StubAskUserHandler returns "ok" — its presence proves claude found + called the tool.
    assert!(
        stdout.contains("ok"),
        "claude must be able to CALL mcp__nsed__user_ask_user (StubAskUserHandler returns \"ok\"); \
         got stdout: {stdout}"
    );
}
