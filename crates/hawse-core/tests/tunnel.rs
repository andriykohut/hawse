mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use common::{
    DYNAMIC_PORTS, RunningClient, RunningServer, client_config, client_config_over, echo_server,
    expect_bound, free_port_outside_pool, next_event, server_config, start_client, start_server,
};
use hawse_core::client::{Client, ClientError, DisconnectCause, Event};
use hawse_core::config::Prefer;
use hawse_core::identity::Identity;
use hawse_core::transport::TransportKind;
use hawse_core::transport::quic::QuicError;
use hawse_proto::msg::BindFailure;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

fn ids() -> (Identity, Identity) {
    (Identity::generate().unwrap(), Identity::generate().unwrap())
}

#[tokio::test]
async fn a_loopback_bind_serves_visitors_on_loopback() {
    let (server_id, client_id) = ids();
    // A fixed port outside the shared dynamic pool: with SO_REUSEADDR a loopback
    // bind and another test's wildcard bind can hold the same port at once, and
    // loopback traffic then reaches whichever is more specific.
    let granted = free_port_outside_pool(&[]).await;
    let grant = granted.to_string();
    let mut cfg = server_config(&[("test", client_id.public_key(), &[&grant])]);
    cfg.bind = Ipv4Addr::LOCALHOST.into();
    let server = start_server(&cfg, &server_id);
    let echo = echo_server().await;
    let mut client = start_client(
        client_config(
            server.addr,
            server.key,
            &[("echo", &echo.to_string(), &granted.to_string())],
        ),
        client_id,
    );
    let port = expect_bound(&mut client.events, "echo").await;
    assert_eq!(port.number, granted);

    let mut visitor = TcpStream::connect(("127.0.0.1", port.number))
        .await
        .unwrap();
    visitor.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    visitor.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");

    // A second loopback address reaches a wildcard listener but not a bound one.
    // Linux has all of 127/8 up; elsewhere the address is absent and the connect
    // fails for a different reason, which proves nothing, so only assert there.
    if cfg!(target_os = "linux") {
        let elsewhere = TcpStream::connect(("127.0.0.2", port.number)).await;
        assert!(
            elsewhere.is_err(),
            "bound to 127.0.0.1 yet answered on 127.0.0.2"
        );
    }

    client.cancel.cancel();
    server.cancel.cancel();
}

async fn echo_through_the_tunnel(prefer: Prefer, expected: TransportKind) {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await;
    let mut client = start_client(
        client_config_over(
            server.addr,
            server.key,
            &[("echo", &echo.to_string(), "any")],
            prefer,
        ),
        client_id,
    );

    assert!(
        matches!(next_event(&mut client.events).await, Event::Connected { name, transport, .. } if name == "test" && transport == expected)
    );
    let port = expect_bound(&mut client.events, "echo").await;
    assert!(DYNAMIC_PORTS.contains(&port.number));

    let mut visitor = TcpStream::connect(("127.0.0.1", port.number))
        .await
        .unwrap();
    visitor.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    visitor.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");

    client.cancel.cancel();
    assert!(client.task.await.unwrap().is_ok());
    server.cancel.cancel();
    server.task.await.unwrap();
}

#[tokio::test]
async fn echoes_through_the_tunnel_over_quic() {
    echo_through_the_tunnel(Prefer::Auto, TransportKind::Quic).await;
}

#[tokio::test]
async fn echoes_through_the_tunnel_over_tcp() {
    echo_through_the_tunnel(Prefer::Tcp, TransportKind::Tcp).await;
}

#[tokio::test]
async fn a_server_without_the_fallback_accepts_no_tcp_client() {
    let (server_id, client_id) = ids();
    let mut cfg = server_config(&[("test", client_id.public_key(), &[])]);
    cfg.transport.tcp_fallback = false;
    let server = start_server(&cfg, &server_id);

    let over_tcp = start_client(
        client_config_over(server.addr, server.key, &[], Prefer::Tcp),
        Identity::from_pem(&client_id.to_pem()).unwrap(),
    );
    let result = tokio::time::timeout(Duration::from_secs(5), over_tcp.task)
        .await
        .expect("the TCP dial ends within 5 s")
        .unwrap();
    assert!(
        matches!(result, Err(ClientError::Transport(_))),
        "{result:?}"
    );

    let mut over_quic = start_client(client_config(server.addr, server.key, &[]), client_id);
    assert!(matches!(
        next_event(&mut over_quic.events).await,
        Event::Connected { .. }
    ));
    over_quic.cancel.cancel();
    server.cancel.cancel();
}

async fn half_close_propagates_to_the_local_service(prefer: Prefer) {
    let exchange = async {
        let (server_id, client_id) = ids();
        let server = start_server(
            &server_config(&[("test", client_id.public_key(), &[])]),
            &server_id,
        );
        let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = local.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = local.accept().await.unwrap();
            let mut all = Vec::new();
            socket.read_to_end(&mut all).await.unwrap();
            socket
                .write_all(all.len().to_string().as_bytes())
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });
        let mut client = start_client(
            client_config_over(
                server.addr,
                server.key,
                &[("len", &local_addr.to_string(), "any")],
                prefer,
            ),
            client_id,
        );
        let port = expect_bound(&mut client.events, "len").await;

        let mut visitor = TcpStream::connect(("127.0.0.1", port.number))
            .await
            .unwrap();
        visitor.write_all(b"abc").await.unwrap();
        visitor.shutdown().await.unwrap();
        let mut reply = String::new();
        visitor.read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, "3");
        server.cancel.cancel();
    };
    tokio::time::timeout(Duration::from_secs(10), exchange)
        .await
        .expect("the half-closed round trip finishes within 10 s");
}

#[tokio::test]
async fn half_close_propagates_to_the_local_service_over_quic() {
    half_close_propagates_to_the_local_service(Prefer::Auto).await;
}

#[tokio::test]
async fn half_close_propagates_to_the_local_service_over_tcp() {
    half_close_propagates_to_the_local_service(Prefer::Tcp).await;
}

async fn transfers_intact(prefer: Prefer, total: usize) {
    let transfer = async {
        let (server_id, client_id) = ids();
        let server = start_server(
            &server_config(&[("test", client_id.public_key(), &[])]),
            &server_id,
        );
        let echo = echo_server().await;
        let mut client = start_client(
            client_config_over(
                server.addr,
                server.key,
                &[("echo", &echo.to_string(), "any")],
                prefer,
            ),
            client_id,
        );
        let port = expect_bound(&mut client.events, "echo").await;

        let visitor = TcpStream::connect(("127.0.0.1", port.number))
            .await
            .unwrap();
        let (mut rd, mut wr) = visitor.into_split();
        let writer = tokio::spawn(async move {
            let chunk: Vec<u8> = (0..65536u32)
                .map(|i| u8::try_from(i % 251).expect("a remainder below 251 fits a byte"))
                .collect();
            let mut sent = 0;
            while sent < total {
                let n = chunk.len().min(total - sent);
                wr.write_all(&chunk[..n]).await.unwrap();
                sent += n;
            }
            wr.shutdown().await.unwrap();
        });
        let mut got = 0usize;
        let mut buf = vec![0u8; 65536];
        let mut expected = 0u32;
        while got < total {
            let n = rd.read(&mut buf).await.unwrap();
            assert!(n > 0, "eof after {got} bytes");
            for &b in &buf[..n] {
                assert_eq!(u32::from(b), expected % 251, "corruption at byte {got}");
                expected = (expected + 1) % 65536;
            }
            got += n;
        }
        // The echo ended the stream once it had it all, so the visitor must read that end and not
        // a reset: the pump aborts its socket unless both directions finished.
        let end = rd.read(&mut buf).await;
        assert!(
            matches!(end, Ok(0)),
            "the transfer did not end cleanly: {end:?}"
        );
        writer.await.unwrap();
        server.cancel.cancel();
    };
    tokio::time::timeout(Duration::from_secs(60), transfer)
        .await
        .expect("the round trip finishes within 60 s");
}

#[tokio::test]
async fn transfers_100_mib_intact_over_quic() {
    transfers_intact(Prefer::Auto, 100 * 1024 * 1024).await;
}

/// Smaller than the QUIC variant to keep CI quick, not because the TCP path caps out here.
#[tokio::test]
async fn transfers_8_mib_intact_over_tcp() {
    transfers_intact(Prefer::Tcp, 8 * 1024 * 1024).await;
}

/// 512 clears both multiplexers' stock ceilings: quinn grants 100 concurrent bidi streams, and
/// yamux parks a new outbound stream past an unacknowledged backlog of 256 — a private constant
/// with no setter, so a failure here cannot be tuned away.
async fn holds_512_visitor_streams_open_at_once(prefer: Prefer) {
    let _ = rlimit::increase_nofile_limit(8192);
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await;
    let mut client = start_client(
        client_config_over(
            server.addr,
            server.key,
            &[("echo", &echo.to_string(), "any")],
            prefer,
        ),
        client_id,
    );
    let port = expect_bound(&mut client.events, "echo").await;

    let burst = async {
        let mut open = Vec::with_capacity(512);
        for i in 0..512u16 {
            let mut v = TcpStream::connect(("127.0.0.1", port.number))
                .await
                .unwrap();
            v.write_all(&i.to_le_bytes()).await.unwrap();
            open.push((i, v));
        }
        for (i, v) in &mut open {
            let mut buf = [0u8; 2];
            v.read_exact(&mut buf).await.unwrap();
            assert_eq!(u16::from_le_bytes(buf), *i);
        }
    };
    tokio::time::timeout(Duration::from_secs(20), burst)
        .await
        .expect("512 concurrent streams within 20 s");
    server.cancel.cancel();
}

#[tokio::test]
async fn holds_512_visitor_streams_open_at_once_over_quic() {
    holds_512_visitor_streams_open_at_once(Prefer::Auto).await;
}

#[tokio::test]
async fn holds_512_visitor_streams_open_at_once_over_tcp() {
    holds_512_visitor_streams_open_at_once(Prefer::Tcp).await;
}

#[tokio::test]
async fn fixed_ports_need_a_grant() {
    let (server_id, client_id) = ids();
    let granted = free_port_outside_pool(&[]).await;
    let denied = free_port_outside_pool(&[granted]).await;
    let grant = granted.to_string();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&grant])]),
        &server_id,
    );
    let echo = echo_server().await.to_string();
    let mut client = start_client(
        client_config(
            server.addr,
            server.key,
            &[
                ("ok", &echo, &granted.to_string()),
                ("nope", &echo, &denied.to_string()),
            ],
        ),
        client_id,
    );
    let mut ok = None;
    let mut nope = None;
    while ok.is_none() || nope.is_none() {
        match next_event(&mut client.events).await {
            Event::Bound { service, port } if service == "ok" => ok = Some(port.number),
            Event::BindFailed { service, reason } if service == "ok" => {
                panic!("granted port {granted} was refused: {reason}")
            }
            Event::BindFailed { service, reason } if service == "nope" => nope = Some(reason),
            Event::Bound { service, port } if service == "nope" => {
                panic!("ungranted port {denied} was bound as {port}")
            }
            _ => {}
        }
    }
    assert_eq!(ok, Some(granted));
    assert_eq!(nope, Some(BindFailure::NotGranted));
    server.cancel.cancel();
}

#[tokio::test]
async fn unknown_key_is_denied_with_its_own_key() {
    let (server_id, client_id) = ids();
    let server = start_server(&server_config(&[]), &server_id);
    let expected = client_id.public_key();
    let mut client = start_client(client_config(server.addr, server.key, &[]), client_id);
    assert_eq!(
        next_event(&mut client.events).await,
        Event::Denied { key: expected }
    );
    assert!(matches!(client.task.await.unwrap(), Err(ClientError::Denied(k)) if k == expected));
    server.cancel.cancel();
}

#[tokio::test]
async fn wrong_server_key_fails_before_any_control_message() {
    let (server_id, client_id) = ids();
    let impostor = Identity::generate().unwrap();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut client = start_client(
        client_config(server.addr, impostor.public_key(), &[]),
        client_id,
    );
    let result = tokio::time::timeout(Duration::from_secs(10), client.task)
        .await
        .expect("the client gives up on the impostor within 10 s")
        .unwrap();
    // `Auto` tries the fallback too, and the impostor fails the pin on both.
    let Err(ClientError::NoTransport { quic, tcp }) = result else {
        panic!("{result:?}");
    };
    assert!(
        matches!(
            *quic,
            ClientError::Quic(QuicError::Connection(
                quinn::ConnectionError::TransportError(_)
            ))
        ),
        "{quic:?}"
    );
    assert!(matches!(*tcp, ClientError::Transport(_)), "{tcp:?}");
    assert!(client.events.try_recv().is_err());
    server.cancel.cancel();
}

/// The client answered on this stream by resetting it with `code`.
async fn expect_reset(r: &mut quinn::RecvStream, code: u32, what: &str) {
    let refusal = tokio::time::timeout(Duration::from_secs(2), r.read_to_end(16)).await;
    assert!(
        matches!(
            &refusal,
            Ok(Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(got))))
                if *got == quinn::VarInt::from_u32(code)
        ),
        "client did not reset {what} with {code:#x}: {refusal:?}"
    );
}

#[tokio::test]
async fn a_stream_for_an_unbound_service_never_dials_local() {
    use futures_util::{SinkExt, StreamExt};
    use hawse_core::frame::write_frame;
    use hawse_core::tls;
    use hawse_core::transport::SendHalf;
    use hawse_core::transport::quic::{self, Tuning};
    use hawse_proto::frame::{codec, decode, encode};
    use hawse_proto::msg::{ClientMessage, ServerMessage, StreamHeader, StreamOpen, reset};
    use tokio_util::codec::{FramedRead, FramedWrite};

    let (server_id, client_id) = ids();
    let (cert, key) = server_id.certificate().unwrap();
    let rogue = quic::listen(
        "127.0.0.1:0".parse().unwrap(),
        tls::server_config(cert, key, tls::provider()).unwrap(),
        Tuning::SERVER,
    )
    .unwrap();
    let rogue_addr = rogue.local_addr().unwrap();

    let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = local.local_addr().unwrap();
    let mut client = start_client(
        client_config(
            rogue_addr,
            server_id.public_key(),
            &[("svc", &local_addr.to_string(), "any")],
        ),
        client_id,
    );

    let conn = rogue.accept().await.unwrap().await.unwrap();
    let (send, recv) = conn.accept_bi().await.unwrap();
    let mut tx = FramedWrite::new(send, codec());
    let mut rx = FramedRead::new(recv, codec());
    assert!(matches!(
        decode::<ClientMessage>(&rx.next().await.unwrap().unwrap()).unwrap(),
        ClientMessage::Hello { .. }
    ));
    tx.send(
        encode(&ServerMessage::Welcome {
            agent: "rogue".into(),
            client_name: "test".into(),
        })
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        decode::<ClientMessage>(&rx.next().await.unwrap().unwrap()).unwrap(),
        ClientMessage::Bind { .. }
    ));
    tx.send(
        encode(&ServerMessage::Bound {
            service: "svc".into(),
            service_id: 7,
            port: 40000,
            address: Ipv4Addr::LOCALHOST.into(),
        })
        .unwrap(),
    )
    .await
    .unwrap();
    expect_bound(&mut client.events, "svc").await;

    let header = |service_id| StreamHeader {
        service_id,
        visitor: "203.0.113.9:1".parse().unwrap(),
        listener: "203.0.113.1:40000".parse().unwrap(),
    };
    let (s, mut r) = conn.open_bi().await.unwrap();
    let mut s = SendHalf::Quic(s);
    write_frame(&mut s, &StreamOpen::Visitor(header(99)))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(500), local.accept())
            .await
            .is_err(),
        "client dialed local for an id it never bound"
    );
    expect_reset(&mut r, reset::UNKNOWN_SERVICE, "an unknown service").await;

    let (s, mut r) = conn.open_bi().await.unwrap();
    let mut s = SendHalf::Quic(s);
    hawse_core::frame::write_body(&mut s, &[0xff])
        .await
        .unwrap();
    expect_reset(
        &mut r,
        reset::UNEXPECTED_STREAM,
        "a stream with no StreamOpen",
    )
    .await;

    let (s, _r) = conn.open_bi().await.unwrap();
    let mut s = SendHalf::Quic(s);
    write_frame(&mut s, &StreamOpen::Visitor(header(7)))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), local.accept())
            .await
            .is_ok(),
        "client did not dial local for the bound id"
    );
    client.cancel.cancel();
}

#[tokio::test]
async fn server_shutdown_tells_the_client_why() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await;
    let cfg = client_config(
        server.addr,
        server.key,
        &[("echo", &echo.to_string(), "any")],
    );
    // `Event::Disconnected` is only emitted by the retry loop, so this test drives
    // `run` directly rather than the `run_once` harness `start_client` uses.
    let (tx, mut events) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let client = Client::new(cfg, client_id);
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move { client.run(cancel, tx).await }
    });
    expect_bound(&mut events, "echo").await;

    server.cancel.cancel();
    match next_event(&mut events).await {
        Event::Disconnected {
            cause: DisconnectCause::Shutdown(why),
            retry_in: Some(wait),
        } => {
            assert!(why.contains("shut"));
            assert!(wait <= Duration::from_secs(1), "first retry after {wait:?}");
        }
        other => panic!("expected shutdown disconnect, got {other:?}"),
    }

    cancel.cancel();
    task.await.unwrap();
    server.task.await.unwrap();
}

#[tokio::test]
async fn a_config_that_cannot_be_retried_stops_the_client() {
    let (server_id, client_id) = ids();
    let mut cfg = client_config("127.0.0.1:1".parse().unwrap(), server_id.public_key(), &[]);
    cfg.server = String::new();

    let (tx, mut events) = mpsc::channel(256);
    let client = Client::new(cfg, client_id);
    let task = tokio::spawn(async move { client.run(CancellationToken::new(), tx).await });

    match next_event(&mut events).await {
        Event::Disconnected {
            cause: DisconnectCause::Config(_),
            retry_in: None,
        } => {}
        other => panic!("expected a final disconnect, got {other:?}"),
    }
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("run returns without retrying")
        .unwrap();
}

#[tokio::test]
async fn a_client_cancelled_mid_dial_announces_no_retry() {
    let (server_id, client_id) = ids();
    // Nothing answers UDP here, so the dial can only end at the shortened idle timeout, well
    // after `cancel` has fired.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = client_config(listener.local_addr().unwrap(), server_id.public_key(), &[]);
    cfg.transport.idle_timeout = Duration::from_secs(1);

    let (tx, mut events) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let client = Client::new(cfg, client_id);
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move { client.run(cancel, tx).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();

    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("run returns once the dial fails")
        .unwrap();
    while let Some(event) = events.recv().await {
        assert!(
            !matches!(
                event,
                Event::Disconnected {
                    retry_in: Some(_),
                    ..
                }
            ),
            "announced a retry after cancel: {event:?}"
        );
    }
}

#[tokio::test]
async fn a_reconnecting_client_supersedes_its_zombie_session() {
    let (server_id, client_id) = ids();
    let granted = free_port_outside_pool(&[]).await;
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&granted.to_string()])]),
        &server_id,
    );
    let echo = echo_server().await.to_string();
    let cfg = client_config(
        server.addr,
        server.key,
        &[("svc", &echo, &granted.to_string())],
    );
    let twin = Identity::from_pem(&client_id.to_pem()).unwrap();
    let mut first = start_client(cfg.clone(), twin);
    assert_eq!(expect_bound(&mut first.events, "svc").await.number, granted);

    // Aborting sends no close frame, so the server still holds the port for the dead session.
    first.task.abort();

    let mut second = start_client(cfg, client_id);
    let bound = tokio::time::timeout(
        Duration::from_secs(10),
        expect_bound(&mut second.events, "svc"),
    )
    .await
    .expect("the reconnecting client binds within 10 s");
    assert_eq!(bound.number, granted);
    second.cancel.cancel();
    server.cancel.cancel();
}

/// The first session's `ClientError::Shutdown` is what matters here: closing discards whatever is
/// still in flight, so reading that message at all proves the shutdown linger held this transport
/// open until the client had it.
async fn a_second_session_for_the_same_key_supersedes_the_first(prefer: Prefer) {
    let (server_id, client_id) = ids();
    let granted = free_port_outside_pool(&[]).await;
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&granted.to_string()])]),
        &server_id,
    );
    let echo = echo_server().await.to_string();
    let cfg = client_config_over(
        server.addr,
        server.key,
        &[("svc", &echo, &granted.to_string())],
        prefer,
    );
    let twin = Identity::from_pem(&client_id.to_pem()).unwrap();
    let mut first = start_client(cfg.clone(), twin);
    assert_eq!(expect_bound(&mut first.events, "svc").await.number, granted);

    let mut second = start_client(cfg, client_id);
    let bound = tokio::time::timeout(
        Duration::from_secs(10),
        expect_bound(&mut second.events, "svc"),
    )
    .await
    .expect("the second session binds within 10 s");
    assert_eq!(bound.number, granted);

    let outcome = tokio::time::timeout(Duration::from_secs(10), first.task)
        .await
        .expect("the first session ends within 10 s")
        .unwrap();
    assert!(
        matches!(outcome, Err(ClientError::Shutdown(_))),
        "{outcome:?}"
    );
    second.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_second_session_for_the_same_key_supersedes_the_first_over_quic() {
    a_second_session_for_the_same_key_supersedes_the_first(Prefer::Auto).await;
}

#[tokio::test]
async fn a_second_session_for_the_same_key_supersedes_the_first_over_tcp() {
    a_second_session_for_the_same_key_supersedes_the_first(Prefer::Tcp).await;
}

/// A server from before the wire changed: ours in every way but the protocol it offers.
fn old_server_tls(identity: &Identity) -> rustls::ServerConfig {
    let (cert, key) = identity.certificate().unwrap();
    let mut cfg = hawse_core::tls::server_config(cert, key, hawse_core::tls::provider()).unwrap();
    cfg.alpn_protocols = vec![b"hawse/1".to_vec()];
    cfg
}

#[tokio::test]
async fn an_old_server_is_reported_as_a_version_mismatch_over_quic() {
    use hawse_core::transport::quic::{self, Tuning};

    let (server_id, client_id) = ids();
    let old = quic::listen(
        "127.0.0.1:0".parse().unwrap(),
        old_server_tls(&server_id),
        Tuning::SERVER,
    )
    .unwrap();
    let addr = old.local_addr().unwrap();
    // The endpoint only answers while something drives its handshakes.
    tokio::spawn(async move {
        while let Some(incoming) = old.accept().await {
            let _ = incoming.await;
        }
    });

    let client = start_client(
        client_config_over(addr, server_id.public_key(), &[], Prefer::Quic),
        client_id,
    );
    let result = tokio::time::timeout(Duration::from_secs(5), client.task)
        .await
        .expect("the dial ends within 5 s")
        .unwrap();
    assert!(matches!(result, Err(ClientError::Version)), "{result:?}");
}

#[tokio::test]
async fn an_old_server_is_reported_as_a_version_mismatch_over_tcp() {
    use hawse_core::transport::quic::Tuning;
    use hawse_core::transport::tcp;

    let (server_id, client_id) = ids();
    let tls = Arc::new(old_server_tls(&server_id));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let _ = tcp::accept(socket, Arc::clone(&tls), Tuning::SERVER).await;
        }
    });

    let client = start_client(
        client_config_over(addr, server_id.public_key(), &[], Prefer::Tcp),
        client_id,
    );
    let result = tokio::time::timeout(Duration::from_secs(5), client.task)
        .await
        .expect("the dial ends within 5 s")
        .unwrap();
    assert!(matches!(result, Err(ClientError::Version)), "{result:?}");
}

/// The TCP twin of `a_stream_for_an_unbound_service_never_dials_local`: a server driven by hand,
/// so the code can be read where it arrives.
#[tokio::test]
async fn a_refusal_carries_its_code_over_tcp() {
    use hawse_core::control::Control;
    use hawse_core::frame::write_frame;
    use hawse_core::tls;
    use hawse_core::transport::quic::Tuning;
    use hawse_core::transport::{Transport, reset_code, tcp};
    use hawse_proto::msg::{ClientMessage, ServerMessage, StreamHeader, StreamOpen, reset};

    let (server_id, client_id) = ids();
    let (cert, key) = server_id.certificate().unwrap();
    let server_tls = Arc::new(tls::server_config(cert, key, tls::provider()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rogue_addr = listener.local_addr().unwrap();

    // Bound and let go, so nothing listens where the client will dial.
    let dead = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let mut client = start_client(
        client_config_over(
            rogue_addr,
            server_id.public_key(),
            &[("svc", &dead.to_string(), "any")],
            Prefer::Tcp,
        ),
        client_id,
    );

    let (socket, _) = listener.accept().await.unwrap();
    let rogue: Arc<dyn Transport> = Arc::new(
        tcp::accept(socket, server_tls, Tuning::SERVER)
            .await
            .unwrap(),
    );
    let (send, recv) = rogue.accept_bi().await.unwrap();
    let mut control = Control::new(send, recv);
    assert!(matches!(
        control.next::<ClientMessage>().await,
        Some(ClientMessage::Hello { .. })
    ));
    control
        .send(&ServerMessage::Welcome {
            agent: "rogue".into(),
            client_name: "test".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        control.next::<ClientMessage>().await,
        Some(ClientMessage::Bind { .. })
    ));
    control
        .send(&ServerMessage::Bound {
            service: "svc".into(),
            service_id: 7,
            port: 40000,
            address: Ipv4Addr::LOCALHOST.into(),
        })
        .await
        .unwrap();
    expect_bound(&mut client.events, "svc").await;

    let header = |service_id| StreamHeader {
        service_id,
        visitor: "203.0.113.9:1".parse().unwrap(),
        listener: "203.0.113.1:40000".parse().unwrap(),
    };
    for (service_id, code) in [(99, reset::UNKNOWN_SERVICE), (7, reset::LOCAL_REFUSED)] {
        let (mut send, mut recv) = rogue.open_bi().await.unwrap();
        write_frame(&mut send, &StreamOpen::Visitor(header(service_id)))
            .await
            .unwrap();
        let mut rest = Vec::new();
        let err = tokio::time::timeout(Duration::from_secs(5), recv.read_to_end(&mut rest))
            .await
            .expect("a refusal within 5 s")
            .expect_err("a refusal must fail the read, not end it");
        assert_eq!(reset_code(&err), Some(code), "{err:?}");
    }
    client.cancel.cancel();
}

/// A server, and a client exposing `local` as `svc` on a dynamic port over `prefer`.
async fn tunnel_to(local: SocketAddr, prefer: Prefer) -> (RunningServer, RunningClient, u16) {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut client = start_client(
        client_config_over(
            server.addr,
            server.key,
            &[("svc", &local.to_string(), "any")],
            prefer,
        ),
        client_id,
    );
    let port = expect_bound(&mut client.events, "svc").await.number;
    (server, client, port)
}

async fn a_local_service_that_resets_gives_the_visitor_a_reset(prefer: Prefer) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 5];
        socket.read_exact(&mut buf).await.unwrap();
        socket.set_zero_linger().unwrap();
    });
    let (server, client, port) = tunnel_to(local, prefer).await;

    let mut visitor = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    visitor.write_all(b"hello").await.unwrap();
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), visitor.read_to_end(&mut rest))
        .await
        .expect("the visitor's connection ends within 5 s");
    let err = read.expect_err("a service that reset must not read as a clean end");
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset, "{err:?}");
    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_local_service_that_resets_gives_the_visitor_a_reset_over_quic() {
    a_local_service_that_resets_gives_the_visitor_a_reset(Prefer::Quic).await;
}

#[tokio::test]
async fn a_local_service_that_resets_gives_the_visitor_a_reset_over_tcp() {
    a_local_service_that_resets_gives_the_visitor_a_reset(Prefer::Tcp).await;
}

async fn nothing_listening_on_local_gives_the_visitor_a_reset(prefer: Prefer) {
    // Bound and let go, so nothing listens where the client will dial.
    let dead = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (server, client, port) = tunnel_to(dead, prefer).await;

    // Only reads from here on. A socket reports a reset once, to whichever call meets it first,
    // so a visitor that kept writing could see it there and read end-of-stream after.
    let mut visitor = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut rest = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), visitor.read_to_end(&mut rest))
        .await
        .expect("the visitor's connection ends within 5 s");
    let err = read.expect_err("a refused visitor must not read a clean end");
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset, "{err:?}");
    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn nothing_listening_on_local_gives_the_visitor_a_reset_over_quic() {
    nothing_listening_on_local_gives_the_visitor_a_reset(Prefer::Quic).await;
}

#[tokio::test]
async fn nothing_listening_on_local_gives_the_visitor_a_reset_over_tcp() {
    nothing_listening_on_local_gives_the_visitor_a_reset(Prefer::Tcp).await;
}

async fn a_visitor_that_resets_gives_the_local_service_a_reset(prefer: Prefer) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    let (seen, hello_arrived) = oneshot::channel();
    let ending = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 5];
        socket.read_exact(&mut buf).await.unwrap();
        seen.send(()).unwrap();
        let mut rest = Vec::new();
        socket.read_to_end(&mut rest).await
    });
    let (server, client, port) = tunnel_to(local, prefer).await;

    let mut visitor = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    visitor.write_all(b"hello").await.unwrap();
    // Only once the service has its bytes: a reset can overtake data still on its way.
    hello_arrived.await.unwrap();
    visitor.set_zero_linger().unwrap();
    drop(visitor);

    let read = tokio::time::timeout(Duration::from_secs(5), ending)
        .await
        .expect("the service's connection ends within 5 s")
        .unwrap();
    let err = read.expect_err("a visitor that reset must not read as a clean end");
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset, "{err:?}");
    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_visitor_that_resets_gives_the_local_service_a_reset_over_quic() {
    a_visitor_that_resets_gives_the_local_service_a_reset(Prefer::Quic).await;
}

#[tokio::test]
async fn a_visitor_that_resets_gives_the_local_service_a_reset_over_tcp() {
    a_visitor_that_resets_gives_the_local_service_a_reset(Prefer::Tcp).await;
}
