//! Additional NATS integration tests targeting uncovered lines in `workers/mod.rs`.
//!
//! Coverage targets:
//! - `handle_round_summary()` — score event handling (~lines 778-829)
//! - `handle_manifest()` — agent NOT in manifest list (~lines 708-770)
//! - Response buffer path — buffered propose task (~lines 1058-1116)
//! - Worker pause/resume — task suppression when paused (~lines 642-648)
//! - `handle_message()` evaluate error path (~lines 1151-1219)
//! - `previous_own_score` recording in status (~lines 962-982)
//! - `agent_working` event publication (~lines 893-913)
//! - Invalid manifest JSON (poison pill manifest) (~lines 708-715)

use crate::common::*;
use async_nats::jetstream;
use futures::StreamExt;
use serial_test::serial;

/// Helper: start a worker in a background task and return the join handle.
/// Panics inside the spawned task if run() returns an error, so that
/// the JoinHandle propagates the failure back to the test.
async fn start_worker_background(
    worker: &quorum_rs::workers::NatsNsedWorker,
) -> tokio::task::JoinHandle<()> {
    let w = worker.clone();
    tokio::spawn(async move {
        if let Err(e) = w.run().await {
            panic!("Worker run() exited with error: {:?}", e);
        }
    })
}

// ---------------------------------------------------------------------------
// handle_round_summary tests
// ---------------------------------------------------------------------------

/// Publish a round_summary event containing this agent's score.
/// Verify the status snapshot records the score entry.
#[tokio::test]
#[serial]
async fn test_worker_round_summary_records_score() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("score_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    // Enable status tracking so we can verify score recording
    let worker = worker.with_status(9100);
    let status_handle = worker.status().expect("status should be Some").clone();

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Build and publish a round_summary event with this agent's score
    let round_summary = quorum_rs::events::RoundSummaryEvent {
        round: 1,
        convergence_score: 0.82,
        decisiveness: 0.82,
        net_support: vec![],
        cesaro_support: vec![],
        raw_distance: None,
        claim_convergence: None,
        total_claims: None,
        leader_claim_convergence: None,
        leader_total_claims: None,
        controversy_scores: vec![],
        proposal_scores: vec![
            quorum_rs::events::ProposalScoreEntry {
                agent_id: agent_name.clone(),
                aggregated_score: 7.5,
                category_breakdown: None,
                controversy_score: None,
                ..Default::default()
            },
            quorum_rs::events::ProposalScoreEntry {
                agent_id: "other_agent".to_string(),
                aggregated_score: 6.0,
                category_breakdown: None,
                controversy_score: None,
                ..Default::default()
            },
        ],
        accumulated_evidence: None,
        evidence_target: None,
        positive_budget: None,
        du_dt: None,
        signed_consensus: None,
        t_opt: None,
        thermo_probability: None,
        ..Default::default()
    };

    let summary_subject = format!("{}.{}.result.event.round_summary", sp, session_id);
    let payload = serde_json::to_vec(&round_summary).expect("serialize round_summary");

    // Publish via core NATS (round_summary uses core NATS subscription, not JetStream)
    client
        .publish(summary_subject, payload.into())
        .await
        .expect("publish round_summary");

    // Give the worker time to process the score event
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Verify the status snapshot has the score entry
    let snap = status_handle.read().await;
    assert!(
        !snap.recent_scores.is_empty(),
        "Should have recorded at least one score, got: {:?}",
        snap.recent_scores
    );

    let score = &snap.recent_scores[0];
    assert_eq!(
        score.job_id, session_id,
        "Score job_id should match session_id"
    );
    assert_eq!(score.round, 1, "Score round should be 1");
    assert!(
        (score.score - 7.5).abs() < f32::EPSILON,
        "Score should be 7.5, got {}",
        score.score
    );

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Publish a round_summary event that does NOT contain this agent's score.
/// Verify no score is recorded in the status.
#[tokio::test]
#[serial]
async fn test_worker_round_summary_ignores_other_agent_scores() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("noscore_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let worker = worker.with_status(9101);
    let status_handle = worker.status().expect("status should be Some").clone();

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Publish a round_summary that doesn't include this agent
    let round_summary = quorum_rs::events::RoundSummaryEvent {
        round: 1,
        convergence_score: 0.5,
        decisiveness: 0.5,
        net_support: vec![],
        cesaro_support: vec![],
        raw_distance: None,
        claim_convergence: None,
        total_claims: None,
        leader_claim_convergence: None,
        leader_total_claims: None,
        controversy_scores: vec![],
        proposal_scores: vec![quorum_rs::events::ProposalScoreEntry {
            agent_id: "completely_different_agent".to_string(),
            aggregated_score: 8.0,
            category_breakdown: None,
            controversy_score: None,
            ..Default::default()
        }],
        accumulated_evidence: None,
        evidence_target: None,
        positive_budget: None,
        du_dt: None,
        signed_consensus: None,
        t_opt: None,
        thermo_probability: None,
        ..Default::default()
    };

    let summary_subject = format!("{}.{}.result.event.round_summary", sp, session_id);
    let payload = serde_json::to_vec(&round_summary).expect("serialize");
    client
        .publish(summary_subject, payload.into())
        .await
        .expect("publish round_summary");

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Verify NO score was recorded for this agent
    let snap = status_handle.read().await;
    assert!(
        snap.recent_scores.is_empty(),
        "Should have no scores when agent not in summary, got: {:?}",
        snap.recent_scores
    );

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Publish malformed JSON to the round_summary subject.
/// Verify the worker does not crash and continues processing.
#[tokio::test]
#[serial]
async fn test_worker_round_summary_malformed_no_crash() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("badscore_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Publish malformed JSON to round_summary subject
    let summary_subject = format!("{}.{}.result.event.round_summary", sp, session_id);
    client
        .publish(summary_subject, b"NOT VALID JSON {{{"[..].into())
        .await
        .expect("publish malformed summary");

    // Wait a moment, then verify the worker is still alive by sending a valid task
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to result
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client.subscribe(result_subject).await.expect("subscribe");

    // Publish a valid task
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack");

    // Worker should still be alive and process the task
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("result should arrive (worker survived malformed summary)")
        .expect("should receive result");

    let proposal: quorum_rs::Proposal =
        serde_json::from_slice(&result.payload).expect("valid Proposal JSON");
    assert!(proposal.content.contains(&agent_name));

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// handle_manifest edge cases
// ---------------------------------------------------------------------------

/// Publish a manifest where the agent is NOT in the agents list.
/// Verify no ACK is published (the manifest is acked to JetStream but
/// no application-level ACK event is emitted).
#[tokio::test]
#[serial]
async fn test_worker_manifest_agent_not_in_list() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("notlisted_agent_{}", uid);
    let job_id = format!("job_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to the ACK subject — we expect NO message
    let ack_subject = format!("{}.jobs.ack.{}.{}", ap, job_id, agent_name);
    let mut ack_sub = client.subscribe(ack_subject).await.expect("subscribe");

    // Publish a manifest that does NOT include this agent
    let manifest = quorum_rs::workers::JobManifest {
        job_id: job_id.clone(),
        task_description: "Not for this agent".to_string(),
        agents: vec!["other_agent_1".to_string(), "other_agent_2".to_string()],
        rounds: 3,
        timestamp: 1704067200,
    };
    let manifest_subject = format!("{}.jobs.manifest.{}", ap, job_id);
    let payload = serde_json::to_vec(&manifest).expect("serialize manifest");
    js.publish(manifest_subject, payload.into())
        .await
        .expect("publish manifest")
        .await
        .expect("ack from server");

    // Wait briefly — if an ACK were sent, it would arrive within this window
    let ack_result = tokio::time::timeout(std::time::Duration::from_secs(2), ack_sub.next()).await;
    assert!(
        ack_result.is_err(),
        "Should NOT receive an ACK when agent is not in the manifest agent list"
    );

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Publish invalid JSON to the manifest subject. Verify the worker
/// does not crash and continues to process subsequent valid manifests.
#[tokio::test]
#[serial]
async fn test_worker_manifest_invalid_json() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("badmanifest_agent_{}", uid);
    let job_id = format!("job_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to ACK subject for the valid manifest that follows
    let ack_subject = format!("{}.jobs.ack.{}.{}", ap, job_id, agent_name);
    let mut ack_sub = client.subscribe(ack_subject).await.expect("subscribe");

    // Publish invalid JSON as a manifest
    let bad_manifest_subject = format!("{}.jobs.manifest.badjob", ap);
    js.publish(bad_manifest_subject, b"NOT VALID JSON {{{"[..].into())
        .await
        .expect("publish bad manifest")
        .await
        .expect("ack");

    // Wait for the worker to process (and discard) the bad manifest
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Now publish a valid manifest — worker should still be alive
    let manifest = quorum_rs::workers::JobManifest {
        job_id: job_id.clone(),
        task_description: "Valid manifest after poison".to_string(),
        agents: vec![agent_name.clone()],
        rounds: 2,
        timestamp: 1704067200,
    };
    let manifest_subject = format!("{}.jobs.manifest.{}", ap, job_id);
    let payload = serde_json::to_vec(&manifest).expect("serialize manifest");
    js.publish(manifest_subject, payload.into())
        .await
        .expect("publish valid manifest")
        .await
        .expect("ack");

    // Worker should process the valid manifest and send ACK
    let ack_msg = tokio::time::timeout(std::time::Duration::from_secs(10), ack_sub.next())
        .await
        .expect("ACK should arrive after valid manifest (worker survived poison)")
        .expect("should receive ACK message");

    let ack: serde_json::Value = serde_json::from_slice(&ack_msg.payload).expect("valid ACK JSON");
    assert_eq!(ack["agent_id"], agent_name);
    assert_eq!(ack["status"], "Accepted");

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// Response buffer path
// ---------------------------------------------------------------------------

/// Create a worker with `.with_response_buffer()`, submit a propose task,
/// and verify the response is buffered (not published immediately).
/// Then manually release the entry and verify the response is published.
///
/// We verify the entry lands in the buffer, then call `mark_for_release()`
/// to force immediate release and confirm the response appears on NATS.
#[tokio::test]
#[serial]
async fn test_worker_response_buffer_holds_then_releases() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("buffer_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    // Enable buffer with a non-zero hold. Disable auto-approve so
    // the buffer actually holds (auto-approve=true would bypass the SLA).
    let worker = worker.with_response_buffer(std::time::Duration::from_secs(10));
    let buffer = worker
        .response_buffer()
        .expect("buffer should be Some")
        .clone();
    buffer.set_auto_approve(false);

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to the result subject
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client.subscribe(result_subject).await.expect("subscribe");

    // Publish a propose task
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack");

    // Wait for the agent to complete the task and buffer the response
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Verify the response is in the buffer
    let buf_len = buffer.len().await;
    assert!(
        buf_len > 0,
        "Buffer should contain at least one entry after task completes, got {}",
        buf_len
    );

    // The response should NOT have been published yet (SLA hold is ~5 min)
    let early_result =
        tokio::time::timeout(std::time::Duration::from_millis(500), result_sub.next()).await;
    assert!(
        early_result.is_err(),
        "Response should NOT be published before buffer hold expires"
    );

    // Get the entry ID and force immediate release
    let entries = buffer.list().await;
    assert_eq!(entries.len(), 1, "Should have exactly one buffered entry");
    let entry_id = &entries[0].id;
    let released = buffer.mark_for_release(entry_id).await;
    assert!(released, "mark_for_release should succeed");

    // Wait for the drain cycle to pick up the released entry (drain runs every 500ms)
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Now the response should have been drained and published
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), result_sub.next())
        .await
        .expect("result should arrive after mark_for_release")
        .expect("should receive result");

    let proposal: quorum_rs::Proposal =
        serde_json::from_slice(&result.payload).expect("valid Proposal JSON");
    assert!(
        proposal.content.contains(&agent_name),
        "Proposal content should contain agent name"
    );

    // Buffer should be empty now
    let buf_len_after = buffer.len().await;
    assert_eq!(
        buf_len_after, 0,
        "Buffer should be empty after drain, got {}",
        buf_len_after
    );

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Create a worker with response buffer AND status tracking.
/// Verify that status records response_buffered and buffer_released events.
#[tokio::test]
#[serial]
async fn test_worker_response_buffer_status_events() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("bufstat_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    // Enable both buffer and status. Disable auto-approve to test hold behavior.
    let worker = worker
        .with_response_buffer(std::time::Duration::from_secs(10))
        .with_status(9102);
    let status_handle = worker.status().expect("status").clone();
    let buffer = worker.response_buffer().expect("buffer").clone();
    buffer.set_auto_approve(false);

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to result so we know when the buffer drains
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client.subscribe(result_subject).await.expect("subscribe");

    // Publish a propose task
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack");

    // Wait for the task to be processed and buffered
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Verify response_buffered event was recorded before we release
    {
        let snap = status_handle.read().await;
        let event_types: Vec<&str> = snap
            .event_log
            .iter()
            .map(|e| e.event_type.as_str())
            .collect();
        assert!(
            event_types.contains(&"response_buffered"),
            "Should have 'response_buffered' event before release, got: {:?}",
            event_types
        );
    }

    // Force immediate release via mark_for_release
    let entries = buffer.list().await;
    assert!(!entries.is_empty(), "Buffer should have at least one entry");
    let entry_id = &entries[0].id;
    buffer.mark_for_release(entry_id).await;

    // Wait for drain cycle
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Consume the result to confirm drain happened
    let _result = tokio::time::timeout(std::time::Duration::from_secs(5), result_sub.next())
        .await
        .expect("result should arrive after mark_for_release")
        .expect("should receive result");

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify status events
    let snap = status_handle.read().await;
    let event_types: Vec<&str> = snap
        .event_log
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();

    assert!(
        event_types.contains(&"response_buffered"),
        "Should have 'response_buffered' event, got: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"buffer_released"),
        "Should have 'buffer_released' event, got: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"task_complete"),
        "Should have 'task_complete' event after buffer drain, got: {:?}",
        event_types
    );

    // Verify task log still records the task
    assert!(
        !snap.recent_tasks.is_empty(),
        "Should have at least one task record"
    );

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// Worker pause/resume
// ---------------------------------------------------------------------------

/// Verify the pause/resume API works correctly.
/// Pause the worker BEFORE starting it, publish a task, verify it is NOT
/// processed. Resume, then verify processing completes.
///
/// The worker's `select!` loop disables the `task_messages.next()` branch
/// when `is_paused` is true, so tasks stay in JetStream unconsumed.
#[tokio::test]
#[serial]
async fn test_worker_pause_resume() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("pause_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    // Pause BEFORE starting the worker so the consumer never pulls tasks
    worker.pause();
    assert!(worker.is_paused(), "Worker should be paused");

    let handle = start_worker_background(&worker).await;
    // Give the worker time to bind consumers and enter the select loop in paused state
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Subscribe to result
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client.subscribe(result_subject).await.expect("subscribe");

    // Publish a task while paused
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack");

    // The task should NOT be processed while paused (wait 3s to be sure)
    let paused_result =
        tokio::time::timeout(std::time::Duration::from_secs(3), result_sub.next()).await;
    assert!(
        paused_result.is_err(),
        "Task should NOT be processed while worker is paused"
    );

    // Resume the worker
    worker.resume();
    assert!(!worker.is_paused(), "Worker should be resumed");

    // Now the task should be processed (JetStream will redeliver or the consumer will pull)
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), result_sub.next())
        .await
        .expect("result should arrive after resume")
        .expect("should receive result");

    let proposal: quorum_rs::Proposal =
        serde_json::from_slice(&result.payload).expect("valid Proposal JSON");
    assert!(proposal.content.contains(&agent_name));

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// FailingMockAgent evaluate error path
// ---------------------------------------------------------------------------

/// Use FailingMockAgent. Publish an EVALUATE task. Verify an error event
/// is emitted on the agent_error subject (covers error path for evaluate action).
#[tokio::test]
#[serial]
async fn test_worker_error_event_on_evaluate_failure() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("evalfail_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = FailingMockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    // Enable status to verify error recording
    let worker = worker.with_status(9103);
    let status_handle = worker.status().expect("status").clone();

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to error event
    let error_subject = format!("{}.{}.result.event.agent_error", sp, session_id);
    let mut error_sub = client.subscribe(error_subject).await.expect("subscribe");

    // Publish an EVALUATE task (not propose)
    let task_subject = format!("{}.{}.task.{}.evaluate", sp, session_id, agent_name);
    let context = test_evaluate_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish evaluate task")
        .await
        .expect("ack");

    // Wait for the error event
    let error_msg = tokio::time::timeout(std::time::Duration::from_secs(10), error_sub.next())
        .await
        .expect("error event should arrive within 10 seconds")
        .expect("should receive error event");

    let error_event: serde_json::Value =
        serde_json::from_slice(&error_msg.payload).expect("valid error JSON");
    assert_eq!(error_event["agent_id"], agent_name);
    assert_eq!(error_event["action"], "evaluate");
    assert_eq!(error_event["status"], "Failed");
    assert!(
        error_event["error"]
            .as_str()
            .unwrap_or("")
            .contains("intentional evaluate failure"),
        "Error message should contain our intentional failure text"
    );

    // Verify status recorded the error
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let snap = status_handle.read().await;
    let last_task = snap.recent_tasks.back().expect("should have task record");
    assert_eq!(last_task.action, "evaluate");
    assert_eq!(last_task.status, "error");

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// previous_own_score recording
// ---------------------------------------------------------------------------

/// Submit a propose task for round 2 with `previous_own_score` set.
/// Verify the status snapshot records the score for round 1.
#[tokio::test]
#[serial]
async fn test_worker_previous_own_score_recorded_in_status() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("prevscore_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let worker = worker.with_status(9104);
    let status_handle = worker.status().expect("status").clone();

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to result to know when the task completes
    let result_subject = format!("{}.{}.result.2.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client.subscribe(result_subject).await.expect("subscribe");

    // Build a round-2 propose context with previous_own_score
    let context = quorum_rs::agents::AgentContext {
        task_description: "Test deliberation task".to_string(),
        round_number: 2,
        total_rounds: 3,
        phase: quorum_rs::DeliberationPhase::Proposing,
        session_id: Some(session_id.clone()),
        previous_own_score: Some(0.72),
        ..Default::default()
    };

    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack");

    // Wait for the result
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("result should arrive")
        .expect("should receive result");

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify status recorded the score for round 1 (previous round)
    let snap = status_handle.read().await;
    assert!(
        !snap.recent_scores.is_empty(),
        "Should have recorded the previous_own_score, got: {:?}",
        snap.recent_scores
    );

    let score = &snap.recent_scores[0];
    assert_eq!(
        score.round, 1,
        "Score should be for round 1 (previous round)"
    );
    assert!(
        (score.score - 0.72).abs() < f32::EPSILON,
        "Score should be 0.72, got {}",
        score.score
    );

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// agent_working event
// ---------------------------------------------------------------------------

/// Submit a propose task and verify the `agent_working` event is published
/// on the result event subject before the task completes.
#[tokio::test]
#[serial]
async fn test_worker_publishes_agent_working_event() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("working_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to the agent_working event subject
    let working_subject = format!("{}.{}.result.event.agent_working", sp, session_id);
    let mut working_sub = client.subscribe(working_subject).await.expect("subscribe");

    // Also subscribe to result to ensure the task completes
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client.subscribe(result_subject).await.expect("subscribe");

    // Publish a propose task
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack");

    // Wait for both the working event and the result
    let working_msg = tokio::time::timeout(std::time::Duration::from_secs(10), working_sub.next())
        .await
        .expect("agent_working event should arrive within 10 seconds")
        .expect("should receive working event");

    let working_event: serde_json::Value =
        serde_json::from_slice(&working_msg.payload).expect("valid working event JSON");
    assert_eq!(working_event["agent_id"], agent_name);
    assert_eq!(working_event["action"], "propose");
    assert_eq!(working_event["status"], "Thinking");
    assert_eq!(working_event["round"], 1);

    // Also confirm the result arrives (task completes successfully)
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("result should arrive")
        .expect("should receive result");

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// agent_accepted event on manifest
// ---------------------------------------------------------------------------

/// Publish a manifest containing this agent. Verify the `agent_accepted`
/// event is published on the result event subject (in addition to the ACK).
#[tokio::test]
#[serial]
async fn test_worker_manifest_publishes_agent_accepted_event() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("accepted_agent_{}", uid);
    let job_id = format!("job_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to the agent_accepted event subject
    let accepted_subject = format!("{}.{}.result.event.agent_accepted", sp, job_id);
    let mut accepted_sub = client.subscribe(accepted_subject).await.expect("subscribe");

    // Publish a manifest
    let manifest = quorum_rs::workers::JobManifest {
        job_id: job_id.clone(),
        task_description: "Testing accepted event".to_string(),
        agents: vec![agent_name.clone()],
        rounds: 3,
        timestamp: 1704067200,
    };
    let manifest_subject = format!("{}.jobs.manifest.{}", ap, job_id);
    let payload = serde_json::to_vec(&manifest).expect("serialize manifest");
    js.publish(manifest_subject, payload.into())
        .await
        .expect("publish manifest")
        .await
        .expect("ack");

    // Wait for the agent_accepted event
    let accepted_msg =
        tokio::time::timeout(std::time::Duration::from_secs(10), accepted_sub.next())
            .await
            .expect("agent_accepted event should arrive within 10 seconds")
            .expect("should receive agent_accepted event");

    let event: serde_json::Value =
        serde_json::from_slice(&accepted_msg.payload).expect("valid event JSON");
    assert_eq!(event["agent_id"], agent_name);
    assert_eq!(event["status"], "Online");
    assert_eq!(event["role"], "Generalist");

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// Heartbeat with status snapshot (busy/idle)
// ---------------------------------------------------------------------------

/// Start worker with status, submit a task, then verify heartbeat
/// contains reliability stats (tasks_completed, tasks_failed fields).
#[tokio::test]
#[serial]
async fn test_worker_heartbeat_with_status_reliability_stats() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("hbstat_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let worker = worker.with_status(9105);

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to result to wait for task completion
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client.subscribe(result_subject).await.expect("subscribe");

    // Publish a propose task
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack");

    // Wait for task to complete
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("result should arrive")
        .expect("should receive result");

    // Subscribe to heartbeat and wait for one after task completion
    let hb_subject = format!("{}.agent.heartbeat.{}", ap, agent_name);
    let mut hb_sub = client.subscribe(hb_subject).await.expect("subscribe");

    let hb_msg = tokio::time::timeout(std::time::Duration::from_secs(15), hb_sub.next())
        .await
        .expect("heartbeat should arrive within 15 seconds")
        .expect("should receive heartbeat");

    let heartbeat: serde_json::Value =
        serde_json::from_slice(&hb_msg.payload).expect("valid heartbeat JSON");
    assert_eq!(heartbeat["agent_id"], agent_name);

    // After completing one task, tasks_completed should be >= 1
    let tasks_completed = heartbeat["tasks_completed"].as_u64().unwrap_or(0);
    assert!(
        tasks_completed >= 1,
        "tasks_completed should be >= 1 after completing a task, got {}",
        tasks_completed
    );

    // Verify other heartbeat fields are present
    assert!(
        heartbeat.get("model_name").is_some(),
        "heartbeat should contain model_name"
    );
    assert!(
        heartbeat.get("provider_id").is_some(),
        "heartbeat should contain provider_id"
    );
    assert!(
        heartbeat.get("status").is_some(),
        "heartbeat should contain status"
    );

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// Duplicate round_summary dedup
// ---------------------------------------------------------------------------

/// Publish the same round_summary twice. Verify only ONE score entry
/// is recorded (deduplication by session_id + round).
#[tokio::test]
#[serial]
async fn test_worker_round_summary_deduplicates_same_round() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("dedupscore_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let worker = worker.with_status(9106);
    let status_handle = worker.status().expect("status").clone();

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let round_summary = quorum_rs::events::RoundSummaryEvent {
        round: 1,
        convergence_score: 0.7,
        decisiveness: 0.7,
        net_support: vec![],
        cesaro_support: vec![],
        raw_distance: None,
        claim_convergence: None,
        total_claims: None,
        leader_claim_convergence: None,
        leader_total_claims: None,
        controversy_scores: vec![],
        proposal_scores: vec![quorum_rs::events::ProposalScoreEntry {
            agent_id: agent_name.clone(),
            aggregated_score: 6.5,
            category_breakdown: None,
            controversy_score: None,
            ..Default::default()
        }],
        accumulated_evidence: None,
        evidence_target: None,
        positive_budget: None,
        du_dt: None,
        signed_consensus: None,
        t_opt: None,
        thermo_probability: None,
        ..Default::default()
    };

    let summary_subject = format!("{}.{}.result.event.round_summary", sp, session_id);
    let payload = serde_json::to_vec(&round_summary).expect("serialize");

    // Publish the same round_summary twice
    client
        .publish(summary_subject.clone(), payload.clone().into())
        .await
        .expect("first publish");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    client
        .publish(summary_subject, payload.into())
        .await
        .expect("second publish");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify only one score entry was recorded (dedup)
    let snap = status_handle.read().await;
    let matching_scores: Vec<_> = snap
        .recent_scores
        .iter()
        .filter(|s| s.job_id == session_id && s.round == 1)
        .collect();
    assert_eq!(
        matching_scores.len(),
        1,
        "Should have exactly 1 score entry for round 1 (dedup), got {}",
        matching_scores.len()
    );

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

// ---------------------------------------------------------------------------
// Manifest with status tracking
// ---------------------------------------------------------------------------

/// Publish a manifest with status enabled. Verify the agent_accepted
/// event is recorded in the status event log.
#[tokio::test]
#[serial]
async fn test_worker_manifest_status_event_recorded() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("manstat_agent_{}", uid);
    let job_id = format!("job_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let worker = worker.with_status(9107);
    let status_handle = worker.status().expect("status").clone();

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to ACK to know when manifest is processed
    let ack_subject = format!("{}.jobs.ack.{}.{}", ap, job_id, agent_name);
    let mut ack_sub = client.subscribe(ack_subject).await.expect("subscribe");

    // Publish a manifest
    let manifest = quorum_rs::workers::JobManifest {
        job_id: job_id.clone(),
        task_description: "Status tracking test".to_string(),
        agents: vec![agent_name.clone()],
        rounds: 2,
        timestamp: 1704067200,
    };
    let manifest_subject = format!("{}.jobs.manifest.{}", ap, job_id);
    let payload = serde_json::to_vec(&manifest).expect("serialize");
    js.publish(manifest_subject, payload.into())
        .await
        .expect("publish manifest")
        .await
        .expect("ack");

    // Wait for ACK
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), ack_sub.next())
        .await
        .expect("ACK should arrive")
        .expect("should receive ACK");

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify status has agent_accepted event
    let snap = status_handle.read().await;
    let event_types: Vec<&str> = snap
        .event_log
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();
    assert!(
        event_types.contains(&"agent_accepted"),
        "Should have 'agent_accepted' event in status, got: {:?}",
        event_types
    );

    // Verify the event detail mentions the task description
    let accepted_event = snap
        .event_log
        .iter()
        .find(|e| e.event_type == "agent_accepted")
        .expect("should find agent_accepted event");
    assert!(
        accepted_event.detail.contains("Status tracking test"),
        "Event detail should contain task description, got: {}",
        accepted_event.detail
    );

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}
