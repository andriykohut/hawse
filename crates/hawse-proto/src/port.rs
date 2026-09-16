use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Kind {
    Tcp,
    Udp,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Tcp => "tcp",
            Kind::Udp => "udp",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Port {
    pub number: u16,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
    pub kind: Kind,
}

/// A run of port numbers with no protocol, used for the dynamic pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortSpan {
    pub first: u16,
    pub last: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PortRequest {
    Any(Kind),
    Fixed(Port),
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParsePortError {
    #[error("port is empty")]
    Empty,
    #[error("`{0}` is not a port number between 1 and 65535")]
    Number(String),
    #[error("unknown protocol `{0}`, expected tcp or udp")]
    Protocol(String),
    #[error("range {0}-{1} runs backwards")]
    Reversed(u16, u16),
}

fn split_kind(s: &str) -> Result<(&str, Kind), ParsePortError> {
    match s.split_once('/') {
        None => Ok((s, Kind::Tcp)),
        Some((n, "tcp")) => Ok((n, Kind::Tcp)),
        Some((n, "udp")) => Ok((n, Kind::Udp)),
        Some((_, proto)) => Err(ParsePortError::Protocol(proto.to_owned())),
    }
}

fn parse_number(s: &str) -> Result<u16, ParsePortError> {
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(ParsePortError::Number(s.to_owned())),
        Ok(n) => Ok(n),
    }
}

fn parse_bounds(s: &str) -> Result<(u16, u16), ParsePortError> {
    if s.is_empty() {
        return Err(ParsePortError::Empty);
    }
    let (first, last) = if let Some((a, b)) = s.split_once('-') {
        (parse_number(a)?, parse_number(b)?)
    } else {
        let n = parse_number(s)?;
        (n, n)
    };
    if first > last {
        return Err(ParsePortError::Reversed(first, last));
    }
    Ok((first, last))
}

impl PortRange {
    pub fn contains(&self, port: Port) -> bool {
        port.kind == self.kind && (self.first..=self.last).contains(&port.number)
    }
}

impl PortSpan {
    pub fn iter(&self) -> impl Iterator<Item = u16> {
        self.first..=self.last
    }

    pub fn len(&self) -> usize {
        usize::from(self.last - self.first) + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn contains_number(&self, n: u16) -> bool {
        self.first <= n && n <= self.last
    }
}

impl PortRequest {
    pub const fn kind(self) -> Kind {
        match self {
            PortRequest::Any(kind) => kind,
            PortRequest::Fixed(port) => port.kind,
        }
    }
}

impl Default for PortRequest {
    fn default() -> Self {
        PortRequest::Any(Kind::Tcp)
    }
}

impl FromStr for Port {
    type Err = ParsePortError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParsePortError::Empty);
        }
        let (number, kind) = split_kind(s)?;
        Ok(Port {
            number: parse_number(number)?,
            kind,
        })
    }
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            Kind::Tcp => write!(f, "{}", self.number),
            Kind::Udp => write!(f, "{}/udp", self.number),
        }
    }
}

impl FromStr for PortRange {
    type Err = ParsePortError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (body, kind) = split_kind(s)?;
        let (first, last) = parse_bounds(body)?;
        Ok(PortRange { first, last, kind })
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.first == self.last {
            write!(f, "{}", self.first)?;
        } else {
            write!(f, "{}-{}", self.first, self.last)?;
        }
        if self.kind == Kind::Udp {
            f.write_str("/udp")?;
        }
        Ok(())
    }
}

impl FromStr for PortSpan {
    type Err = ParsePortError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((_, proto)) = s.split_once('/') {
            return Err(ParsePortError::Protocol(proto.to_owned()));
        }
        let (first, last) = parse_bounds(s)?;
        Ok(PortSpan { first, last })
    }
}

impl fmt::Display for PortSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

impl FromStr for PortRequest {
    type Err = ParsePortError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "any" | "any/tcp" => Ok(PortRequest::Any(Kind::Tcp)),
            "any/udp" => Ok(PortRequest::Any(Kind::Udp)),
            other if other.starts_with("any/") => {
                Err(ParsePortError::Protocol(other["any/".len()..].to_owned()))
            }
            other => other.parse().map(PortRequest::Fixed),
        }
    }
}

impl fmt::Display for PortRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortRequest::Any(Kind::Tcp) => f.write_str("any"),
            PortRequest::Any(Kind::Udp) => f.write_str("any/udp"),
            PortRequest::Fixed(port) => write!(f, "{port}"),
        }
    }
}

macro_rules! text_serde {
    ($t:ty) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                String::deserialize(d)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

text_serde!(PortRange);
text_serde!(PortSpan);

impl Serialize for PortRequest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            PortRequest::Fixed(Port {
                number,
                kind: Kind::Tcp,
            }) => s.serialize_u16(*number),
            other => s.serialize_str(&other.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for PortRequest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(u16),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Number(0) => Err(serde::de::Error::custom(ParsePortError::Number("0".into()))),
            Raw::Number(n) => Ok(PortRequest::Fixed(Port {
                number: n,
                kind: Kind::Tcp,
            })),
            Raw::Text(t) => t.parse().map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn tcp(n: u16) -> Port {
        Port {
            number: n,
            kind: Kind::Tcp,
        }
    }
    fn udp(n: u16) -> Port {
        Port {
            number: n,
            kind: Kind::Udp,
        }
    }

    #[test]
    fn parses_ports() {
        assert_eq!("443".parse::<Port>().unwrap(), tcp(443));
        assert_eq!("443/tcp".parse::<Port>().unwrap(), tcp(443));
        assert_eq!("51820/udp".parse::<Port>().unwrap(), udp(51820));
    }

    #[test]
    fn rejects_bad_ports() {
        assert_eq!("".parse::<Port>(), Err(ParsePortError::Empty));
        assert_eq!("0".parse::<Port>(), Err(ParsePortError::Number("0".into())));
        assert_eq!(
            "70000".parse::<Port>(),
            Err(ParsePortError::Number("70000".into()))
        );
        assert_eq!(
            "22/sctp".parse::<Port>(),
            Err(ParsePortError::Protocol("sctp".into()))
        );
    }

    #[test]
    fn displays_ports_like_docker() {
        assert_eq!(tcp(443).to_string(), "443");
        assert_eq!(udp(51820).to_string(), "51820/udp");
    }

    #[test]
    fn parses_ranges() {
        let r: PortRange = "8000-8100".parse().unwrap();
        assert_eq!(
            r,
            PortRange {
                first: 8000,
                last: 8100,
                kind: Kind::Tcp
            }
        );
        let r: PortRange = "8000-8100/udp".parse().unwrap();
        assert_eq!(r.kind, Kind::Udp);
        let r: PortRange = "443".parse().unwrap();
        assert_eq!((r.first, r.last), (443, 443));
        assert_eq!(
            "9000-8000".parse::<PortRange>(),
            Err(ParsePortError::Reversed(9000, 8000))
        );
    }

    #[test]
    fn range_contains_respects_kind() {
        let r: PortRange = "8000-8100".parse().unwrap();
        assert!(r.contains(tcp(8000)));
        assert!(r.contains(tcp(8100)));
        assert!(!r.contains(tcp(8101)));
        assert!(!r.contains(udp(8050)));
    }

    #[test]
    fn range_serde_is_text() {
        let r: PortRange = serde_json::from_str("\"1-2/udp\"").unwrap();
        assert_eq!(serde_json::to_string(&r).unwrap(), "\"1-2/udp\"");
    }

    #[test]
    fn span_parses_and_iterates() {
        let s: PortSpan = "40000-40002".parse().unwrap();
        assert_eq!(s.iter().collect::<Vec<u16>>(), vec![40000, 40001, 40002]);
        assert_eq!(
            "5".parse::<PortSpan>().unwrap(),
            PortSpan { first: 5, last: 5 }
        );
        assert_eq!(
            "2-1".parse::<PortSpan>(),
            Err(ParsePortError::Reversed(2, 1))
        );
        assert_eq!(
            "1-2/udp".parse::<PortSpan>(),
            Err(ParsePortError::Protocol("udp".into()))
        );
    }

    #[test]
    fn request_accepts_int_string_and_any() {
        assert_eq!(
            serde_json::from_str::<PortRequest>("443").unwrap(),
            PortRequest::Fixed(tcp(443))
        );
        assert_eq!(
            serde_json::from_str::<PortRequest>("\"53/udp\"").unwrap(),
            PortRequest::Fixed(udp(53))
        );
        assert_eq!(
            serde_json::from_str::<PortRequest>("\"any\"").unwrap(),
            PortRequest::Any(Kind::Tcp)
        );
        assert_eq!(
            serde_json::from_str::<PortRequest>("\"any/udp\"").unwrap(),
            PortRequest::Any(Kind::Udp)
        );
        assert_eq!(PortRequest::default(), PortRequest::Any(Kind::Tcp));
        assert!(serde_json::from_str::<PortRequest>("\"any/sctp\"").is_err());
    }

    proptest! {
        #[test]
        fn any_port_round_trips(n in 1u16..=u16::MAX, is_udp in any::<bool>()) {
            let p = Port { number: n, kind: if is_udp { Kind::Udp } else { Kind::Tcp } };
            prop_assert_eq!(p.to_string().parse::<Port>().unwrap(), p);
        }
    }
}
