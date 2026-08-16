//! Agent-scoped, NATS-JetStream-backed event log for the operator dashboard.
//!
//! Each agent persists its own lifecycle events — API errors, task/query
//! start/finish, and tool-call start/finish — to a JetStream stream under the
//! subject `agent.events.<agent_name>`. The stream is retained for 24h (and
//! hard-capped at [`STREAM_MAX_MESSAGES`]) so the dashboard's "last 24h" views
//! are backed by a real, restart-surviving window rather than an in-memory ring
//! buffer.
//!
//! This log lives entirely in the agent's own NATS scope. The orchestrator
//! never publishes to it or consumes from it.
//!
//! The dashboard (which runs in the agent process and therefore holds the
//! agent's JetStream context) reads the stream with [`AgentEventStore::read_since`]
//! and turns the raw events into operator views via the pure functions
//! [`collect_errors`], [`reconcile_tasks`], and [`reconcile_tool_calls`].

use async_nats::jetstream::{self, consumer::DeliverPolicy, consumer::pull, stream};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa::ToSchema;

/// JetStream stream that holds every agent's event log.
pub const STREAM_NAME: &str = "agent_events";

/// Subject root; each agent publishes to `agent.events.<agent_name>`.
pub const SUBJECT_ROOT: &str = "agent.events";

/// Retention window for the event log.
pub const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Hard per-stream message cap. Bounds a read even if an agent is extremely
/// noisy inside the 24h window; the stream evicts the oldest events past this.
pub const STREAM_MAX_MESSAGES: i64 = 10_000;

/// Kind of a persisted agent event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    /// An API/tool error the agent raised (mirrors the `agent_error` snapshot event).
    AgentError,
    /// A task/query the agent accepted and began working.
    TaskStarted,
    /// A task/query that finished successfully.
    TaskCompleted,
    /// A task/query that ended in error.
    TaskFailed,
    /// A tool invocation the agent started (awaiting a result).
    ToolCallStarted,
    /// A tool invocation that returned (success or error carried in `status`).
    ToolCallFinished,
}

/// One persisted agent event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AgentEvent {
    /// RFC3339 timestamp the event was recorded.
    pub timestamp: String,
    /// Agent that produced the event.
    pub agent: String,
    /// What happened.
    pub kind: AgentEventKind,
    /// Job / session / query id the event belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// Deliberation round, for task events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    /// Task phase (`"propose"`/`"evaluate"`) or `"queued"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Correlates `ToolCallStarted` with its `ToolCallFinished`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Tool name, for tool-call events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Short, truncated summary of the tool arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_summary: Option<String>,
    /// Outcome for a finished event: `"ok"` or `"error"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Human-readable detail (error text, result preview, etc.).
    #[serde(default)]
    pub detail: String,
}

impl AgentEvent {
    /// Build an event stamped with the current time and the given identity.
    pub fn now(agent: impl Into<String>, kind: AgentEventKind) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            agent: agent.into(),
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

    /// Parse the RFC3339 `timestamp` into a UTC datetime, if well-formed.
    pub fn parsed_time(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.timestamp)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
}

#[derive(Clone, Debug)]
pub struct AgentEventStore {
    js: jetstream::Context,
    agent: String,
}

impl AgentEventStore {
    /// Build a store bound to `agent`'s event subject.
    pub fn new(js: jetstream::Context, agent: String) -> Self {
        Self { js, agent }
    }

    fn subject(&self) -> String {
        format!("{}.{}", SUBJECT_ROOT, self.agent)
    }

    /// Idempotently ensure the shared `agent_events` stream exists with 24h
    /// retention. Safe to call from every agent on startup.
    pub async fn ensure_stream(js: &jetstream::Context) -> anyhow::Result<()> {
        js.get_or_create_stream(stream::Config {
            name: STREAM_NAME.to_string(),
            subjects: vec![format!("{}.>", SUBJECT_ROOT)],
            max_age: RETENTION,
            max_messages: STREAM_MAX_MESSAGES,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("ensure agent_events stream: {e}"))?;
        Ok(())
    }

    /// Persist one event to the agent's subject.
    pub async fn publish(&self, event: &AgentEvent) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(event)?;
        self.js
            .publish(self.subject(), payload.into())
            .await
            .map_err(|e| anyhow::anyhow!("publish agent event: {e}"))?;
        Ok(())
    }

    /// Read this agent's events at or after `cutoff`, newest first.
    ///
    /// Drains an ephemeral consumer filtered to the agent subject. Because the
    /// stream retains only the last 24h (and at most [`STREAM_MAX_MESSAGES`]),
    /// this returns the full window in one pass.
    pub async fn read_since(&self, cutoff: DateTime<Utc>) -> anyhow::Result<Vec<AgentEvent>> {
        read_subject_since(&self.js, &self.subject(), cutoff).await
    }
}

/// Drain every event on `filter_subject` at or after `cutoff`, newest first.
async fn read_subject_since(
    js: &jetstream::Context,
    filter_subject: &str,
    cutoff: DateTime<Utc>,
) -> anyhow::Result<Vec<AgentEvent>> {
    let js_stream = js
        .get_stream(STREAM_NAME)
        .await
        .map_err(|e| anyhow::anyhow!("get agent_events stream: {e}"))?;
    let consumer = js_stream
        .create_consumer(pull::Config {
            filter_subject: filter_subject.to_string(),
            deliver_policy: DeliverPolicy::All,
            ack_policy: jetstream::consumer::AckPolicy::None,
            inactive_threshold: Duration::from_secs(30),
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("create agent_events consumer: {e}"))?;

    let mut batch = consumer
        .fetch()
        .max_messages(STREAM_MAX_MESSAGES as usize)
        .messages()
        .await
        .map_err(|e| anyhow::anyhow!("fetch agent events: {e}"))?;

    let mut events = Vec::new();
    while let Some(message) = batch.next().await {
        let message = message.map_err(|e| anyhow::anyhow!("read agent event: {e}"))?;
        if let Ok(event) = serde_json::from_slice::<AgentEvent>(&message.payload) {
            events.push(event);
        }
    }

    events.retain(|event| match event.parsed_time() {
        Some(at) => at >= cutoff,
        None => false,
    });
    events.sort_by_key(|event| std::cmp::Reverse(event.parsed_time()));
    Ok(events)
}

/// Errors among `events`, newest first (input is expected already newest-first).
pub fn collect_errors(events: &[AgentEvent]) -> Vec<AgentEvent> {
    events
        .iter()
        .filter(|event| event.kind == AgentEventKind::AgentError)
        .cloned()
        .collect()
}

/// A task/query as reconstructed from its start + optional finish events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TaskView {
    pub job_id: String,
    pub round: Option<u32>,
    pub phase: Option<String>,
    /// RFC3339 start time.
    pub started_at: String,
    /// RFC3339 finish time, absent while in flight.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// `"in_flight"`, `"completed"`, or `"failed"`.
    pub state: String,
    /// Finish detail (error text / result preview), when finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// In-flight vs finished tasks for one agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct TasksView {
    /// Tasks started with no finish event yet, newest first.
    pub in_flight: Vec<TaskView>,
    /// Finished tasks, newest first.
    pub finished: Vec<TaskView>,
}

/// Pair task start events with their finish events (keyed by job_id + round).
/// `events` must be newest-first; the split preserves that order.
pub fn reconcile_tasks(events: &[AgentEvent]) -> TasksView {
    let mut view = TasksView::default();
    for event in events {
        match event.kind {
            AgentEventKind::TaskStarted => {
                let key = task_key(event);
                let finish = events.iter().find(|other| {
                    matches!(
                        other.kind,
                        AgentEventKind::TaskCompleted | AgentEventKind::TaskFailed
                    ) && task_key(other) == key
                });
                match finish {
                    Some(done) => view.finished.push(TaskView {
                        job_id: event.job_id.clone().unwrap_or_default(),
                        round: event.round,
                        phase: event.phase.clone(),
                        started_at: event.timestamp.clone(),
                        finished_at: Some(done.timestamp.clone()),
                        state: if done.kind == AgentEventKind::TaskFailed {
                            "failed".to_string()
                        } else {
                            "completed".to_string()
                        },
                        detail: Some(done.detail.clone()),
                    }),
                    None => view.in_flight.push(TaskView {
                        job_id: event.job_id.clone().unwrap_or_default(),
                        round: event.round,
                        phase: event.phase.clone(),
                        started_at: event.timestamp.clone(),
                        finished_at: None,
                        state: "in_flight".to_string(),
                        detail: None,
                    }),
                }
            }
            _ => continue,
        }
    }
    view
}

fn task_key(event: &AgentEvent) -> (Option<String>, Option<u32>) {
    (event.job_id.clone(), event.round)
}

/// A tool call reconstructed from its start + optional finish events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ToolCallView {
    pub call_id: String,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_summary: Option<String>,
    pub job_id: Option<String>,
    /// RFC3339 start time.
    pub started_at: String,
    /// RFC3339 finish time, absent while pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// `"pending"`, `"ok"`, or `"error"`.
    pub state: String,
    /// Result preview / error text, when finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Pending vs finished tool calls for one agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct ToolCallsView {
    /// Started tool calls with no finish event yet, newest first.
    pub pending: Vec<ToolCallView>,
    /// Finished tool calls, newest first.
    pub finished: Vec<ToolCallView>,
}

/// Pair tool-call start events with their finish events (keyed by call_id).
/// `events` must be newest-first; the split preserves that order.
pub fn reconcile_tool_calls(events: &[AgentEvent]) -> ToolCallsView {
    let mut view = ToolCallsView::default();
    for event in events {
        if event.kind != AgentEventKind::ToolCallStarted {
            continue;
        }
        let call_id = match &event.call_id {
            Some(id) => id.clone(),
            None => continue,
        };
        let finish = events.iter().find(|other| {
            other.kind == AgentEventKind::ToolCallFinished
                && other.call_id.as_deref() == Some(&call_id)
        });
        match finish {
            Some(done) => view.finished.push(ToolCallView {
                call_id,
                tool_name: event.tool_name.clone().unwrap_or_default(),
                args_summary: event.args_summary.clone(),
                job_id: event.job_id.clone(),
                started_at: event.timestamp.clone(),
                finished_at: Some(done.timestamp.clone()),
                state: done.status.clone().unwrap_or_else(|| "ok".to_string()),
                detail: Some(done.detail.clone()),
            }),
            None => view.pending.push(ToolCallView {
                call_id,
                tool_name: event.tool_name.clone().unwrap_or_default(),
                args_summary: event.args_summary.clone(),
                job_id: event.job_id.clone(),
                started_at: event.timestamp.clone(),
                finished_at: None,
                state: "pending".to_string(),
                detail: None,
            }),
        }
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: AgentEventKind, ts: &str) -> AgentEvent {
        AgentEvent {
            timestamp: ts.to_string(),
            agent: "ALPHA".to_string(),
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

    #[test]
    fn collect_errors_keeps_only_error_kind() {
        let events = vec![
            event(AgentEventKind::AgentError, "2026-08-16T10:00:00+00:00"),
            event(AgentEventKind::TaskStarted, "2026-08-16T10:01:00+00:00"),
        ];
        let errors = collect_errors(&events);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, AgentEventKind::AgentError);
    }

    #[test]
    fn reconcile_tasks_splits_in_flight_from_finished() {
        let mut started_done = event(AgentEventKind::TaskStarted, "2026-08-16T10:00:00+00:00");
        started_done.job_id = Some("job-done".to_string());
        started_done.round = Some(1);
        started_done.phase = Some("propose".to_string());
        let mut completed = event(AgentEventKind::TaskCompleted, "2026-08-16T10:00:05+00:00");
        completed.job_id = Some("job-done".to_string());
        completed.round = Some(1);
        completed.detail = "ok".to_string();

        let mut started_live = event(AgentEventKind::TaskStarted, "2026-08-16T10:02:00+00:00");
        started_live.job_id = Some("job-live".to_string());
        started_live.round = Some(2);
        started_live.phase = Some("evaluate".to_string());

        let events = vec![started_live.clone(), completed, started_done];
        let view = reconcile_tasks(&events);
        assert_eq!(view.in_flight.len(), 1);
        assert_eq!(view.in_flight[0].job_id, "job-live");
        assert_eq!(view.in_flight[0].state, "in_flight");
        assert_eq!(view.finished.len(), 1);
        assert_eq!(view.finished[0].job_id, "job-done");
        assert_eq!(view.finished[0].state, "completed");
        assert_eq!(
            view.finished[0].finished_at.as_deref(),
            Some("2026-08-16T10:00:05+00:00")
        );
    }

    #[test]
    fn reconcile_tasks_marks_failed() {
        let mut started = event(AgentEventKind::TaskStarted, "2026-08-16T10:00:00+00:00");
        started.job_id = Some("job-x".to_string());
        started.round = Some(1);
        let mut failed = event(AgentEventKind::TaskFailed, "2026-08-16T10:00:03+00:00");
        failed.job_id = Some("job-x".to_string());
        failed.round = Some(1);
        failed.detail = "boom".to_string();

        let view = reconcile_tasks(&[failed, started]);
        assert_eq!(view.finished.len(), 1);
        assert_eq!(view.finished[0].state, "failed");
        assert_eq!(view.finished[0].detail.as_deref(), Some("boom"));
    }

    #[test]
    fn reconcile_tool_calls_pairs_by_call_id() {
        let mut started = event(AgentEventKind::ToolCallStarted, "2026-08-16T10:00:00+00:00");
        started.call_id = Some("c1".to_string());
        started.tool_name = Some("scoped_read".to_string());
        started.args_summary = Some("{\"path\":\"a.rs\"}".to_string());
        let mut finished = event(
            AgentEventKind::ToolCallFinished,
            "2026-08-16T10:00:01+00:00",
        );
        finished.call_id = Some("c1".to_string());
        finished.status = Some("ok".to_string());
        finished.detail = "42 lines".to_string();

        let mut pending = event(AgentEventKind::ToolCallStarted, "2026-08-16T10:00:02+00:00");
        pending.call_id = Some("c2".to_string());
        pending.tool_name = Some("sandbox".to_string());

        let view = reconcile_tool_calls(&[pending, finished, started]);
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].call_id, "c2");
        assert_eq!(view.pending[0].state, "pending");
        assert_eq!(view.finished.len(), 1);
        assert_eq!(view.finished[0].call_id, "c1");
        assert_eq!(view.finished[0].state, "ok");
        assert_eq!(view.finished[0].tool_name, "scoped_read");
    }

    #[test]
    fn tool_call_started_without_call_id_is_skipped() {
        let started = event(AgentEventKind::ToolCallStarted, "2026-08-16T10:00:00+00:00");
        let view = reconcile_tool_calls(&[started]);
        assert!(view.pending.is_empty());
        assert!(view.finished.is_empty());
    }
}
