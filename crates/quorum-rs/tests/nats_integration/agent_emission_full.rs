//! Integration test: emit one of each `TelemetryEvent` variant and verify
//! all 11 land on NATS, deserialize correctly, and pass validation.

use super::common::{try_connect_nats, unique_id};
use futures::StreamExt;
use quorum_rs::DeliberationPhase;
use quorum_rs::telemetry::{
    AgentEventCommon, ClaudeSessionLockCollision, ClaudeSubprocessExit, ClaudeSubprocessSpawn,
    LlmErrorClass, LlmRequestComplete, LlmRequestFailed, LlmRequestStalled, LlmRequestStart,
    NatsConnectionState, NatsConnectionStateChanged, PromptExposureDetected, RetryLoopAttempt,
    RetryReason, TaskAccepted, TaskCompleted, TaskFailed, TaskFailureClass, TelemetryEmitter,
    TelemetryEvent, TelemetrySource, ToolCallExecuted, derive_trace_id,
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

/// Emit one instance of every `TelemetryEvent` variant and verify:
/// 1. All 11 reach the NATS subscriber
/// 2. Each deserializes as `TelemetryEvent`
/// 3. `PromptExposureDetected` passes its `validate()` invariant check
/// 4. No events are dropped
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_event_type_lands_on_nats_and_validates() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let agent_id = format!("all-events-{uid}");
    let prefix = format!("test_telemetry_{uid}");
    let job_id = format!("job-{uid}");

    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");

    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    let ctx_common = agent_common(&agent_id, &job_id);

    // 1. LlmRequestStart
    let evt1 = TelemetryEvent::LlmRequestStart(LlmRequestStart {
        common: ctx_common.clone(),
        request_id: "r1".into(),
        model: "gpt-4".into(),
        provider_id: "openai".into(),
        attempt: 1,
        estimated_input_tokens: 100,
        context_utilization_pct: 0.0,
        recent_tool_output_bytes: 0,
    });
    emitter.emit(&evt1);

    // 2. LlmRequestComplete
    let evt2 = TelemetryEvent::LlmRequestComplete(LlmRequestComplete {
        common: ctx_common.clone(),
        request_id: "r1".into(),
        latency_ms: 1500,
        ttft_ms: Some(200),
        generation_ms: Some(1300),
        input_tokens: 100,
        output_tokens: 50,
        reasoning_tokens: 10,
        cached_tokens: 0,
        cost_usd: 0.003,
        reported_cost_usd: Some(0.0028),
        cache_write_tokens: None,
        finish_reason: quorum_rs::telemetry::FinishReason::Stop,
        provider_backend: None,
        claim_assessments_emitted: None,
        disagreements_emitted: None,
        messages_chars: 800,
        max_tokens_requested: Some(1_000),
        response_chars: 250,
        tool_calls_emitted: 0,
        max_tokens_shrunk_to_floor: false,
        available_space_at_dispatch: None,
    });
    emitter.emit(&evt2);

    // 3. LlmRequestFailed
    let evt3 = TelemetryEvent::LlmRequestFailed(LlmRequestFailed {
        common: ctx_common.clone(),
        request_id: "r2".into(),
        error_class: LlmErrorClass::RateLimit,
        http_status: Some(429),
        retry_after_ms: None,
        latency_ms: 5000,
        provider_id: "openai".into(),
        provider_backend: None,
    });
    emitter.emit(&evt3);

    // 4. LlmRequestStalled
    let evt4 = TelemetryEvent::LlmRequestStalled(LlmRequestStalled {
        common: ctx_common.clone(),
        request_id: "r3".into(),
        elapsed_ms: 30000,
        ttft_received: false,
        last_token_ms: None,
    });
    emitter.emit(&evt4);

    // 5. ToolCallExecuted
    let evt5 = TelemetryEvent::ToolCallExecuted(ToolCallExecuted {
        common: ctx_common.clone(),
        tool_name: "read_proposal".into(),
        latency_ms: 50,
        success: true,
        output_bytes: 0,
        output_tokens_estimated: None,
        truncated: false,
        paginated: false,
    });
    emitter.emit(&evt5);

    // 6. RetryLoopAttempt
    let evt6 = TelemetryEvent::RetryLoopAttempt(RetryLoopAttempt {
        common: ctx_common.clone(),
        attempt: 2,
        reason: RetryReason::SchemaError,
        cumulative_latency_ms: 6000,
        cumulative_cost_usd: 0.006,
        cumulative_input_tokens: 200,
        cumulative_output_tokens: 0,
    });
    emitter.emit(&evt6);

    // 7. TaskAccepted
    let evt7 = TelemetryEvent::TaskAccepted(TaskAccepted {
        common: ctx_common.clone(),
        dispatch_delay_ms: 10,
        task_publish_ts: None,
        job_age_at_accept_ms: None,
    });
    emitter.emit(&evt7);

    // 8. TaskCompleted
    let evt8 = TelemetryEvent::TaskCompleted(TaskCompleted {
        common: ctx_common.clone(),
        duration_ms: 15000,
        dispatch_delay_ms: 10,
        queue_wait_ms: Some(50),
        phase_budget_remaining_ms: 45000,
        llm_attempts: Some(1),
        tool_call_count: Some(3),
        pending_publish_depth: Some(0),
    });
    emitter.emit(&evt8);

    // 9. TaskFailed
    let evt9 = TelemetryEvent::TaskFailed(TaskFailed {
        common: ctx_common.clone(),
        duration_ms: 30000,
        dispatch_delay_ms: 10,
        queue_wait_ms: Some(50),
        phase_budget_remaining_ms: -1000,
        llm_attempts: Some(5),
        tool_call_count: Some(8),
        failure_class: TaskFailureClass::LlmExhausted,
        pending_publish_depth: Some(0),
        reason: None,
    });
    emitter.emit(&evt9);

    // 10. NatsConnectionStateChanged
    let evt10 = TelemetryEvent::NatsConnectionStateChanged(NatsConnectionStateChanged {
        common: AgentEventCommon {
            agent_id: agent_id.to_string(),
            job_id: None,
            round: None,
            phase: None,
            ts: 1_776_790_692_747,
            trace_id: derive_trace_id("connection", 0, DeliberationPhase::Proposing, &agent_id),
        },
        state: NatsConnectionState::Connected,
        reconnects_so_far: 0,
        pending_publish_depth: Some(0),
        buffer_bytes: Some(0),
    });
    emitter.emit(&evt10);

    // 11. PromptExposureDetected
    let evt11 = TelemetryEvent::PromptExposureDetected(PromptExposureDetected {
        common: ctx_common.clone(),
        terminal_tool: "submit_proposal".into(),
        blocked: true,
        hit_count: 3,
        response_length_chars: 1482,
        suspicion_score: 4.76,
        xml_tag_hits: 2,
        tool_name_hits: 1,
        instruction_hits: 0,
        wrong_acronym_hits: 0,
        sample_hits: vec![
            "xml-tag <working_memory>".into(),
            "xml-tag <key_findings>".into(),
            "tool-name submit_proposal".into(),
        ],
    });
    emitter.emit(&evt11);

    // 12. ClaudeSubprocessSpawn
    let evt12 = TelemetryEvent::ClaudeSubprocessSpawn(ClaudeSubprocessSpawn {
        common: ctx_common.clone(),
        session_id: "8ce6aa3f-d7c2-0000-0000-000000000000".into(),
        lock_present_at_spawn: false,
    });
    emitter.emit(&evt12);

    // 13. ClaudeSubprocessExit
    let evt13 = TelemetryEvent::ClaudeSubprocessExit(ClaudeSubprocessExit {
        common: ctx_common.clone(),
        session_id: "8ce6aa3f-d7c2-0000-0000-000000000000".into(),
        exit_code: 0,
        wallclock_ms: 12_345,
        session_lock_released: true,
    });
    emitter.emit(&evt13);

    // 14. ClaudeSessionLockCollision
    let evt14 = TelemetryEvent::ClaudeSessionLockCollision(ClaudeSessionLockCollision {
        common: ctx_common.clone(),
        session_id: "8ce6aa3f-d7c2-0000-0000-000000000000".into(),
        prior_lock_age_secs: 42,
        prior_pid: Some(31415),
    });
    emitter.emit(&evt14);

    // Collect all 14 events
    let mut received = Vec::new();
    for _ in 0..14 {
        let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("event timeout")
            .expect("subscription closed");
        let evt: TelemetryEvent =
            serde_json::from_slice(&msg.payload).expect("payload must deserialize");
        received.push(evt);
    }

    // Verify PromptExposureDetected passes validate()
    for evt in &received {
        if let TelemetryEvent::PromptExposureDetected(p) = evt {
            p.validate()
                .expect("PromptExposureDetected must pass validate()");
        }
    }

    // Verify no drops
    assert_eq!(emitter.dropped_count(), 0, "no events should drop");

    // Verify we got all 14 distinct types by checking kind strings
    let kinds: Vec<_> = received.iter().map(|e| e.kind()).collect();
    assert_eq!(kinds.len(), 14, "should have 14 events");
    let mut unique_kinds = kinds.clone();
    unique_kinds.sort();
    unique_kinds.dedup();
    assert_eq!(
        unique_kinds.len(),
        14,
        "all 14 event types should be distinct"
    );
}
