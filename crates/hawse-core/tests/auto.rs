mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{
    client_config, echo_server, expect_bound, next_event, server_config, start, start_server,
};
use hawse_core::client::{Client, ClientError, DisconnectCause, Event};
use hawse_core::config::Prefer;
use hawse_core::identity::Identity;
use hawse_core::transport::TransportKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Short enough to keep the tests quick, long enough that a loopback QUIC handshake never loses.
const AFTER: Duration = Duration::from_millis(300);

fn ids() -> (Identity, Identity) {
    (Identity::generate().unwrap(), Identity::generate().unwrap())
}

/// A network that blocks QUIC, in front of `server`: TCP is relayed to it, and UDP on the same
/// port is swallowed. The socket is returned so it stays bound, and is never read.
async fn udp_blocking_relay(server: SocketAddr) -> (SocketAddr, UdpSocket) {
    loop {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        // The same number on both protocols, as a server's listen port is.
        let Ok(udp) = UdpSocket::bind(addr).await else {
            continue;
        };
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = tcp.accept().await {
                tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(server).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        });
        return (addr, udp);
    }
}

#[tokio::test]
async fn auto_falls_back_to_tcp_when_quic_does_not_answer() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let (relay, _swallowed) = udp_blocking_relay(server.addr).await;
    let echo = echo_server().await.to_string();
    let cfg = client_config(relay, server.key, &[("echo", &echo, "any")]);
    assert_eq!(cfg.transport.prefer, Prefer::Auto);
    let mut client = start(Client::new(cfg, client_id).with_fallback_after(AFTER));

    assert!(
        matches!(
            next_event(&mut client.events).await,
            Event::Connected {
                transport: TransportKind::Tcp,
                ..
            }
        ),
        "auto did not land on the fallback"
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

#[tokio::test]
async fn auto_reports_both_failures_when_the_server_has_no_fallback() {
    let (server_id, client_id) = ids();
    let mut cfg = server_config(&[("test", client_id.public_key(), &[])]);
    cfg.transport.tcp_fallback = false;
    let server = start_server(&cfg, &server_id);
    let (relay, _swallowed) = udp_blocking_relay(server.addr).await;
    let mut cfg = client_config(relay, server.key, &[]);
    // QUIC meets silence, so its dial ends only at the idle timeout.
    cfg.transport.idle_timeout = Duration::from_secs(1);
    let client = start(Client::new(cfg, client_id).with_fallback_after(AFTER));

    let result = tokio::time::timeout(Duration::from_secs(10), client.task)
        .await
        .expect("both dials end within 10 s")
        .unwrap();
    assert!(
        matches!(result, Err(ClientError::NoTransport { .. })),
        "{result:?}"
    );
    server.cancel.cancel();
}

/// With one failure allowed a minute, a fallback that counted against the client would lock the
/// second connect out.
#[tokio::test]
async fn falling_back_twice_never_counts_against_the_client() {
    let (server_id, client_id) = ids();
    let mut cfg = server_config(&[("test", client_id.public_key(), &[])]);
    cfg.limits.auth_failures_per_minute = 1;
    let server = start_server(&cfg, &server_id);
    let (relay, _swallowed) = udp_blocking_relay(server.addr).await;
    let pem = client_id.to_pem();
    for _ in 0..2 {
        let cfg = client_config(relay, server.key, &[]);
        let identity = Identity::from_pem(&pem).unwrap();
        let mut client = start(Client::new(cfg, identity).with_fallback_after(AFTER));
        assert!(matches!(
            next_event(&mut client.events).await,
            Event::Connected {
                transport: TransportKind::Tcp,
                ..
            }
        ));
        client.cancel.cancel();
        assert!(client.task.await.unwrap().is_ok());
    }
    server.cancel.cancel();
}

/// Nothing here speaks hawse: UDP meets nothing and the TCP listener never answers TLS. `auto`
/// must still try the fallback, and report a failure worth retrying when both dials end.
#[tokio::test]
async fn auto_dials_the_fallback_and_retries_when_both_dials_fail() {
    let (server_id, client_id) = ids();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = client_config(listener.local_addr().unwrap(), server_id.public_key(), &[]);
    cfg.transport.idle_timeout = Duration::from_secs(1);

    let (tx, mut events) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let client = Client::new(cfg, client_id).with_fallback_after(AFTER);
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move { client.run(cancel, tx).await }
    });

    tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("auto never dialed the TCP fallback")
        .unwrap();
    let disconnect = tokio::time::timeout(Duration::from_secs(20), events.recv())
        .await
        .expect("a disconnect within 20 s");
    match disconnect {
        Some(Event::Disconnected {
            cause: DisconnectCause::Transport(reason),
            retry_in: Some(_),
        }) => assert!(reason.contains("TCP fallback"), "{reason}"),
        other => panic!("expected a retryable disconnect, got {other:?}"),
    }

    cancel.cancel();
    task.await.unwrap();
}
