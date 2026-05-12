//! Integration tests for `TelemetryEmitterMux` — multi-endpoint
//! fan-out behaviour against a real NATS connection.
//!
//! Skips locally when NATS is unreachable (per the repo's
//! `try_connect_nats` convention); panics in CI so coverage is not
//! silently lost.

use super::common::{nats_url, try_connect_nats, unique_id};
use futures::StreamExt;
use quorum_rs::DeliberationPhase;
use quorum_rs::telemetry::{
    AgentEventCommon, TaskAccepted, TelemetryConfig, TelemetryEmitter, TelemetryEmitterMux,
    TelemetryEndpointConfig, TelemetryEvent, TelemetryMuxError, TelemetrySource, connect_endpoints,
    derive_trace_id,
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

/// `mux.emit()` fans an event out to every configured endpoint.
/// Two subscribers — one per endpoint prefix — both receive a copy
/// of the same event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mux_emit_fans_out_to_every_endpoint() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let agent_id = format!("mux-agent-{uid}");
    let prefix_a = format!("test_mux_{uid}_a");
    let prefix_b = format!("test_mux_{uid}_b");

    let mut sub_a = client
        .subscribe(format!("{prefix_a}.{agent_id}.>"))
        .await
        .expect("subscribe a");
    let mut sub_b = client
        .subscribe(format!("{prefix_b}.{agent_id}.>"))
        .await
        .expect("subscribe b");

    // Two emitters sharing one NATS client, distinct subject
    // prefixes — same shape an operator gets when configuring two
    // endpoints both pointing at the same NATS but different
    // subject roots (cheap simulation; "different cluster" works
    // identically on the wire).
    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter_a = TelemetryEmitter::new(client.clone(), source.clone()).with_prefix(&prefix_a);
    let emitter_b = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix_b);

    let mux = TelemetryEmitterMux::new(vec![
        ("endpoint_a".into(), emitter_a),
        ("endpoint_b".into(), emitter_b),
    ])
    .expect("unique names");
    assert_eq!(mux.len(), 2);
    assert_eq!(mux.endpoint_names(), vec!["endpoint_a", "endpoint_b"]);

    let evt = TelemetryEvent::TaskAccepted(TaskAccepted {
        common: agent_common(&agent_id, &format!("job-{uid}")),
        dispatch_delay_ms: 42,
        task_publish_ts: None,
        job_age_at_accept_ms: None,
    });
    mux.emit(&evt);

    let msg_a = tokio::time::timeout(Duration::from_secs(5), sub_a.next())
        .await
        .expect("endpoint_a timeout")
        .expect("subscription a closed");
    let msg_b = tokio::time::timeout(Duration::from_secs(5), sub_b.next())
        .await
        .expect("endpoint_b timeout")
        .expect("subscription b closed");

    assert_eq!(
        msg_a.subject.as_str(),
        format!("{prefix_a}.{agent_id}.task_accepted")
    );
    assert_eq!(
        msg_b.subject.as_str(),
        format!("{prefix_b}.{agent_id}.task_accepted")
    );
    let evt_a: TelemetryEvent =
        serde_json::from_slice(&msg_a.payload).expect("a payload deserialises");
    let evt_b: TelemetryEvent =
        serde_json::from_slice(&msg_b.payload).expect("b payload deserialises");
    assert_eq!(evt_a, evt);
    assert_eq!(evt_b, evt);
    assert_eq!(mux.dropped_count(), 0);
}

/// `mux.emit_for(name)` targets one endpoint. The other endpoint's
/// subscriber sees nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mux_emit_for_routes_to_named_endpoint_only() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let agent_id = format!("mux-target-{uid}");
    let prefix_target = format!("test_target_{uid}_target");
    let prefix_other = format!("test_target_{uid}_other");

    let mut sub_target = client
        .subscribe(format!("{prefix_target}.{agent_id}.>"))
        .await
        .expect("subscribe target");
    let mut sub_other = client
        .subscribe(format!("{prefix_other}.{agent_id}.>"))
        .await
        .expect("subscribe other");

    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter_target =
        TelemetryEmitter::new(client.clone(), source.clone()).with_prefix(&prefix_target);
    let emitter_other = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix_other);

    let mux = TelemetryEmitterMux::new(vec![
        ("target".into(), emitter_target),
        ("other".into(), emitter_other),
    ])
    .expect("unique names");

    let evt = TelemetryEvent::TaskAccepted(TaskAccepted {
        common: agent_common(&agent_id, &format!("job-{uid}")),
        dispatch_delay_ms: 0,
        task_publish_ts: None,
        job_age_at_accept_ms: None,
    });
    mux.emit_for("target", &evt);

    // Targeted endpoint receives the event.
    let msg = tokio::time::timeout(Duration::from_secs(5), sub_target.next())
        .await
        .expect("target timeout")
        .expect("target sub closed");
    assert_eq!(
        msg.subject.as_str(),
        format!("{prefix_target}.{agent_id}.task_accepted")
    );

    // The other endpoint must NOT receive the event — wait briefly
    // and assert no message arrives.
    let other_got = tokio::time::timeout(Duration::from_millis(500), sub_other.next()).await;
    assert!(
        other_got.is_err(),
        "non-targeted endpoint must not receive (got {other_got:?})"
    );
}

/// Calling `emit_for` with an unknown name is a silent no-op — drops
/// don't accumulate, no panic. Lets operators ship events with
/// optional targeting hints that simply don't fire when the named
/// endpoint isn't configured.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mux_emit_for_unknown_name_is_silent_noop() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let agent_id = format!("mux-unk-{uid}");
    let prefix = format!("test_unk_{uid}");

    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");

    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    let mux = TelemetryEmitterMux::new(vec![("real".into(), emitter)]).expect("unique names");

    let evt = TelemetryEvent::TaskAccepted(TaskAccepted {
        common: agent_common(&agent_id, &format!("job-{uid}")),
        dispatch_delay_ms: 0,
        task_publish_ts: None,
        job_age_at_accept_ms: None,
    });
    mux.emit_for("does-not-exist", &evt);

    // Nothing should land — the named endpoint isn't configured.
    let got = tokio::time::timeout(Duration::from_millis(500), sub.next()).await;
    assert!(got.is_err(), "unknown endpoint must not publish anywhere");
    assert_eq!(
        mux.dropped_count(),
        0,
        "unknown endpoint is silent, not a drop"
    );
}

/// `connect_endpoints` happy path: builds a mux with one connected
/// emitter per endpoint, both publish onto the right NATS subjects.
/// Both endpoints point at the same NATS instance (different subject
/// prefixes) — the wire shape is identical to a "different cluster"
/// deployment, just cheaper to test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_endpoints_builds_mux_that_publishes() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let agent_id = format!("conn-{uid}");
    let prefix_a = format!("test_conn_{uid}_a");
    let prefix_b = format!("test_conn_{uid}_b");

    let mut sub_a = client
        .subscribe(format!("{prefix_a}.{agent_id}.>"))
        .await
        .expect("subscribe a");
    let mut sub_b = client
        .subscribe(format!("{prefix_b}.{agent_id}.>"))
        .await
        .expect("subscribe b");

    let cfg = TelemetryConfig {
        enabled: true,
        endpoints: vec![
            TelemetryEndpointConfig {
                name: "service".into(),
                nats_url: Some(nats_url()),
                creds: None,
                subject_prefix: Some(prefix_a.clone()),
            },
            TelemetryEndpointConfig {
                name: "own".into(),
                nats_url: Some(nats_url()),
                creds: None,
                subject_prefix: Some(prefix_b.clone()),
            },
        ],
    };

    let mux = connect_endpoints(&cfg, &agent_id)
        .await
        .expect("connect ok")
        .expect("mux returned because enabled + endpoints non-empty");

    assert_eq!(mux.len(), 2);
    assert_eq!(mux.endpoint_names(), vec!["service", "own"]);

    let evt = TelemetryEvent::TaskAccepted(TaskAccepted {
        common: AgentEventCommon {
            agent_id: agent_id.clone(),
            job_id: Some(format!("job-{uid}")),
            round: Some(1),
            phase: Some(DeliberationPhase::Proposing),
            ts: 1_776_790_692_747,
            trace_id: derive_trace_id(
                &format!("job-{uid}"),
                1,
                DeliberationPhase::Proposing,
                &agent_id,
            ),
        },
        dispatch_delay_ms: 0,
        task_publish_ts: None,
        job_age_at_accept_ms: None,
    });
    mux.emit(&evt);

    let msg_a = tokio::time::timeout(Duration::from_secs(5), sub_a.next())
        .await
        .expect("a timeout")
        .expect("a closed");
    let msg_b = tokio::time::timeout(Duration::from_secs(5), sub_b.next())
        .await
        .expect("b timeout")
        .expect("b closed");
    assert_eq!(
        msg_a.subject.as_str(),
        format!("{prefix_a}.{agent_id}.task_accepted")
    );
    assert_eq!(
        msg_b.subject.as_str(),
        format!("{prefix_b}.{agent_id}.task_accepted")
    );
}

/// Construction with duplicate endpoint names must fail. Exercises
/// the `TelemetryEmitterMux::new` Result path with real emitters —
/// the synchronous unit-test variant lives in
/// `crates/nsed-agent-sdk/src/telemetry.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mux_new_with_duplicate_names_returns_err() {
    let Some(client) = try_connect_nats().await else {
        return;
    };

    let uid = unique_id();
    let agent_id = format!("mux-dup-{uid}");
    let prefix = format!("test_dup_{uid}");
    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");

    let emitter_a = TelemetryEmitter::new(client.clone(), source.clone()).with_prefix(&prefix);
    let emitter_b = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);

    let err = TelemetryEmitterMux::new(vec![("dup".into(), emitter_a), ("dup".into(), emitter_b)])
        .expect_err("duplicate names must fail");

    match err {
        TelemetryMuxError::DuplicateNames(names) => {
            assert_eq!(names, vec!["dup".to_string()]);
        }
    }
}
