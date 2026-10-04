mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use common::{
    client_config_over, expect_bound, server_config, start_client, start_server, udp_recv,
};
use hawse_core::config::{Prefer, ServerConfig};
use hawse_core::identity::Identity;
use hawse_proto::proxy;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Hands over every datagram it receives, and answers each with `answer`.
async fn proxied_udp_service() -> (SocketAddr, mpsc::Receiver<Vec<u8>>) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let (seen, received) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((len, peer)) = socket.recv_from(&mut buf).await {
            let _ = seen.send(buf[..len].to_vec()).await;
            let _ = socket.send_to(b"answer", peer).await;
        }
    });
    (addr, received)
}

/// Asks up to five times: UDP may lose a packet without the tunnel being wrong.
async fn ask(visitor: &UdpSocket, to: SocketAddr, payload: &[u8]) -> Vec<u8> {
    for _ in 0..5 {
        visitor.send_to(payload, to).await.unwrap();
        if let Some(reply) = udp_recv(visitor).await {
            return reply;
        }
    }
    panic!("no reply from {to} in five tries");
}

struct Case {
    prefer: Prefer,
    /// The address the visitor sends from and to.
    visitor: IpAddr,
    payload: usize,
    /// The server's `bind`; `None` leaves the default wildcard.
    bind: Option<IpAddr>,
}

/// Returns the header the service read, for the caller to look at its form.
async fn the_header_names_the_udp_visitor(case: Case) -> Vec<u8> {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let mut cfg: ServerConfig = server_config(&[("test", client_id.public_key(), &[])]);
    if let Some(bind) = case.bind {
        cfg.bind = bind;
    }
    let server = start_server(&cfg, &server_id);
    let (local, mut seen) = proxied_udp_service().await;
    let mut cfg = client_config_over(
        server.addr,
        server.key,
        &[("dns", &local.to_string(), "any/udp")],
        case.prefer,
    );
    cfg.expose.get_mut("dns").unwrap().proxy_protocol = true;
    let mut client = start_client(cfg, client_id);
    let port = expect_bound(&mut client.events, "dns").await.number;

    let visitor = UdpSocket::bind((case.visitor, 0)).await.unwrap();
    let payload: Vec<u8> = (0..case.payload)
        .map(|n| u8::try_from(n % 251).unwrap())
        .collect();
    let public = SocketAddr::new(case.visitor, port);
    // Twice, because the header goes in front of every datagram and not only a session's first.
    for _ in 0..2 {
        let reply = ask(&visitor, public, &payload).await;
        assert_eq!(
            reply, b"answer",
            "the reply must cross back as the service sent it"
        );
    }

    // A wildcard bind names no address, so the header's destination is the one the client dialed.
    let destination = SocketAddr::new(case.bind.unwrap_or(server.addr.ip()), port);
    let header = proxy::v2_udp(visitor.local_addr().unwrap(), destination);
    let mut expected = header.clone();
    expected.extend_from_slice(&payload);
    let mut datagrams = 0;
    while let Ok(datagram) = seen.try_recv() {
        assert_eq!(datagram, expected);
        datagrams += 1;
    }
    assert!(datagrams >= 2, "the service saw {datagrams} datagrams");

    client.cancel.cancel();
    server.cancel.cancel();
    header
}

/// `bind` stays at its default `::`, so this visitor reaches the server as `::ffff:127.0.0.1`
/// and the header still has to take the IPv4 form.
#[tokio::test]
async fn a_proxied_udp_service_reads_an_ipv4_visitor_in_front_of_each_datagram() {
    let header = the_header_names_the_udp_visitor(Case {
        prefer: Prefer::Quic,
        visitor: Ipv4Addr::LOCALHOST.into(),
        payload: 5,
        bind: None,
    })
    .await;
    assert_eq!(header.len(), 28);
    assert_eq!(header[13], 0x12, "UDP over IPv4");
}

#[tokio::test]
async fn a_proxied_udp_service_reads_an_ipv6_visitor_in_front_of_each_datagram() {
    let header = the_header_names_the_udp_visitor(Case {
        prefer: Prefer::Quic,
        visitor: Ipv6Addr::LOCALHOST.into(),
        payload: 5,
        bind: None,
    })
    .await;
    assert_eq!(header.len(), 52);
    assert_eq!(header[13], 0x22, "UDP over IPv6");
}

/// The dialed address is `127.0.0.1` here, so a destination of `::1` can only have come from the
/// address the server said it bound.
#[tokio::test]
async fn the_header_names_the_address_the_server_bound() {
    the_header_names_the_udp_visitor(Case {
        prefer: Prefer::Quic,
        visitor: Ipv6Addr::LOCALHOST.into(),
        payload: 5,
        bind: Some(Ipv6Addr::LOCALHOST.into()),
    })
    .await;
}

#[tokio::test]
async fn a_payload_no_datagram_holds_arrives_behind_its_header() {
    the_header_names_the_udp_visitor(Case {
        prefer: Prefer::Quic,
        visitor: Ipv4Addr::LOCALHOST.into(),
        payload: 4000,
        bind: None,
    })
    .await;
}

#[tokio::test]
async fn a_proxied_udp_service_works_over_the_tcp_fallback() {
    the_header_names_the_udp_visitor(Case {
        prefer: Prefer::Tcp,
        visitor: Ipv4Addr::LOCALHOST.into(),
        payload: 5,
        bind: None,
    })
    .await;
}
