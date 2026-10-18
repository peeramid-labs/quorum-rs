//! Shared scheduling types used by both orchestrators and agents. The
//! definitions live in `noolog-policy`, a leaf crate, so crates quorum-rs
//! depends on can name them too.

pub use noolog_policy::PolicySla;
