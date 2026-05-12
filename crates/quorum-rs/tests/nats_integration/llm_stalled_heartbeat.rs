//! Integration coverage for #364 — `llm_request_stalled` 30s heartbeat.
//!
//! Tests use a sub-second `stalled_interval` via `start_with_stalled_interval`
//! so the spawn-cancel contract can be exercised in real time without
//! 90-second waits. The production `start()` constructor still locks the
//! cadence at the 30s the issue requires; the override is `#[doc(hidden)]`.

use super::common::{try_connect_nats, unique_id};

use async_openai::types::CreateChatCompletionResponse;
use futures::StreamExt;
use quorum_rs::DeliberationPhase;
use quorum_rs::llms::span::LlmRequestSpan;
use quorum_rs::llms::{ChatCompletionResult, TimingMetadata};
use quorum_rs::telemetry::{
    LlmError, TelemetryContext, TelemetryEmitter, TelemetryEmitterMux, TelemetrySource,
};

use std::time::Duration;

fn ctx(agent_id: &str, job_id: &str) -> TelemetryContext {
    TelemetryContext::new(
        agent_id,
        Some(job_id),
        Some(1),
        Some(DeliberationPhase::Proposing),
    )
}

/// Minimal `ChatCompletionResult` used to exercise the success-path
/// terminal `complete()`. The fields not relevant to the heartbeat
/// cancel-vs-emit ordering contract are zero-valued.
fn empty_chat_result() -> ChatCompletionResult {
    ChatCompletionResult {
        response: CreateChatCompletionResponse {
            id: "test-resp".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "mock".to_string(),
            choices: vec![],
            usage: None,
            service_tier: None,
            system_fingerprint: None,
        },
        raw_request: String::new(),
        timing: TimingMetadata {
            ttft_ms: None,
            generation_ms: None,
        },
        provider_backend: None,
        shrink_info: None,
    }
}

async fn drain_kinds(
    sub: &mut async_nats::Subscriber,
    deadline: std::time::Instant,
) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), sub.next()).await {
            Ok(Some(msg)) => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg.payload)
                    && let Some(t) = v["type"].as_str()
                {
                    out.push((t.to_string(), v));
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    out
}

/// Acceptance criterion: a 90s in-flight request emits exactly 3
/// `llm_request_stalled` events (at +30s, +60s, +90s). Test scales the
/// interval to 100ms so the wall-clock equivalent runs in ~300ms; the
/// invariant is "N stalled events for 3N intervals of in-flight time".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_stalled_events_across_three_intervals() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("stalled-agent-{uid}");
    let prefix = format!("test_stalled_{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");

    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);
    let mux = TelemetryEmitterMux::single("test", emitter);
    let context = ctx(&agent_id, &format!("job-{uid}"));

    let interval = Duration::from_millis(100);
    let mut span = LlmRequestSpan::start_with_stalled_interval(
        Some(&mux),
        &context,
        "req-1",
        1,
        "mock-model",
        "mock",
        100,
        0.0,
        0,
        interval,
    );

    // Three intervals + a small safety margin for scheduler jitter.
    tokio::time::sleep(interval * 3 + Duration::from_millis(40)).await;

    // Failure path also cancels the heartbeat — covers the spawn-cancel
    // contract for the failure terminal too. The Stalled count must
    // reflect what fired BEFORE this terminal call.
    span.fail(&LlmError::Other(Box::new(std::io::Error::other(
        "simulated",
    ))))
    .await;

    client.flush().await.expect("flush");

    let drained = drain_kinds(
        &mut sub,
        std::time::Instant::now() + Duration::from_millis(300),
    )
    .await;
    let stalled_count = drained
        .iter()
        .filter(|(k, _)| k == "llm_request_stalled")
        .count();
    assert_eq!(
        stalled_count,
        3,
        "expected 3 stalled events across 3 intervals; saw {stalled_count}: {:?}",
        drained.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );

    let elapsed_values: Vec<u64> = drained
        .iter()
        .filter_map(|(k, v)| {
            if k == "llm_request_stalled" {
                v["elapsed_ms"].as_u64()
            } else {
                None
            }
        })
        .collect();
    // Each subsequent stalled event must have a strictly greater
    // elapsed_ms than the previous; guards against the heartbeat
    // emitting on a stale instant.
    for window in elapsed_values.windows(2) {
        assert!(
            window[1] > window[0],
            "stalled events must be monotonically increasing: {elapsed_values:?}"
        );
    }
}

/// Acceptance criterion: cancellation is exact — no
/// `llm_request_stalled` is published after the corresponding
/// terminal event. Strategy: terminate the span immediately, sleep
/// well past several intervals, and assert zero stalled events arrived
/// post-terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_stalled_after_complete() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("cancel-agent-{uid}");
    let prefix = format!("test_cancel_{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");

    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);
    let mux = TelemetryEmitterMux::single("test", emitter);
    let context = ctx(&agent_id, &format!("job-{uid}"));

    let interval = Duration::from_millis(100);
    let mut span = LlmRequestSpan::start_with_stalled_interval(
        Some(&mux),
        &context,
        "req-cancel",
        1,
        "mock-model",
        "mock",
        100,
        0.0,
        0,
        interval,
    );

    // Sleep enough to land one stalled event, then terminate.
    tokio::time::sleep(interval + Duration::from_millis(40)).await;
    span.fail(&LlmError::Other(Box::new(std::io::Error::other(
        "terminate now",
    ))))
    .await;

    // Now wait several more intervals — no further stalled may arrive.
    tokio::time::sleep(interval * 4).await;
    client.flush().await.expect("flush");

    let drained = drain_kinds(
        &mut sub,
        std::time::Instant::now() + Duration::from_millis(300),
    )
    .await;

    let stalled_after_terminal = drained
        .iter()
        .scan(false, |seen_terminal, (k, _)| {
            if *seen_terminal && k == "llm_request_stalled" {
                Some(true)
            } else {
                if k == "llm_request_failed" || k == "llm_request_complete" {
                    *seen_terminal = true;
                }
                Some(false)
            }
        })
        .filter(|x| *x)
        .count();
    assert_eq!(
        stalled_after_terminal, 0,
        "no stalled events may follow the terminal event; got {drained:?}"
    );
    let total_stalled = drained
        .iter()
        .filter(|(k, _)| k == "llm_request_stalled")
        .count();
    assert!(
        total_stalled <= 2,
        "with a single ~interval window before terminate, at most 1-2 stalled fire (jitter); got {total_stalled}"
    );
}

/// Companion to `no_stalled_after_complete` (which exercises `fail()`):
/// asserts the success-path terminal `complete()` provides the same
/// no-Stalled-after-terminal guarantee. The cancel-await is shared
/// between both terminals, but having both branches under test guards
/// against a future regression that wires only one path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_stalled_after_complete_success_path() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("complete-agent-{uid}");
    let prefix = format!("test_complete_{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");

    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);
    let mux = TelemetryEmitterMux::single("test", emitter);
    let context = ctx(&agent_id, &format!("job-{uid}"));

    let interval = Duration::from_millis(100);
    let mut span = LlmRequestSpan::start_with_stalled_interval(
        Some(&mux),
        &context,
        "req-complete",
        1,
        "mock-model",
        "mock",
        100,
        0.0,
        0,
        interval,
    );

    // Land at least one stalled before terminating.
    tokio::time::sleep(interval + Duration::from_millis(40)).await;
    span.complete(&empty_chat_result(), 0.0, 0, None).await;

    // Wait several more intervals — no further stalled.
    tokio::time::sleep(interval * 4).await;
    client.flush().await.expect("flush");

    let drained = drain_kinds(
        &mut sub,
        std::time::Instant::now() + Duration::from_millis(300),
    )
    .await;
    let stalled_after_terminal = drained
        .iter()
        .scan(false, |seen_terminal, (k, _)| {
            if *seen_terminal && k == "llm_request_stalled" {
                Some(true)
            } else {
                if k == "llm_request_complete" || k == "llm_request_failed" {
                    *seen_terminal = true;
                }
                Some(false)
            }
        })
        .filter(|x| *x)
        .count();
    assert_eq!(
        stalled_after_terminal, 0,
        "no Stalled may follow Complete; got {drained:?}"
    );
}

/// First emission must be at `+interval`, not `+0s`. A request that
/// completes inside the first interval emits zero stalled events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_request_emits_no_stalled() {
    let Some(client) = try_connect_nats().await else {
        return;
    };
    let uid = unique_id();
    let agent_id = format!("fast-agent-{uid}");
    let prefix = format!("test_fast_{uid}");
    let mut sub = client
        .subscribe(format!("{prefix}.{agent_id}.>"))
        .await
        .expect("subscribe");

    let source = TelemetrySource::agent(&agent_id).expect("valid agent_id");
    let emitter = TelemetryEmitter::new(client.clone(), source).with_prefix(&prefix);
    let mux = TelemetryEmitterMux::single("test", emitter);
    let context = ctx(&agent_id, &format!("job-{uid}"));

    let interval = Duration::from_millis(200);
    let mut span = LlmRequestSpan::start_with_stalled_interval(
        Some(&mux),
        &context,
        "req-fast",
        1,
        "mock-model",
        "mock",
        100,
        0.0,
        0,
        interval,
    );

    // Terminate well before the first interval would tick.
    tokio::time::sleep(Duration::from_millis(30)).await;
    span.fail(&LlmError::Other(Box::new(std::io::Error::other(
        "fast-fail",
    ))))
    .await;
    client.flush().await.expect("flush");

    let drained = drain_kinds(
        &mut sub,
        std::time::Instant::now() + Duration::from_millis(400),
    )
    .await;
    let stalled = drained
        .iter()
        .filter(|(k, _)| k == "llm_request_stalled")
        .count();
    assert_eq!(
        stalled, 0,
        "fast request must emit zero stalled (first tick is at +interval, not +0); got {drained:?}"
    );
}
