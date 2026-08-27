use crate::common::*;
use async_nats::jetstream::{self, object_store};
use quorum_rs::nats_utils::{ensure_object_bucket, get_content_addressed, put_content_addressed};
use serial_test::serial;

/// A uniquely-named memory-backed object bucket, so tests never collide.
async fn create_test_bucket(js: &jetstream::Context, uid: &str) -> object_store::ObjectStore {
    ensure_object_bucket(
        js,
        object_store::Config {
            bucket: format!("test_objects_{uid}"),
            description: Some("Test objects".to_string()),
            max_age: std::time::Duration::from_secs(300),
            storage: jetstream::stream::StorageType::Memory,
            num_replicas: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create object bucket")
}

/// Bytes come back exactly as they went in, addressed by the returned digest.
#[tokio::test]
#[serial]
async fn content_round_trips_byte_identically() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client);
    let bucket = create_test_bucket(&js, "roundtrip").await;

    // Deliberately not valid UTF-8, so nothing along the path can be assuming text.
    let content: Vec<u8> = vec![0x00, 0xff, 0xfe, b'a', b'\n', 0x80];

    let digest = put_content_addressed(&bucket, &content)
        .await
        .expect("put succeeds");
    let fetched = get_content_addressed(&bucket, &digest)
        .await
        .expect("get succeeds");

    assert_eq!(fetched, content, "the bytes stored are the bytes returned");
}

/// The digest is over the content, so the same bytes always yield the same
/// address — that is what lets a proposal name an answer without carrying it.
#[tokio::test]
#[serial]
async fn the_same_content_yields_the_same_digest() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client);
    let bucket = create_test_bucket(&js, "stable").await;

    let content = b"the deliberated answer";
    let first = put_content_addressed(&bucket, content)
        .await
        .expect("first");
    let second = put_content_addressed(&bucket, content)
        .await
        .expect("storing the same content twice is idempotent");

    assert_eq!(first, second, "the address is a function of the content");

    let other = put_content_addressed(&bucket, b"a different answer")
        .await
        .expect("other");
    assert_ne!(first, other, "different content is a different address");
}

/// An address that was never stored is an error, not an empty answer — a caller
/// must not be able to read "no content" as "the answer is blank".
#[tokio::test]
#[serial]
async fn an_unknown_digest_is_an_error() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client);
    let bucket = create_test_bucket(&js, "unknown").await;

    let absent = "0".repeat(64);
    let err = get_content_addressed(&bucket, &absent).await;

    assert!(err.is_err(), "an unstored digest does not resolve");
}

/// The digest is verified on read. Storage the reader does not control could
/// return the wrong bytes; an address is only trustworthy if reading checks it.
#[tokio::test]
#[serial]
async fn content_that_does_not_match_its_digest_is_refused() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client);
    let bucket = create_test_bucket(&js, "tampered").await;

    let digest = put_content_addressed(&bucket, b"the real answer")
        .await
        .expect("put");

    // Overwrite the object in place, keeping the name — exactly what a store
    // that has been tampered with would present.
    let mut forged: &[u8] = b"a substituted answer";
    bucket
        .put(digest.as_str(), &mut forged)
        .await
        .expect("overwrite under the same name");

    let err = get_content_addressed(&bucket, &digest)
        .await
        .expect_err("content that does not hash to its address is refused");
    assert!(
        err.to_string().contains(&digest),
        "the error names the address that failed to verify: {err}"
    );
}

/// A malformed address is rejected before any network call, so a caller cannot
/// probe the store with arbitrary object names.
#[tokio::test]
#[serial]
async fn a_digest_that_is_not_a_digest_is_refused() {
    let client = match try_connect_nats().await {
        Some(c) => c,
        None => return,
    };
    let js = jetstream::new(client);
    let bucket = create_test_bucket(&js, "malformed").await;

    for bad in ["", "nope", "../escape", &"z".repeat(64), &"a".repeat(63)] {
        assert!(
            get_content_addressed(&bucket, bad).await.is_err(),
            "{bad:?} is not a valid address"
        );
    }
}
