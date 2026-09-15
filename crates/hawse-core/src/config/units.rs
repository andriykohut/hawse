use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub u64);

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a size like 512, 16KiB or 8MiB")]
pub struct ParseSizeError(String);

const UNITS: [(&str, u64); 4] = [
    ("GiB", 1 << 30),
    ("MiB", 1 << 20),
    ("KiB", 1 << 10),
    ("B", 1),
];

impl FromStr for ByteSize {
    type Err = ParseSizeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseSizeError(s.to_owned());
        let text = s.trim();
        let digits = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(digits);
        let n: u64 = number.parse().map_err(|_| err())?;
        let multiplier = match unit.trim() {
            "" => 1,
            unit => UNITS
                .iter()
                .find(|(name, _)| *name == unit)
                .map(|(_, m)| *m)
                .ok_or_else(err)?,
        };
        n.checked_mul(multiplier).map(ByteSize).ok_or_else(err)
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, m) = UNITS
            .iter()
            .find(|(_, m)| self.0.is_multiple_of(*m))
            .copied()
            .unwrap_or(("B", 1));
        write!(f, "{}{name}", self.0 / m)
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Number(n) => Ok(ByteSize(n)),
            Raw::Text(t) => t.parse().map_err(serde::de::Error::custom),
        }
    }
}

pub mod duration {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&humantime::format_duration(*d).to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let text = String::deserialize(d)?;
        humantime::parse_duration(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!("16KiB".parse::<ByteSize>().unwrap(), ByteSize(16 * 1024));
        assert_eq!("8MiB".parse::<ByteSize>().unwrap(), ByteSize(8 << 20));
        assert_eq!("1GiB".parse::<ByteSize>().unwrap(), ByteSize(1 << 30));
        assert_eq!("512".parse::<ByteSize>().unwrap(), ByteSize(512));
        assert_eq!("512B".parse::<ByteSize>().unwrap(), ByteSize(512));
        assert!("8MB".parse::<ByteSize>().is_err());
        assert!("MiB".parse::<ByteSize>().is_err());
    }

    #[test]
    fn displays_the_largest_exact_unit() {
        assert_eq!(ByteSize(16 * 1024).to_string(), "16KiB");
        assert_eq!(ByteSize(3 << 20).to_string(), "3MiB");
        assert_eq!(ByteSize(1500).to_string(), "1500B");
    }

    #[test]
    fn size_serde_accepts_string_or_integer() {
        #[derive(serde::Deserialize)]
        struct T {
            a: ByteSize,
            b: ByteSize,
        }
        let t: T = toml::from_str("a = \"2MiB\"\nb = 4096").unwrap();
        assert_eq!(t.a, ByteSize(2 << 20));
        assert_eq!(t.b, ByteSize(4096));
    }

    #[test]
    fn duration_serde_uses_humantime() {
        #[derive(serde::Deserialize)]
        struct T {
            #[serde(with = "duration")]
            d: std::time::Duration,
        }
        let t: T = toml::from_str("d = \"1m 30s\"").unwrap();
        assert_eq!(t.d, std::time::Duration::from_secs(90));
        assert!(toml::from_str::<T>("d = \"soon\"").is_err());
    }
}
