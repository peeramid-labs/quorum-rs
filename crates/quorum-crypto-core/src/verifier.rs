//! Signature verification traits and registry.
//!
//! [`AuditVerifier`] is stateless — it verifies signatures given the algorithm,
//! message, signature bytes, and public key bytes. Used for cold-start replay
//! and cross-agent verification.
//!
//! [`VerifierRegistry`] maps algorithm strings to verifier implementations.

use crate::CryptoError;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

/// Stateless signature verifier.
///
/// Unlike [`AuditSigner`] (which holds a private key), verifiers are stateless
/// and can verify any message given the algorithm, signature, and public key.
pub trait AuditVerifier: Send + Sync + Debug {
    /// Algorithm this verifier handles (e.g., "ed25519", "secp256k1").
    fn algorithm(&self) -> &str;

    /// Verify a signature.
    fn verify(
        &self,
        message: &[u8],
        signature: &[u8],
        public_key: &[u8],
    ) -> Result<bool, CryptoError>;
}

/// Registry mapping algorithm names to verifier implementations.
///
/// Used to verify signatures from unknown agents during cold-start replay
/// or cross-orchestrator verification.
#[derive(Debug, Default)]
pub struct VerifierRegistry {
    verifiers: HashMap<String, Arc<dyn AuditVerifier>>,
}

impl VerifierRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry pre-populated with Ed25519 and Secp256k1 verifiers.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(Ed25519Verifier));
        registry.register(Arc::new(Secp256k1Verifier));
        registry
    }

    /// Register a verifier for its algorithm.
    pub fn register(&mut self, verifier: Arc<dyn AuditVerifier>) {
        self.verifiers
            .insert(verifier.algorithm().to_string(), verifier);
    }

    /// Verify a signature using the appropriate verifier.
    pub fn verify(
        &self,
        algorithm: &str,
        message: &[u8],
        signature: &[u8],
        public_key: &[u8],
    ) -> Result<bool, CryptoError> {
        let verifier = self
            .verifiers
            .get(algorithm)
            .ok_or_else(|| CryptoError::UnknownAlgorithm(algorithm.to_string()))?;
        verifier.verify(message, signature, public_key)
    }

    /// List registered algorithms.
    pub fn algorithms(&self) -> Vec<&str> {
        self.verifiers.keys().map(|s| s.as_str()).collect()
    }
}

// ---------------------------------------------------------------------------
// Built-in verifiers
// ---------------------------------------------------------------------------

/// Ed25519 signature verifier.
#[derive(Debug)]
pub struct Ed25519Verifier;

impl AuditVerifier for Ed25519Verifier {
    fn algorithm(&self) -> &str {
        "ed25519"
    }

    fn verify(
        &self,
        message: &[u8],
        signature: &[u8],
        public_key: &[u8],
    ) -> Result<bool, CryptoError> {
        use ed25519_dalek::Verifier;

        let pubkey_bytes: [u8; 32] = public_key
            .try_into()
            .map_err(|_| CryptoError::InvalidKey("Ed25519 public key must be 32 bytes".into()))?;
        let sig_bytes: [u8; 64] = signature.try_into().map_err(|_| {
            CryptoError::VerificationFailed("Ed25519 signature must be 64 bytes".into())
        })?;

        let pubkey = ed25519_dalek::VerifyingKey::from_bytes(&pubkey_bytes)
            .map_err(|e| CryptoError::InvalidKey(format!("Invalid Ed25519 public key: {e}")))?;
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);

        match pubkey.verify(message, &sig) {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}

/// Secp256k1 ECDSA signature verifier.
#[derive(Debug)]
pub struct Secp256k1Verifier;

impl AuditVerifier for Secp256k1Verifier {
    fn algorithm(&self) -> &str {
        "secp256k1"
    }

    /// Verify a secp256k1 signature.
    ///
    /// Accepts both 64-byte raw ECDSA signatures (from `sign()`) and 65-byte
    /// recoverable signatures (from `sign_typed()` — the recovery byte `v` is
    /// stripped before verification).
    fn verify(
        &self,
        message: &[u8],
        signature: &[u8],
        public_key: &[u8],
    ) -> Result<bool, CryptoError> {
        use k256::ecdsa::signature::Verifier;
        use k256::ecdsa::VerifyingKey;

        let pubkey = VerifyingKey::from_sec1_bytes(public_key)
            .map_err(|e| CryptoError::InvalidKey(format!("Invalid secp256k1 public key: {e}")))?;

        // Handle both 64-byte (raw) and 65-byte (recoverable, strip v) signatures
        let sig_bytes = if signature.len() == 65 {
            &signature[..64] // strip recovery byte v
        } else {
            signature
        };
        let sig = k256::ecdsa::Signature::from_slice(sig_bytes).map_err(|e| {
            CryptoError::VerificationFailed(format!("Invalid secp256k1 signature: {e}"))
        })?;

        match pubkey.verify(message, &sig) {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{AuditSigner, Ed25519Signer, Secp256k1Signer};

    #[tokio::test]
    async fn ed25519_verifier_roundtrip() {
        let signer = Ed25519Signer::generate();
        let message = b"test message";
        let sig = signer.sign(message).await.unwrap();

        let verifier = Ed25519Verifier;
        assert!(verifier
            .verify(message, &sig, &signer.public_key_bytes())
            .unwrap());
    }

    #[tokio::test]
    async fn ed25519_verifier_rejects_wrong_message() {
        let signer = Ed25519Signer::generate();
        let sig = signer.sign(b"correct").await.unwrap();

        let verifier = Ed25519Verifier;
        assert!(!verifier
            .verify(b"wrong", &sig, &signer.public_key_bytes())
            .unwrap());
    }

    #[tokio::test]
    async fn secp256k1_verifier_roundtrip() {
        let signer = Secp256k1Signer::generate();
        let message = b"test message";
        let sig = signer.sign(message).await.unwrap();

        let verifier = Secp256k1Verifier;
        assert!(verifier
            .verify(message, &sig, &signer.public_key_bytes())
            .unwrap());
    }

    #[tokio::test]
    async fn secp256k1_verifier_rejects_wrong_key() {
        let signer = Secp256k1Signer::generate();
        let other = Secp256k1Signer::generate();
        let sig = signer.sign(b"test").await.unwrap();

        let verifier = Secp256k1Verifier;
        assert!(!verifier
            .verify(b"test", &sig, &other.public_key_bytes())
            .unwrap());
    }

    #[tokio::test]
    async fn registry_with_defaults_verifies_both() {
        let registry = VerifierRegistry::with_defaults();

        let ed = Ed25519Signer::generate();
        let ed_sig = ed.sign(b"hello").await.unwrap();
        assert!(registry
            .verify("ed25519", b"hello", &ed_sig, &ed.public_key_bytes())
            .unwrap());

        let secp = Secp256k1Signer::generate();
        let secp_sig = secp.sign(b"hello").await.unwrap();
        assert!(registry
            .verify("secp256k1", b"hello", &secp_sig, &secp.public_key_bytes())
            .unwrap());
    }

    #[test]
    fn registry_unknown_algorithm_errors() {
        let registry = VerifierRegistry::new();
        let result = registry.verify("unknown", b"msg", b"sig", b"key");
        assert!(result.is_err());
    }

    #[test]
    fn ed25519_verifier_rejects_invalid_key_size() {
        let verifier = Ed25519Verifier;
        let result = verifier.verify(b"msg", &[0u8; 64], &[0u8; 16]); // wrong key size
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), CryptoError::InvalidKey(_)));
    }

    #[test]
    fn ed25519_verifier_rejects_invalid_sig_size() {
        let verifier = Ed25519Verifier;
        let result = verifier.verify(b"msg", &[0u8; 32], &[0u8; 32]); // wrong sig size
        assert!(result.is_err());
    }

    #[test]
    fn secp256k1_verifier_rejects_invalid_key() {
        let verifier = Secp256k1Verifier;
        let result = verifier.verify(b"msg", &[0u8; 64], &[0u8; 5]); // garbage key
        assert!(result.is_err());
    }

    #[test]
    fn registry_algorithms_lists_registered() {
        let registry = VerifierRegistry::with_defaults();
        let algos = registry.algorithms();
        assert!(algos.contains(&"ed25519"));
        assert!(algos.contains(&"secp256k1"));
    }

    #[test]
    fn empty_registry_has_no_algorithms() {
        let registry = VerifierRegistry::new();
        assert!(registry.algorithms().is_empty());
    }
}
