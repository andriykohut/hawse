mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use common::{
    RunningServer, client_config, client_config_over, echo_server, expect_bound, next_event,
    server_config, start_client, start_server,
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
