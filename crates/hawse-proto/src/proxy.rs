use std::net::{IpAddr, Ipv6Addr, SocketAddr};

const SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];
const VERSION_2_PROXY: u8 = 0x21;
const TCP_OVER_IPV4: u8 = 0x11;
const TCP_OVER_IPV6: u8 = 0x21;

/// A PROXY protocol v2 header for a TCP connection from `src` to `dst`, with no TLVs.
pub fn v2_tcp(src: SocketAddr, dst: SocketAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(SIGNATURE.len() + 4 + 36);
    out.extend_from_slice(&SIGNATURE);
    out.push(VERSION_2_PROXY);
    match (src.ip().to_canonical(), dst.ip().to_canonical()) {
        (IpAddr::V4(from), IpAddr::V4(to)) => {
            out.push(TCP_OVER_IPV4);
            out.extend_from_slice(&12u16.to_be_bytes());
            out.extend_from_slice(&from.octets());
            out.extend_from_slice(&to.octets());
        }
        // One socket cannot produce a mixed pair, but the header stays valid if one appears.
        (from, to) => {
            out.push(TCP_OVER_IPV6);
            out.extend_from_slice(&36u16.to_be_bytes());
            out.extend_from_slice(&ipv6(from).octets());
            out.extend_from_slice(&ipv6(to).octets());
        }
    }
    out.extend_from_slice(&src.port().to_be_bytes());
    out.extend_from_slice(&dst.port().to_be_bytes());
    out
}

fn ipv6(ip: IpAddr) -> Ipv6Addr {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(rest: &[u8]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        out.extend_from_slice(rest);
        out
    }

    const IPV4_PAIR: [u8; 16] = [
        0x21, 0x11, 0x00, 0x0C, // PROXY, TCP over IPv4, 12 address bytes
        192, 0, 2, 1, // source
        198, 51, 100, 7, // destination
        0xDC, 0x04, // 56324
        0x01, 0xBB, // 443
    ];

    #[test]
    fn an_ipv4_pair_is_twelve_address_bytes() {
        let bytes = v2_tcp(
            "192.0.2.1:56324".parse().unwrap(),
            "198.51.100.7:443".parse().unwrap(),
        );
        assert_eq!(bytes, header(&IPV4_PAIR));
    }

    #[test]
    fn mapped_addresses_are_sent_as_ipv4() {
        let bytes = v2_tcp(
            "[::ffff:192.0.2.1]:56324".parse().unwrap(),
            "[::ffff:198.51.100.7]:443".parse().unwrap(),
        );
        assert_eq!(bytes, header(&IPV4_PAIR));
    }

    #[test]
    fn an_ipv6_pair_is_thirty_six_address_bytes() {
        let bytes = v2_tcp(
            "[2001:db8::1]:40000".parse().unwrap(),
            "[2001:db8::2]:8443".parse().unwrap(),
        );
        let mut rest = vec![0x21, 0x21, 0x00, 0x24];
        rest.extend_from_slice(&[
            0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ]);
        rest.extend_from_slice(&[
            0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
        ]);
        rest.extend_from_slice(&[0x9C, 0x40, 0x20, 0xFB]);
        assert_eq!(bytes, header(&rest));
    }

    #[test]
    fn mixed_families_fall_back_to_the_ipv6_form() {
        let bytes = v2_tcp(
            "192.0.2.1:56324".parse().unwrap(),
            "[2001:db8::2]:443".parse().unwrap(),
        );
        let mut rest = vec![0x21, 0x21, 0x00, 0x24];
        rest.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 192, 0, 2, 1]);
        rest.extend_from_slice(&[
            0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
        ]);
        rest.extend_from_slice(&[0xDC, 0x04, 0x01, 0xBB]);
        assert_eq!(bytes, header(&rest));
    }
}
