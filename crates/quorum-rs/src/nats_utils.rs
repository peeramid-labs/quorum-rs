//! NATS utility functions for subject validation, KV bucket management, and
//! authenticated connections.
//!
//! These are pure utility functions with no dependency on orchestrator internals.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// NATS authentication credentials.
///
/// Supports four modes (checked in this order):
/// 1. **Token auth** — set `token` for NATS token authentication
/// 2. **User/password** — set both `username` and `password`
/// 3. **Inline credentials** — set `inline_creds` with `.creds` file content (in-memory NKey auth)
/// 4. **Credentials file** — set `creds_file` to a `.creds` file path (NKey auth)
///
/// If no fields are set, the connection falls back to unauthenticated.
///
/// # Environment variable mapping (orchestrator)
///
/// ```text
/// APP_NATS__AUTH__TOKEN=my-secret-token
/// APP_NATS__AUTH__USERNAME=agent-user
/// APP_NATS__AUTH__PASSWORD=agent-pass
/// APP_NATS__AUTH__CREDS_FILE=/path/to/agent.creds
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct NatsAuth {
    /// NATS token for token-based authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Username for user/password authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Password for user/password authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// In-memory `.creds` content for NKey-based authentication.
    ///
    /// This is used by the JWT credential issuance flow — the orchestrator
    /// issues a scoped User JWT and the agent combines it with its own NKey
    /// seed into `.creds` format without touching the filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_creds: Option<String>,
    /// Path to a `.creds` file for NKey-based authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creds_file: Option<String>,
}

impl NatsAuth {
    /// Returns true if any authentication fields are set.
    pub fn is_configured(&self) -> bool {
        self.token.is_some()
            || (self.username.is_some() && self.password.is_some())
            || self.inline_creds.is_some()
            || self.creds_file.is_some()
    }
}

/// An orchestrator endpoint that an agent can register with to obtain
/// NATS connection credentials via the JWT challenge-response protocol.
///
/// Used in agent YAML configuration:
/// ```yaml
/// orchestrators:
///   - id: "primary"
///     url: "http://localhost:8080"
///     bearer_token: "${NSED_BEARER_TOKEN}"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct OrchestratorEntry {
    /// Human-readable identifier. Derived from URL hostname if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// HTTP base URL of the orchestrator (e.g. `"http://localhost:8080"`).
    #[serde(default)]
    pub url: String,
    /// Bearer token for API authentication.
    /// Supports `${ENV_VAR}` syntax for environment variable expansion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
}

/// Connect to NATS with optional authentication.
///
/// If `auth` is `None` or has no fields set, connects without credentials.
/// Otherwise, builds [`async_nats::ConnectOptions`] from the provided credentials.
pub async fn connect_nats(url: &str, auth: Option<&NatsAuth>) -> Result<async_nats::Client> {
    match auth.filter(|a| a.is_configured()) {
        Some(auth) => {
            // Priority: token > user/password > inline_creds > credentials file
            let opts = if let Some(token) = &auth.token {
                async_nats::ConnectOptions::new().token(token.clone())
            } else if let (Some(user), Some(pass)) = (&auth.username, &auth.password) {
                async_nats::ConnectOptions::new().user_and_password(user.clone(), pass.clone())
            } else if let Some(inline) = &auth.inline_creds {
                async_nats::ConnectOptions::with_credentials(inline)
                    .context("Failed to parse inline NATS credentials")?
            } else if let Some(creds) = &auth.creds_file {
                async_nats::ConnectOptions::new()
                    .credentials_file(creds)
                    .await
                    .context(format!("Failed to load NATS credentials file: {}", creds))?
            } else {
                async_nats::ConnectOptions::new()
            };
            opts.connect(url)
                .await
                .map_err(|e| anyhow::anyhow!(e))
                .context(format!("Failed to connect to NATS at {} with auth", url))
        }
        None => async_nats::connect(url)
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .context(format!("Failed to connect to NATS at {}", url)),
    }
}

/// Characters forbidden in NATS subjects, KV bucket names, and consumer names.
///
/// - `\0` is forbidden by the protocol
/// - ` ` (space) breaks parsing
/// - `.` is the NATS subject hierarchy separator
/// - `*` and `>` are reserved wildcards
/// - `/` is not technically forbidden but breaks many client tools and KV paths
const NATS_FORBIDDEN_CHARS: &[char] = &['\0', ' ', '.', '*', '>', '/'];

/// Validates that a user-supplied name is safe for use in NATS subjects, stream
/// names, and KV bucket names. Rejects characters that could cause injection or
/// parsing issues.
pub fn validate_nats_name(name: &str, field_label: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{} must not be empty", field_label));
    }

    let invalid_chars: Vec<char> = name
        .chars()
        .filter(|c| NATS_FORBIDDEN_CHARS.contains(c) || c.is_whitespace() || c.is_control())
        .collect();

    if invalid_chars.is_empty() {
        Ok(())
    } else {
        // Deduplicate for cleaner error messages
        let mut unique_chars: Vec<char> = invalid_chars;
        unique_chars.sort();
        unique_chars.dedup();
        Err(format!(
            "{} contains invalid characters: {:?}",
            field_label, unique_chars
        ))
    }
}

/// Sanitizes a string for use as a NATS subject component.
/// Replaces characters that aren't alphanumeric, `-`, or `_` with an underscore.
/// Preserves hyphens and underscores to avoid key collisions.
pub fn sanitize_subject_component(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Helper to ensure a KV bucket exists, creating it if necessary.
///
/// This is idempotent — if the bucket already exists, it returns the existing store.
/// Used by both agent workers and orchestrator workers.
pub async fn ensure_kv_bucket(js: &jetstream::Context, config: kv::Config) -> Result<kv::Store> {
    match js.create_key_value(config.clone()).await {
        Ok(store) => Ok(store),
        Err(e) => {
            if e.to_string().contains("already in use") {
                js.get_key_value(&config.bucket)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
                    .context(format!(
                        "Failed to get existing KV bucket {}",
                        config.bucket
                    ))
            } else {
                Err(anyhow::anyhow!(e).context("Failed to create KV bucket"))
            }
        }
    }
}

/// Format a User JWT and NKey seed into the `.creds` file format that
/// `async-nats` expects.
///
/// The `.creds` format uses asymmetric dashes: 5 on BEGIN, 6 on END.
/// This produces an in-memory string — no temp file needed.
pub fn format_nats_creds(user_jwt: &str, user_seed: &str) -> String {
    format!(
        "-----BEGIN NATS USER JWT-----\n\
         {user_jwt}\n\
         ------END NATS USER JWT------\n\
         \n\
         ************************* IMPORTANT *************************\n\
         NKEY Seed printed below can be used to sign and prove identity.\n\
         NKEYs are sensitive and should be treated as secrets.\n\
         \n\
         -----BEGIN USER NKEY SEED-----\n\
         {user_seed}\n\
         ------END USER NKEY SEED------\n\
         \n\
         *************************************************************\n"
    )
}

/// Typed error for a NATS URL hash mismatch in the credential challenge-response.
///
/// Using a concrete type (instead of matching on error-message strings) lets
/// [`is_retryable_registration_error`] classify this as a permanent failure via
/// `downcast_ref` without depending on fragile error-message text.
#[derive(Debug)]
pub struct HashMismatchError {
    /// The SHA-256 hash the orchestrator committed to in the challenge.
    pub expected: String,
    /// The SHA-256 hash computed from the URL the orchestrator returned.
    pub computed: String,
}

impl std::fmt::Display for HashMismatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NATS URL hash mismatch — expected {} but computed {}; \
             the orchestrator response may have been tampered with",
            self.expected, self.computed
        )
    }
}

impl std::error::Error for HashMismatchError {}

/// The orchestrator does not have credential issuance enabled (returned 503).
/// Agents should fall back to direct NATS connection without JWT credentials.
#[derive(Debug)]
pub struct CredentialsNotEnabledError;

impl std::fmt::Display for CredentialsNotEnabledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Credential issuance not enabled on orchestrator (503); \
             fall back to direct NATS connection"
        )
    }
}

impl std::error::Error for CredentialsNotEnabledError {}

/// Orchestrator challenge-response data returned by `GET /credentials/challenge`.
///
/// The `nats_url_hash` is a SHA-256 commitment — the actual NATS URL is only
/// revealed in the [`RegistrationResponse`] after the agent proves key possession.
/// This prevents an attacker who obtains a bearer token (but not an NKey private
/// key) from learning the NATS infrastructure topology.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChallengeResponse {
    /// The orchestrator's Account public key (AA... prefix).
    pub orchestrator_pub_key: String,
    /// `hex(SHA-256(nats_url))` — commitment to the NATS server address.
    /// The agent signs this blindly; the actual URL is revealed after registration.
    pub nats_url_hash: String,
    /// Random nonce — must be signed and returned in the registration request.
    pub nonce: String,
    /// How many seconds the nonce is valid.
    pub expires_in_secs: u64,
}

/// Registration response returned by `POST /credentials/register`.
///
/// The `nats_url` is only revealed here — after the agent has proven it holds the
/// NKey private key by signing the challenge (which includes the URL's SHA-256 hash).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RegistrationResponse {
    /// Scoped User JWT signed by the orchestrator's Account key.
    pub user_jwt: String,
    /// NATS server URL — revealed only after signature verification.
    /// The agent MUST verify `hex(SHA-256(nats_url)) == nats_url_hash` from
    /// the challenge to detect tampering.
    pub nats_url: String,
}

/// Compute `hex(SHA-256(input))` — used for the NATS URL hash commitment.
pub fn sha256_hex(input: &str) -> String {
    let hash = Sha256::digest(input.as_bytes());
    hash.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
        s
    })
}

/// Result of registering with the orchestrator.
#[derive(Debug)]
pub struct RegistrationResult {
    /// `.creds` content ready for [`NatsAuth::inline_creds`].
    pub creds: String,
    /// NATS server URL obtained from the orchestrator (hash-verified).
    pub nats_url: String,
    /// The agent's User NKey — private key stays with the agent.
    pub keypair: nkeys::KeyPair,
}

/// Register with the orchestrator via the challenge-response protocol and
/// obtain NATS credentials + the NATS server URL.
///
/// The agent only needs `orchestrator_url` and a `bearer_token` — the NATS
/// address is provided by the orchestrator and integrity-checked via a
/// SHA-256 hash commitment in the challenge.
///
/// # Protocol
///
/// 1. Generate a fresh User NKey
/// 2. `GET {orchestrator_url}/credentials/challenge` — receive `nats_url_hash` commitment
/// 3. Sign `"{nonce}:{orchestrator_pub_key}:{nats_url_hash}"` with the User NKey
/// 4. `POST {orchestrator_url}/credentials/register` — receive `user_jwt` + `nats_url`
/// 5. Verify `SHA-256(nats_url) == nats_url_hash` to detect tampering
/// 6. Combine JWT + own seed → `.creds`
pub async fn register_with_orchestrator(
    orchestrator_url: &str,
    agent_id: &str,
    bearer_token: &str,
) -> Result<RegistrationResult> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;
    let base = orchestrator_url.trim_end_matches('/');

    // 1. Generate a fresh User NKey
    let user_kp = nkeys::KeyPair::new_user();
    let user_pub = user_kp.public_key();

    // 2. GET /credentials/challenge
    let challenge_resp = http
        .get(format!("{base}/credentials/challenge"))
        .bearer_auth(bearer_token)
        .send()
        .await
        .context("Failed to request credentials challenge")?;

    // 503 = credential issuance not enabled — caller should fall back to direct NATS
    if challenge_resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return Err(CredentialsNotEnabledError.into());
    }

    let challenge: ChallengeResponse = challenge_resp
        .error_for_status()
        .context("Credentials challenge request rejected")?
        .json()
        .await
        .context("Failed to parse challenge response")?;

    // 3. Sign "{nonce}:{orchestrator_pub_key}:{nats_url_hash}"
    let challenge_msg = format!(
        "{}:{}:{}",
        challenge.nonce, challenge.orchestrator_pub_key, challenge.nats_url_hash
    );
    let signature = user_kp
        .sign(challenge_msg.as_bytes())
        .map_err(|e| anyhow::anyhow!("Failed to sign challenge: {e}"))?;

    // 4. POST /credentials/register
    let reg_body = serde_json::json!({
        "agent_id": agent_id,
        "user_pub_key": user_pub,
        "nonce": challenge.nonce,
        "signature": signature,
    });

    let reg_resp: RegistrationResponse = http
        .post(format!("{base}/credentials/register"))
        .bearer_auth(bearer_token)
        .json(&reg_body)
        .send()
        .await
        .context("Failed to send registration request")?
        .error_for_status()
        .context("Registration request rejected")?
        .json()
        .await
        .context("Failed to parse registration response")?;

    // 5. Verify hash commitment: SHA-256(nats_url) == nats_url_hash
    let computed_hash = sha256_hex(&reg_resp.nats_url);
    if computed_hash != challenge.nats_url_hash {
        return Err(anyhow::Error::new(HashMismatchError {
            expected: challenge.nats_url_hash.clone(),
            computed: computed_hash,
        }));
    }

    // 6. Combine JWT + own seed → .creds
    let seed_str = user_kp
        .seed()
        .map_err(|e| anyhow::anyhow!("Failed to extract user seed: {e}"))?;
    let creds = format_nats_creds(&reg_resp.user_jwt, &seed_str);

    Ok(RegistrationResult {
        creds,
        nats_url: reg_resp.nats_url,
        keypair: user_kp,
    })
}

/// Like [`register_with_orchestrator`] but retries on transient failures.
///
/// Retries up to `max_attempts` times with exponential backoff (1 s, 2 s, 4 s, …).
/// **Does not** retry on permanent failures:
/// - 4xx HTTP responses (auth/config problems won't self-heal)
/// - NATS URL hash mismatch (security violation)
pub async fn register_with_orchestrator_with_retry(
    orchestrator_url: &str,
    agent_id: &str,
    bearer_token: &str,
    max_attempts: u32,
) -> Result<RegistrationResult> {
    let base_delay_ms = 1_000u64;
    let mut last_err = anyhow::anyhow!("No attempts made");

    for attempt in 1..=max_attempts {
        match register_with_orchestrator(orchestrator_url, agent_id, bearer_token).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if !is_retryable_registration_error(&e) || attempt == max_attempts {
                    return Err(e);
                }
                let wait = Duration::from_millis(base_delay_ms * 2u64.pow(attempt.min(6) - 1));
                tracing::warn!(
                    orchestrator_url,
                    attempt,
                    max_attempts,
                    wait_ms = wait.as_millis(),
                    error = %e,
                    "Orchestrator not yet reachable, retrying registration..."
                );
                tokio::time::sleep(wait).await;
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// Returns `true` if `e` is a transient error that should be retried.
///
/// Permanent failures (4xx HTTP status, [`HashMismatchError`]) return `false`.
fn is_retryable_registration_error(e: &anyhow::Error) -> bool {
    // Walk the cause chain once, looking for either:
    //   • a reqwest error with a 4xx status  → permanent (config/auth problem)
    //   • a HashMismatchError                → permanent (security violation)
    for cause in e.chain() {
        if let Some(reqwest_err) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(status) = reqwest_err.status() {
                if status.is_client_error() {
                    return false;
                }
            }
        }
        if cause.downcast_ref::<HashMismatchError>().is_some() {
            return false;
        }
        if cause.downcast_ref::<CredentialsNotEnabledError>().is_some() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_subject_component() {
        assert_eq!(sanitize_subject_component("hello"), "hello");
        assert_eq!(sanitize_subject_component("hello.world"), "hello_world");
        assert_eq!(
            sanitize_subject_component("my.session>with*wildcards"),
            "my_session_with_wildcards"
        );
        assert_eq!(sanitize_subject_component("agent-1_v2"), "agent-1_v2");
    }

    #[test]
    fn test_validate_nats_name_valid() {
        assert!(validate_nats_name("my-job-123", "job_id").is_ok());
        assert!(validate_nats_name("agent_v2", "agent_id").is_ok());
    }

    #[test]
    fn test_validate_nats_name_empty() {
        assert!(validate_nats_name("", "job_id").is_err());
    }

    #[test]
    fn test_validate_nats_name_forbidden_chars() {
        assert!(validate_nats_name("my.job", "job_id").is_err());
        assert!(validate_nats_name("my job", "job_id").is_err());
        assert!(validate_nats_name("my*job", "job_id").is_err());
        assert!(validate_nats_name("my>job", "job_id").is_err());
    }

    #[test]
    fn test_nats_auth_default_not_configured() {
        let auth = NatsAuth::default();
        assert!(!auth.is_configured());
    }

    #[test]
    fn test_nats_auth_token_configured() {
        let auth = NatsAuth {
            token: Some("secret".into()),
            ..Default::default()
        };
        assert!(auth.is_configured());
    }

    #[test]
    fn test_nats_auth_user_pass_configured() {
        let auth = NatsAuth {
            username: Some("user".into()),
            password: Some("pass".into()),
            ..Default::default()
        };
        assert!(auth.is_configured());
    }

    #[test]
    fn test_nats_auth_partial_user_not_configured() {
        // Only username without password should NOT be configured
        let auth = NatsAuth {
            username: Some("user".into()),
            ..Default::default()
        };
        assert!(!auth.is_configured());
    }

    #[test]
    fn test_nats_auth_is_configured_priority() {
        // When ALL auth methods are set, is_configured() should return true.
        // The actual connect_nats() prioritizes token > user/pass > inline_creds > creds_file,
        // but is_configured() just checks if *any* are present.
        let auth = NatsAuth {
            token: Some("my-token".into()),
            username: Some("user".into()),
            password: Some("pass".into()),
            inline_creds: Some("creds-content".into()),
            creds_file: Some("/path/to/creds".into()),
        };
        assert!(
            auth.is_configured(),
            "All auth methods set should be configured"
        );

        // Token alone should be enough
        let token_only = NatsAuth {
            token: Some("t".into()),
            ..Default::default()
        };
        assert!(token_only.is_configured());

        // Creds file alone should be enough
        let creds_only = NatsAuth {
            creds_file: Some("/path".into()),
            ..Default::default()
        };
        assert!(creds_only.is_configured());

        // Only password without username should NOT be configured
        let pass_only = NatsAuth {
            password: Some("pass".into()),
            ..Default::default()
        };
        assert!(
            !pass_only.is_configured(),
            "Password alone should not count as configured"
        );
    }

    #[test]
    fn test_inline_creds_is_configured() {
        let auth = NatsAuth {
            inline_creds: Some("some-creds-content".into()),
            ..Default::default()
        };
        assert!(
            auth.is_configured(),
            "inline_creds alone should count as configured"
        );
    }

    #[test]
    fn test_nats_auth_serde_roundtrip() {
        let auth = NatsAuth {
            token: Some("my-token".into()),
            username: None,
            password: None,
            inline_creds: None,
            creds_file: Some("/path/to/creds".into()),
        };
        let json = serde_json::to_string(&auth).unwrap();
        let parsed: NatsAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.token.as_deref(), Some("my-token"));
        assert_eq!(parsed.creds_file.as_deref(), Some("/path/to/creds"));
        assert!(parsed.username.is_none());
        assert!(parsed.inline_creds.is_none());
    }

    #[test]
    fn test_nats_auth_serde_with_inline_creds() {
        let auth = NatsAuth {
            inline_creds: Some("jwt-and-seed-content".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&auth).unwrap();
        // inline_creds should be present in serialized JSON
        assert!(json.contains("inline_creds"));
        let parsed: NatsAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.inline_creds.as_deref(), Some("jwt-and-seed-content"));
        // Other fields should be None
        assert!(parsed.token.is_none());
        assert!(parsed.creds_file.is_none());
    }

    #[test]
    fn test_sha256_hex() {
        // Known SHA-256 hash of "nats://localhost:4222"
        let hash = sha256_hex("nats://localhost:4222");
        assert_eq!(hash.len(), 64, "SHA-256 hex should be 64 chars");
        // Verify it's stable (deterministic)
        assert_eq!(hash, sha256_hex("nats://localhost:4222"));
        // Different input → different hash
        assert_ne!(hash, sha256_hex("nats://other:4222"));
    }

    #[test]
    fn test_format_nats_creds() {
        let jwt = "eyJ0eXAiOiJKV1QiLCJhbGciOiJlZDI1NTE5LW5rZXkifQ.test";
        let seed = "SUACIWOXXKLRCL7DTPOV3P7CQHNDCAP5JBHFPAGKE32GVVHZCPIBXAVBU";
        let creds = format_nats_creds(jwt, seed);

        // Verify the asymmetric dashes format (5 on BEGIN, 6 on END)
        assert!(creds.contains("-----BEGIN NATS USER JWT-----"));
        assert!(creds.contains("------END NATS USER JWT------"));
        assert!(creds.contains("-----BEGIN USER NKEY SEED-----"));
        assert!(creds.contains("------END USER NKEY SEED------"));

        // Verify JWT and seed are present
        assert!(creds.contains(jwt));
        assert!(creds.contains(seed));
    }

    #[test]
    fn test_validate_nats_name_unicode() {
        // Unicode chars are not in NATS_FORBIDDEN_CHARS and not whitespace/control
        assert!(validate_nats_name("agent_日本語", "agent_id").is_ok());
    }

    #[test]
    fn test_validate_nats_name_control_chars() {
        assert!(validate_nats_name("agent\tid", "field").is_err());
        assert!(validate_nats_name("agent\nid", "field").is_err());
        assert!(validate_nats_name("agent\rid", "field").is_err());
    }

    #[test]
    fn test_validate_nats_name_null_byte() {
        assert!(validate_nats_name("agent\0id", "field").is_err());
    }

    #[test]
    fn test_validate_nats_name_slash() {
        assert!(validate_nats_name("path/to/thing", "field").is_err());
    }

    #[test]
    fn test_validate_nats_name_long() {
        // A 256-char alphanumeric name should pass (NATS doesn't limit length in this function)
        let long_name: String = "a".repeat(256);
        assert!(validate_nats_name(&long_name, "field").is_ok());
    }

    #[test]
    fn test_validate_nats_name_single_char() {
        assert!(validate_nats_name("a", "field").is_ok());
        assert!(validate_nats_name(".", "field").is_err());
    }

    #[test]
    fn test_sanitize_dot_and_space() {
        assert_eq!(
            sanitize_subject_component("hello world.v2"),
            "hello_world_v2"
        );
    }

    #[test]
    fn test_sanitize_preserves_hyphen_underscore() {
        assert_eq!(sanitize_subject_component("my-agent_v2"), "my-agent_v2");
    }

    #[test]
    fn test_sanitize_empty() {
        assert_eq!(sanitize_subject_component(""), "");
    }

    #[test]
    fn test_sanitize_all_special() {
        assert_eq!(sanitize_subject_component(".*>"), "___");
    }

    #[test]
    fn test_orchestrator_entry_serde_roundtrip() {
        let entry = OrchestratorEntry {
            id: Some("primary".into()),
            url: "http://localhost:8080".into(),
            bearer_token: Some("secret-token".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: OrchestratorEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("primary"));
        assert_eq!(parsed.url, "http://localhost:8080");
        assert_eq!(parsed.bearer_token.as_deref(), Some("secret-token"));
    }

    #[test]
    fn test_orchestrator_entry_defaults() {
        let parsed: OrchestratorEntry = serde_json::from_str("{}").unwrap();
        assert!(parsed.id.is_none());
        assert_eq!(parsed.url, "");
        assert!(parsed.bearer_token.is_none());
    }

    #[test]
    fn test_challenge_response_serde() {
        let cr = ChallengeResponse {
            orchestrator_pub_key: "AAXYZ".into(),
            nats_url_hash: "abc123def456".into(),
            nonce: "random-nonce".into(),
            expires_in_secs: 300,
        };
        let json = serde_json::to_string(&cr).unwrap();
        let parsed: ChallengeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.orchestrator_pub_key, "AAXYZ");
        assert_eq!(parsed.nats_url_hash, "abc123def456");
        assert_eq!(parsed.nonce, "random-nonce");
        assert_eq!(parsed.expires_in_secs, 300);
    }

    #[test]
    fn test_registration_response_serde() {
        let rr = RegistrationResponse {
            user_jwt: "eyJ0eXAi.test.jwt".into(),
            nats_url: "nats://example.com:4222".into(),
        };
        let json = serde_json::to_string(&rr).unwrap();
        let parsed: RegistrationResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user_jwt, "eyJ0eXAi.test.jwt");
        assert_eq!(parsed.nats_url, "nats://example.com:4222");
    }

    // -------------------------------------------------------------------
    // is_retryable_registration_error tests
    // -------------------------------------------------------------------

    #[test]
    fn test_hash_mismatch_not_retryable() {
        let hm_err = HashMismatchError {
            expected: "aabbcc".into(),
            computed: "ddeeff".into(),
        };
        let anyhow_err = anyhow::Error::new(hm_err);
        assert!(
            !is_retryable_registration_error(&anyhow_err),
            "HashMismatchError should be classified as non-retryable"
        );
    }

    #[test]
    fn test_hash_mismatch_display() {
        let hm_err = HashMismatchError {
            expected: "abc123".into(),
            computed: "def456".into(),
        };
        let display = format!("{}", hm_err);
        assert!(
            display.contains("abc123"),
            "Display should include expected hash"
        );
        assert!(
            display.contains("def456"),
            "Display should include computed hash"
        );
        assert!(
            display.contains("tampered"),
            "Display should warn about tampering"
        );
    }

    #[test]
    fn test_generic_error_is_retryable() {
        // A generic anyhow error (e.g. network timeout) should be retryable
        let err = anyhow::anyhow!("connection timed out");
        assert!(
            is_retryable_registration_error(&err),
            "Generic error should be retryable"
        );
    }

    #[test]
    fn test_hash_mismatch_wrapped_in_context_not_retryable() {
        let hm_err = HashMismatchError {
            expected: "aaa".into(),
            computed: "bbb".into(),
        };
        // Wrap in anyhow context chain — downcast_ref should still find it
        let anyhow_err = anyhow::Error::new(hm_err).context("registration failed");
        assert!(
            !is_retryable_registration_error(&anyhow_err),
            "HashMismatchError wrapped in context should still be non-retryable"
        );
    }

    #[test]
    fn test_credentials_not_enabled_not_retryable() {
        let err = anyhow::Error::new(CredentialsNotEnabledError);
        assert!(
            !is_retryable_registration_error(&err),
            "CredentialsNotEnabledError should be classified as non-retryable"
        );
    }

    #[test]
    fn test_credentials_not_enabled_wrapped_in_context_not_retryable() {
        let err = anyhow::Error::new(CredentialsNotEnabledError).context("registration failed");
        assert!(
            !is_retryable_registration_error(&err),
            "CredentialsNotEnabledError wrapped in context should still be non-retryable"
        );
    }

    /// Verifies that `register_with_orchestrator_with_retry` returns an error
    /// after exhausting all retry attempts when the orchestrator returns 500.
    #[tokio::test]
    async fn test_register_exhausts_retries() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        // Always return 500 for the challenge endpoint
        Mock::given(method("GET"))
            .and(path("/credentials/challenge"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .expect(2) // exactly 2 attempts
            .mount(&mock_server)
            .await;

        let result = register_with_orchestrator_with_retry(
            &mock_server.uri(),
            "test-agent",
            "test-token",
            2, // max 2 attempts
        )
        .await;

        match result {
            Err(e) => {
                let err_msg = format!("{:#}", e);
                assert!(
                    err_msg.contains("challenge")
                        || err_msg.contains("500")
                        || err_msg.contains("rejected")
                        || err_msg.contains("status"),
                    "Error should mention the challenge failure: {}",
                    err_msg
                );
            }
            Ok(_) => panic!("Should return error after exhausting retries"),
        }
    }

    /// Verifies that a 4xx response is NOT retried (permanent failure).
    #[tokio::test]
    async fn test_register_no_retry_on_4xx() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        // Return 401 Unauthorized — should NOT be retried
        Mock::given(method("GET"))
            .and(path("/credentials/challenge"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .expect(1) // only 1 attempt — no retries for 4xx
            .mount(&mock_server)
            .await;

        let result = register_with_orchestrator_with_retry(
            &mock_server.uri(),
            "test-agent",
            "bad-token",
            5, // max 5 attempts, but should stop at 1
        )
        .await;

        assert!(result.is_err(), "Should return error immediately on 4xx");
    }

    /// 503 on /credentials/challenge → CredentialsNotEnabledError (no retries).
    #[tokio::test]
    async fn test_register_503_returns_credentials_not_enabled() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        // Return 503 — credential issuance not enabled
        Mock::given(method("GET"))
            .and(path("/credentials/challenge"))
            .respond_with(
                ResponseTemplate::new(503).set_body_string("Credential issuance is not enabled"),
            )
            .expect(1) // only 1 attempt — no retries
            .mount(&mock_server)
            .await;

        let result = register_with_orchestrator_with_retry(
            &mock_server.uri(),
            "test-agent",
            "test-token",
            5, // max 5 attempts, but should stop at 1
        )
        .await;

        let err = result.expect_err("Should return error on 503");
        assert!(
            err.downcast_ref::<CredentialsNotEnabledError>().is_some(),
            "Error should be CredentialsNotEnabledError, got: {err:#}"
        );
    }
}
