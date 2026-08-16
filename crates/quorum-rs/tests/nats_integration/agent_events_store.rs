//! End-to-end coverage for the NATS-backed agent event log: publish events to
//! JetStream, read them back through the 24h window, and reconstruct the
//! operator views (errors / tasks / tool-calls).

use super::common::{try_connect_nats, unique_id};
use async_nats::jetstream;
use chrono::{Duration, Utc};
use quorum_rs::status::agent_events::{
    AgentEvent, AgentEventKind, AgentEventStore, collect_errors, reconcile_tasks,
    reconcile_tool_calls,
};

fn base_event(agent: &str, kind: AgentEventKind) -> AgentEvent {
    AgentEvent {
        timestamp: Utc::now().to_rfc3339(),
        agent: agent.to_string(),
        kind,
        job_id: None,
        round: None,
        phase: None,
        call_id: None,
        tool_name: None,
        args_summary: None,
        status: None,
        detail: String::new(),
    }
}

#[tokio::test]
async fn publish_then_read_reconstructs_views_and_honours_cutoff() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client);
    AgentEventStore::ensure_stream(&js)
        .await
        .expect("ensure stream");

    let agent = format!("AGENT_{}", unique_id());
    let store = AgentEventStore::new(js.clone(), agent.clone());

    // A completed task.
    let mut task_start = base_event(&agent, AgentEventKind::TaskStarted);
    task_start.job_id = Some("job-1".to_string());
    task_start.round = Some(1);
    task_start.phase = Some("propose".to_string());
    let mut task_done = base_event(&agent, AgentEventKind::TaskCompleted);
    task_done.job_id = Some("job-1".to_string());
    task_done.round = Some(1);
    task_done.detail = "ok".to_string();

    // An in-flight task (no finish event).
    let mut task_live = base_event(&agent, AgentEventKind::TaskStarted);
    task_live.job_id = Some("job-2".to_string());
    task_live.round = Some(2);
    task_live.phase = Some("evaluate".to_string());

    // A finished tool call and a pending one.
    let mut tool_start = base_event(&agent, AgentEventKind::ToolCallStarted);
    tool_start.call_id = Some("call-1".to_string());
    tool_start.tool_name = Some("scoped_read".to_string());
    tool_start.args_summary = Some("{\"path\":\"a.rs\"}".to_string());
    let mut tool_done = base_event(&agent, AgentEventKind::ToolCallFinished);
    tool_done.call_id = Some("call-1".to_string());
    tool_done.status = Some("ok".to_string());
    tool_done.detail = "42 lines".to_string();
    let mut tool_pending = base_event(&agent, AgentEventKind::ToolCallStarted);
    tool_pending.call_id = Some("call-2".to_string());
    tool_pending.tool_name = Some("sandbox".to_string());

    // An error inside the window and a stale one (timestamped 30h ago) that the
    // cutoff must drop even though JetStream still holds it.
    let mut recent_error = base_event(&agent, AgentEventKind::AgentError);
    recent_error.job_id = Some("job-1".to_string());
    recent_error.detail = "evaluate failed: API request failed with status 404".to_string();
    let mut stale_error = base_event(&agent, AgentEventKind::AgentError);
    stale_error.timestamp = (Utc::now() - Duration::hours(30)).to_rfc3339();
    stale_error.detail = "too old to show".to_string();

    for event in [
        &task_start,
        &task_done,
        &task_live,
        &tool_start,
        &tool_done,
        &tool_pending,
        &recent_error,
        &stale_error,
    ] {
        store.publish(event).await.expect("publish event");
    }

    let cutoff = Utc::now() - Duration::hours(24);
    let events = store.read_since(cutoff).await.expect("read events");

    // The stale error is filtered out by the cutoff.
    assert!(
        events.iter().all(|e| e.detail != "too old to show"),
        "stale error must be excluded by the 24h cutoff"
    );

    let errors = collect_errors(&events);
    assert_eq!(errors.len(), 1, "only the in-window error surfaces");
    assert!(errors[0].detail.contains("status 404"));

    let tasks = reconcile_tasks(&events);
    assert_eq!(tasks.in_flight.len(), 1, "one task still in flight");
    assert_eq!(tasks.in_flight[0].job_id, "job-2");
    assert_eq!(tasks.finished.len(), 1, "one task finished");
    assert_eq!(tasks.finished[0].job_id, "job-1");
    assert_eq!(tasks.finished[0].state, "completed");

    let tool_calls = reconcile_tool_calls(&events);
    assert_eq!(tool_calls.pending.len(), 1, "one tool call pending");
    assert_eq!(tool_calls.pending[0].call_id, "call-2");
    assert_eq!(tool_calls.finished.len(), 1, "one tool call finished");
    assert_eq!(tool_calls.finished[0].call_id, "call-1");
    assert_eq!(tool_calls.finished[0].state, "ok");
}
