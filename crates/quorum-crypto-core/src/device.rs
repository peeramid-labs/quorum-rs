//! Device identity — a NATS **user** nkey (`U…`) behind an ergonomic API so
//! consumers (e.g. noolog-app) never link `nkeys`/`nats` directly.
//!
//! The encoding matches exactly what the nsed orchestrator's
//! `register_self_serve` verifies (`nkeys` 0.4, user key). Register is
//! **one-shot** — there is no challenge/nonce round-trip: sign the message
//! `nsed-operator-register:{public_key}`, hex-encode the signature, and
//! `POST /register {pubkey, signature}`. See nsed `docs/invites.md`.
//!
//! Gated behind the default-on `device` feature so a `wasm32` consumer that
//! doesn't need register (the web path uses WebAuthn) can drop the `nkeys`
//! dependency via `default-features = false`.

use crate::error::CryptoError;
use nkeys::KeyPair;

/// A device's NATS user nkey. Persist [`seed`](Self::seed) (guard it `0600`);
/// present [`public_key`](Self::public_key) to the server as `operator_pubkey`.
pub struct DeviceIdentity {
    inner: KeyPair,
}

impl DeviceIdentity {
    /// A fresh **user**-type nkey (`U…`) — register rejects account/server keys,
    /// so this is the only correct kind to mint.
    pub fn generate() -> Self {
        Self {
            inner: KeyPair::new_user(),
        }
    }

    /// Rehydrate from a persisted seed (`SU…`). Errors on a malformed seed or a
    /// non-user key type — the orchestrator accepts only user keys.
    pub fn from_seed(seed: &str) -> Result<Self, CryptoError> {
        let inner =
            KeyPair::from_seed(seed).map_err(|e| CryptoError::InvalidKey(format!("seed: {e}")))?;
        let id = Self { inner };
        if !id.public_key().starts_with('U') {
            return Err(CryptoError::InvalidKey(
                "not a NATS user key (expected a U… public key)".into(),
            ));
        }
        Ok(id)
    }

    /// The seed to persist (`SU…`). Store it `0600`; never transmit it.
    pub fn seed(&self) -> Result<String, CryptoError> {
        self.inner
            .seed()
            .map_err(|e| CryptoError::InvalidKey(format!("seed export: {e}")))
    }

    /// The public key (`U…`) — what the server stores as `operator_pubkey`.
    pub fn public_key(&self) -> String {
        self.inner.public_key()
    }

    /// Raw signature over `msg`.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.inner
            .sign(msg)
            .map_err(|e| CryptoError::SigningFailed(e.to_string()))
    }

    /// Lowercase-hex signature — the exact encoding `POST /register` hex-decodes.
    pub fn sign_hex(&self, msg: &[u8]) -> Result<String, CryptoError> {
        Ok(hex::encode(self.sign(msg)?))
    }

    /// Verify a signature under the server's scheme (mirrors the orchestrator's
    /// `KeyPair::from_public_key(pk).verify(msg, sig)`), so the SDK can self-test.
    pub fn verify(pubkey: &str, msg: &[u8], sig: &[u8]) -> Result<(), CryptoError> {
        let kp = KeyPair::from_public_key(pubkey)
            .map_err(|e| CryptoError::InvalidKey(format!("pubkey: {e}")))?;
        kp.verify(msg, sig)
            .map_err(|e| CryptoError::VerificationFailed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register_msg(id: &DeviceIdentity) -> Vec<u8> {
        format!("nsed-operator-register:{}", id.public_key()).into_bytes()
    }

    #[test]
    fn generate_produces_user_key() {
        assert!(DeviceIdentity::generate().public_key().starts_with('U'));
    }

    #[test]
    fn sign_verify_roundtrip_under_server_scheme() {
        let id = DeviceIdentity::generate();
        let msg = register_msg(&id);
        let sig = id.sign(&msg).unwrap();
        DeviceIdentity::verify(&id.public_key(), &msg, &sig).expect("valid sig verifies");
        let mut tampered = msg.clone();
        tampered[0] ^= 0xff;
        assert!(
            DeviceIdentity::verify(&id.public_key(), &tampered, &sig).is_err(),
            "tampered message must fail"
        );
    }

    #[test]
    fn sign_hex_is_hex_of_raw() {
        let id = DeviceIdentity::generate();
        let msg = b"nsed-operator-register:test";
        // ed25519 signatures are deterministic, so this is stable.
        assert_eq!(
            id.sign_hex(msg).unwrap(),
            hex::encode(id.sign(msg).unwrap())
        );
    }

    #[test]
    fn from_seed_reproduces_public_key() {
        let id = DeviceIdentity::generate();
        let seed = id.seed().unwrap();
        assert!(seed.starts_with("SU"), "user seed is SU…: {seed}");
        let restored = DeviceIdentity::from_seed(&seed).unwrap();
        assert_eq!(restored.public_key(), id.public_key());
    }

    #[test]
    fn from_seed_rejects_non_user_seed() {
        // An account seed (SA…) must be rejected — the server is user-key only.
        let account_seed = KeyPair::new_account().seed().unwrap();
        assert!(DeviceIdentity::from_seed(&account_seed).is_err());
    }
}
