//! Files an operator uploads, and what it takes to serve them.
//!
//! [`blob`] is the storage mechanism: named bytes, their metadata, and a byte
//! ceiling. [`hls`] turns an uploaded video into something a viewer can seek
//! through, which the storage layer alone cannot offer.
//!
//! What a bucket is called and how much space an operator gets are deployment
//! policy and live with the side that owns storage, not here.

pub mod blob;
pub mod hls;
pub mod pipeline;
pub mod upload;

use sha2::{Digest as _, Sha256};

/// The lowercase hex SHA-256 of `bytes`: the name a blob is stored under.
pub fn sha256_hex_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The object store `config` names, created or, when another test made it
/// first, opened.
#[cfg(test)]
pub(crate) async fn object_bucket(
    js: &async_nats::jetstream::Context,
    config: async_nats::jetstream::object_store::Config,
) -> anyhow::Result<async_nats::jetstream::object_store::ObjectStore> {
    match js.create_object_store(config.clone()).await {
        Ok(store) => Ok(store),
        Err(_) => Ok(js.get_object_store(&config.bucket).await?),
    }
}
