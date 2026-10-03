use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use bytes::Bytes;

use crate::frame::{FrameError, MAX_FRAME};
use crate::msg::DatagramHeader;

/// A `u16` and a `u32` as postcard varints.
const HEADER_MAX: usize = 3 + 5;

const IPV4: u8 = 0x04;
const IPV6: u8 = 0x06;

/// A postcard `DatagramHeader`, then the payload untouched. The same bytes travel as a QUIC
/// datagram or as the body of a frame on a bulk stream, so `decode` serves both. `encode_from` is
/// the same with the visitor's address after the header.
pub fn encode(header: DatagramHeader, payload: &[u8]) -> Result<Bytes, FrameError> {
    let mut buf = [0u8; HEADER_MAX];
    let head = postcard::to_slice(&header, &mut buf).map_err(FrameError::Encode)?;
    let len = head.len() + payload.len();
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut packet = Vec::with_capacity(len);
    packet.extend_from_slice(head);
    packet.extend_from_slice(payload);
    Ok(Bytes::from(packet))
}

pub fn decode(packet: &[u8]) -> Result<(DatagramHeader, &[u8]), FrameError> {
    postcard::take_from_bytes(packet).map_err(FrameError::Decode)
}

/// `encode`, with the visitor's address between the header and the payload: the layout of a packet
/// from server to client on a service bound with `proxy_protocol`. Both ends know from the bind
/// which layout a service uses, so no packet says. The address goes out canonical, so an IPv4
/// visitor seen as `::ffff:a.b.c.d` costs 7 bytes and not 19.
pub fn encode_from(
    header: DatagramHeader,
    visitor: SocketAddr,
    payload: &[u8],
) -> Result<Bytes, FrameError> {
    let mut buf = [0u8; HEADER_MAX];
    let head = postcard::to_slice(&header, &mut buf).map_err(FrameError::Encode)?;
    let ip = visitor.ip().to_canonical();
    let address = if ip.is_ipv4() { 4 } else { 16 };
    let len = head.len() + 1 + address + 2 + payload.len();
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut packet = Vec::with_capacity(len);
    packet.extend_from_slice(head);
    match ip {
        IpAddr::V4(ip) => {
            packet.push(IPV4);
            packet.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            packet.push(IPV6);
            packet.extend_from_slice(&ip.octets());
        }
    }
    packet.extend_from_slice(&visitor.port().to_be_bytes());
    packet.extend_from_slice(payload);
    Ok(Bytes::from(packet))
}

pub fn decode_from(packet: &[u8]) -> Result<(DatagramHeader, SocketAddr, &[u8]), FrameError> {
    let (header, rest) = decode(packet)?;
    let (ip, rest): (IpAddr, &[u8]) = match rest {
        [IPV4, a, b, c, d, rest @ ..] => (Ipv4Addr::new(*a, *b, *c, *d).into(), rest),
        [IPV6, rest @ ..] => {
            let (ip, rest) = rest.split_first_chunk::<16>().ok_or(FrameError::Visitor)?;
            (Ipv6Addr::from(*ip).into(), rest)
        }
        _ => return Err(FrameError::Visitor),
    };
    let [hi, lo, payload @ ..] = rest else {
        return Err(FrameError::Visitor);
    };
    let visitor = SocketAddr::new(ip, u16::from_be_bytes([*hi, *lo]));
    Ok((header, visitor, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const WIDEST: DatagramHeader = DatagramHeader {
        service_id: u16::MAX,
        session: u32::MAX,
    };

    #[test]
    fn the_largest_ipv6_udp_payload_fits_a_frame() {
        let packet = encode(WIDEST, &vec![0u8; 65527]).unwrap();
        assert!(packet.len() <= MAX_FRAME);
    }

    #[test]
    fn a_packet_past_the_frame_limit_is_refused() {
        assert!(matches!(
            encode(WIDEST, &vec![0u8; MAX_FRAME]),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn small_ids_take_a_byte_each_and_the_payload_follows_untouched() {
        let header = DatagramHeader {
            service_id: 7,
            session: 9,
        };
        assert_eq!(&encode(header, b"hi").unwrap()[..], [7, 9, b'h', b'i']);
    }

    /// postcard varints are little-endian base 128 with the high bit set while more follows:
    /// 300 is `0xAC 0x02` and 70000 is `0xF0 0xA2 0x04`.
    #[test]
    fn ids_past_127_spread_over_several_varint_bytes() {
        let header = DatagramHeader {
            service_id: 300,
            session: 70000,
        };
        assert_eq!(
            &encode(header, b"x").unwrap()[..],
            [0xAC, 0x02, 0xF0, 0xA2, 0x04, b'x']
        );
    }

    #[test]
    fn a_truncated_header_does_not_decode() {
        assert!(decode(&[0xff]).is_err());
    }

    proptest! {
        #[test]
        fn packets_round_trip(
            service_id: u16,
            session: u32,
            payload in proptest::collection::vec(any::<u8>(), 0..2048),
        ) {
            let header = DatagramHeader { service_id, session };
            let packet = encode(header, &payload).unwrap();
            let (decoded, rest) = decode(&packet).unwrap();
            prop_assert_eq!(decoded, header);
            prop_assert_eq!(rest, &payload[..]);
        }
    }

    #[test]
    fn an_ipv4_visitor_takes_seven_bytes_after_the_header() {
        let header = DatagramHeader {
            service_id: 7,
            session: 9,
        };
        let packet = encode_from(header, "192.0.2.1:53".parse().unwrap(), b"hi").unwrap();
        assert_eq!(
            &packet[..],
            [7, 9, 0x04, 192, 0, 2, 1, 0x00, 0x35, b'h', b'i']
        );
    }

    #[test]
    fn an_ipv6_visitor_takes_nineteen() {
        let header = DatagramHeader {
            service_id: 7,
            session: 9,
        };
        let packet = encode_from(header, "[2001:db8::1]:53".parse().unwrap(), b"hi").unwrap();
        let mut expected = vec![7, 9, 0x06];
        expected.extend_from_slice(&[
            0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ]);
        expected.extend_from_slice(&[0x00, 0x35, b'h', b'i']);
        assert_eq!(&packet[..], expected);
    }

    #[test]
    fn a_mapped_visitor_is_sent_as_ipv4() {
        let header = DatagramHeader {
            service_id: 7,
            session: 9,
        };
        let mapped = encode_from(header, "[::ffff:192.0.2.1]:53".parse().unwrap(), b"hi").unwrap();
        let plain = encode_from(header, "192.0.2.1:53".parse().unwrap(), b"hi").unwrap();
        assert_eq!(mapped, plain);
    }

    #[test]
    fn a_packet_without_a_whole_address_is_refused() {
        // The header alone, an unknown family, and each family cut short.
        for packet in [
            &[7, 9][..],
            &[7, 9, 0x05, 1, 2, 3, 4, 0, 53],
            &[7, 9, 0x04, 192, 0, 2],
            &[7, 9, 0x04, 192, 0, 2, 1, 0],
            &[
                7, 9, 0x06, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 0,
            ],
        ] {
            assert!(
                matches!(decode_from(packet), Err(FrameError::Visitor)),
                "{packet:?}"
            );
        }
    }

    #[test]
    fn an_address_that_pushes_a_packet_past_the_frame_limit_is_refused() {
        let payload = vec![0u8; MAX_FRAME - 2 - 7 + 1];
        assert!(matches!(
            encode_from(
                DatagramHeader {
                    service_id: 7,
                    session: 9
                },
                "192.0.2.1:53".parse().unwrap(),
                &payload
            ),
            Err(FrameError::TooLarge(_))
        ));
    }

    proptest! {
        #[test]
        fn packets_with_a_visitor_round_trip(
            service_id: u16,
            session: u32,
            v4 in any::<[u8; 4]>(),
            v6 in any::<[u8; 16]>(),
            port: u16,
            payload in proptest::collection::vec(any::<u8>(), 0..2048),
        ) {
            let header = DatagramHeader { service_id, session };
            for ip in [std::net::IpAddr::from(v4), std::net::IpAddr::from(v6)] {
                let visitor = std::net::SocketAddr::new(ip, port);
                let packet = encode_from(header, visitor, &payload).unwrap();
                let (decoded, from, rest) = decode_from(&packet).unwrap();
                prop_assert_eq!(decoded, header);
                prop_assert_eq!(from, std::net::SocketAddr::new(ip.to_canonical(), port));
                prop_assert_eq!(rest, &payload[..]);
            }
        }
    }
}
