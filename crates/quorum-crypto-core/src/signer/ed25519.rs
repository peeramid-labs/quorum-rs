//! Ed25519 signer using `ed25519-dalek`.

use crate::error::CryptoError;
use crate::signer::AuditSigner;

/// Ed25519 signer — lightweight, fast, SSH-key compatible.
#[derive(Debug)]
pub struct Ed25519Signer {
    keypair: ed25519_dalek::SigningKey,
}

impl Ed25519Signer {
    /// Create from a random key.
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        Self {
            keypair: ed25519_dalek::SigningKey::generate(&mut rng),
        }
    }

    /// Create from a 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            keypair: ed25519_dalek::SigningKey::from_bytes(seed),
        }
    }

    /// Create from raw secret key bytes (32 bytes).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        let seed: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CryptoError::InvalidKey("Ed25519 key must be 32 bytes".into()))?;
        Ok(Self::from_seed(&seed))
    }
}

#[async_trait::async_trait]
impl AuditSigner for Ed25519Signer {
    fn algorithm(&self) -> &str {
        "ed25519"
    }

    fn public_key_bytes(&self) -> Vec<u8> {
        self.keypair.verifying_key().to_bytes().to_vec()
    }

    fn public_key_display(&self) -> String {
        hex::encode(self.keypair.verifying_key().to_bytes())
    }

    async fn sign(&self, message: &[u8]) -> Result<Vec<u8>, CryptoError> {
        use ed25519_dalek::Signer;
        let sig = self.keypair.sign(message);
        Ok(sig.to_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ed25519_sign_verify_roundtrip() {
        let signer = Ed25519Signer::generate();
        let message = b"hello world";
        let sig = signer.sign(message).await.unwrap();

        // Verify with dalek directly
        use ed25519_dalek::Verifier;
        let pubkey =
            ed25519_dalek::VerifyingKey::from_bytes(&signer.public_key_bytes().try_into().unwrap())
                .unwrap();
        let signature = ed25519_dalek::Signature::from_bytes(&sig.try_into().unwrap());
        pubkey.verify(message, &signature).unwrap();
    }

    #[tokio::test]
    async fn ed25519_from_seed_deterministic() {
        let seed = [42u8; 32];
        let a = Ed25519Signer::from_seed(&seed);
        let b = Ed25519Signer::from_seed(&seed);
        assert_eq!(a.public_key_bytes(), b.public_key_bytes());
        let sig_a = a.sign(b"test").await.unwrap();
        let sig_b = b.sign(b"test").await.unwrap();
        assert_eq!(sig_a, sig_b);
    }

    #[test]
    fn ed25519_algorithm_name() {
        let signer = Ed25519Signer::generate();
        assert_eq!(signer.algorithm(), "ed25519");
    }

    #[test]
    fn ed25519_does_not_support_eip712() {
        let signer = Ed25519Signer::generate();
        assert!(!signer.supports_eip712());
    }

    #[test]
    fn ed25519_from_invalid_bytes_fails() {
        assert!(Ed25519Signer::from_bytes(&[0u8; 16]).is_err());
    }
}
