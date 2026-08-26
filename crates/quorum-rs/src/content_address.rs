//! Addresses that name content by its own hash.
//!
//! A proposal with [`crate::agents::ProposalType::Uri`] carries one of these
//! instead of its answer. The digest is the whole point: whoever resolves the
//! address can hash what came back and prove it is the thing that was named, so
//! the address survives passing through parties that are not trusted to preserve
//! content.
//!
//! Nothing here decides *which* scheme or bucket to use. Those are deployment
//! conventions and stay with the side that owns storage; this only agrees on the
//! shape so a producer and a reader can exchange one.

use std::fmt;
use std::str::FromStr;

/// Why a string is not a content address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// No `://`. A bare path names a substrate nobody agreed on.
    MissingScheme,
    /// Scheme, bucket or digest was empty.
    EmptyComponent(&'static str),
    /// Not `<scheme>://<bucket>/<digest>`.
    Malformed,
    /// The digest is not 64 lowercase hex characters.
    NotADigest,
    /// A component contained a character that would change how the address parses.
    IllegalCharacter(&'static str),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingScheme => write!(f, "no scheme: an address must be <scheme>://…"),
            Self::EmptyComponent(w) => write!(f, "empty {w}"),
            Self::Malformed => write!(f, "not <scheme>://<bucket>/<digest>"),
            Self::NotADigest => write!(f, "digest is not 64 lowercase hex characters"),
            Self::IllegalCharacter(w) => write!(f, "illegal character in {w}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// `<scheme>://<bucket>/<digest>`, where `digest` is hex SHA-256 of the content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentAddress {
    scheme: String,
    bucket: String,
    digest: String,
}

impl ContentAddress {
    /// Build an address, rejecting components that would not round-trip.
    pub fn new(
        scheme: impl Into<String>,
        bucket: impl Into<String>,
        digest: impl Into<String>,
    ) -> Result<Self, ParseError> {
        let (scheme, bucket, digest) = (scheme.into(), bucket.into(), digest.into());

        if scheme.is_empty() {
            return Err(ParseError::EmptyComponent("scheme"));
        }
        if bucket.is_empty() {
            return Err(ParseError::EmptyComponent("bucket"));
        }
        if !is_sha256_hex(&digest) {
            return Err(ParseError::NotADigest);
        }
        // A separator inside a component would make the address parse back into
        // different parts than it was built from.
        if scheme.contains(':') || scheme.contains('/') {
            return Err(ParseError::IllegalCharacter("scheme"));
        }
        if bucket.contains('/') {
            return Err(ParseError::IllegalCharacter("bucket"));
        }

        Ok(Self {
            scheme,
            bucket,
            digest,
        })
    }

    /// The substrate this resolves through — what a reader dispatches on.
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Which store within the scheme holds the content.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Hex SHA-256 of the content this names.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl fmt::Display for ContentAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}/{}", self.scheme, self.bucket, self.digest)
    }
}

impl FromStr for ContentAddress {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (scheme, rest) = s.split_once("://").ok_or(ParseError::MissingScheme)?;
        let (bucket, digest) = rest.split_once('/').ok_or(ParseError::Malformed)?;
        // A second separator means the caller meant a path, which this is not.
        if digest.contains('/') {
            return Err(ParseError::Malformed);
        }
        Self::new(scheme, bucket, digest)
    }
}

/// True when `s` is exactly 64 lowercase hex characters.
///
/// Lowercase only: the same content must produce one address, and uppercase hex
/// would give a second spelling of the same digest.
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "9f2c1bd4e5a6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f80";

    #[test]
    fn an_address_round_trips_through_its_text_form() {
        let addr = ContentAddress::new("obj", "answers_acme", D).expect("valid");
        let text = addr.to_string();
        assert_eq!(text, format!("obj://answers_acme/{D}"));
        assert_eq!(text.parse::<ContentAddress>().expect("reparses"), addr);
    }

    #[test]
    fn components_are_recoverable() {
        let addr: ContentAddress = format!("obj://answers_acme/{D}").parse().expect("valid");
        assert_eq!(addr.scheme(), "obj");
        assert_eq!(addr.bucket(), "answers_acme");
        assert_eq!(addr.digest(), D);
    }

    #[test]
    fn a_bare_path_is_refused() {
        // Named substrate or nothing — a path alone says nothing about how to resolve it.
        assert_eq!(
            format!("answers_acme/{D}").parse::<ContentAddress>(),
            Err(ParseError::MissingScheme)
        );
    }

    #[test]
    fn a_digest_that_is_not_a_sha256_is_refused() {
        for bad in [
            "".to_string(),
            "nope".to_string(),
            "z".repeat(64),   // hex-length, not hex
            "a".repeat(63),   // one short
            "a".repeat(65),   // one long
            D.to_uppercase(), // one content, one address
        ] {
            assert_eq!(
                format!("obj://b/{bad}").parse::<ContentAddress>(),
                Err(ParseError::NotADigest),
                "{bad:?} must not parse"
            );
        }
    }

    #[test]
    fn a_path_is_not_an_address() {
        // Extra segments would let two different strings name the same object.
        assert_eq!(
            format!("obj://bucket/sub/{D}").parse::<ContentAddress>(),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            "obj://bucket".parse::<ContentAddress>(),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn components_that_would_not_round_trip_are_refused() {
        assert_eq!(
            ContentAddress::new("ob/j", "b", D),
            Err(ParseError::IllegalCharacter("scheme"))
        );
        assert_eq!(
            ContentAddress::new("obj", "a/b", D),
            Err(ParseError::IllegalCharacter("bucket"))
        );
        assert_eq!(
            ContentAddress::new("", "b", D),
            Err(ParseError::EmptyComponent("scheme"))
        );
        assert_eq!(
            ContentAddress::new("obj", "", D),
            Err(ParseError::EmptyComponent("bucket"))
        );
    }

    #[test]
    fn nothing_here_assumes_a_scheme_or_bucket() {
        // Conventions belong to whoever owns storage; this agrees only on shape.
        let a = ContentAddress::new("s3", "whatever", D).expect("any scheme");
        let b = ContentAddress::new("obj", "other", D).expect("any bucket");
        assert_ne!(a, b);
        assert_eq!(a.digest(), b.digest(), "the same content, addressed twice");
    }
}
