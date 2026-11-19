//! Validated SHA-256 digests.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ProtocolError;

/// A SHA-256 digest rendered as 64 lowercase hexadecimal characters.
///
/// Construction validates the format, so a value of this type always
/// serializes to a digest accepted by the wire schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Parses a digest, requiring exactly 64 lowercase hexadecimal characters.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidDigest`] when `value` is not a valid
    /// digest.
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(ProtocolError::InvalidDigest)
        }
    }

    /// Returns the digest as lowercase hexadecimal text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn accepts_lowercase_hex() {
        let digest = Sha256Digest::parse(VALID).expect("valid digest");
        assert_eq!(digest.as_str(), VALID);
        assert_eq!(digest.to_string(), VALID);
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(
            Sha256Digest::parse("abc"),
            Err(ProtocolError::InvalidDigest)
        );
    }

    #[test]
    fn rejects_uppercase() {
        let uppercase = "A".repeat(64);
        assert_eq!(
            Sha256Digest::parse(&uppercase),
            Err(ProtocolError::InvalidDigest)
        );
    }

    #[test]
    fn rejects_non_hex() {
        let digest = "g".repeat(64);
        assert_eq!(
            Sha256Digest::parse(&digest),
            Err(ProtocolError::InvalidDigest)
        );
    }

    #[test]
    fn deserialization_validates() {
        assert!(serde_json::from_str::<Sha256Digest>(&format!("\"{VALID}\"")).is_ok());
        assert!(serde_json::from_str::<Sha256Digest>("\"not-a-digest\"").is_err());
    }
}
