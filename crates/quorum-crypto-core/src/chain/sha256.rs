//! SHA-256 chain hasher — fast, universal, non-ZK.

use super::ChainHasher;
use sha2::{Digest, Sha256};

/// SHA-256 based chain hasher (test/fallback — not ZK-friendly).
#[derive(Debug, Clone, Default)]
pub struct Sha256ChainHasher;

impl ChainHasher for Sha256ChainHasher {
    fn hash_pair(&self, left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(left);
        hasher.update(right);
        let result = hasher.finalize();
        let mut output = [0u8; 32];
        output.copy_from_slice(&result);
        output
    }

    fn algorithm(&self) -> &str {
        "sha256"
    }

    fn parameters_json(&self) -> String {
        r#"{"algorithm":"sha256","version":"2.0"}"#.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hash_pair_deterministic() {
        let hasher = Sha256ChainHasher;
        let left = [1u8; 32];
        let right = [2u8; 32];
        let a = hasher.hash_pair(&left, &right);
        let b = hasher.hash_pair(&left, &right);
        assert_eq!(a, b);
    }

    #[test]
    fn sha256_hash_pair_order_matters() {
        let hasher = Sha256ChainHasher;
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_ne!(hasher.hash_pair(&a, &b), hasher.hash_pair(&b, &a));
    }

    #[test]
    fn sha256_algorithm_name() {
        let hasher = Sha256ChainHasher;
        assert_eq!(hasher.algorithm(), "sha256");
    }
}
