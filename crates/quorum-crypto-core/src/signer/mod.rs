//! Signing traits and implementations.
//!
//! - [`AuditSigner`] — trait for instance-bound signing
//! - [`Ed25519Signer`] — Ed25519 via `ed25519-dalek`
//! - [`Secp256k1Signer`] — secp256k1 ECDSA with EIP-712 typed data support

pub mod ed25519;
pub mod secp256k1;

pub use self::ed25519::Ed25519Signer;
pub use self::secp256k1::Secp256k1Signer;

use crate::CryptoError;
use std::fmt::Debug;

/// Instance-bound signing trait.
///
/// Each implementation holds a private key and can produce signatures.
/// Object-safe — can be used as `Box<dyn AuditSigner>` or `Arc<dyn AuditSigner>`.
#[async_trait::async_trait]
pub trait AuditSigner: Send + Sync + Debug {
    /// Algorithm identifier string (e.g., "ed25519", "secp256k1", "ml-dsa-65").
    fn algorithm(&self) -> &str;

    /// Public key bytes (raw, not encoded).
    fn public_key_bytes(&self) -> Vec<u8>;

    /// Human-readable public key display (e.g., hex-encoded, base58, etc.).
    fn public_key_display(&self) -> String;

    /// Sign a raw message. Returns the signature bytes.
    async fn sign(&self, message: &[u8]) -> Result<Vec<u8>, CryptoError>;

    /// Sign EIP-712 typed data. Default returns `UnsupportedOperation`.
    ///
    /// Only `Secp256k1Signer` overrides this — Ed25519 and ML-DSA-65 don't
    /// support EIP-712 (it requires keccak256 + secp256k1 recovery).
    async fn sign_typed(
        &self,
        _domain_separator: &[u8],
        _struct_hash: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::UnsupportedOperation(format!(
            "{} does not support EIP-712 typed signing",
            self.algorithm()
        )))
    }

    /// Whether this signer supports EIP-712 typed signing.
    fn supports_eip712(&self) -> bool {
        false
    }
}
