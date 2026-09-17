use bytes::Bytes;

use crate::frame::{FrameError, MAX_FRAME};
use crate::msg::DatagramHeader;

/// A `u16` and a `u32` as postcard varints.
const HEADER_MAX: usize = 3 + 5;

/// A postcard `DatagramHeader`, then the payload untouched. The same bytes travel as a QUIC
/// datagram or as the body of a frame on a bulk stream, so `decode` serves both.
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
}
