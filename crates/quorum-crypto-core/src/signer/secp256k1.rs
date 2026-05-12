//! Secp256k1 ECDSA signer with EIP-712 typed data support.

use crate::error::CryptoError;
use crate::signer::AuditSigner;
use k256::ecdsa::SigningKey;

/// Secp256k1 ECDSA signer — EVM-compatible with EIP-712 typed signing.
#[derive(Debug)]
pub struct Secp256k1Signer {
    key: SigningKey,
}

impl Secp256k1Signer {
    /// Generate a random secp256k1 key.
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        Self {
            key: SigningKey::random(&mut rng),
        }
    }

    /// Create from a 32-byte secret key.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        let bytes: &[u8; 32] = bytes
            .try_into()
            .map_err(|_| CryptoError::InvalidKey("Secp256k1 secret key must be 32 bytes".into()))?;
        let key = SigningKey::from_bytes(bytes.into())
            .map_err(|e| CryptoError::InvalidKey(format!("Invalid secp256k1 key: {e}")))?;
        Ok(Self { key })
    }

    /// Get the compressed public key (33 bytes).
    pub fn compressed_pubkey(&self) -> Vec<u8> {
        #[allow(unused_imports)]
        use k256::elliptic_curve::sec1::ToEncodedPoint;
        self.key
            .verifying_key()
            .to_encoded_point(true)
            .as_bytes()
            .to_vec()
    }

    /// Get the uncompressed public key (65 bytes, 04 prefix).
    pub fn uncompressed_pubkey(&self) -> Vec<u8> {
        #[allow(unused_imports)]
        use k256::elliptic_curve::sec1::ToEncodedPoint;
        self.key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec()
    }

    /// Derive Ethereum address from the public key (last 20 bytes of keccak256).
    pub fn eth_address(&self) -> [u8; 20] {
        use sha3::{Digest as Sha3Digest, Keccak256};
        let uncompressed = self.uncompressed_pubkey();
        // Skip the 0x04 prefix byte
        let hash = Keccak256::digest(&uncompressed[1..]);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&hash[12..]);
        addr
    }

    /// Ethereum address as hex string with 0x prefix.
    pub fn eth_address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.eth_address()))
    }
}

#[async_trait::async_trait]
impl AuditSigner for Secp256k1Signer {
    /// Returns "secp256k1" for both `sign()` (64-byte ECDSA) and `sign_typed()`
    /// (65-byte recoverable EIP-712). Callers that need to distinguish should
    /// check the signature length: 64 = raw ECDSA, 65 = recoverable (r||s||v).
    fn algorithm(&self) -> &str {
        "secp256k1"
    }

    fn public_key_bytes(&self) -> Vec<u8> {
        self.compressed_pubkey()
    }

    fn public_key_display(&self) -> String {
        format!("0x{}", hex::encode(self.compressed_pubkey()))
    }

    async fn sign(&self, message: &[u8]) -> Result<Vec<u8>, CryptoError> {
        // Standard ECDSA signing — k256 handles SHA-256 internally via the Signer trait
        use k256::ecdsa::signature::Signer;
        let sig: k256::ecdsa::Signature = Signer::sign(&self.key, message);
        Ok(sig.to_bytes().to_vec())
    }

    async fn sign_typed(
        &self,
        domain_separator: &[u8],
        struct_hash: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        use sha3::{Digest as Sha3Digest, Keccak256};

        // Validate inputs are 32-byte hashes (EIP-712 requirement)
        if domain_separator.len() != 32 {
            return Err(CryptoError::InvalidKey(format!(
                "EIP-712 domain_separator must be 32 bytes, got {}",
                domain_separator.len()
            )));
        }
        if struct_hash.len() != 32 {
            return Err(CryptoError::InvalidKey(format!(
                "EIP-712 struct_hash must be 32 bytes, got {}",
                struct_hash.len()
            )));
        }

        // EIP-712: keccak256("\x19\x01" || domainSeparator || structHash)
        let mut hasher = Keccak256::new();
        hasher.update(b"\x19\x01");
        hasher.update(domain_separator);
        hasher.update(struct_hash);
        let digest = hasher.finalize();

        // Sign the EIP-712 digest with recoverable signature
        let (sig, recovery_id) = self
            .key
            .sign_prehash_recoverable(&digest)
            .map_err(|e| CryptoError::SigningFailed(format!("EIP-712 signing failed: {e}")))?;

        // Return 65-byte signature: r (32) || s (32) || v (1)
        // v = recovery_id + 27 (legacy/pre-EIP-155 format, used for off-chain EIP-712 signing)
        // Note: EIP-155 on-chain format uses v = chainId*2 + 35 + recovery_id
        let mut result = Vec::with_capacity(65);
        result.extend_from_slice(&sig.to_bytes());
        result.push(recovery_id.to_byte() + 27);
        Ok(result)
    }

    fn supports_eip712(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::signature::Verifier;

    #[tokio::test]
    async fn secp256k1_sign_verify_roundtrip() {
        let signer = Secp256k1Signer::generate();
        let message = b"hello world";
        let sig_bytes = signer.sign(message).await.unwrap();

        // Verify with k256 directly
        let pubkey = *signer.key.verifying_key();
        let sig = k256::ecdsa::Signature::from_slice(&sig_bytes).unwrap();
        pubkey.verify(message, &sig).unwrap();
    }

    #[tokio::test]
    async fn secp256k1_from_bytes_deterministic() {
        let secret = [42u8; 32];
        let a = Secp256k1Signer::from_bytes(&secret).unwrap();
        let b = Secp256k1Signer::from_bytes(&secret).unwrap();
        assert_eq!(a.public_key_bytes(), b.public_key_bytes());
        assert_eq!(a.eth_address(), b.eth_address());
    }

    #[test]
    fn secp256k1_eth_address_is_20_bytes() {
        let signer = Secp256k1Signer::generate();
        assert_eq!(signer.eth_address().len(), 20);
        assert!(signer.eth_address_hex().starts_with("0x"));
        assert_eq!(signer.eth_address_hex().len(), 42); // 0x + 40 hex chars
    }

    #[test]
    fn secp256k1_supports_eip712() {
        let signer = Secp256k1Signer::generate();
        assert!(signer.supports_eip712());
    }

    #[tokio::test]
    async fn secp256k1_sign_typed_returns_65_bytes() {
        let signer = Secp256k1Signer::generate();
        let domain = [1u8; 32];
        let struct_hash = [2u8; 32];
        let sig = signer.sign_typed(&domain, &struct_hash).await.unwrap();
        assert_eq!(sig.len(), 65); // r(32) + s(32) + v(1)
        assert!(sig[64] == 27 || sig[64] == 28); // v is 27 or 28
    }

    #[test]
    fn secp256k1_algorithm_name() {
        let signer = Secp256k1Signer::generate();
        assert_eq!(signer.algorithm(), "secp256k1");
    }

    #[tokio::test]
    async fn secp256k1_sign_typed_rejects_wrong_domain_size() {
        let signer = Secp256k1Signer::generate();
        let bad_domain = [1u8; 16]; // should be 32
        let struct_hash = [2u8; 32];
        assert!(signer.sign_typed(&bad_domain, &struct_hash).await.is_err());
    }

    #[tokio::test]
    async fn secp256k1_sign_typed_rejects_wrong_struct_hash_size() {
        let signer = Secp256k1Signer::generate();
        let domain = [1u8; 32];
        let bad_hash = [2u8; 48]; // should be 32
        assert!(signer.sign_typed(&domain, &bad_hash).await.is_err());
    }

    #[test]
    fn secp256k1_compressed_pubkey_is_33_bytes() {
        let signer = Secp256k1Signer::generate();
        assert_eq!(signer.compressed_pubkey().len(), 33);
    }

    #[test]
    fn secp256k1_uncompressed_pubkey_is_65_bytes() {
        let signer = Secp256k1Signer::generate();
        let uncompressed = signer.uncompressed_pubkey();
        assert_eq!(uncompressed.len(), 65);
        assert_eq!(uncompressed[0], 0x04); // uncompressed prefix
    }

    #[test]
    fn secp256k1_from_invalid_bytes_fails() {
        assert!(Secp256k1Signer::from_bytes(&[0u8; 5]).is_err()); // wrong length
        assert!(Secp256k1Signer::from_bytes(&[]).is_err()); // empty
    }

    #[test]
    fn secp256k1_different_keys_different_addresses() {
        let a = Secp256k1Signer::generate();
        let b = Secp256k1Signer::generate();
        assert_ne!(a.eth_address(), b.eth_address());
        assert_ne!(a.compressed_pubkey(), b.compressed_pubkey());
    }
}
