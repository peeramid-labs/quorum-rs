use crate::common::*;
use async_nats::jetstream::{self, kv};
use quorum_rs::nats_utils::{NatsAuth, connect_nats, ensure_kv_bucket};
use serial_test::serial;

/// `connect_nats(url, None)` should succeed against a running NATS server.
#[tokio::test]
#[serial]
async fn test_connect_nats_unauthenticated() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    // The fact that try_connect_nats() returned Some is proof; but let's
    // verify via an explicit call as well.
    drop(client);

    let url = nats_url();
    let result = connect_nats(&url, None).await;
    assert!(
        result.is_ok(),
        "connect_nats with no auth should succeed: {:?}",
        result.err()
    );
}

/// `connect_nats` with token auth should succeed against default NATS
/// (which accepts any token when no authorization is configured).
#[tokio::test]
#[serial]
async fn test_connect_nats_with_token_auth() {
    if try_connect_nats().await.is_none() {
        return;
    }

    let url = nats_url();
    let auth = NatsAuth {
        token: Some("test-token-12345".to_string()),
        ..Default::default()
    };
    let result = connect_nats(&url, Some(&auth)).await;
    assert!(
        result.is_ok(),
        "connect_nats with token auth should succeed: {:?}",
        result.err()
    );
}

/// `connect_nats` with a default (unconfigured) NatsAuth should fall through
/// to unauthenticated and succeed.
#[tokio::test]
#[serial]
async fn test_connect_nats_unconfigured_auth_falls_through() {
    if try_connect_nats().await.is_none() {
        return;
    }

    let url = nats_url();
    let auth = NatsAuth::default();
    assert!(
        !auth.is_configured(),
        "default NatsAuth should not be configured"
    );
    let result = connect_nats(&url, Some(&auth)).await;
    assert!(
        result.is_ok(),
        "connect_nats with unconfigured auth should fall through: {:?}",
        result.err()
    );
}

/// `ensure_kv_bucket` should create a new bucket when it doesn't exist.
#[tokio::test]
#[serial]
async fn test_ensure_kv_bucket_create_new() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());
    let uid = unique_id();
    let bucket_name = format!("test_bucket_{}", uid);

    // Ensure the bucket doesn't exist yet
    let _ = js.delete_key_value(&bucket_name).await;

    // Create it
    let store = ensure_kv_bucket(
        &js,
        kv::Config {
            bucket: bucket_name.clone(),
            description: "Test bucket".to_string(),
            storage: jetstream::stream::StorageType::Memory,
            ..Default::default()
        },
    )
    .await
    .expect("ensure_kv_bucket should succeed");

    // Verify it exists
    let status = store.status().await.expect("status should work");
    assert_eq!(status.bucket, bucket_name);

    // Clean up
    cleanup_kv_bucket(&js, &bucket_name).await;
}

/// Calling `ensure_kv_bucket` twice with the same name should be idempotent.
#[tokio::test]
#[serial]
async fn test_ensure_kv_bucket_idempotent() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client.clone());
    let uid = unique_id();
    let bucket_name = format!("test_idem_bucket_{}", uid);

    let _ = js.delete_key_value(&bucket_name).await;

    let config = kv::Config {
        bucket: bucket_name.clone(),
        description: "Idempotency test bucket".to_string(),
        storage: jetstream::stream::StorageType::Memory,
        ..Default::default()
    };

    // First call: creates the bucket
    let store1 = ensure_kv_bucket(&js, config.clone())
        .await
        .expect("first ensure_kv_bucket should succeed");

    // Second call: should succeed (idempotent)
    let store2 = ensure_kv_bucket(&js, config)
        .await
        .expect("second ensure_kv_bucket should succeed (idempotent)");

    // Both should point to the same bucket
    let s1 = store1.status().await.expect("status 1");
    let s2 = store2.status().await.expect("status 2");
    assert_eq!(s1.bucket, s2.bucket);

    // Verify we can read/write
    store1
        .put("idem_test_key", "hello".into())
        .await
        .expect("put via store1");
    let val = store2
        .get("idem_test_key")
        .await
        .expect("get via store2")
        .expect("key should exist");
    assert_eq!(val.as_ref(), b"hello");

    // Clean up
    cleanup_kv_bucket(&js, &bucket_name).await;
}

/// `connect_nats` with user/pass auth. Default NATS server (no ACL) should accept any credentials.
/// This covers the `user_and_password()` branch (lines 96-97).
#[tokio::test]
#[serial]
async fn test_connect_nats_with_user_pass_auth() {
    if try_connect_nats().await.is_none() {
        return;
    }

    let url = nats_url();
    let auth = NatsAuth {
        username: Some("testuser".to_string()),
        password: Some("testpass".to_string()),
        ..Default::default()
    };
    let result = connect_nats(&url, Some(&auth)).await;
    assert!(
        result.is_ok(),
        "connect_nats with user/pass should succeed on default NATS: {:?}",
        result.err()
    );
}
