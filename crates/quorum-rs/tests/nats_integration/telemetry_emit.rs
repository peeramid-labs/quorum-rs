//! End-to-end `TelemetryEmitter` tests against a real NATS server.
//!
//! Skips locally when NATS is unreachable (per the repo's
//! `try_connect_nats` convention); panics in CI so coverage is not
//! silently lost. Each test uses a UUID-scoped subject prefix so
//! concurrent runs do not interfere.

use super::common::{try_connect_nats, unique_id};
use quorum_rs::DeliberationPhase;
use quorum_rs::telemetry::{
    AgentEventCommon, PromptExposureDetected, TaskAccepted, TelemetryEmitter, TelemetryEvent,
    TelemetrySource, derive_trace_id,
};

use futures::StreamExt;
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

/// Emit a `PromptExposureDetected` event and verify the subscriber
/// receives it on the expected agent-subtree subject with the exact
/// JSON payload the operator-facing contract documents.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emit_prompt_exposure_reaches_agent_subject() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let agent_id = format!("ex-agent-{uid}");
    let prefix = format!("test_telemetry_{uid}");
    let expected_subject = format!("{prefix}.{agent_id}.prompt_exposure_detected");

    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");

    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    let payload = PromptExposureDetected {
        common: agent_common(&agent_id, &format!("job-{uid}")),
        terminal_tool: "submit_proposal".into(),
        blocked: true,
        hit_count: 3,
        response_length_chars: 1_482,
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
    };
    let sent = TelemetryEvent::PromptExposureDetected(payload);
    emitter.emit(&sent);

    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("waiting for emitted message")
        .expect("subscription stream closed unexpectedly");
    assert_eq!(msg.subject.as_str(), expected_subject);
    assert_eq!(emitter.dropped_count(), 0, "emit should not have dropped");

    let received: TelemetryEvent =
        serde_json::from_slice(&msg.payload).expect("payload must deserialize as TelemetryEvent");
    assert_eq!(received, sent, "payload must match bit-for-bit");
}

/// Emit under a strict wildcard that would catch foreign-subtree traffic
/// — prove agent isolation at the subject layer. Each agent's emitter
/// must land only on its own subtree so the JWT-bound contract (PR2)
/// has a stable foundation to enforce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_subtree_does_not_leak_into_peer() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let alice = format!("alice-{uid}");
    let bob = format!("bob-{uid}");
    let prefix = format!("test_telemetry_{uid}");

    // Subscribe only to bob's subtree. Anything alice emits must not
    // arrive here.
    let mut sub_bob = client
        .subscribe(format!("{prefix}.{bob}.>"))
        .await
        .expect("subscribe");

    let source = TelemetrySource::agent(&alice).expect("valid");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);
    emitter.emit(&TelemetryEvent::TaskAccepted(TaskAccepted {
        common: agent_common(&alice, &format!("job-{uid}")),
        dispatch_delay_ms: 0,
        task_publish_ts: None,
        job_age_at_accept_ms: None,
    }));

    // Give the server a moment to route; then assert bob's subscription
    // is still waiting.
    let got = tokio::time::timeout(Duration::from_millis(500), sub_bob.next()).await;
    assert!(
        got.is_err(),
        "bob must not receive alice's events (got {got:?})"
    );
}

/// Emit with an explicitly invalid `agent_id` through the struct-literal
/// bypass path — `subject()` rejects, `emit()` counts the drop, no
/// message reaches NATS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_agent_id_in_literal_is_dropped_not_published() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let prefix = format!("test_telemetry_{uid}");

    let mut sub = client
        .subscribe(format!("{prefix}.>"))
        .await
        .expect("subscribe");

    // Bypass the checking constructor with a struct literal carrying a
    // forbidden `.` in the agent_id.
    let source = TelemetrySource::Agent {
        agent_id: "evil.injection".into(),
    };
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);
    emitter.emit(&TelemetryEvent::TaskAccepted(TaskAccepted {
        common: agent_common("evil.injection", &format!("job-{uid}")),
        dispatch_delay_ms: 0,
        task_publish_ts: None,
        job_age_at_accept_ms: None,
    }));

    // Nothing must be published; the drop counter must have incremented.
    let got = tokio::time::timeout(Duration::from_millis(500), sub.next()).await;
    assert!(got.is_err(), "no message should have been published");
    assert_eq!(emitter.dropped_count(), 1);
}
