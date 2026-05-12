use crate::common::*;
use async_nats::jetstream;
use serial_test::serial;

/// Verify that `NatsNsedWorker::new()` creates the expected KV buckets.
#[tokio::test]
#[serial]
async fn test_worker_new_creates_kv_buckets() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("lifecycle_agent_{}", uid);
    let safe_name = agent_name.replace(|c: char| !c.is_alphanumeric(), "_");

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    // Create the worker — this should create KV buckets
    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("NatsNsedWorker::new should succeed");

    // Verify the processed-idempotency KV bucket exists
    let proc_bucket_name = format!("nsed_proc_{}", safe_name);
    let proc_store = js
        .get_key_value(&proc_bucket_name)
        .await
        .expect("Processed KV bucket should exist");
    assert_eq!(proc_store.status().await.unwrap().bucket, proc_bucket_name);

    // Verify the scratchpad KV bucket exists
    let mem_bucket_name = format!("nsed_local_mem_{}", safe_name);
    let mem_store = js
        .get_key_value(&mem_bucket_name)
        .await
        .expect("Scratchpad KV bucket should exist");
    assert_eq!(mem_store.status().await.unwrap().bucket, mem_bucket_name);

    // Verify the agent_id is correct
    assert_eq!(worker.agent_id(), agent_name);

    // Clean up
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Verify builder method `with_status()` and accessors.
#[tokio::test]
#[serial]
async fn test_worker_builder_methods() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("builder_agent_{}", uid);

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker =
        quorum_rs::workers::NatsNsedWorker::new(agent, config.clone(), worker_config, None)
            .await
            .expect("NatsNsedWorker::new should succeed");

    // Before calling with_status, status() should be None
    assert!(
        worker.status().is_none(),
        "status should be None before with_status()"
    );

    // Chain with_status
    let worker = worker.with_status(8080);

    // After calling with_status, status() should be Some
    assert!(
        worker.status().is_some(),
        "status should be Some after with_status()"
    );

    // Verify agent_id
    assert_eq!(worker.agent_id(), agent_name);

    // Verify agent_config returns the correct config
    assert_eq!(worker.agent_config().name, config.name);
    assert_eq!(worker.agent_config().provider_id, config.provider_id);
    assert_eq!(worker.agent_config().model_name, config.model_name);

    // Clean up
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Verify that `NatsNsedWorker::new()` is idempotent — creating the same
/// agent twice (same KV bucket names) should succeed.
#[tokio::test]
#[serial]
async fn test_worker_new_idempotent_bucket_creation() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("idempotent_agent_{}", uid);

    let agent1 = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config1 = test_agent_config(&agent_name);
    let worker_config1 = test_worker_config(&uid, &agent_name);

    // First creation
    let _worker1 = quorum_rs::workers::NatsNsedWorker::new(agent1, config1, worker_config1, None)
        .await
        .expect("First NatsNsedWorker::new should succeed");

    // Second creation with the same agent name (buckets already exist)
    let agent2 = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config2 = test_agent_config(&agent_name);
    let worker_config2 = test_worker_config(&uid, &agent_name);

    let _worker2 = quorum_rs::workers::NatsNsedWorker::new(agent2, config2, worker_config2, None)
        .await
        .expect("Second NatsNsedWorker::new should succeed (idempotent)");

    // Clean up
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}

/// Verify `.with_hook()`, `.with_chat()`, and `.chat_agent()` builder methods.
#[tokio::test]
#[serial]
async fn test_worker_with_hook_and_chat() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());

    let uid = unique_id();
    let agent_name = format!("hook_chat_agent_{}", uid);

    let agent = MockAgent {
        agent_name: agent_name.clone(),
    };
    let config = test_agent_config(&agent_name);
    let worker_config = test_worker_config(&uid, &agent_name);

    let worker = quorum_rs::workers::NatsNsedWorker::new(agent, config, worker_config, None)
        .await
        .expect("NatsNsedWorker::new should succeed");

    // Before: no hook, no chat
    assert!(
        worker.chat_agent().is_none(),
        "chat_agent should be None initially"
    );

    // Create a mock hook (uses default implementation)
    #[derive(Debug)]
    struct TestHook;
    #[async_trait::async_trait]
    impl quorum_rs::workers::WorkerHook for TestHook {}

    // Create a mock chat agent
    struct TestChat;
    #[async_trait::async_trait]
    impl quorum_rs::agents::ChatCapable for TestChat {
        async fn chat(
            &self,
            _messages: Vec<async_openai::types::ChatCompletionRequestMessage>,
        ) -> anyhow::Result<String> {
            Ok("test chat response".to_string())
        }
    }

    let worker = worker
        .with_hook(std::sync::Arc::new(TestHook))
        .with_chat(std::sync::Arc::new(TestChat));

    // After: chat should be Some
    assert!(
        worker.chat_agent().is_some(),
        "chat_agent should be Some after with_chat()"
    );

    // Clean up
    cleanup_nats_resources(&js, &uid, &agent_name).await;
}
