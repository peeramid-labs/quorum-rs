//! Error types for crypto operations.

/// Errors from signing, verification, and chain operations.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("signing failed: {0}")]
    SigningFailed(String),

    #[error("verification failed: {0}")]
    VerificationFailed(String),

    #[error("invalid key: {0}")]
    InvalidKey(String),

    #[error("unsupported operation: {0}")]
    UnsupportedOperation(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("unknown algorithm: {0}")]
    UnknownAlgorithm(String),

    #[error("chain error: {0}")]
    ChainError(String),

    #[error("publish failed: {0}")]
    PublishFailed(String),
}
