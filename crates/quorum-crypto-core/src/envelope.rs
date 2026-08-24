//! Signed audit envelope wrapping any serializable payload.
//!
//! Supports multi-signature with chaining — each signer attests the payload
//! AND all previous signatures, preventing signature stripping attacks.

use crate::CryptoError;
use serde::{Deserialize, Serialize};

/// Status of signature verification on an audit envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureStatus {
    /// All signatures verified.
    Verified,
    /// Signatures present but not yet verified.
    Unverified,
    /// No signatures (unsigned payload — dev mode or legacy).
    Unsigned,
    /// At least one signature verification failed.
    Invalid,
}

/// Role of a signer in the signature chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema, utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SignerRole {
    /// Agent that produced the content.
    Author,
    /// Agent that scored/reviewed the content.
    Evaluator,
    /// Orchestrator attestation (job metadata, round results).
    Orchestrator,
    /// Human operator approval (HITL buffer release).
    Operator,
    /// Third-party co-signer / witness.
    Witness,
}

/// A single signature in the envelope's signature chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema, utoipa::ToSchema))]
pub struct EnvelopeSignature {
    /// Algorithm used (e.g., "ed25519", "secp256k1", "ml-dsa-65").
    pub algorithm: String,
    /// Signer's public key (hex-encoded).
    pub public_key: String,
    /// Signature bytes (base64-encoded).
    pub signature: String,
    /// Role of this signer.
    pub role: SignerRole,
    /// Signer identity (agent_id, operator principal, etc.).
    pub signer_id: String,
}

/// A signed wrapper around any serializable payload.
///
/// The envelope carries the payload plus a chain of signatures. Each signature
/// in the chain signs the canonical bytes of the payload AND all previous
/// signatures, creating a tamper-evident chain:
///
/// ```text
/// sig[0] = sign(canonical(payload))
/// sig[1] = sign(canonical(payload) + sig[0].signature_bytes)
/// sig[2] = sign(canonical(payload) + sig[0].signature_bytes + sig[1].signature_bytes)
/// ```
///
/// Removing or reordering any signature invalidates all subsequent signatures.
///
/// # Security note
///
/// Fields are `pub` for serialization compatibility. **Mutating any signed field
/// (`payload`, `subject`, `timestamp`, `agent_id`, `signatures`) invalidates
/// `status` without detection.** Always call `verify_chain()` after
/// deserialization or if the envelope may have been modified. For defense in
/// depth, prefer reading via accessor methods and treat `status` as advisory
/// until re-verified.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEnvelope<T: Serialize> {
    payload: T,
    subject: String,
    timestamp: u64,
    agent_id: String,
    #[serde(default)]
    signatures: Vec<EnvelopeSignature>,
    status: SignatureStatus,

    // Legacy single-signature fields (backward compat)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    algorithm: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    public_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    signature: String,
}

impl<T: Serialize> AuditEnvelope<T> {
    /// Create an unsigned envelope (dev mode / legacy).
    pub fn unsigned(payload: T, subject: &str, agent_id: &str) -> Self {
        Self {
            payload,
            subject: subject.to_string(),
            timestamp: now_secs(),
            agent_id: agent_id.to_string(),
            signatures: Vec::new(),
            status: SignatureStatus::Unsigned,
            algorithm: String::new(),
            public_key: String::new(),
            signature: String::new(),
        }
    }

    /// Create a signed envelope with a single author signature.
    pub async fn signed(
        payload: T,
        subject: &str,
        agent_id: &str,
        signer: &dyn crate::AuditSigner,
    ) -> Result<Self, CryptoError> {
        let timestamp = now_secs();
        let payload_json =
            serde_json::to_vec(&payload).map_err(|e| CryptoError::Serialization(e.to_string()))?;
        let canonical = crate::canonical_bytes(subject, &payload_json, timestamp, agent_id);

        let sig_bytes = signer.sign(&canonical).await?;
        let sig_b64 = b64_encode(&sig_bytes);

        let envelope_sig = EnvelopeSignature {
            algorithm: signer.algorithm().to_string(),
            public_key: signer.public_key_display(),
            signature: sig_b64.clone(),
            role: SignerRole::Author,
            signer_id: agent_id.to_string(),
        };

        Ok(Self {
            payload,
            subject: subject.to_string(),
            timestamp,
            agent_id: agent_id.to_string(),
            signatures: vec![envelope_sig],
            status: SignatureStatus::Unverified,
            // Legacy compat
            algorithm: signer.algorithm().to_string(),
            public_key: signer.public_key_display(),
            signature: sig_b64,
        })
    }

    /// Add a co-signature to the chain. The new signer signs the payload
    /// canonical bytes + all existing signatures, creating an ordered chain.
    pub async fn co_sign(
        &mut self,
        signer: &dyn crate::AuditSigner,
        role: SignerRole,
        signer_id: &str,
    ) -> Result<(), CryptoError> {
        let payload_json = serde_json::to_vec(&self.payload)
            .map_err(|e| CryptoError::Serialization(e.to_string()))?;

        let chained = canonical_bytes_chained(
            &self.subject,
            &payload_json,
            self.timestamp,
            &self.agent_id,
            &self.signatures,
        );

        let sig_bytes = signer.sign(&chained).await?;

        self.signatures.push(EnvelopeSignature {
            algorithm: signer.algorithm().to_string(),
            public_key: signer.public_key_display(),
            signature: b64_encode(&sig_bytes),
            role,
            signer_id: signer_id.to_string(),
        });

        // Reset status — needs re-verification
        self.status = SignatureStatus::Unverified;
        Ok(())
    }

    /// Verify the entire signature chain using a verifier registry.
    ///
    /// Each signature is verified against the payload canonical bytes + all
    /// prior signatures. If any signature fails, the chain is invalid.
    pub fn verify_chain(
        &mut self,
        registry: &crate::VerifierRegistry,
    ) -> Result<bool, CryptoError> {
        // Migrate legacy single-signature to chain if needed
        if self.signatures.is_empty() && !self.signature.is_empty() {
            self.signatures.push(EnvelopeSignature {
                algorithm: self.algorithm.clone(),
                public_key: self.public_key.clone(),
                signature: self.signature.clone(),
                role: SignerRole::Author,
                signer_id: self.agent_id.clone(),
            });
        }

        if self.signatures.is_empty() {
            self.status = SignatureStatus::Unsigned;
            return Ok(true);
        }

        let payload_json = serde_json::to_vec(&self.payload)
            .map_err(|e| CryptoError::Serialization(e.to_string()))?;

        for (i, sig) in self.signatures.iter().enumerate() {
            // Canonical bytes for signature i include all prior signatures
            let canonical = canonical_bytes_chained(
                &self.subject,
                &payload_json,
                self.timestamp,
                &self.agent_id,
                &self.signatures[..i], // prior signatures only
            );

            let sig_bytes = b64_decode(&sig.signature).map_err(|e| {
                CryptoError::VerificationFailed(format!(
                    "Invalid base64 in signature {i} ({}): {e}",
                    sig.signer_id
                ))
            })?;

            let pubkey_bytes =
                hex::decode(sig.public_key.trim_start_matches("0x")).map_err(|e| {
                    CryptoError::InvalidKey(format!("Invalid hex public key in signature {i}: {e}"))
                })?;

            if !registry.verify(&sig.algorithm, &canonical, &sig_bytes, &pubkey_bytes)? {
                self.status = SignatureStatus::Invalid;
                return Ok(false);
            }
        }

        self.status = SignatureStatus::Verified;
        Ok(true)
    }

    /// Backward-compatible verify (legacy single-signature envelopes).
    /// Delegates to `verify_chain`.
    pub fn verify(&mut self, registry: &crate::VerifierRegistry) -> Result<bool, CryptoError> {
        self.verify_chain(registry)
    }

    /// Number of signatures in the chain.
    pub fn signature_count(&self) -> usize {
        self.signatures.len()
    }

    /// Check if the chain contains a signature with the given role.
    pub fn has_role(&self, role: &SignerRole) -> bool {
        self.signatures.iter().any(|s| &s.role == role)
    }

    /// Read-only access to the payload.
    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Read-only access to the subject.
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Read-only access to the agent_id.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Read-only access to the timestamp.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Current verification status. **Advisory only** — always call
    /// `verify_chain()` after deserialization or if the envelope may have been
    /// modified externally.
    pub fn status(&self) -> &SignatureStatus {
        &self.status
    }

    /// Read-only access to the signature chain.
    pub fn signatures(&self) -> &[EnvelopeSignature] {
        &self.signatures
    }

    /// Explicitly invalidate the cached verification status.
    /// Call this after any mutation to signed fields.
    pub fn invalidate(&mut self) {
        if self.signatures.is_empty() {
            self.status = SignatureStatus::Unsigned;
        } else {
            self.status = SignatureStatus::Unverified;
        }
    }

    /// Mutable access to signatures — **invalidates status**.
    /// Use only for testing or deserialization fixup.
    #[doc(hidden)]
    pub fn signatures_mut(&mut self) -> &mut Vec<EnvelopeSignature> {
        self.status = SignatureStatus::Unverified;
        &mut self.signatures
    }

    /// Mutable access to payload — **invalidates status**.
    /// Use only for testing tamper detection.
    #[doc(hidden)]
    pub fn payload_mut(&mut self) -> &mut T {
        self.status = SignatureStatus::Unverified;
        &mut self.payload
    }
}

// ---------------------------------------------------------------------------
// Chained canonical bytes
// ---------------------------------------------------------------------------

/// Build canonical bytes for a chained signature.
///
/// The canonical form includes the base payload fields (same as `canonical_bytes`)
/// plus the **full metadata** of all prior signatures in the chain — algorithm,
/// public key, signature bytes, role, and signer_id. This prevents tampering
/// with any field of an intermediate signature without invalidating downstream
/// signatures.
fn canonical_bytes_chained(
    subject: &str,
    payload_json: &[u8],
    timestamp: u64,
    agent_id: &str,
    prior_signatures: &[EnvelopeSignature],
) -> Vec<u8> {
    // Start with the standard canonical bytes
    let mut buf = crate::canonical_bytes(subject, payload_json, timestamp, agent_id);

    // Append each prior signature's full metadata (not just the signature string)
    for sig in prior_signatures {
        buf.push(0x00); // separator
        buf.extend_from_slice(sig.algorithm.as_bytes());
        buf.push(0x00);
        buf.extend_from_slice(sig.public_key.as_bytes());
        buf.push(0x00);
        buf.extend_from_slice(sig.signature.as_bytes());
        buf.push(0x00);
        // Serialize role as its debug string for deterministic encoding
        buf.extend_from_slice(format!("{:?}", sig.role).as_bytes());
        buf.push(0x00);
        buf.extend_from_slice(sig.signer_id.as_bytes());
    }

    buf
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn b64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn b64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{Ed25519Signer, Secp256k1Signer};
    use crate::verifier::VerifierRegistry;

    #[tokio::test]
    async fn envelope_ed25519_sign_verify() {
        let signer = Ed25519Signer::generate();
        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"content": "hello"}),
            "proposal",
            "agent-1",
            &signer,
        )
        .await
        .unwrap();

        assert_eq!((*envelope.status()), SignatureStatus::Unverified);
        assert_eq!(envelope.signature_count(), 1);

        let registry = VerifierRegistry::with_defaults();
        assert!(envelope.verify(&registry).unwrap());
        assert_eq!((*envelope.status()), SignatureStatus::Verified);
    }

    #[tokio::test]
    async fn envelope_secp256k1_sign_verify() {
        let signer = Secp256k1Signer::generate();
        let mut envelope =
            AuditEnvelope::signed("evaluation result", "evaluation", "agent-2", &signer)
                .await
                .unwrap();

        let registry = VerifierRegistry::with_defaults();
        assert!(envelope.verify(&registry).unwrap());
    }

    #[tokio::test]
    async fn envelope_tampered_payload_fails() {
        let signer = Ed25519Signer::generate();
        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"score": 8.5}),
            "evaluation",
            "agent-1",
            &signer,
        )
        .await
        .unwrap();

        // Tamper with payload
        *envelope.payload_mut() = serde_json::json!({"score": 10.0});

        let registry = VerifierRegistry::with_defaults();
        assert!(!envelope.verify(&registry).unwrap());
        assert_eq!((*envelope.status()), SignatureStatus::Invalid);
    }

    #[test]
    fn unsigned_envelope_verifies_trivially() {
        let mut envelope = AuditEnvelope::<String>::unsigned(
            "unsigned content".to_string(),
            "proposal",
            "agent-dev",
        );

        let registry = VerifierRegistry::with_defaults();
        assert!(envelope.verify(&registry).unwrap());
        assert_eq!((*envelope.status()), SignatureStatus::Unsigned);
    }

    // --- Multi-signature chain tests ---

    #[tokio::test]
    async fn multi_sig_chain_verify() {
        let author = Ed25519Signer::generate();
        let evaluator = Secp256k1Signer::generate();
        let orchestrator = Ed25519Signer::generate();

        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"content": "proposal text"}),
            "proposal",
            "agent-author",
            &author,
        )
        .await
        .unwrap();

        // Evaluator co-signs
        envelope
            .co_sign(&evaluator, SignerRole::Evaluator, "agent-evaluator")
            .await
            .unwrap();

        // Orchestrator co-signs
        envelope
            .co_sign(&orchestrator, SignerRole::Orchestrator, "orchestrator-1")
            .await
            .unwrap();

        assert_eq!(envelope.signature_count(), 3);
        assert!(envelope.has_role(&SignerRole::Author));
        assert!(envelope.has_role(&SignerRole::Evaluator));
        assert!(envelope.has_role(&SignerRole::Orchestrator));

        let registry = VerifierRegistry::with_defaults();
        assert!(envelope.verify_chain(&registry).unwrap());
        assert_eq!((*envelope.status()), SignatureStatus::Verified);
    }

    #[tokio::test]
    async fn chain_detects_removed_signature() {
        let author = Ed25519Signer::generate();
        let evaluator = Ed25519Signer::generate();
        let orchestrator = Ed25519Signer::generate();

        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"content": "test"}),
            "proposal",
            "agent-1",
            &author,
        )
        .await
        .unwrap();

        envelope
            .co_sign(&evaluator, SignerRole::Evaluator, "agent-2")
            .await
            .unwrap();
        envelope
            .co_sign(&orchestrator, SignerRole::Orchestrator, "orch-1")
            .await
            .unwrap();

        // Remove the evaluator's signature (middle of chain)
        envelope.signatures_mut().remove(1);

        // Orchestrator's signature should now fail (it committed to the evaluator's sig)
        let registry = VerifierRegistry::with_defaults();
        assert!(!envelope.verify_chain(&registry).unwrap());
        assert_eq!((*envelope.status()), SignatureStatus::Invalid);
    }

    #[tokio::test]
    async fn chain_detects_reordered_signatures() {
        let author = Ed25519Signer::generate();
        let evaluator = Ed25519Signer::generate();

        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"content": "test"}),
            "proposal",
            "agent-1",
            &author,
        )
        .await
        .unwrap();

        envelope
            .co_sign(&evaluator, SignerRole::Evaluator, "agent-2")
            .await
            .unwrap();

        // Swap signature order
        envelope.signatures_mut().swap(0, 1);

        let registry = VerifierRegistry::with_defaults();
        // First signature (was evaluator, now verifying as first with no priors) should fail
        assert!(!envelope.verify_chain(&registry).unwrap());
    }

    #[tokio::test]
    async fn chain_detects_tampered_payload_with_multi_sig() {
        let author = Ed25519Signer::generate();
        let evaluator = Ed25519Signer::generate();

        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"score": 8.0}),
            "evaluation",
            "agent-1",
            &author,
        )
        .await
        .unwrap();

        envelope
            .co_sign(&evaluator, SignerRole::Evaluator, "agent-2")
            .await
            .unwrap();

        // Tamper with payload — both signatures should fail
        *envelope.payload_mut() = serde_json::json!({"score": 10.0});

        let registry = VerifierRegistry::with_defaults();
        assert!(!envelope.verify_chain(&registry).unwrap());
    }

    #[tokio::test]
    async fn legacy_single_sig_migrates_to_chain() {
        let signer = Ed25519Signer::generate();
        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"content": "legacy"}),
            "proposal",
            "agent-1",
            &signer,
        )
        .await
        .unwrap();

        // Simulate legacy: clear signatures array, keep single-sig fields
        let legacy_sig = envelope.signatures()[0].signature.clone();
        envelope.signatures_mut().clear();
        // Legacy fields are already populated by signed()

        assert_eq!(envelope.signature_count(), 0);
        assert!(!envelope.signature.is_empty());

        // verify_chain should auto-migrate
        let registry = VerifierRegistry::with_defaults();
        assert!(envelope.verify_chain(&registry).unwrap());
        assert_eq!(envelope.signature_count(), 1); // migrated
        assert_eq!(envelope.signatures()[0].signature, legacy_sig);
    }

    #[tokio::test]
    async fn has_role_queries() {
        let signer = Ed25519Signer::generate();
        let mut envelope = AuditEnvelope::signed(serde_json::json!({}), "test", "agent-1", &signer)
            .await
            .unwrap();

        assert!(envelope.has_role(&SignerRole::Author));
        assert!(!envelope.has_role(&SignerRole::Evaluator));
        assert!(!envelope.has_role(&SignerRole::Operator));

        let eval_signer = Ed25519Signer::generate();
        envelope
            .co_sign(&eval_signer, SignerRole::Operator, "human-1")
            .await
            .unwrap();

        assert!(envelope.has_role(&SignerRole::Operator));
    }

    #[tokio::test]
    async fn chain_detects_tampered_role() {
        let author = Ed25519Signer::generate();
        let evaluator = Ed25519Signer::generate();
        let orchestrator = Ed25519Signer::generate();

        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"content": "test"}),
            "proposal",
            "agent-1",
            &author,
        )
        .await
        .unwrap();

        envelope
            .co_sign(&evaluator, SignerRole::Evaluator, "agent-2")
            .await
            .unwrap();
        envelope
            .co_sign(&orchestrator, SignerRole::Orchestrator, "orch-1")
            .await
            .unwrap();

        // Tamper: change evaluator's role to Operator
        envelope.signatures_mut()[1].role = SignerRole::Operator;

        // Orchestrator's signature should fail (it committed to Evaluator role)
        let registry = VerifierRegistry::with_defaults();
        assert!(!envelope.verify_chain(&registry).unwrap());
    }

    #[tokio::test]
    async fn chain_detects_tampered_signer_id() {
        let author = Ed25519Signer::generate();
        let evaluator = Ed25519Signer::generate();
        let orchestrator = Ed25519Signer::generate();

        let mut envelope = AuditEnvelope::signed(
            serde_json::json!({"content": "test"}),
            "proposal",
            "agent-1",
            &author,
        )
        .await
        .unwrap();

        envelope
            .co_sign(&evaluator, SignerRole::Evaluator, "agent-2")
            .await
            .unwrap();
        envelope
            .co_sign(&orchestrator, SignerRole::Orchestrator, "orch-1")
            .await
            .unwrap();

        // Tamper: change evaluator's signer_id
        envelope.signatures_mut()[1].signer_id = "impersonator".to_string();

        // Orchestrator's signature should fail (it committed to "agent-2")
        let registry = VerifierRegistry::with_defaults();
        assert!(!envelope.verify_chain(&registry).unwrap());
    }
}
