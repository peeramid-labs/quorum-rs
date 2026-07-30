//! # quorum-crypto-core
//!
//! Cryptographic primitives and traits for agent signing and verification.
//!
//! - [`AuditSigner`] — sign messages (Ed25519, Secp256k1)
//! - [`AuditVerifier`] — verify signatures from unknown agents
//! - [`VerifierRegistry`] — route algorithm strings to verifier implementations
//! - [`AuditEnvelope`] — multi-signature chained envelope for any serializable payload
//! - [`ChainHasher`] — pluggable hash function for commitment chains
//! - [`DeviceIdentity`] — NATS user-nkey wrapper for self-serve register
//!   (`device` feature, default-on)
//!
//! Downstream crates can extend this surface with post-quantum signing
//! (e.g. ML-DSA-65), Poseidon-style hashing, commitment chains, and
//! settlement provers without changing the core trait shapes here.

pub mod canonical;
pub mod chain;
#[cfg(feature = "device")]
pub mod device;
pub mod envelope;
pub mod error;
pub mod signer;
pub mod verifier;

pub use canonical::canonical_bytes;
pub use chain::ChainHasher;
#[cfg(feature = "device")]
pub use device::DeviceIdentity;
pub use envelope::{AuditEnvelope, SignatureStatus};
pub use error::CryptoError;
pub use signer::AuditSigner;
pub use verifier::{AuditVerifier, VerifierRegistry};
