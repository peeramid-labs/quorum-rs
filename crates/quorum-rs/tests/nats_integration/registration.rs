use quorum_rs::nats_utils::{
    ChallengeResponse, RegistrationResponse, register_with_orchestrator,
    register_with_orchestrator_with_retry, sha256_hex,
};
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Start a wiremock MockServer that returns a valid challenge + registration.
/// Verify that `register_with_orchestrator()` returns correct credentials.
#[tokio::test]
#[serial]
async fn test_register_success() {
    let mock_server = MockServer::start().await;

    let nats_url = "nats://test-nats:4222";
    let nats_url_hash = sha256_hex(nats_url);
    let nonce = "test-nonce-abc123";
    let account_kp = nkeys::KeyPair::new_account();
    let orchestrator_pub_key = account_kp.public_key();

    // Mount GET /credentials/challenge
    let challenge = ChallengeResponse {
        orchestrator_pub_key: orchestrator_pub_key.clone(),
        nats_url_hash: nats_url_hash.clone(),
        nonce: nonce.to_string(),
        expires_in_secs: 300,
    };
    Mock::given(method("GET"))
        .and(path("/credentials/challenge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&challenge))
        .mount(&mock_server)
        .await;

    // Mount POST /credentials/register
    let registration = RegistrationResponse {
        user_jwt: "eyJ0eXAiOiJKV1QiLCJhbGciOiJlZDI1NTE5LW5rZXkifQ.test-jwt".to_string(),
        nats_url: nats_url.to_string(),
    };
    Mock::given(method("POST"))
        .and(path("/credentials/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&registration))
        .mount(&mock_server)
        .await;

    // Call register_with_orchestrator
    let result =
        register_with_orchestrator(&mock_server.uri(), "test-agent", "bearer-token-123").await;

    assert!(
        result.is_ok(),
        "register_with_orchestrator should succeed: {:?}",
        result.err()
    );

    let reg = result.unwrap();
    assert_eq!(reg.nats_url, nats_url);
    assert!(
        reg.creds.contains("BEGIN NATS USER JWT"),
        "Credentials should contain JWT section"
    );
    assert!(
        reg.creds.contains("BEGIN USER NKEY SEED"),
        "Credentials should contain NKEY seed section"
    );
    // Verify keypair is a valid User key (starts with U)
    assert!(
        reg.keypair.public_key().starts_with('U'),
        "Agent keypair should be a User NKey"
    );
}

/// Return a nats_url that does NOT match the hash in the challenge.
/// Verify the error contains "hash mismatch".
#[tokio::test]
#[serial]
async fn test_register_hash_mismatch() {
    let mock_server = MockServer::start().await;

    let correct_nats_url = "nats://correct:4222";
    let wrong_nats_url = "nats://wrong:4222";
    let nats_url_hash = sha256_hex(correct_nats_url);
    let nonce = "mismatch-nonce";
    let account_kp = nkeys::KeyPair::new_account();
    let orchestrator_pub_key = account_kp.public_key();

    // Challenge uses hash of correct_nats_url
    let challenge = ChallengeResponse {
        orchestrator_pub_key: orchestrator_pub_key.clone(),
        nats_url_hash: nats_url_hash.clone(),
        nonce: nonce.to_string(),
        expires_in_secs: 300,
    };
    Mock::given(method("GET"))
        .and(path("/credentials/challenge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&challenge))
        .mount(&mock_server)
        .await;

    // Registration returns wrong_nats_url (hash won't match)
    let registration = RegistrationResponse {
        user_jwt: "eyJ0eXAi.wrong-jwt".to_string(),
        nats_url: wrong_nats_url.to_string(),
    };
    Mock::given(method("POST"))
        .and(path("/credentials/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&registration))
        .mount(&mock_server)
        .await;

    let result = register_with_orchestrator(&mock_server.uri(), "test-agent", "token").await;

    assert!(result.is_err(), "Should fail due to hash mismatch");
    let err_msg = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("Expected error but got Ok"),
    };
    assert!(
        err_msg.contains("hash mismatch") || err_msg.contains("tampered"),
        "Error should mention hash mismatch or tampering, got: {}",
        err_msg
    );
}

/// Mock returns 403 on the challenge endpoint. Verify error.
#[tokio::test]
#[serial]
async fn test_register_challenge_rejected() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/credentials/challenge"))
        .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
        .mount(&mock_server)
        .await;

    let result = register_with_orchestrator(&mock_server.uri(), "test-agent", "bad-token").await;

    assert!(result.is_err(), "Should fail with 403 rejection");
    let err_msg = match result {
        Err(e) => format!("{:?}", e),
        Ok(_) => panic!("Expected error but got Ok"),
    };
    assert!(
        err_msg.contains("rejected") || err_msg.contains("403") || err_msg.contains("status"),
        "Error should indicate rejection, got: {}",
        err_msg
    );
}

/// A 503 on the challenge endpoint is transient. `register_with_orchestrator_with_retry`
/// must retry and succeed once the server recovers.
///
/// This is the **failure-reproducing test**: without the retry variant the agent dies on
/// the first 502, mirroring what users see with `make dev-sim-all` when the orchestrator
/// is still connecting to NATS (502 Bad Gateway from a reverse proxy).
/// Note: 503 is reserved for "credentials not enabled" and is non-retryable.
#[tokio::test]
#[serial]
async fn test_register_with_retry_succeeds_after_transient_failure() {
    let mock_server = MockServer::start().await;

    let nats_url = "nats://test-nats:4222";
    let nats_url_hash = sha256_hex(nats_url);
    let nonce = "retry-nonce-abc";
    let account_kp = nkeys::KeyPair::new_account();
    let orchestrator_pub_key = account_kp.public_key();

    // First 2 GET /credentials/challenge calls → 502 (proxy/orchestrator still starting)
    Mock::given(method("GET"))
        .and(path("/credentials/challenge"))
        .respond_with(ResponseTemplate::new(502).set_body_string("Bad Gateway"))
        .up_to_n_times(2)
        .mount(&mock_server)
        .await;

    // 3rd attempt onwards → success
    let challenge = ChallengeResponse {
        orchestrator_pub_key: orchestrator_pub_key.clone(),
        nats_url_hash: nats_url_hash.clone(),
        nonce: nonce.to_string(),
        expires_in_secs: 300,
    };
    Mock::given(method("GET"))
        .and(path("/credentials/challenge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&challenge))
        .mount(&mock_server)
        .await;

    let registration = RegistrationResponse {
        user_jwt: "eyJ0eXAiOiJKV1QiLCJhbGciOiJlZDI1NTE5LW5rZXkifQ.retry-jwt".to_string(),
        nats_url: nats_url.to_string(),
    };
    Mock::given(method("POST"))
        .and(path("/credentials/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&registration))
        .mount(&mock_server)
        .await;

    let result =
        register_with_orchestrator_with_retry(&mock_server.uri(), "test-agent", "token", 3).await;

    assert!(
        result.is_ok(),
        "Should succeed after retrying past transient 502s: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().nats_url, nats_url);
}

/// Auth failures (401) are permanent — wrong config won't self-heal.
/// `register_with_orchestrator_with_retry` must surface the error immediately
/// rather than burning through all retry slots.
#[tokio::test]
#[serial]
async fn test_register_with_retry_does_not_retry_auth_failure() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/credentials/challenge"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&mock_server)
        .await;

    let result =
        register_with_orchestrator_with_retry(&mock_server.uri(), "test-agent", "bad-token", 5)
            .await;

    assert!(result.is_err(), "Should fail immediately on 401");

    // Only 1 request should have been made — no retries for auth errors.
    let reqs = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(
        reqs.len(),
        1,
        "Should have made exactly 1 attempt, got {}",
        reqs.len()
    );
}
