use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const PREFIX: &str = "ed25519:";

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey([u8; 32]);

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseKeyError {
    #[error("public key must start with `ed25519:`")]
    MissingPrefix,
    #[error("public key is not unpadded base64url")]
    Base64,
    #[error("public key must decode to 32 bytes, got {0}")]
    Length(usize),
}

impl PublicKey {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, ParseKeyError> {
        <[u8; 32]>::try_from(bytes)
            .map(Self)
            .map_err(|_| ParseKeyError::Length(bytes.len()))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The first 8 characters of the encoding, for log lines.
    pub fn short(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)[..8].to_owned()
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}", URL_SAFE_NO_PAD.encode(self.0))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

impl FromStr for PublicKey {
    type Err = ParseKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s.strip_prefix(PREFIX).ok_or(ParseKeyError::MissingPrefix)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(rest)
            .map_err(|_| ParseKeyError::Base64)?;
        Self::from_slice(&bytes)
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn round_trips_through_text() {
        let key = PublicKey::from_bytes([7u8; 32]);
        let text = key.to_string();
        assert!(text.starts_with("ed25519:"));
        assert_eq!(text.len(), 8 + 43);
        assert_eq!(text.parse::<PublicKey>().unwrap(), key);
    }

    #[test]
    fn rejects_missing_prefix() {
        assert_eq!(
            "AAAA".parse::<PublicKey>(),
            Err(ParseKeyError::MissingPrefix)
        );
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(
            "ed25519:AAAA".parse::<PublicKey>(),
            Err(ParseKeyError::Length(3))
        );
    }

    #[test]
    fn rejects_standard_alphabet_and_padding() {
        let plus = format!("ed25519:{}", "+".repeat(43));
        assert_eq!(plus.parse::<PublicKey>(), Err(ParseKeyError::Base64));
        let padded = format!("{}=", PublicKey::from_bytes([1u8; 32]));
        assert_eq!(padded.parse::<PublicKey>(), Err(ParseKeyError::Base64));
    }

    #[test]
    fn short_is_eight_chars_of_the_encoding() {
        let key = PublicKey::from_bytes([0xAB; 32]);
        let text = key.to_string();
        assert_eq!(key.short(), &text[8..16]);
    }

    #[test]
    fn serde_uses_text_form() {
        let key = PublicKey::from_bytes([1u8; 32]);
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, format!("\"{key}\""));
        assert_eq!(serde_json::from_str::<PublicKey>(&json).unwrap(), key);
    }

    proptest! {
        #[test]
        fn any_key_round_trips(bytes in any::<[u8; 32]>()) {
            let key = PublicKey::from_bytes(bytes);
            prop_assert_eq!(key.to_string().parse::<PublicKey>().unwrap(), key);
        }
    }
}
