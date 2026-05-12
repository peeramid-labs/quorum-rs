//! Commitment chain hash function trait and SHA-256 implementation.

pub mod sha256;

pub use self::sha256::Sha256ChainHasher;

use std::fmt::Debug;

/// Swappable hash function for commitment chains.
///
/// The commitment chain hashes pairs of 32-byte values to produce
/// a new 32-byte commitment. Different hash functions provide different
/// trade-offs: SHA-256 is fast and universal; Poseidon is ZK-friendly.
pub trait ChainHasher: Send + Sync + Debug {
    /// Hash a pair of 32-byte values into a 32-byte commitment.
    fn hash_pair(&self, left: &[u8; 32], right: &[u8; 32]) -> [u8; 32];

    /// Algorithm name (e.g., "sha256", "poseidon-bn254").
    fn algorithm(&self) -> &str;

    /// Algorithm parameters as JSON (for reproducibility).
    fn parameters_json(&self) -> String;
}
