use crate::common::*;
use async_nats::jetstream::{self, kv};
use quorum_rs::agents::PersistenceStore;
use quorum_rs::workers::NatsScratchpadStore;
use serial_test::serial;

/// Helper: create a NatsScratchpadStore backed by a uniquely-named KV bucket.
async fn create_test_scratchpad(
    js: &jetstream::Context,
    uid: &str,
) -> (NatsScratchpadStore, String) {
    let bucket_name = format!("test_scratchpad_{}", uid);

    let store = quorum_rs::nats_utils::ensure_kv_bucket(
        js,
        kv::Config {
            bucket: bucket_name.clone(),
            description: "Test scratchpad".to_string(),
            max_age: std::time::Duration::from_secs(300),
            storage: jetstream::stream::StorageType::Memory,
            num_replicas: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create scratchpad bucket");

    let scope = format!("scope_{}", uid);
    let scratchpad = NatsScratchpadStore::new(store, js.clone(), scope);
    (scratchpad, bucket_name)
}

/// `set("key", "value")` then `get("key")` should return "value".
#[tokio::test]
#[serial]
async fn test_scratchpad_get_set_roundtrip() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());
    let uid = unique_id();

    let (scratchpad, bucket_name) = create_test_scratchpad(&js, &uid).await;

    // Set a value
    scratchpad
        .set("mykey", "myvalue")
        .await
        .expect("set should succeed");

    // Get it back
    let result = scratchpad
        .get("mykey")
        .await
        .expect("get should succeed")
        .expect("key should exist");

    assert_eq!(result, "myvalue");

    // Clean up
    cleanup_kv_bucket(&js, &bucket_name).await;
}

/// `append("new_key", "hello")` on a fresh store should create the key.
#[tokio::test]
#[serial]
async fn test_scratchpad_append_creates_key() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());
    let uid = unique_id();

    let (scratchpad, bucket_name) = create_test_scratchpad(&js, &uid).await;

    // Append to a key that doesn't exist yet
    scratchpad
        .append("new_key", "hello")
        .await
        .expect("append should succeed on new key");

    // Get it back
    let result = scratchpad
        .get("new_key")
        .await
        .expect("get should succeed")
        .expect("key should exist after append");

    assert_eq!(result, "hello");

    // Clean up
    cleanup_kv_bucket(&js, &bucket_name).await;
}

/// Sequential appends should concatenate values.
#[tokio::test]
#[serial]
async fn test_scratchpad_append_concatenates() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());
    let uid = unique_id();

    let (scratchpad, bucket_name) = create_test_scratchpad(&js, &uid).await;

    // Append "a" then "b"
    scratchpad
        .append("concat_key", "a")
        .await
        .expect("first append");
    scratchpad
        .append("concat_key", "b")
        .await
        .expect("second append");

    // Should be "ab"
    let result = scratchpad
        .get("concat_key")
        .await
        .expect("get should succeed")
        .expect("key should exist");

    assert_eq!(result, "ab");

    // Clean up
    cleanup_kv_bucket(&js, &bucket_name).await;
}

/// `get_round_history(1)` with no history bucket should return `Ok(None)`.
#[tokio::test]
#[serial]
async fn test_scratchpad_get_round_history_not_found() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());
    let uid = unique_id();

    let (scratchpad, bucket_name) = create_test_scratchpad(&js, &uid).await;

    // No history bucket exists, so get_round_history should return None
    let result = scratchpad
        .get_round_history(1)
        .await
        .expect("get_round_history should not error");

    assert!(
        result.is_none(),
        "get_round_history should return None when no history bucket exists"
    );

    // Clean up
    cleanup_kv_bucket(&js, &bucket_name).await;
}

/// `set` should overwrite a previous value.
#[tokio::test]
#[serial]
async fn test_scratchpad_set_overwrites() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());
    let uid = unique_id();

    let (scratchpad, bucket_name) = create_test_scratchpad(&js, &uid).await;

    // Set initial value
    scratchpad
        .set("overwrite_key", "first")
        .await
        .expect("set 1");

    // Overwrite
    scratchpad
        .set("overwrite_key", "second")
        .await
        .expect("set 2");

    let result = scratchpad
        .get("overwrite_key")
        .await
        .expect("get")
        .expect("key should exist");

    assert_eq!(result, "second");

    // Clean up
    cleanup_kv_bucket(&js, &bucket_name).await;
}

/// `get` on a non-existent key returns `Ok(None)`.
#[tokio::test]
#[serial]
async fn test_scratchpad_get_nonexistent_key() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());
    let uid = unique_id();

    let (scratchpad, bucket_name) = create_test_scratchpad(&js, &uid).await;

    let result = scratchpad
        .get("does_not_exist")
        .await
        .expect("get should not error");

    assert!(result.is_none(), "Non-existent key should return None");

    // Clean up
    cleanup_kv_bucket(&js, &bucket_name).await;
}
