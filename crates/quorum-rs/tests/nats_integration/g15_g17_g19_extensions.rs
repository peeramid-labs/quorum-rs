//! Schema-shape contract over NATS for the new fields on
//! `LlmRequestStart` / `LlmRequestComplete` / `ToolCallExecuted` /
//! `TaskAccepted` plus the `ContextEmergencyShrink` variant.

use super::common::{try_connect_nats, unique_id};
use futures::StreamExt;
use quorum_rs::DeliberationPhase;
use quorum_rs::telemetry::{
    AgentEventCommon, ContextEmergencyShrink, FinishReason, LlmRequestComplete, LlmRequestStart,
    RecentToolOutput, TaskAccepted, TelemetryEmitter, TelemetryEvent, TelemetrySource,
    ToolCallExecuted, derive_trace_id,
};
use std::time::Duration;

fn agent_common(agent_id: &str, job_id: &str) -> AgentEventCommon {
    AgentEventCommon {
        agent_id: agent_id.to_string(),
        job_id: Some(job_id.to_string()),
        round: Some(1),
        phase: Some(DeliberationPhase::Proposing),
        ts: 1_776_790_692_747,
        trace_id: derive_trace_id(job_id, 1, DeliberationPhase::Proposing, agent_id),
    }
}

/// Drain every event from a subscription until the deadline. Returns
/// `(subject, event)` so call sites can assert the NATS subject
/// shape (`telemetry.agent.<agent_id>.<event_kind>`) alongside the
/// deserialized payload — guards against the catalog drifting on
/// the subject side without breaking the typed payload check.
async fn drain_events(
    sub: &mut async_nats::Subscriber,
    deadline: std::time::Instant,
) -> Vec<(String, TelemetryEvent)> {
    let mut out = Vec::new();
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), sub.next()).await {
            Ok(Some(msg)) => {
                if let Ok(evt) = serde_json::from_slice::<TelemetryEvent>(&msg.payload) {
                    out.push((msg.subject.to_string(), evt));
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g15_llm_request_start_carries_context_utilization_and_tool_bytes() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("g15start-{uid}");
    let prefix = format!("test_g15_start_{uid}");
    let job_id = format!("job-{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");
    let source = TelemetrySource::agent(&agent_id).expect("valid");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    emitter.emit(&TelemetryEvent::LlmRequestStart(LlmRequestStart {
        common: agent_common(&agent_id, &job_id),
        request_id: "r-bloat-1".into(),
        model: "tongyi-deepresearch-30b".into(),
        provider_id: "openrouter".into(),
        attempt: 1,
        estimated_input_tokens: 117_217,
        context_utilization_pct: 89.5,
        recent_tool_output_bytes: 244_000,
    }));

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let events = drain_events(&mut sub, deadline).await;
    let (subject, start) = events
        .iter()
        .find_map(|(s, e)| match e {
            TelemetryEvent::LlmRequestStart(start) => Some((s, start)),
            _ => None,
        })
        .expect("LlmRequestStart should arrive");
    assert_eq!(*subject, format!("{prefix}.{agent_id}.llm_request_start"));
    assert!(
        (start.context_utilization_pct - 89.5).abs() < 1e-6,
        "context_utilization_pct should round-trip"
    );
    assert_eq!(start.recent_tool_output_bytes, 244_000);
    assert_eq!(start.estimated_input_tokens, 117_217);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g15_llm_request_complete_carries_shrunk_to_floor() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("g15comp-{uid}");
    let prefix = format!("test_g15_comp_{uid}");
    let job_id = format!("job-{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");
    let source = TelemetrySource::agent(&agent_id).expect("valid");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    emitter.emit(&TelemetryEvent::LlmRequestComplete(LlmRequestComplete {
        common: agent_common(&agent_id, &job_id),
        request_id: "r-floor-1".into(),
        latency_ms: 1500,
        ttft_ms: Some(200),
        generation_ms: Some(1300),
        input_tokens: 117_000,
        output_tokens: 200,
        reasoning_tokens: 0,
        cached_tokens: 0,
        cost_usd: 0.001,
        reported_cost_usd: None,
        cache_write_tokens: None,
        finish_reason: FinishReason::Length,
        provider_backend: None,
        claim_assessments_emitted: None,
        disagreements_emitted: None,
        messages_chars: 470_000,
        max_tokens_requested: Some(4_000),
        response_chars: 800,
        tool_calls_emitted: 0,
        max_tokens_shrunk_to_floor: true,
        available_space_at_dispatch: Some(200),
    }));

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let events = drain_events(&mut sub, deadline).await;
    let (subject, complete) = events
        .iter()
        .find_map(|(s, e)| match e {
            TelemetryEvent::LlmRequestComplete(c) => Some((s, c)),
            _ => None,
        })
        .expect("LlmRequestComplete should arrive");
    assert_eq!(
        *subject,
        format!("{prefix}.{agent_id}.llm_request_complete")
    );
    assert!(complete.max_tokens_shrunk_to_floor);
    assert_eq!(complete.available_space_at_dispatch, Some(200));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g15_context_emergency_shrink_event_round_trips() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("g15emerg-{uid}");
    let prefix = format!("test_g15_emerg_{uid}");
    let job_id = format!("job-{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");
    let source = TelemetrySource::agent(&agent_id).expect("valid");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    emitter.emit(&TelemetryEvent::ContextEmergencyShrink(
        ContextEmergencyShrink {
            common: agent_common(&agent_id, &job_id),
            available_space: 200,
            requested_max: 4_000,
            floor_used: 200,
            estimated_input: 130_700,
            context_window: 131_072,
            recent_tool_outputs: vec![
                RecentToolOutput {
                    tool: "read_file".into(),
                    bytes: 244_000,
                },
                RecentToolOutput {
                    tool: "grep_search".into(),
                    bytes: 18_400,
                },
            ],
        },
    ));

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let events = drain_events(&mut sub, deadline).await;
    let (subject, shrink) = events
        .iter()
        .find_map(|(s, e)| match e {
            TelemetryEvent::ContextEmergencyShrink(shrink) => Some((s, shrink)),
            _ => None,
        })
        .expect("ContextEmergencyShrink should arrive");
    assert_eq!(
        *subject,
        format!("{prefix}.{agent_id}.context_emergency_shrink")
    );
    assert_eq!(shrink.available_space, 200);
    assert_eq!(shrink.requested_max, 4_000);
    assert_eq!(shrink.floor_used, 200);
    assert_eq!(shrink.estimated_input, 130_700);
    assert_eq!(shrink.context_window, 131_072);
    assert_eq!(shrink.recent_tool_outputs.len(), 2);
    assert_eq!(shrink.recent_tool_outputs[0].tool, "read_file");
    assert_eq!(shrink.recent_tool_outputs[0].bytes, 244_000);
    assert_eq!(shrink.recent_tool_outputs[1].tool, "grep_search");
    assert_eq!(shrink.recent_tool_outputs[1].bytes, 18_400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g17_tool_call_executed_carries_sizing_fields() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("g17-{uid}");
    let prefix = format!("test_g17_{uid}");
    let job_id = format!("job-{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");
    let source = TelemetrySource::agent(&agent_id).expect("valid");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    emitter.emit(&TelemetryEvent::ToolCallExecuted(ToolCallExecuted {
        common: agent_common(&agent_id, &job_id),
        tool_name: "read_file".into(),
        latency_ms: 12,
        success: true,
        output_bytes: 244_000,
        output_tokens_estimated: Some(61_000),
        truncated: false,
        paginated: true,
    }));

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let events = drain_events(&mut sub, deadline).await;
    let (subject, tce) = events
        .iter()
        .find_map(|(s, e)| match e {
            TelemetryEvent::ToolCallExecuted(t) => Some((s, t)),
            _ => None,
        })
        .expect("ToolCallExecuted should arrive");
    assert_eq!(*subject, format!("{prefix}.{agent_id}.tool_call_executed"));
    assert_eq!(tce.output_bytes, 244_000);
    assert_eq!(tce.output_tokens_estimated, Some(61_000));
    assert!(!tce.truncated);
    assert!(tce.paginated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g19_task_accepted_carries_publish_ts_and_job_age() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("g19-{uid}");
    let prefix = format!("test_g19_{uid}");
    let job_id = format!("job-{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");
    let source = TelemetrySource::agent(&agent_id).expect("valid");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    let publish_ts = 1_776_790_000_000_i64;
    let receive_ts = 1_776_790_003_500_i64;
    emitter.emit(&TelemetryEvent::TaskAccepted(TaskAccepted {
        common: agent_common(&agent_id, &job_id),
        dispatch_delay_ms: 5,
        task_publish_ts: Some(publish_ts),
        job_age_at_accept_ms: Some(receive_ts - publish_ts),
    }));

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let events = drain_events(&mut sub, deadline).await;
    let (subject, task) = events
        .iter()
        .find_map(|(s, e)| match e {
            TelemetryEvent::TaskAccepted(t) => Some((s, t)),
            _ => None,
        })
        .expect("TaskAccepted should arrive");
    assert_eq!(*subject, format!("{prefix}.{agent_id}.task_accepted"));
    assert_eq!(task.task_publish_ts, Some(publish_ts));
    assert_eq!(task.job_age_at_accept_ms, Some(3_500));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g19_task_accepted_backward_compat_with_omitted_fields() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("g19compat-{uid}");
    let prefix = format!("test_g19_compat_{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");

    // A future "remove default" refactor would silently break older
    // orchestrators if this round-trip stops yielding `None`.
    let legacy = serde_json::json!({
        "type": "task_accepted",
        "agent_id": agent_id,
        "job_id": format!("job-{uid}"),
        "round": 1,
        "phase": "Proposing",
        "ts": 1_776_790_692_747i64,
        "trace_id": derive_trace_id(
            &format!("job-{uid}"),
            1,
            DeliberationPhase::Proposing,
            &agent_id,
        ),
        "dispatch_delay_ms": 7
    });
    client
        .publish(
            format!("{prefix}.{agent_id}.task_accepted"),
            serde_json::to_vec(&legacy).unwrap().into(),
        )
        .await
        .expect("publish");

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let events = drain_events(&mut sub, deadline).await;
    let (subject, task) = events
        .iter()
        .find_map(|(s, e)| match e {
            TelemetryEvent::TaskAccepted(t) => Some((s, t)),
            _ => None,
        })
        .expect("legacy TaskAccepted payload must still deserialize");
    assert_eq!(*subject, format!("{prefix}.{agent_id}.task_accepted"));
    assert_eq!(task.dispatch_delay_ms, 7);
    assert_eq!(task.task_publish_ts, None);
    assert_eq!(task.job_age_at_accept_ms, None);
}
