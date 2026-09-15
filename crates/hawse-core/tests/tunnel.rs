mod common;

use std::time::Duration;

use common::{
    client_config, echo_server, expect_bound, free_port, next_event, server_config, start_client,
    start_server,
};
use hawse_core::client::{ClientError, Event};
use hawse_core::identity::Identity;
use hawse_core::transport::quic::QuicError;
use hawse_proto::msg::BindFailure;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn ids() -> (Identity, Identity) {
    (Identity::generate().unwrap(), Identity::generate().unwrap())
}

#[tokio::test]
async fn tcp_echo_through_the_tunnel() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await;
    let mut client = start_client(
        client_config(
            server.addr,
            server.key,
            &[("echo", &echo.to_string(), "any")],
        ),
        client_id,
    );

    assert!(
        matches!(next_event(&mut client.events).await, Event::Connected { name, .. } if name == "test")
    );
    let port = expect_bound(&mut client.events, "echo").await;
    assert!((47000..=47999).contains(&port.number));

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
async fn half_close_propagates_to_the_local_service() {
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
        client_config(
            server.addr,
            server.key,
            &[("len", &local_addr.to_string(), "any")],
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
}

#[tokio::test]
async fn transfers_100_mib_intact() {
    const TOTAL: usize = 100 * 1024 * 1024;
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await;
    let mut client = start_client(
        client_config(
            server.addr,
            server.key,
            &[("echo", &echo.to_string(), "any")],
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
        while sent < TOTAL {
            let n = chunk.len().min(TOTAL - sent);
            wr.write_all(&chunk[..n]).await.unwrap();
            sent += n;
        }
        wr.shutdown().await.unwrap();
    });
    let mut got = 0usize;
    let mut buf = vec![0u8; 65536];
    let mut expected = 0u32;
    while got < TOTAL {
        let n = rd.read(&mut buf).await.unwrap();
        assert!(n > 0, "eof after {got} bytes");
        for &b in &buf[..n] {
            assert_eq!(u32::from(b), expected % 251, "corruption at byte {got}");
            expected = (expected + 1) % 65536;
        }
        got += n;
    }
    writer.await.unwrap();
    server.cancel.cancel();
}

#[tokio::test]
async fn holds_512_visitor_streams_open_at_once() {
    let _ = rlimit::increase_nofile_limit(8192);
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await;
    let mut client = start_client(
        client_config(
            server.addr,
            server.key,
            &[("echo", &echo.to_string(), "any")],
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
        .expect("512 streams within 20 s; quinn's default credit is 100");
    server.cancel.cancel();
}

#[tokio::test]
async fn fixed_ports_need_a_grant() {
    let (server_id, client_id) = ids();
    let granted = free_port().await;
    let denied = free_port().await;
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
            Event::BindFailed { service, reason } if service == "nope" => nope = Some(reason),
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
    let result = client.task.await.unwrap();
    assert!(
        matches!(result, Err(ClientError::Quic(QuicError::Connection(_)))),
        "{result:?}"
    );
    assert!(client.events.try_recv().is_err());
    server.cancel.cancel();
}

#[tokio::test]
async fn a_stream_for_an_unbound_service_never_dials_local() {
    use futures_util::{SinkExt, StreamExt};
    use hawse_core::frame::write_frame;
    use hawse_core::tls;
    use hawse_core::transport::quic::{self, Tuning};
    use hawse_proto::frame::{codec, decode, encode};
    use hawse_proto::msg::{ClientMessage, ServerMessage, StreamHeader};
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
    let (mut s, _r) = conn.open_bi().await.unwrap();
    write_frame(&mut s, &header(99)).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(500), local.accept())
            .await
            .is_err(),
        "client dialed local for an id it never bound"
    );

    let (mut s, _r) = conn.open_bi().await.unwrap();
    write_frame(&mut s, &header(7)).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), local.accept())
            .await
            .is_ok(),
        "client did not dial local for the bound id"
    );
    client.cancel.cancel();
}
