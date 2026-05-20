# quorum-crypto-core

[![Crates.io](https://img.shields.io/crates/v/quorum-crypto-core.svg)](https://crates.io/crates/quorum-crypto-core)
[![Docs.rs](https://docs.rs/quorum-crypto-core/badge.svg)](https://docs.rs/quorum-crypto-core)
[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue.svg)](#license)

Cryptographic primitives + audit envelope for signing agent outputs.

A focused trait surface over Ed25519, secp256k1 (with EIP-712 typed-data
support), and a multi-signature chained envelope that makes signatures
non-strippable. Drop-in for any Rust project that needs to attest
agent-produced content and prove the attestation chain later.

## Install

> **Status: pre-release.** Latest on crates.io is `0.7.0-rc.2`. Cargo doesn't pick pre-releases without an explicit pin, so the version below is required.

```toml
[dependencies]
quorum-crypto-core = "0.7.0-rc.2"
```

MSRV: Rust 1.88.

## What's inside

| Type | Purpose |
|---|---|
| [`AuditSigner`] | Instance-bound signing trait — implementations hold a private key and produce signatures. Object-safe (`Box<dyn AuditSigner>`, `Arc<dyn AuditSigner>`). |
| [`Ed25519Signer`], [`Secp256k1Signer`] | Reference implementations. `Secp256k1Signer` adds EIP-712 typed-data signing. |
| [`AuditVerifier`] | Verify signatures from unknown agents — algorithm-agnostic. |
| [`VerifierRegistry`] | Maps algorithm strings (`"ed25519"`, `"secp256k1"`, …) to verifier implementations. Extensible for downstream algorithms. |
| [`AuditEnvelope`] | Multi-signature **chained** envelope. Each signer attests the payload AND all previous signatures — strip-resistant. |
| [`ChainHasher`] | Pluggable hash function for commitment chains. Defaults to SHA3-256. |

[`AuditSigner`]: https://docs.rs/quorum-crypto-core/latest/quorum_crypto_core/signer/trait.AuditSigner.html
[`Ed25519Signer`]: https://docs.rs/quorum-crypto-core/latest/quorum_crypto_core/signer/ed25519/struct.Ed25519Signer.html
[`Secp256k1Signer`]: https://docs.rs/quorum-crypto-core/latest/quorum_crypto_core/signer/secp256k1/struct.Secp256k1Signer.html
[`AuditVerifier`]: https://docs.rs/quorum-crypto-core/latest/quorum_crypto_core/verifier/trait.AuditVerifier.html
[`VerifierRegistry`]: https://docs.rs/quorum-crypto-core/latest/quorum_crypto_core/verifier/struct.VerifierRegistry.html
[`AuditEnvelope`]: https://docs.rs/quorum-crypto-core/latest/quorum_crypto_core/envelope/struct.AuditEnvelope.html
[`ChainHasher`]: https://docs.rs/quorum-crypto-core/latest/quorum_crypto_core/chain/trait.ChainHasher.html

## Sign and verify a payload

```rust
use quorum_crypto_core::{
    AuditEnvelope, AuditSigner, VerifierRegistry,
    envelope::SignerRole,
    signer::ed25519::Ed25519Signer,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let signer = Ed25519Signer::generate();
    let payload = serde_json::json!({ "claim": "this is the agent's answer" });

    let envelope = AuditEnvelope::new(payload)
        .sign(&signer, SignerRole::Author, "agent-1".into())
        .await?;

    let registry = VerifierRegistry::with_defaults();
    assert!(envelope.verify_all(&registry).await?);
    Ok(())
}
```

## Why chained signatures?

A naive multi-signature scheme signs only the payload — anyone can strip
later signatures and still produce a "validly signed" envelope. Each
signature in [`AuditEnvelope`] commits to the payload **and** every prior
signature in the chain, so dropping a signature invalidates everything
downstream of it.

This matters for deliberation systems: an author signs their proposal, an
evaluator signs the proposal-plus-author-signature, an orchestrator signs
the whole thing for round attestation. Any tampering with the chain is
detectable.

## Extending with a new algorithm

Implement [`AuditSigner`] + [`AuditVerifier`], register the verifier with
[`VerifierRegistry::register`], and the envelope flow handles the rest.
Downstream crates extend this surface with post-quantum signing
(e.g. ML-DSA-65), Poseidon hashing, and other primitives without touching
the core trait shapes.

## Used by

[`quorum-rs`](https://crates.io/crates/quorum-rs) gates cryptographic
agent-output attestation behind its `audit` feature, which pulls in this
crate. Outside that context, this crate is fully usable standalone —
nothing here depends on the deliberation protocol.

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/peeramid-labs/quorum-rs/blob/HEAD/LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](https://github.com/peeramid-labs/quorum-rs/blob/HEAD/LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
