//! # nsed-crypto-core
//!
//! Cryptographic primitives and traits for NSED agent signing and verification.
//!
//! - [`AuditSigner`] — sign messages (Ed25519, Secp256k1)
//! - [`AuditVerifier`] — verify signatures from unknown agents
//! - [`VerifierRegistry`] — route algorithm strings to verifier implementations
//! - [`AuditEnvelope`] — multi-signature chained envelope for any serializable payload
//! - [`ChainHasher`] — pluggable hash function for commitment chains
//!
//! `nsed-crypto` extends this crate with post-quantum signing (ML-DSA-65),
//! Poseidon hashing, commitment chains, and settlement provers.

pub mod canonical;
pub mod chain;
pub mod envelope;
pub mod error;
pub mod signer;
pub mod verifier;

pub use canonical::canonical_bytes;
pub use chain::ChainHasher;
pub use envelope::{AuditEnvelope, SignatureStatus};
pub use error::CryptoError;
pub use signer::AuditSigner;
pub use verifier::{AuditVerifier, VerifierRegistry};
