//! The `Id` data type (RFC 8620 §1.2).

use std::fmt;

/// 1 to 255 characters of the URL-safe base64 alphabet, without padding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct Id(String);

/// A string that is not a valid [`Id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId(pub String);

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a JMAP Id: {:?}", self.0)
    }
}

impl std::error::Error for InvalidId {}

impl Id {
    /// `raw` as an `Id`, or the reason it is not one.
    pub fn new(raw: impl Into<String>) -> Result<Self, InvalidId> {
        let raw = raw.into();
        let valid = (1..=255).contains(&raw.len())
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if valid {
            Ok(Self(raw))
        } else {
            Err(InvalidId(raw))
        }
    }

    /// The id as sent on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Id::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
