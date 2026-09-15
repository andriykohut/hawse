use std::fmt;
use std::net::SocketAddr;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::key::PublicKey;
use crate::port::Kind;

pub const ALPN: &[u8] = b"hawse/1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello {
        name: Option<String>,
        agent: String,
    },
    Bind {
        service: String,
        kind: Kind,
        port: Option<u16>,
        allow: Vec<IpNet>,
        proxy_protocol: bool,
    },
    Unbind {
        service: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome {
        agent: String,
        client_name: String,
    },
    Denied {
        reason: DenyReason,
        key: PublicKey,
    },
    Bound {
        service: String,
        service_id: u16,
        port: u16,
    },
    BindFailed {
        service: String,
        reason: BindFailure,
    },
    Shutdown {
        reason: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    UnknownKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindFailure {
    NotGranted,
    InUse,
    BadPort,
}

impl fmt::Display for BindFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BindFailure::NotGranted => "port is not granted to this client",
            BindFailure::InUse => "port is already in use on the server",
            BindFailure::BadPort => "port cannot be bound",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHeader {
    pub service_id: u16,
    pub visitor: SocketAddr,
    pub listener: SocketAddr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatagramHeader {
    pub service_id: u16,
    pub session: u32,
}

/// Error codes for `SendStream::reset`, so the peer can tell why a stream was dropped.
pub mod reset {
    pub const UNKNOWN_SERVICE: u32 = 0x10;
    pub const LOCAL_REFUSED: u32 = 0x11;
    pub const ABORTED: u32 = 0x12;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{decode, encode};
    use crate::key::PublicKey;
    use proptest::prelude::*;

    fn arb_kind() -> impl Strategy<Value = Kind> {
        prop_oneof![Just(Kind::Tcp), Just(Kind::Udp)]
    }

    fn arb_ipnet() -> impl Strategy<Value = IpNet> {
        prop_oneof![
            (any::<[u8; 4]>(), 0u8..=32).prop_map(|(a, p)| IpNet::new(a.into(), p).unwrap()),
            (any::<[u8; 16]>(), 0u8..=128).prop_map(|(a, p)| IpNet::new(a.into(), p).unwrap()),
        ]
    }

    fn arb_client() -> impl Strategy<Value = ClientMessage> {
        prop_oneof![
            (proptest::option::of("[a-z0-9-]{1,32}"), "[ -~]{0,40}")
                .prop_map(|(name, agent)| ClientMessage::Hello { name, agent }),
            (
                "[a-z0-9-]{1,32}",
                arb_kind(),
                proptest::option::of(1u16..),
                proptest::collection::vec(arb_ipnet(), 0..4),
                any::<bool>()
            )
                .prop_map(|(service, kind, port, allow, proxy_protocol)| {
                    ClientMessage::Bind {
                        service,
                        kind,
                        port,
                        allow,
                        proxy_protocol,
                    }
                }),
            "[a-z0-9-]{1,32}".prop_map(|service| ClientMessage::Unbind { service }),
            any::<u64>().prop_map(|nonce| ClientMessage::Ping { nonce }),
            any::<u64>().prop_map(|nonce| ClientMessage::Pong { nonce }),
        ]
    }

    fn arb_server() -> impl Strategy<Value = ServerMessage> {
        prop_oneof![
            ("[ -~]{0,40}", "[a-z0-9-]{1,32}")
                .prop_map(|(agent, client_name)| ServerMessage::Welcome { agent, client_name }),
            any::<[u8; 32]>().prop_map(|k| ServerMessage::Denied {
                reason: DenyReason::UnknownKey,
                key: PublicKey::from_bytes(k)
            }),
            ("[a-z0-9-]{1,32}", any::<u16>(), 1u16..).prop_map(|(service, service_id, port)| {
                ServerMessage::Bound {
                    service,
                    service_id,
                    port,
                }
            }),
            (
                "[a-z0-9-]{1,32}",
                prop_oneof![
                    Just(BindFailure::NotGranted),
                    Just(BindFailure::InUse),
                    Just(BindFailure::BadPort)
                ]
            )
                .prop_map(|(service, reason)| ServerMessage::BindFailed { service, reason }),
            "[ -~]{0,40}".prop_map(|reason| ServerMessage::Shutdown { reason }),
            any::<u64>().prop_map(|nonce| ServerMessage::Ping { nonce }),
            any::<u64>().prop_map(|nonce| ServerMessage::Pong { nonce }),
        ]
    }

    proptest! {
        #[test]
        fn client_messages_round_trip(m in arb_client()) {
            let bytes = encode(&m).unwrap();
            prop_assert_eq!(decode::<ClientMessage>(&bytes).unwrap(), m);
        }

        #[test]
        fn server_messages_round_trip(m in arb_server()) {
            let bytes = encode(&m).unwrap();
            prop_assert_eq!(decode::<ServerMessage>(&bytes).unwrap(), m);
        }

        #[test]
        fn stream_headers_round_trip(id in any::<u16>(), v4 in any::<[u8; 4]>(), v6 in any::<[u8; 16]>(), p in any::<u16>()) {
            let h = StreamHeader { service_id: id, visitor: (v4, p).into(), listener: (v6, p).into() };
            let bytes = encode(&h).unwrap();
            prop_assert_eq!(decode::<StreamHeader>(&bytes).unwrap(), h);
        }
    }

    #[test]
    fn datagram_header_is_small() {
        let h = DatagramHeader {
            service_id: 1,
            session: 7,
        };
        assert!(encode(&h).unwrap().len() <= 6);
    }

    #[test]
    fn alpn_is_versioned() {
        assert_eq!(ALPN, b"hawse/1");
    }
}
