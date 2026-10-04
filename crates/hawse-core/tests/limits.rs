mod common;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use common::{
    RunningClient, RunningServer, client_config, client_config_over, echo_server, expect_bound,
    next_event, server_config, start_client, start_server, udp_ask, udp_echo_server, udp_visitor,
};
use hawse_core::client::{ClientError, Event};
use hawse_core::config::{Prefer, ServerConfig};
use hawse_core::identity::Identity;
use hawse_core::transport::quic::QuicError;
use hawse_proto::key::PublicKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Listens on `[::]`, so one test reaches the server from two addresses: `127.0.0.1` and `::1`.
fn dual_stack(known: PublicKey, per_minute: u32) -> ServerConfig {
    let mut cfg = server_config(&[("test", known, &[])]);
    cfg.listen = "[::]:0".parse().unwrap();
    cfg.limits.auth_failures_per_minute = per_minute;
    cfg
}

fn at(server: &RunningServer, ip: impl Into<IpAddr>) -> SocketAddr {
    SocketAddr::new(ip.into(), server.addr.port())
}

/// Connects with a key the server does not know and waits for the denial.
async fn denied(to: SocketAddr, server_key: PublicKey, prefer: Prefer) {
    let mut stranger = start_client(
        client_config_over(to, server_key, &[], prefer),
        Identity::generate().unwrap(),
    );
    assert!(matches!(
        next_event(&mut stranger.events).await,
        Event::Denied { .. }
    ));
    assert!(matches!(
        stranger.task.await.unwrap(),
        Err(ClientError::Denied(_))
    ));
}

async fn an_address_past_its_limit_is_refused(prefer: Prefer) {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(&dual_stack(client_id.public_key(), 2), &server_id);
    let v4 = at(&server, Ipv4Addr::LOCALHOST);
    denied(v4, server.key, prefer).await;
    denied(v4, server.key, prefer).await;

    let refused = start_client(
        client_config_over(v4, server.key, &[], prefer),
        Identity::generate().unwrap(),
    );
    let result = tokio::time::timeout(Duration::from_secs(5), refused.task)
        .await
        .expect("refused within 5 s")
        .unwrap();
    if prefer == Prefer::Tcp {
        assert!(
            matches!(result, Err(ClientError::Transport(_))),
            "{result:?}"
        );
    } else {
        assert!(
            matches!(
                &result,
                Err(ClientError::Quic(QuicError::Connection(
                    quinn::ConnectionError::ConnectionClosed(close)
                ))) if close.error_code == quinn::TransportErrorCode::CONNECTION_REFUSED
            ),
            "{result:?}"
        );
    }

    let mut known = start_client(
        client_config_over(at(&server, Ipv6Addr::LOCALHOST), server.key, &[], prefer),
        client_id,
    );
    assert!(matches!(
        next_event(&mut known.events).await,
        Event::Connected { .. }
    ));
    known.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn an_address_past_its_limit_is_refused_over_quic() {
    an_address_past_its_limit_is_refused(Prefer::Quic).await;
}

#[tokio::test]
async fn an_address_past_its_limit_is_refused_over_tcp() {
    an_address_past_its_limit_is_refused(Prefer::Tcp).await;
}

#[tokio::test]
async fn a_known_client_reconnecting_never_counts_against_its_address() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(&dual_stack(client_id.public_key(), 1), &server_id);
    let pem = client_id.to_pem();
    for _ in 0..3 {
        let mut client = start_client(
            client_config(at(&server, Ipv4Addr::LOCALHOST), server.key, &[]),
            Identity::from_pem(&pem).unwrap(),
        );
        assert!(matches!(
            next_event(&mut client.events).await,
            Event::Connected { .. }
        ));
        client.cancel.cancel();
        assert!(client.task.await.unwrap().is_ok());
    }
    server.cancel.cancel();
}

#[tokio::test]
async fn refused_tcp_connections_give_back_their_handshake_permit() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(&dual_stack(client_id.public_key(), 1), &server_id);
    let v4 = at(&server, Ipv4Addr::LOCALHOST);
    denied(v4, server.key, Prefer::Tcp).await;

    // More refusals than the server has handshake permits: each has to hand its permit back.
    for _ in 0..300 {
        let mut socket = TcpStream::connect(v4).await.unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf))
            .await
            .expect("closed within 5 s");
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    }

    let mut known = start_client(
        client_config_over(
            at(&server, Ipv6Addr::LOCALHOST),
            server.key,
            &[],
            Prefer::Tcp,
        ),
        client_id,
    );
    assert!(matches!(
        next_event(&mut known.events).await,
        Event::Connected { .. }
    ));
    known.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_client_still_connects_with_quic_retry_on() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let mut cfg = server_config(&[("test", client_id.public_key(), &[])]);
    cfg.quic_retry = true;
    let server = start_server(&cfg, &server_id);
    let echo = echo_server().await.to_string();
    let mut client = start_client(
        client_config(server.addr, server.key, &[("echo", &echo, "any")]),
        client_id,
    );
    let port = expect_bound(&mut client.events, "echo").await;

    let mut visitor = TcpStream::connect(("127.0.0.1", port.number))
        .await
        .unwrap();
    visitor.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    visitor.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");

    client.cancel.cancel();
    server.cancel.cancel();
}

struct Capped {
    server: RunningServer,
    client: RunningClient,
    ports: Vec<u16>,
}

/// A server whose `streams_per_client` is `streams`, and a client with `exposes` bound on it;
/// `ports` in the same order.
async fn capped(prefer: Prefer, streams: u32, exposes: &[(&str, &str, &str)]) -> Capped {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let mut cfg = server_config(&[("test", client_id.public_key(), &[])]);
    cfg.limits.streams_per_client = streams;
    let server = start_server(&cfg, &server_id);
    let mut client = start_client(
        client_config_over(server.addr, server.key, exposes, prefer),
        client_id,
    );
    let mut ports = Vec::new();
    for (name, _, _) in exposes {
        ports.push(expect_bound(&mut client.events, name).await.number);
    }
    Capped {
        server,
        client,
        ports,
    }
}

impl Capped {
    fn close(self) {
        self.client.cancel.cancel();
        self.server.cancel.cancel();
    }
}

async fn echoes(visitor: &mut TcpStream) -> io::Result<()> {
    visitor.write_all(b"x").await?;
    let mut buf = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), visitor.read_exact(&mut buf))
        .await
        .expect("an echo within 5 s")?;
    assert_eq!(&buf, b"x");
    Ok(())
}

/// A visitor whose stream is up, the service behind the tunnel having answered it, or the error
/// that turned it away.
async fn visit(port: u16) -> io::Result<TcpStream> {
    let mut visitor = TcpStream::connect(("127.0.0.1", port)).await?;
    echoes(&mut visitor).await?;
    Ok(visitor)
}

async fn admitted(port: u16) -> TcpStream {
    visit(port).await.unwrap()
}

/// Only reads: a socket reports a reset once, to whichever call meets it first. That can be the
/// connect itself, when the server's reset is back before it returns.
async fn expect_reset(port: u16) {
    let refused = async {
        let mut visitor = TcpStream::connect(("127.0.0.1", port)).await?;
        let mut buf = [0u8; 1];
        visitor.read(&mut buf).await
    };
    let read = tokio::time::timeout(Duration::from_secs(5), refused)
        .await
        .expect("refused within 5 s");
    assert_eq!(
        read.as_ref().map_err(io::Error::kind),
        Err(io::ErrorKind::ConnectionReset),
        "{read:?}"
    );
}

/// Visits again and again until the session has a stream to give. A port nothing listens on is
/// the session gone, not a visitor turned away.
async fn admitted_in_time(port: u16) -> TcpStream {
    loop {
        match visit(port).await {
            Ok(visitor) => break visitor,
            Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
                panic!("nothing listens on the port: {err}")
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
        }
    }
}

async fn a_visitor_past_the_limit_is_reset_and_the_session_lives_on(prefer: Prefer) {
    let echo = echo_server().await.to_string();
    // Four streams: the control stream and three visitors.
    let tunnel = capped(prefer, 4, &[("echo", &echo, "any")]).await;
    let port = tunnel.ports[0];
    let mut visitors = Vec::new();
    for _ in 0..3 {
        visitors.push(admitted(port).await);
    }

    expect_reset(port).await;

    for visitor in &mut visitors {
        echoes(visitor).await.unwrap();
    }
    tunnel.close();
}

#[tokio::test]
async fn a_visitor_past_the_limit_is_reset_and_the_session_lives_on_over_quic() {
    a_visitor_past_the_limit_is_reset_and_the_session_lives_on(Prefer::Quic).await;
}

#[tokio::test]
async fn a_visitor_past_the_limit_is_reset_and_the_session_lives_on_over_tcp() {
    a_visitor_past_the_limit_is_reset_and_the_session_lives_on(Prefer::Tcp).await;
}

async fn a_visitor_that_leaves_makes_room_for_the_next(prefer: Prefer) {
    let echo = echo_server().await.to_string();
    // Two streams: the control stream and one visitor.
    let tunnel = capped(prefer, 2, &[("echo", &echo, "any")]).await;
    let port = tunnel.ports[0];
    let first = admitted(port).await;
    expect_reset(port).await;

    drop(first);

    // The room comes back when the first visitor's pump ends, a moment after its socket closes.
    tokio::time::timeout(Duration::from_secs(5), admitted_in_time(port))
        .await
        .expect("a visitor admitted within 5 s of the first leaving");
    tunnel.close();
}

#[tokio::test]
async fn a_visitor_that_leaves_makes_room_for_the_next_over_quic() {
    a_visitor_that_leaves_makes_room_for_the_next(Prefer::Quic).await;
}

#[tokio::test]
async fn a_visitor_that_leaves_makes_room_for_the_next_over_tcp() {
    a_visitor_that_leaves_makes_room_for_the_next(Prefer::Tcp).await;
}

async fn a_udp_service_takes_its_bulk_stream_from_the_limit(prefer: Prefer) {
    let echo = echo_server().await.to_string();
    let udp_echo = udp_echo_server().await.to_string();
    // Three streams: the control stream, the UDP service's bulk stream and one visitor.
    let tunnel = capped(
        prefer,
        3,
        &[("echo", &echo, "any"), ("udp", &udp_echo, "any/udp")],
    )
    .await;
    let (port, udp_port) = (tunnel.ports[0], tunnel.ports[1]);
    // Too large for a datagram, so its echo shows the bulk stream is open on either transport.
    let large = vec![7u8; 4000];
    assert_eq!(udp_ask(&udp_visitor().await, udp_port, &large).await, large);
    let mut first = admitted(port).await;

    expect_reset(port).await;

    echoes(&mut first).await.unwrap();
    tunnel.close();
}

#[tokio::test]
async fn a_udp_service_takes_its_bulk_stream_from_the_limit_over_quic() {
    a_udp_service_takes_its_bulk_stream_from_the_limit(Prefer::Quic).await;
}

#[tokio::test]
async fn a_udp_service_takes_its_bulk_stream_from_the_limit_over_tcp() {
    a_udp_service_takes_its_bulk_stream_from_the_limit(Prefer::Tcp).await;
}

/// Every visitor leaves at once and as many arrive at once, over and over: the moment a stream is
/// given back and taken again is where the session's count and the transport's can disagree.
async fn visitors_replaced_at_the_limit_keep_the_session(prefer: Prefer) {
    let echo = echo_server().await.to_string();
    // Five streams: the control stream and four visitors.
    let tunnel = capped(prefer, 5, &[("echo", &echo, "any")]).await;
    let port = tunnel.ports[0];
    // Each visitor leaves two sockets in TIME_WAIT, and a run that left thousands would use up
    // the ports the rest of the suite draws on.
    for round in 0..20 {
        let arrivals: Vec<_> = (0..4)
            .map(|_| tokio::spawn(admitted_in_time(port)))
            .collect();
        let mut visitors = Vec::new();
        for arrival in arrivals {
            let visitor = tokio::time::timeout(Duration::from_secs(5), arrival)
                .await
                .unwrap_or_else(|_| panic!("round {round}: four visitors admitted within 5 s"))
                .unwrap_or_else(|err| panic!("round {round}: {err}"));
            visitors.push(visitor);
        }
        drop(visitors);
    }
    tunnel.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn visitors_replaced_at_the_limit_keep_the_session_over_quic() {
    visitors_replaced_at_the_limit_keep_the_session(Prefer::Quic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn visitors_replaced_at_the_limit_keep_the_session_over_tcp() {
    visitors_replaced_at_the_limit_keep_the_session(Prefer::Tcp).await;
}
