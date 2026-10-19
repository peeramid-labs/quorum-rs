//! Who is calling, as the node asserts it to the services behind it.
//!
//! The node authenticates every request (session, API token, passkey) and
//! forwards to a service's internal listener with [`HEADER`] set to a signed
//! [`Caller`]. The service verifies the signature with the key both derive
//! from the KMS (`internal/node-to-orch/v1`) and takes the caller as given: it
//! keeps no credential store of its own.
//!
//! The value is `<base64url(json)>.<hex HMAC-SHA256(key, base64url(json))>`.
//! An assertion lives [`LIFETIME_SECS`]: long enough for one forwarded request,
//! short enough that a captured header is useless.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// The header carrying the assertion.
pub const HEADER: &str = "x-noolog-caller";
/// How long an assertion is accepted after it is signed.
pub const LIFETIME_SECS: u64 = 60;

/// The caller: the fields a service decides access by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caller {
    pub principal: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// A platform service acting for itself, not an operator.
    #[serde(default)]
    pub system: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_cap: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_pubkey: Option<String>,
    /// Identity tags the caller's agents inherit.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Glob patterns matched against other entities' tags.
    #[serde(default)]
    pub grants: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Signed {
    #[serde(flatten)]
    caller: Caller,
    /// Unix seconds after which the assertion is refused.
    exp: u64,
}

/// Why an assertion was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("not an assertion")]
    Malformed,
    #[error("signature does not verify")]
    BadSignature,
    #[error("assertion expired")]
    Expired,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mac(key: &str, payload: &str) -> Hmac<Sha256> {
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(key.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(payload.as_bytes());
    mac
}

/// The header value asserting `caller`, signed with `key`.
pub fn sign(caller: &Caller, key: &str) -> String {
    sign_at(caller, key, now() + LIFETIME_SECS)
}

fn sign_at(caller: &Caller, key: &str, exp: u64) -> String {
    let json = serde_json::to_vec(&Signed {
        caller: caller.clone(),
        exp,
    })
    .expect("a caller always serializes");
    let payload = URL_SAFE_NO_PAD.encode(json);
    let signature = hex::encode(mac(key, &payload).finalize().into_bytes());
    format!("{payload}.{signature}")
}

/// The caller `value` asserts, if `key` signed it and it has not expired.
/// An empty key verifies nothing.
pub fn verify(value: &str, key: &str) -> Result<Caller, Refusal> {
    let (payload, signature) = value.split_once('.').ok_or(Refusal::Malformed)?;
    let signature = hex::decode(signature).map_err(|_| Refusal::Malformed)?;
    if key.is_empty() || mac(key, payload).verify_slice(&signature).is_err() {
        return Err(Refusal::BadSignature);
    }
    let json = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| Refusal::Malformed)?;
    let signed: Signed = serde_json::from_slice(&json).map_err(|_| Refusal::Malformed)?;
    if signed.exp < now() {
        return Err(Refusal::Expired);
    }
    Ok(signed.caller)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann() -> Caller {
        Caller {
            principal: "op_ann".into(),
            roles: vec!["operator".into()],
            display_name: Some("Ann".into()),
            system: false,
            budget_cap: Some(5.0),
            operator_pubkey: None,
            tags: vec!["op:ann".into()],
            grants: vec!["op:ann*".into()],
        }
    }

    #[test]
    fn a_signed_caller_verifies_with_the_same_key_only() {
        let asserted = sign(&ann(), "k1");
        assert_eq!(verify(&asserted, "k1"), Ok(ann()));
        assert_eq!(verify(&asserted, "k2"), Err(Refusal::BadSignature));
        assert_eq!(verify(&asserted, ""), Err(Refusal::BadSignature));
    }

    #[test]
    fn a_changed_payload_or_an_old_assertion_is_refused() {
        let asserted = sign(&ann(), "k1");
        let (_, signature) = asserted.split_once('.').unwrap();
        let mut admin = ann();
        admin.roles = vec!["admin".into()];
        let forged = format!(
            "{}.{signature}",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&Signed {
                    caller: admin,
                    exp: now() + 60
                })
                .unwrap()
            )
        );
        assert_eq!(verify(&forged, "k1"), Err(Refusal::BadSignature));

        let old = sign_at(&ann(), "k1", now() - 1);
        assert_eq!(verify(&old, "k1"), Err(Refusal::Expired));
        for junk in ["", "abc", "abc.zz", "abc.00"] {
            assert!(verify(junk, "k1").is_err(), "{junk}");
        }
    }
}
