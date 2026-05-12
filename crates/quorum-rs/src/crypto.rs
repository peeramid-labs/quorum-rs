//! Transparent cryptographic signing for NSED agents.
//!
//! - [`AgentKeyPair`] — ergonomic wrapper around Ed25519 for agent identity
//! - [`SigningHook`] — [`WorkerHook`] that wraps outbound payloads in [`AuditEnvelope`]
//!
//! Agent developers don't need to interact with crypto directly — the worker
//! builder installs [`SigningHook`] by default with an auto-generated keypair.

use crate::workers::WorkerHook;
use anyhow::Result;
use nsed_crypto_core::{AuditEnvelope, AuditSigner, signer::ed25519::Ed25519Signer};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// AgentKeyPair
// ---------------------------------------------------------------------------

/// Ergonomic wrapper around an Ed25519 signing key for agent identity.
///
/// Hides the [`AuditSigner`] trait — agent developers work with this struct
/// directly. The inner signer is always Ed25519 (matching NATS NKey scheme).
#[derive(Debug, Clone)]
pub struct AgentKeyPair {
    signer: Arc<Ed25519Signer>,
}

impl AgentKeyPair {
    /// Generate a new random keypair.
    pub fn generate() -> Self {
        Self {
            signer: Arc::new(Ed25519Signer::generate()),
        }
    }

    /// Create from a 32-byte seed (deterministic — same seed → same key).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            signer: Arc::new(Ed25519Signer::from_seed(seed)),
        }
    }

    /// Create from the `NSED_AGENT_SEED` environment variable (hex-encoded 32 bytes).
    ///
    /// Returns `None` if the env var is missing or invalid.
    pub fn from_env(var_name: &str) -> Option<Self> {
        let hex_seed = std::env::var(var_name).ok()?;
        let bytes = hex::decode(hex_seed.trim()).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes);
        Some(Self::from_seed(&seed))
    }

    /// Get the public key as hex string for display/logging.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signer.public_key_bytes())
    }

    /// Access the inner signer (for advanced use / interop with nsed-crypto).
    pub fn signer(&self) -> &Arc<Ed25519Signer> {
        &self.signer
    }

    /// Get an `Arc<dyn AuditSigner>` for use with envelope signing.
    pub fn as_audit_signer(&self) -> Arc<dyn AuditSigner> {
        self.signer.clone()
    }
}

// ---------------------------------------------------------------------------
// SigningHook
// ---------------------------------------------------------------------------

/// [`WorkerHook`] that wraps outbound NATS payloads in signed [`AuditEnvelope`]s.
///
/// Installed automatically by the worker builder. Agent developers don't
/// interact with this directly.
///
/// The hook:
/// 1. Deserializes the raw payload as `serde_json::Value`
/// 2. Extracts `agent_id` from the NATS subject
/// 3. Wraps in `AuditEnvelope::signed()` with the agent's keypair
/// 4. Replaces the payload bytes with the serialized envelope
///
/// If signing fails (shouldn't happen with Ed25519), the original payload
/// is passed through unchanged with a warning log.
#[derive(Debug)]
pub struct SigningHook {
    keypair: AgentKeyPair,
    agent_id: String,
}

impl SigningHook {
    /// Create a new signing hook for the given agent.
    pub fn new(keypair: AgentKeyPair, agent_id: String) -> Self {
        Self { keypair, agent_id }
    }

    /// Extract the audit-relevant subject type from a NATS subject.
    ///
    /// Only deliberation content subjects are signed:
    /// - `nsed.{session}.result.{round}.{agent_id}.propose` → `Some("proposal")`
    /// - `nsed.{session}.result.{round}.{agent_id}.evaluate` → `Some("evaluation")`
    /// - Everything else (heartbeats, manifest ACKs, control messages) → `None`
    ///
    /// Returns `None` for subjects that should pass through unsigned.
    fn audit_subject_type(subject: &str) -> Option<&'static str> {
        let last = subject.rsplit('.').next().unwrap_or("");
        match last {
            "propose" => Some("proposal"),
            "evaluate" => Some("evaluation"),
            _ => None,
        }
    }
}

#[async_trait::async_trait]
impl WorkerHook for SigningHook {
    async fn before_publish(&self, subject: &str, payload: &mut Vec<u8>) -> Result<()> {
        // Only sign audit-relevant subjects (proposals, evaluations).
        // Control-plane messages (manifest ACKs, heartbeats) pass through unsigned.
        let Some(subject_type) = Self::audit_subject_type(subject) else {
            return Ok(());
        };

        // Parse payload as JSON Value for envelope wrapping
        let value: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    error = %e,
                    "SigningHook: payload is not JSON, passing through unsigned"
                );
                return Ok(());
            }
        };

        // Sign into an AuditEnvelope
        let signer = self.keypair.as_audit_signer();
        match AuditEnvelope::signed(value, subject_type, &self.agent_id, &*signer).await {
            Ok(envelope) => {
                // Replace payload with the serialized envelope
                match serde_json::to_vec(&envelope) {
                    Ok(signed_bytes) => {
                        *payload = signed_bytes;
                    }
                    Err(e) => {
                        tracing::warn!(
                            agent_id = %self.agent_id,
                            error = %e,
                            "SigningHook: failed to serialize envelope, passing unsigned"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    error = %e,
                    "SigningHook: signing failed, passing unsigned"
                );
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- AgentKeyPair ----

    #[test]
    fn keypair_generate_produces_unique_keys() {
        let a = AgentKeyPair::generate();
        let b = AgentKeyPair::generate();
        assert_ne!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn keypair_from_seed_is_deterministic() {
        let seed = [42u8; 32];
        let a = AgentKeyPair::from_seed(&seed);
        let b = AgentKeyPair::from_seed(&seed);
        assert_eq!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn keypair_from_seed_different_seeds_differ() {
        let a = AgentKeyPair::from_seed(&[1u8; 32]);
        let b = AgentKeyPair::from_seed(&[2u8; 32]);
        assert_ne!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn keypair_public_key_hex_is_64_chars() {
        let kp = AgentKeyPair::generate();
        assert_eq!(kp.public_key_hex().len(), 64); // 32 bytes = 64 hex chars
    }

    #[test]
    fn keypair_from_env_missing_returns_none() {
        assert!(AgentKeyPair::from_env("NSED_TEST_NONEXISTENT_SEED_VAR").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn keypair_from_env_invalid_hex_returns_none() {
        // SAFETY: test-only env var manipulation, tests run single-threaded
        unsafe { std::env::set_var("NSED_TEST_BAD_SEED", "not-hex") };
        assert!(AgentKeyPair::from_env("NSED_TEST_BAD_SEED").is_none());
        unsafe { std::env::remove_var("NSED_TEST_BAD_SEED") };
    }

    #[test]
    #[serial_test::serial]
    fn keypair_from_env_wrong_length_returns_none() {
        unsafe { std::env::set_var("NSED_TEST_SHORT_SEED", "abcd1234") };
        assert!(AgentKeyPair::from_env("NSED_TEST_SHORT_SEED").is_none());
        unsafe { std::env::remove_var("NSED_TEST_SHORT_SEED") };
    }

    #[test]
    #[serial_test::serial]
    fn keypair_from_env_valid_works() {
        let seed = [99u8; 32];
        let hex_seed = hex::encode(seed);
        unsafe { std::env::set_var("NSED_TEST_VALID_SEED", &hex_seed) };
        let kp = AgentKeyPair::from_env("NSED_TEST_VALID_SEED").unwrap();
        let expected = AgentKeyPair::from_seed(&seed);
        assert_eq!(kp.public_key_hex(), expected.public_key_hex());
        unsafe { std::env::remove_var("NSED_TEST_VALID_SEED") };
    }

    #[test]
    fn keypair_as_audit_signer_returns_ed25519() {
        let kp = AgentKeyPair::generate();
        let signer = kp.as_audit_signer();
        assert_eq!(signer.algorithm(), "ed25519");
    }

    // ---- SigningHook ----

    #[test]
    fn audit_subject_type_extracts_proposal() {
        assert_eq!(
            SigningHook::audit_subject_type("nsed.abc.result.1.agent-1.propose"),
            Some("proposal")
        );
    }

    #[test]
    fn audit_subject_type_extracts_evaluation() {
        assert_eq!(
            SigningHook::audit_subject_type("nsed.abc.result.1.agent-1.evaluate"),
            Some("evaluation")
        );
    }

    #[test]
    fn audit_subject_type_returns_none_for_non_audit() {
        // Control-plane subjects should not be signed
        assert_eq!(
            SigningHook::audit_subject_type("nsed.abc.result.something"),
            None
        );
        assert_eq!(
            SigningHook::audit_subject_type("sphera.jobs.ack.job1.agent1"),
            None
        );
    }

    #[test]
    fn audit_subject_type_handles_empty() {
        assert_eq!(SigningHook::audit_subject_type(""), None);
    }

    #[tokio::test]
    async fn signing_hook_skips_non_audit_subjects() {
        let kp = AgentKeyPair::generate();
        let hook = SigningHook::new(kp, "agent".to_string());

        let original = serde_json::json!({"manifest": true});
        let mut payload = serde_json::to_vec(&original).unwrap();
        let original_bytes = payload.clone();

        // Manifest ACK subject — should pass through unsigned
        hook.before_publish("sphera.jobs.ack.job1.agent1", &mut payload)
            .await
            .unwrap();

        assert_eq!(
            payload, original_bytes,
            "Non-audit subject should not be wrapped"
        );
    }

    #[tokio::test]
    async fn signing_hook_wraps_json_in_envelope() {
        let kp = AgentKeyPair::from_seed(&[1u8; 32]);
        let hook = SigningHook::new(kp.clone(), "test-agent".to_string());

        let original = serde_json::json!({"content": "hello", "thought_process": "thinking"});
        let mut payload = serde_json::to_vec(&original).unwrap();

        hook.before_publish("nsed.session.result.1.test-agent.propose", &mut payload)
            .await
            .unwrap();

        // Payload should now be an AuditEnvelope
        let envelope: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&payload).unwrap();

        assert_eq!(envelope.agent_id(), "test-agent");
        assert_eq!(envelope.subject(), "proposal");
        assert_eq!(envelope.payload()["content"], "hello");
        assert_eq!(envelope.signature_count(), 1);
        assert!(envelope.has_role(&nsed_crypto_core::envelope::SignerRole::Author));
    }

    #[tokio::test]
    async fn signing_hook_envelope_verifies() {
        let kp = AgentKeyPair::from_seed(&[2u8; 32]);
        let hook = SigningHook::new(kp.clone(), "verify-agent".to_string());

        let mut payload = serde_json::to_vec(&serde_json::json!({"score": 8.5})).unwrap();

        hook.before_publish("nsed.session.result.1.verify-agent.evaluate", &mut payload)
            .await
            .unwrap();

        let mut envelope: AuditEnvelope<serde_json::Value> =
            serde_json::from_slice(&payload).unwrap();

        let registry = nsed_crypto_core::VerifierRegistry::with_defaults();
        assert!(envelope.verify_chain(&registry).unwrap());
    }

    #[tokio::test]
    async fn signing_hook_passes_through_non_json() {
        let kp = AgentKeyPair::generate();
        let hook = SigningHook::new(kp, "agent".to_string());

        let mut payload = b"not json".to_vec();
        let original = payload.clone();

        hook.before_publish("nsed.session.result.1.agent.propose", &mut payload)
            .await
            .unwrap();

        // Non-JSON payload should pass through unchanged
        assert_eq!(payload, original);
    }

    #[tokio::test]
    async fn signing_hook_deterministic_with_same_seed() {
        let seed = [3u8; 32];
        let hook1 = SigningHook::new(AgentKeyPair::from_seed(&seed), "agent".to_string());
        let hook2 = SigningHook::new(AgentKeyPair::from_seed(&seed), "agent".to_string());

        let json = serde_json::json!({"test": true});
        let mut p1 = serde_json::to_vec(&json).unwrap();
        let mut p2 = serde_json::to_vec(&json).unwrap();

        hook1
            .before_publish("nsed.s.result.1.agent.propose", &mut p1)
            .await
            .unwrap();
        hook2
            .before_publish("nsed.s.result.1.agent.propose", &mut p2)
            .await
            .unwrap();

        // Both should produce valid envelopes with the same public key
        let env1: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&p1).unwrap();
        let env2: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&p2).unwrap();

        assert_eq!(
            env1.signatures()[0].public_key,
            env2.signatures()[0].public_key
        );
    }

    #[tokio::test]
    async fn signing_hook_different_subjects_produce_different_envelopes() {
        let kp = AgentKeyPair::from_seed(&[4u8; 32]);
        let hook = SigningHook::new(kp, "agent".to_string());

        let json = serde_json::json!({"data": 1});
        let mut p1 = serde_json::to_vec(&json).unwrap();
        let mut p2 = serde_json::to_vec(&json).unwrap();

        hook.before_publish("nsed.s.result.1.agent.propose", &mut p1)
            .await
            .unwrap();
        hook.before_publish("nsed.s.result.1.agent.evaluate", &mut p2)
            .await
            .unwrap();

        let env1: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&p1).unwrap();
        let env2: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&p2).unwrap();

        assert_eq!(env1.subject(), "proposal");
        assert_eq!(env2.subject(), "evaluation");
        // Different subjects → different signatures
        assert_ne!(
            env1.signatures()[0].signature,
            env2.signatures()[0].signature
        );
    }
}
