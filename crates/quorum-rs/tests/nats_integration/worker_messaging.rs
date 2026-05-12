use crate::common::*;
use async_nats::jetstream;
use futures::StreamExt;
use serial_test::serial;

/// Helper: start a worker in a background task and return the join handle.
/// The caller is responsible for aborting the handle.
async fn start_worker_background(
    worker: &quorum_rs::workers::NatsNsedWorker,
) -> tokio::task::JoinHandle<()> {
    let w = worker.clone();
    tokio::spawn(async move {
        if let Err(e) = w.run().await {
            eprintln!("Worker run() exited with error: {:?}", e);
        }
    })
}

/// Start worker, publish a propose task via JetStream, verify a Proposal JSON
/// arrives on the result subject via core NATS.
#[tokio::test]
#[serial]
async fn test_worker_processes_propose_task() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("propose_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    // Create the JetStream stream FIRST (worker retries looking for it)
    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    // Create and start the worker
    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    let handle = start_worker_background(&worker).await;

    // Give the worker a moment to bind its consumers
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to the result subject via core NATS
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client
        .subscribe(result_subject.clone())
        .await
        .expect("subscribe to result subject");

    // Publish a propose task via JetStream
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize context");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack from server");

    // Wait for the proposal result with a timeout
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("result should arrive within 10 seconds")
        .expect("should receive a message");

    // Parse the response
    let proposal: quorum_rs::Proposal =
        serde_json::from_slice(&result.payload).expect("valid Proposal JSON");
    assert!(
        proposal.content.contains(&agent_name),
        "Proposal content should contain agent name"
    );
    assert_eq!(proposal.thought_process, "Mock thought process");

    // Clean up
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Start worker, publish a JobManifest with the agent in its list.
/// Verify an ACK arrives on the correct subject.
#[tokio::test]
#[serial]
async fn test_worker_handles_manifest_and_acks() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("manifest_agent_{}", uid);
    let job_id = format!("job_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    // Create the stream
    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    // Create and start the worker
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

    // Subscribe to the ACK subject
    let ack_subject = format!("{}.jobs.ack.{}.{}", ap, job_id, agent_name);
    let mut ack_sub = client
        .subscribe(ack_subject.clone())
        .await
        .expect("subscribe to ack subject");

    // Publish a manifest via JetStream
    let manifest = quorum_rs::workers::JobManifest {
        job_id: job_id.clone(),
        task_description: "Test deliberation".to_string(),
        agents: vec![agent_name.clone(), "other_agent".to_string()],
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

    // Wait for the ACK
    let ack_msg = tokio::time::timeout(std::time::Duration::from_secs(10), ack_sub.next())
        .await
        .expect("ACK should arrive within 10 seconds")
        .expect("should receive ACK message");

    let ack: serde_json::Value = serde_json::from_slice(&ack_msg.payload).expect("valid ACK JSON");
    assert_eq!(ack["agent_id"], agent_name);
    assert_eq!(ack["status"], "Accepted");

    // Clean up
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Start worker, subscribe to heartbeat subject. Verify a heartbeat arrives
/// within 15 seconds and contains expected fields.
#[tokio::test]
#[serial]
async fn test_worker_heartbeat_published() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("heartbeat_agent_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    // Create the stream
    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    // Create and start the worker
    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("worker creation");

    // Subscribe to heartbeat before starting worker
    let hb_subject = format!("{}.agent.heartbeat.{}", ap, agent_name);
    let mut hb_sub = client
        .subscribe(hb_subject)
        .await
        .expect("subscribe to heartbeat");

    let handle = start_worker_background(&worker).await;

    // Wait for a heartbeat (interval is 10 seconds, so wait up to 15)
    let hb_msg = tokio::time::timeout(std::time::Duration::from_secs(15), hb_sub.next())
        .await
        .expect("heartbeat should arrive within 15 seconds")
        .expect("should receive heartbeat message");

    let heartbeat: serde_json::Value =
        serde_json::from_slice(&hb_msg.payload).expect("valid heartbeat JSON");
    assert_eq!(heartbeat["agent_id"], agent_name);
    assert!(
        heartbeat.get("status").is_some(),
        "heartbeat should contain status"
    );
    assert!(
        heartbeat.get("uptime_secs").is_some(),
        "heartbeat should contain uptime_secs"
    );

    // Clean up
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Verify the worker's KV-based message tracking:
/// 1. Publish a task and verify it is processed (result emitted).
/// 2. Confirm the processed KV bucket now contains a dedup key.
/// 3. Publish a second distinct task (different JetStream sequence) and
///    verify it is also processed — distinct messages must NOT be falsely
///    deduplicated.
#[tokio::test]
#[serial]
async fn test_worker_deduplicates_messages() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("dedup_agent_{}", uid);
    let session_id = format!("session_{}", uid);
    let stream_name = format!("test_stream_{}", uid);
    let sp = subject_prefix(&uid);
    let ap = api_prefix(&uid);

    create_test_stream(&js, &stream_name, &sp, &ap)
        .await
        .expect("stream creation");

    // Create and start the worker
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

    // Subscribe to result subject
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client
        .subscribe(result_subject)
        .await
        .expect("subscribe to result");

    // Step 1: publish a valid task and verify it gets processed
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");

    js.publish(task_subject.clone(), payload.clone().into())
        .await
        .expect("first publish")
        .await
        .expect("first ack");

    // First message should be processed
    let first_result = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("first result should arrive")
        .expect("should receive first message");

    let proposal: quorum_rs::Proposal =
        serde_json::from_slice(&first_result.payload).expect("valid Proposal JSON");
    assert!(proposal.content.contains(&agent_name));

    // Step 2: verify that the processed KV bucket now has a dedup key.
    // The key format is "{stream}-{stream_sequence}-{subject}" where
    // the stream name and sequence come from the JetStream reply metadata.
    // Wait a moment for the mark_processed write to complete (async spawn).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let safe_name = agent_name.replace(|c: char| !c.is_alphanumeric(), "_");
    let proc_bucket_name = format!("nsed_proc_{}", safe_name);
    let proc_store = js
        .get_key_value(&proc_bucket_name)
        .await
        .expect("processed KV bucket should exist");

    // We don't know the exact key format (depends on JetStream internal
    // reply subject parsing), but we can verify a key was stored by
    // listing keys in the bucket. At least one key should exist after
    // the first message was processed.
    use futures::TryStreamExt;
    let keys: Vec<String> = proc_store
        .keys()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert!(
        !keys.is_empty(),
        "Processed KV should contain at least one dedup key after processing a message"
    );

    // Step 3: publish a second task (different sequence, so it should also
    // be processed). This verifies that distinct messages are NOT falsely
    // deduplicated.
    js.publish(task_subject.clone(), payload.into())
        .await
        .expect("second publish")
        .await
        .expect("second ack");

    let second_result = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("second result should arrive (different sequence = different dedup key)")
        .expect("should receive second message");

    let proposal2: quorum_rs::Proposal =
        serde_json::from_slice(&second_result.payload).expect("valid Proposal JSON");
    assert!(proposal2.content.contains(&agent_name));

    // After two messages processed, there should be two dedup keys
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let keys_after: Vec<String> = proc_store
        .keys()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        keys_after.len(),
        2,
        "Should have 2 dedup keys after processing 2 distinct messages, got {}",
        keys_after.len()
    );

    // Clean up
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Publish invalid JSON (poison pill) to the task subject, then a valid task.
/// Verify the worker does not crash and processes the valid task.
#[tokio::test]
#[serial]
async fn test_worker_handles_poison_pill() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("poison_agent_{}", uid);
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

    // Subscribe to the result subject
    let result_subject = format!("{}.{}.result.1.{}.propose", sp, session_id, agent_name);
    let mut result_sub = client
        .subscribe(result_subject)
        .await
        .expect("subscribe to result");

    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);

    // Publish invalid JSON (poison pill)
    js.publish(task_subject.clone(), b"NOT VALID JSON {{{"[..].into())
        .await
        .expect("publish poison pill")
        .await
        .expect("ack");

    // Small delay to let the worker process (and discard) the poison pill
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Now publish a valid task
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish valid task")
        .await
        .expect("ack");

    // The valid task should still be processed
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("valid task should be processed within 10 seconds")
        .expect("should receive result");

    let proposal: quorum_rs::Proposal =
        serde_json::from_slice(&result.payload).expect("valid Proposal JSON");
    assert!(
        proposal.content.contains(&agent_name),
        "Proposal should come from the correct agent"
    );

    // Clean up
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Use FailingMockAgent. Publish a propose task. Verify an error event
/// is emitted on the agent_error subject.
#[tokio::test]
#[serial]
async fn test_worker_error_event_on_failure() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("failing_agent_{}", uid);
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

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to the error event subject
    let error_subject = format!("{}.{}.result.event.agent_error", sp, session_id);
    let mut error_sub = client
        .subscribe(error_subject)
        .await
        .expect("subscribe to error events");

    // Publish a propose task
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
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
    assert_eq!(error_event["status"], "Failed");
    assert!(
        error_event["error"]
            .as_str()
            .unwrap_or("")
            .contains("intentional propose failure"),
        "Error message should contain our intentional failure text"
    );

    // Clean up
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Start worker, publish an evaluate task, verify evaluations arrive.
#[tokio::test]
#[serial]
async fn test_worker_processes_evaluate_task() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("eval_agent_{}", uid);
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

    // Subscribe to result subject for evaluate
    let result_subject = format!("{}.{}.result.1.{}.evaluate", sp, session_id, agent_name);
    let mut result_sub = client
        .subscribe(result_subject.clone())
        .await
        .expect("subscribe to result subject");

    // Publish an evaluate task
    let task_subject = format!("{}.{}.task.{}.evaluate", sp, session_id, agent_name);
    let context = crate::common::test_evaluate_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize context");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish evaluate task")
        .await
        .expect("ack from server");

    // Wait for evaluate result
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("result should arrive within 10 seconds")
        .expect("should receive a message");

    // Parse the evaluations (Vec<(String, Evaluation)>)
    let evaluations: Vec<(String, quorum_rs::Evaluation)> =
        serde_json::from_slice(&result.payload).expect("valid evaluations JSON");
    assert!(
        !evaluations.is_empty(),
        "Should have at least one evaluation"
    );
    // MockAgent returns one evaluation per candidate
    assert_eq!(
        evaluations.len(),
        2,
        "Should have 2 evaluations for 2 candidates"
    );
    assert_eq!(evaluations[0].0, "candidate_1");
    assert_eq!(evaluations[1].0, "candidate_2");

    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Start worker with `.with_status()`, publish a propose task, verify
/// the status snapshot is updated with task events.
/// Covers all `if let Some(ref status) = self.status { ... }` blocks.
#[tokio::test]
#[serial]
async fn test_worker_with_status_records_events() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("status_agent_{}", uid);
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

    // Enable status tracking
    let worker = worker.with_status(9090);

    // Verify status is enabled
    let status_handle = worker.status().expect("status should be Some").clone();

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to result to know when task is done
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

    // Wait for result
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), result_sub.next())
        .await
        .expect("result should arrive")
        .expect("should receive message");

    // Give a moment for status writes to complete
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify the status snapshot has events
    let snap = status_handle.read().await;
    assert!(snap.nats_connected, "Should be connected");
    assert!(
        !snap.event_log.is_empty(),
        "Should have recorded at least one event"
    );

    // Check for expected event types
    let event_types: Vec<&str> = snap
        .event_log
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();
    assert!(
        event_types.contains(&"connected"),
        "Should have 'connected' event, got: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"task_complete"),
        "Should have 'task_complete' event, got: {:?}",
        event_types
    );

    // Verify task log
    assert!(
        !snap.recent_tasks.is_empty(),
        "Should have recorded at least one task"
    );
    let last_task = &snap.recent_tasks[snap.recent_tasks.len() - 1];
    assert_eq!(last_task.action, "propose");
    assert_eq!(last_task.status, "ok");
    // duration_ms may be 0 if the mock agent completes instantly — that's fine
    assert!(
        last_task.duration_ms < 60_000,
        "Duration should be reasonable"
    );

    // Current job should be cleared after completion
    assert!(
        snap.current_job.is_none(),
        "Current job should be None after task completes"
    );

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Start worker with `.with_status()` and a FailingMockAgent. Verify
/// that error status events are recorded properly.
#[tokio::test]
#[serial]
async fn test_worker_with_status_records_error_events() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("status_err_agent_{}", uid);
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

    // Enable status tracking
    let worker = worker.with_status(9091);
    let status_handle = worker.status().expect("status should be Some").clone();

    let handle = start_worker_background(&worker).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Subscribe to error event to know when task fails
    let error_subject = format!("{}.{}.result.event.agent_error", sp, session_id);
    let mut error_sub = client.subscribe(error_subject).await.expect("subscribe");

    // Publish a propose task
    let task_subject = format!("{}.{}.task.{}.propose", sp, session_id, agent_name);
    let context = test_propose_context(&session_id);
    let payload = serde_json::to_vec(&context).expect("serialize");
    js.publish(task_subject, payload.into())
        .await
        .expect("publish task")
        .await
        .expect("ack");

    // Wait for error event
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), error_sub.next())
        .await
        .expect("error event should arrive")
        .expect("should receive error");

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify the status snapshot has error events
    let snap = status_handle.read().await;
    let event_types: Vec<&str> = snap
        .event_log
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();
    assert!(
        event_types.contains(&"agent_error"),
        "Should have 'agent_error' event, got: {:?}",
        event_types
    );

    // Verify task log shows error
    assert!(!snap.recent_tasks.is_empty(), "Should have a task record");
    let last_task = &snap.recent_tasks[snap.recent_tasks.len() - 1];
    assert_eq!(last_task.status, "error");

    drop(snap);
    handle.abort();
    let _ = handle.await;
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}
