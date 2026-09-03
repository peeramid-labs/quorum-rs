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
pub mod upload;
